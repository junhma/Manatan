use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, RwLock, atomic::AtomicUsize},
};
use tracing::warn;

use serde::{Deserialize, Serialize};

use crate::logic::OcrResult;

#[derive(Clone, Copy, Serialize, Debug)]
pub struct JobProgress {
    pub current: usize,
    pub total: usize,
}

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
    pub cache_path: PathBuf,
    pub active_jobs: Arc<AtomicUsize>,
    pub requests_processed: Arc<AtomicUsize>,
    pub active_chapter_jobs: Arc<RwLock<HashMap<String, JobProgress>>>,
    pub chapter_pages_map: Arc<RwLock<HashMap<String, usize>>>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub context: String,
    pub data: Vec<OcrResult>,
}

// Struct for the persistent state (cache and metadata)
#[derive(Serialize, Deserialize, Default)]
struct PersistentState {
    cache: HashMap<String, CacheEntry>,
    chapter_pages_map: HashMap<String, usize>,
}

impl AppState {
    pub fn new(cache_dir: PathBuf) -> Self {
        let cache_path = cache_dir.join("ocr-cache.json");

        let persistent_state = if cache_path.exists() {
            if let Ok(file) = fs::File::open(&cache_path) {
                serde_json::from_reader(file).unwrap_or_else(|e| {
                    warn!("Failed to deserialize cache file: {e}. Starting fresh.");
                    PersistentState::default()
                })
            } else {
                warn!("Failed to open cache file. Starting fresh.");
                PersistentState::default()
            }
        } else {
            PersistentState::default()
        };

        Self {
            cache: Arc::new(RwLock::new(persistent_state.cache)),
            chapter_pages_map: Arc::new(RwLock::new(persistent_state.chapter_pages_map)),
            cache_path,
            active_jobs: Arc::new(AtomicUsize::new(0)),
            requests_processed: Arc::new(AtomicUsize::new(0)),
            active_chapter_jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn save_cache(&self) {
        let state_to_save = {
            let cache = self.cache.read().expect("cache lock poisoned");
            let pages_map = self
                .chapter_pages_map
                .read()
                .expect("pages map lock poisoned");

            let state = PersistentState {
                cache: cache.clone(),
                chapter_pages_map: pages_map.clone(),
            };
            serde_json::to_vec_pretty(&state).unwrap_or_default()
        };

        let tmp_path = self.cache_path.with_extension("tmp");

        if let Ok(mut file) = fs::File::create(&tmp_path) {
            if file.write_all(&state_to_save).is_ok() {
                let _ = file.sync_all();
                let _ = fs::rename(&tmp_path, &self.cache_path);
            }
        } else {
            tracing::error!("Failed to create temp file for saving cache");
        }
    }
}
