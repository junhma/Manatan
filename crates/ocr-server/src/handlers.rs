use std::{collections::hash_map::Entry, sync::atomic::Ordering};

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    jobs, logic,
    state::{AppState, CacheEntry},
};

#[derive(Deserialize)]
pub struct OcrRequest {
    pub url: String,
    pub user: Option<String>,
    pub pass: Option<String>,
    #[serde(default = "default_context")]
    pub context: String,
}

fn default_context() -> String {
    "No Context".to_string()
}

// --- Handlers ---

pub async fn status_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cache_size = state.cache.read().expect("cache lock poisoned").len();
    Json(serde_json::json!({
        "status": "running",
        "backend": "Rust (mangatan-ocr-server)",
        "requests_processed": state.requests_processed.load(Ordering::Relaxed),
        "items_in_cache": cache_size,
        "active_jobs": state.active_jobs.load(Ordering::Relaxed),
    }))
}

pub async fn ocr_handler(
    State(state): State<AppState>,
    Query(params): Query<OcrRequest>,
) -> Result<Json<Vec<crate::logic::OcrResult>>, (StatusCode, String)> {
    let cache_key = logic::get_cache_key(&params.url);
    info!("OCR Handler: Incoming request for cache_key={}", cache_key);

    info!("OCR Handler: Attempting to acquire cache read lock for check...");
    if let Some(entry) = state.cache.read().expect("lock").get(&cache_key) {
        info!("OCR Handler: Cache HIT for cache_key={}", cache_key);
        state.requests_processed.fetch_add(1, Ordering::Relaxed);
        return Ok(Json(entry.data.clone()));
    }
    info!(
        "OCR Handler: Cache MISS for cache_key={}. Starting processing.",
        cache_key
    );

    let result =
        logic::fetch_and_process(&params.url, params.user.clone(), params.pass.clone()).await;

    match result {
        Ok(data) => {
            state.requests_processed.fetch_add(1, Ordering::Relaxed);
            info!(
                "OCR Handler: Processing successful for cache_key={}",
                cache_key
            );

            info!("OCR Handler: Attempting to acquire cache write lock for insertion...");
            {
                let mut w = state.cache.write().expect("lock");
                info!("OCR Handler: Cache write lock acquired.");
                w.insert(
                    cache_key.clone(),
                    CacheEntry {
                        context: params.context,
                        data: data.clone(),
                    },
                );
                info!("OCR Handler: Cache data inserted. Releasing write lock.");
            }

            info!("OCR Handler: Triggering cache save to disk...");
            state.save_cache();
            info!("OCR Handler: Cache save complete.");

            Ok(Json(data))
        }
        Err(e) => {
            warn!(
                "OCR Handler: Processing FAILED for cache_key={}: {}",
                cache_key, e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

#[derive(Deserialize)]
pub struct JobRequest {
    pub base_url: String,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub context: String,
    pub pages: Option<Vec<String>>,
}

pub async fn is_chapter_preprocessed_handler(
    State(state): State<AppState>,
    Json(req): Json<JobRequest>,
) -> Json<serde_json::Value> {
    let progress = {
        state
            .active_chapter_jobs
            .read()
            .expect("lock poisoned")
            .get(&req.base_url)
            .cloned()
    };

    if let Some(p) = progress {
        return Json(serde_json::json!({
            "status": "processing",
            "progress": p.current,
            "total": p.total
        }));
    }

    let chapter_base_path = logic::get_cache_key(&req.base_url);

    let total = state
        .chapter_pages_map
        .read()
        .expect("pages map lock poisoned")
        .get(&chapter_base_path)
        .cloned();

    let total = match total {
        Some(total) => total,
        None => {
            match logic::resolve_total_pages_from_graphql(&req.base_url, req.user, req.pass).await {
                Ok(total) => {
                    state
                        .chapter_pages_map
                        .write()
                        .expect("lock")
                        .insert(chapter_base_path.clone(), total);
                    state.save_cache();
                    total
                }
                Err(e) => {
                    warn!(
                        "is_chapter_preprocessed_handler: Failed GraphQL fallback: {}",
                        e
                    );
                    return Json(serde_json::json!({ "status": "idle" }));
                }
            }
        }
    };

    let mut cached_count = 0;
    for key in state.cache.read().expect("cache lock poisoned").keys() {
        if key.starts_with(&chapter_base_path) {
            cached_count += 1;
        }
    }
    if cached_count >= total {
        return Json(
            serde_json::json!({ "status": "processed", "cached_count": cached_count, "total_expected": total }),
        );
    }
    return Json(
        serde_json::json!({ "status": "idle", "cached_count": cached_count, "total_expected": total }),
    );
}

pub async fn preprocess_handler(
    State(state): State<AppState>,
    Json(req): Json<JobRequest>,
) -> Json<serde_json::Value> {
    let pages = match req.pages {
        Some(p) => p,
        None => return Json(serde_json::json!({ "error": "No pages provided" })),
    };

    let is_processing = {
        state
            .active_chapter_jobs
            .read()
            .expect("lock poisoned")
            .contains_key(&req.base_url)
    };

    if is_processing {
        return Json(serde_json::json!({ "status": "already_processing" }));
    }

    let state_clone = state.clone();
    tokio::spawn(async move {
        jobs::run_chapter_job(
            state_clone,
            req.base_url,
            pages,
            req.user,
            req.pass,
            req.context,
        )
        .await;
    });

    Json(serde_json::json!({ "status": "started" }))
}

pub async fn purge_cache_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut cache = state.cache.write().expect("lock");
    cache.clear();

    drop(cache);

    state.save_cache();
    Json(serde_json::json!({ "status": "cleared" }))
}

pub async fn export_cache_handler(
    State(state): State<AppState>,
) -> Json<std::collections::HashMap<String, CacheEntry>> {
    let cache = state.cache.read().expect("lock");
    Json(cache.clone())
}

pub async fn import_cache_handler(
    State(state): State<AppState>,
    Json(data): Json<std::collections::HashMap<String, CacheEntry>>,
) -> Json<serde_json::Value> {
    let mut added = 0;

    {
        let mut cache = state.cache.write().expect("lock");
        for (k, v) in data {
            if let Entry::Vacant(e) = cache.entry(k) {
                e.insert(v);
                added += 1;
            }
        }
    }

    if added > 0 {
        state.save_cache();
    }
    Json(serde_json::json!({ "message": "Import successful", "added": added }))
}
