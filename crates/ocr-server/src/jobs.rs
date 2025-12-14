use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::StreamExt;
use tokio::sync::Mutex;

use crate::state::{AppState, JobProgress};

pub async fn run_chapter_job(
    state: AppState,
    base_url: String,
    pages: Vec<String>,
    user: Option<String>,
    pass: Option<String>,
    context: String,
) {
    let total = pages.len();
    let job_id = base_url.clone();

    {
        state
            .active_chapter_jobs
            .write()
            .expect("lock poisoned")
            .insert(base_url.clone(), JobProgress { current: 0, total });
    }

    state.active_jobs.fetch_add(1, Ordering::Relaxed);
    tracing::info!("[Job] Started for {} ({} pages)", context, total);

    let completed_counter = Arc::new(AtomicUsize::new(0));
    let save_lock = Arc::new(Mutex::new(()));
    let stream = futures::stream::iter(pages.into_iter());

    stream
        .for_each_concurrent(6, |url| {
            let state = state.clone();
            let base_url = base_url.clone();
            let user = user.clone();
            let pass = pass.clone();
            let context = context.clone();
            let completed_counter = completed_counter.clone();
            let save_lock = save_lock.clone();

            let page_id = url.split('/').last().unwrap_or("unknown").to_string();

            async move {
                let cache_key = crate::logic::get_cache_key(&url);
                let exists = { state.cache.read().expect("lock").contains_key(&cache_key) };
                if exists {
                    tracing::info!("[Page {page_id}] Skip (Cached)");
                } else {
                    tracing::info!("[Page {page_id}] Starting fetch_and_process (Async)...");

                    match crate::logic::fetch_and_process(&url, user, pass).await {
                        Ok(res) => {
                            state.cache.write().expect("lock").insert(
                                cache_key,
                                crate::state::CacheEntry {
                                    context: context.clone(),
                                    data: res,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::warn!("[Page {page_id}] Failed: {err:?}");
                        }
                    }
                }

                let current = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;

                {
                    if let Some(prog) = state
                        .active_chapter_jobs
                        .write()
                        .expect("lock")
                        .get_mut(&base_url)
                    {
                        prog.current = current;
                    }
                }

                if current % 5 == 0 {
                    if let Ok(_guard) = save_lock.try_lock() {
                        state.save_cache();
                    }
                }
            }
        })
        .await;

    // Final Save
    tracing::info!("[Job {job_id}] Final save...");
    state.save_cache();
    tracing::info!("[Job {job_id}] Final save complete.");

    state.active_jobs.fetch_sub(1, Ordering::Relaxed);

    {
        state
            .active_chapter_jobs
            .write()
            .expect("lock poisoned")
            .remove(&base_url);
    }

    tracing::info!("[Job {job_id}] Finished for {}", context);
}
