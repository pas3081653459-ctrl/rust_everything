use crate::{
    commands::{NodeInfoRequest, SearchJob, WatchConfigUpdate},
    lifecycle::{APP_QUIT, AppLifecycleState, load_app_state, update_app_state},
    monitoring::{MonitoringStatus, publish as publish_monitoring},
};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use cardinal_sdk::{EventFlag, EventWatcher, FsEvent};
use crossbeam_channel::{Receiver, Sender};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rayon::spawn;
use search_cache::{
    HandleFSEError, SearchCache, SearchOptions, SearchResultNode, SlabIndex, WalkData,
};
use search_cancel::CancellationToken;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter};
use tracing::{error, info};

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusBarUpdate {
    pub scanned_files: usize,
    pub processed_events: usize,
    pub rescan_errors: usize,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IconPayload {
    pub slab_index: SlabIndex,
    pub icon: String,
}

pub struct BackgroundLoopChannels {
    pub finish_rx: Receiver<()>,
    pub update_window_state_rx: Receiver<()>,
    pub search_rx: Receiver<SearchJob>,
    pub node_info_rx: Receiver<NodeInfoRequest>,
    pub icon_viewport_rx: Receiver<(u64, Vec<SlabIndex>)>,
    pub rescan_rx: Receiver<CancellationToken>,
    pub watch_config_rx: Receiver<WatchConfigUpdate>,
    pub icon_update_tx: Sender<IconPayload>,
}

pub fn reset_status_bar(app_handle: &AppHandle) {
    app_handle
        .emit(
            "status_bar_update",
            StatusBarUpdate {
                scanned_files: 0,
                processed_events: 0,
                rescan_errors: 0,
            },
        )
        .unwrap();
}

pub fn emit_status_bar_update(
    app_handle: &AppHandle,
    scanned_files: usize,
    processed_events: usize,
    rescan_errors: usize,
) {
    static LAST_EMIT: Lazy<Mutex<Instant>> =
        Lazy::new(|| Mutex::new(Instant::now() - Duration::from_secs(1)));

    {
        let mut last_emit = LAST_EMIT.lock();
        if Instant::now().duration_since(*last_emit) < Duration::from_millis(100) {
            return;
        }
        app_handle
            .emit(
                "status_bar_update",
                StatusBarUpdate {
                    scanned_files,
                    processed_events,
                    rescan_errors,
                },
            )
            .unwrap();
        *last_emit = Instant::now();
    }
}

fn handle_watch_config_update(
    app_handle: &AppHandle,
    update: WatchConfigUpdate,
    cache: &mut SearchCache,
    event_watcher: &mut EventWatcher,
    watch_root: &mut String,
) {
    *event_watcher = EventWatcher::noop();
    *watch_root = update.watch_root;
    *cache = SearchCache::noop(
        PathBuf::from(&*watch_root),
        update.ignore_paths.into_iter().map(PathBuf::from).collect(),
        update.include_paths.into_iter().map(PathBuf::from).collect(),
        &APP_QUIT,
    );
    reset_status_bar(app_handle);
    update_app_state(app_handle, AppLifecycleState::Ready);
}

struct EventSnapshot {
    path: PathBuf,
    event_id: u64,
    flag: EventFlag,
    timestamp: i64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RecentEvent {
    path: String,
    flag_bits: u32,
    event_id: u64,
    timestamp: i64,
}

fn handle_event_watcher_events(
    app_handle: &AppHandle,
    cache: &mut SearchCache,
    events: Vec<FsEvent>,
    history_ready: &mut bool,
    processed_events: &mut usize,
    record_events: bool,
) -> Result<(), HandleFSEError> {
    *processed_events += events.len();

    let mut snapshots = Vec::with_capacity(events.len());
    let mut next_history_ready = *history_ready;
    for event in events.iter() {
        if event.flag.contains(EventFlag::HistoryDone) {
            next_history_ready = true;
        } else if next_history_ready && record_events {
            snapshots.push(EventSnapshot {
                path: event.path.clone(),
                event_id: event.id,
                flag: event.flag,
                timestamp: unix_timestamp_now(),
            });
        }
    }

    cache.handle_fs_events(events)?;
    *history_ready = next_history_ready;
    emit_status_bar_update(app_handle, cache.get_total_files(), *processed_events,
        cache.rescan_count() as usize);

    if *history_ready && !snapshots.is_empty() {
        forward_new_events(app_handle, &snapshots);
    }
    Ok(())
}

fn handle_icon_viewport_update(
    cache: &mut SearchCache,
    update: (u64, Vec<SlabIndex>),
    icon_update_tx: &Sender<IconPayload>,
) {
    let (_request_id, viewport) = update;

    let nodes = cache.expand_file_nodes(&viewport);
    let icon_jobs: Vec<_> = viewport
        .into_iter()
        .zip(nodes)
        .map(|(slab_index, SearchResultNode { path, .. })| (slab_index, path))
        .collect();

    if icon_jobs.is_empty() {
        return;
    }

    icon_jobs
        .into_iter()
        .map(|(slab_index, path)| (slab_index, path.to_string_lossy().into_owned()))
        .filter(|(_, path)| {
            // OneDrive
            // iCloud Drive
            // Google Drive
            // Dropbox
            !path.contains("OneDrive")
                && !path.contains("com~apple~CloudDocs")
                && !path.contains("Google Drive")
                && !path.contains("Dropbox")
        })
        .for_each(|(slab_index, path)| {
            let icon_update_tx = icon_update_tx.clone();
            spawn(move || {
                if let Some(icon) = fs_icon::icon_of_path_ql(&path).map(|data| {
                    format!(
                        "data:image/png;base64,{}",
                        general_purpose::STANDARD.encode(&data)
                    )
                }) {
                    let _ = icon_update_tx.send(IconPayload { slab_index, icon });
                }
            });
        });
}

#[allow(clippy::too_many_arguments)]
pub fn run_background_event_loop(
    app_handle: &AppHandle,
    mut cache: SearchCache,
    mut event_watcher: EventWatcher,
    channels: BackgroundLoopChannels,
    mut watch_root: String,
    fse_latency_secs: f64,
    db_path: PathBuf,
) -> Option<SearchCache> {
    let BackgroundLoopChannels {
        finish_rx,
        update_window_state_rx,
        search_rx,
        node_info_rx,
        icon_viewport_rx,
        rescan_rx,
        watch_config_rx,
        icon_update_tx,
    } = channels;
    let mut processed_events = 0usize;
    let mut metadata_revision = 0u64;
    let mut index_generation = 0u64;
    let mut history_ready = load_app_state() == AppLifecycleState::Ready;

    let mut monitoring = MonitoringStatus {
        needs_initial_scan: cache.is_noop(),
        ..MonitoringStatus::default()
    };
    let mut fallback_attempted = false;
    let mut refresh_token = CancellationToken::noop();
    publish_monitoring(app_handle, &mut monitoring);

    loop {
        crossbeam_channel::select! {
            recv(finish_rx) -> _ => {
                // Return ownership only after the watcher is stopped. Empty caches
                // must not overwrite a previously saved snapshot.
                drop(event_watcher);
                return (!cache.is_noop()).then_some(cache);
            }
            recv(update_window_state_rx) -> _ => {
                // Window visibility does not trigger scans, watchers, or cache writes.
            }
            recv(search_rx) -> job => {
                let SearchJob {
                    query,
                    options,
                    cancellation_token,
                    result_tx
                } = job.expect("Search channel closed");
                let opts = SearchOptions::from(options);
                let payload = cache.search_query_with_options(query, opts, cancellation_token);
                result_tx.send(payload).expect("Failed to send result");
            }
            recv(node_info_rx) -> request => {
                let request = request.expect("Node info channel closed");
                match request {
                    NodeInfoRequest::RootPage { page, response_tx } => {
                        let _ = response_tx.send(cache.root_page(page, 100));
                    }
                    NodeInfoRequest::SetMonitoring { enabled, token } => {
                        if token.is_cancelled().is_none() { continue; }
                        refresh_token = token;
                        metadata_revision += 1;
                        index_generation += 1;
                        monitoring.enabled = enabled;
                        if enabled {
                            if !monitoring.refreshing {
                                fallback_attempted = false;
                                begin_refresh(app_handle, &mut cache, &mut event_watcher,
                                    &watch_root, fse_latency_secs, &mut history_ready,
                                    &mut processed_events, &mut monitoring, false,
                                    refresh_token);
                            } else {
                                publish_monitoring(app_handle, &mut monitoring);
                            }
                        } else {
                            event_watcher = EventWatcher::noop();
                            monitoring.refreshing = false;
                            monitoring.needs_refresh = true;
                            monitoring.error = None;
                            update_app_state(app_handle, AppLifecycleState::Ready);
                            save_requested_snapshot(&mut cache, &db_path, &mut monitoring);
                            publish_monitoring(app_handle, &mut monitoring);
                        }
                    }
                    NodeInfoRequest::Nodes { slab_indices, response_tx } => {
                        let node_info_results = cache.expand_file_nodes(&slab_indices);
                        let _ = response_tx.send(node_info_results);
                    }
                    NodeInfoRequest::LargeFiles(request) => {
                        crate::large_files::handle_cache_request(
                            request, &mut cache, metadata_revision, index_generation,
                        );
                    }
                }
            }
            recv(icon_viewport_rx) -> update => {
                let update = update.expect("Icon viewport channel closed");
                handle_icon_viewport_update(&mut cache, update, &icon_update_tx);
            }
            recv(rescan_rx) -> request => {
                metadata_revision += 1;
                index_generation += 1;
                let scan_cancellation_token = request.expect("Rescan channel closed");
                if scan_cancellation_token.is_cancelled().is_none() { continue; }
                refresh_token = scan_cancellation_token;
                fallback_attempted = false;
                begin_refresh(app_handle, &mut cache, &mut event_watcher,
                    &watch_root, fse_latency_secs, &mut history_ready,
                    &mut processed_events, &mut monitoring, false, scan_cancellation_token);
            }
            recv(watch_config_rx) -> update => {
                metadata_revision += 1;
                index_generation += 1;
                let next_update = update.expect("Watch config channel closed");
                refresh_token = next_update.scan_cancellation_token;
                handle_watch_config_update(
                    app_handle,
                    next_update,
                    &mut cache,
                    &mut event_watcher,
                    &mut watch_root,
                );
                history_ready = false;
                processed_events = 0;
                monitoring.needs_initial_scan = true;
                monitoring.needs_refresh = true;
                monitoring.refreshing = false;
                monitoring.error = None;
                if monitoring.enabled {
                    fallback_attempted = false;
                    begin_refresh(app_handle, &mut cache, &mut event_watcher,
                        &watch_root, fse_latency_secs, &mut history_ready,
                        &mut processed_events, &mut monitoring, true, refresh_token);
                } else {
                    publish_monitoring(app_handle, &mut monitoring);
                }
                let _ = app_handle.emit("index_refreshed", ());
            }
            recv(event_watcher) -> events => {
                metadata_revision += 1;
                let Ok(events) = events else {
                    event_watcher = EventWatcher::noop();
                    monitoring.enabled = false;
                    monitoring.refreshing = false;
                    monitoring.needs_refresh = true;
                    monitoring.error = Some("Unable to start or continue FSEvents. Check disk access and retry.".into());
                    update_app_state(app_handle, AppLifecycleState::Ready);
                    publish_monitoring(app_handle, &mut monitoring);
                    continue;
                };
                let result = handle_event_watcher_events(
                    app_handle,
                    &mut cache,
                    events,
                    &mut history_ready,
                    &mut processed_events,
                    monitoring.enabled,
                );
                if result.is_err() {
                    index_generation += 1;
                    if fallback_attempted {
                        event_watcher = EventWatcher::noop();
                        monitoring.enabled = false;
                        monitoring.refreshing = false;
                        monitoring.needs_refresh = true;
                        monitoring.error = Some("Filesystem history is still incomplete after rebuilding. Please refresh again.".into());
                        update_app_state(app_handle, AppLifecycleState::Ready);
                        publish_monitoring(app_handle, &mut monitoring);
                    } else {
                        fallback_attempted = true;
                        begin_refresh(app_handle, &mut cache, &mut event_watcher,
                            &watch_root, fse_latency_secs, &mut history_ready,
                            &mut processed_events, &mut monitoring, true, refresh_token);
                    }
                } else if monitoring.refreshing && history_ready {
                    // Apply the entire HistoryDone batch before publishing Ready.
                    fallback_attempted = false;
                    if !monitoring.enabled { event_watcher = EventWatcher::noop(); }
                    monitoring.refreshing = false;
                    monitoring.needs_initial_scan = false;
                    monitoring.needs_refresh = false;
                    save_requested_snapshot(&mut cache, &db_path, &mut monitoring);
                    let _ = app_handle.emit("status_bar_update", StatusBarUpdate {
                        scanned_files: cache.get_total_files(),
                        processed_events,
                        rescan_errors: cache.rescan_count() as usize,
                    });
                    update_app_state(app_handle, AppLifecycleState::Ready);
                    publish_monitoring(app_handle, &mut monitoring);
                    let _ = app_handle.emit("index_refreshed", ());
                }
            }
        }
    }
}

pub(crate) fn build_search_cache(
    app_handle: &AppHandle,
    watch_root: &str,
    ignore_paths: &[PathBuf],
    include_paths: &[PathBuf],
    scan_cancellation_token: CancellationToken,
) -> Option<SearchCache> {
    let path = Path::new(watch_root);
    let walk_data = WalkData::new(path, ignore_paths, include_paths, false, move || {
        APP_QUIT.load(Ordering::Relaxed) || scan_cancellation_token.is_cancelled().is_none()
    });
    let walking_done = AtomicBool::new(false);

    std::thread::scope(|s| {
        s.spawn(|| {
            while !walking_done.load(Ordering::Relaxed) {
                let dirs = walk_data.num_dirs.load(Ordering::Relaxed);
                let files = walk_data.num_files.load(Ordering::Relaxed);
                let total = dirs + files;
                emit_status_bar_update(app_handle, total, 0, 0);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let cache = SearchCache::walk_fs_with_walk_data(&walk_data, &APP_QUIT);
        walking_done.store(true, Ordering::Relaxed);
        cache
    })
}

#[allow(clippy::too_many_arguments)]
fn begin_refresh(
    app: &AppHandle,
    cache: &mut SearchCache,
    watcher: &mut EventWatcher,
    root: &str,
    latency: f64,
    history_ready: &mut bool,
    processed: &mut usize,
    status: &mut MonitoringStatus,
    rebuild: bool,
    token: CancellationToken,
) {
    if token.is_cancelled().is_none() { return; }
    *watcher = EventWatcher::noop();
    *history_ready = false;
    *processed = 0;
    status.refreshing = true;
    status.needs_refresh = true;
    status.error = None;
    publish_monitoring(app, status);
    let rebuild = rebuild || cache.is_noop() || cache.last_event_id() > cardinal_sdk::current_event_id();
    if rebuild {
        update_app_state(app, AppLifecycleState::Initializing);
        let next = build_search_cache(app, root, &cache.ignore_paths(), &cache.include_paths(), token);
        let Some(next) = next else {
            status.refreshing = false;
            status.needs_initial_scan = cache.is_noop();
            status.error = Some("Index refresh cancelled. Refresh again to complete it.".into());
            update_app_state(app, AppLifecycleState::Ready);
            publish_monitoring(app, status);
            return;
        };
        *cache = next;
        status.needs_initial_scan = false;
    }
    if token.is_cancelled().is_none() {
        status.refreshing = false;
        update_app_state(app, AppLifecycleState::Ready);
        publish_monitoring(app, status);
        return;
    }
    update_app_state(app, AppLifecycleState::Updating);
    *watcher = EventWatcher::spawn(
        root.to_string(), cache.last_event_id(), latency,
        cache.ignore_paths(), cache.include_paths(),
    ).1;
}

fn save_requested_snapshot(cache: &mut SearchCache, path: &Path, status: &mut MonitoringStatus) {
    if !cache.is_noop() {
        if let Err(error) = cache.flush_snapshot_to_file(path) {
            error!("Failed to save refreshed index: {error:?}");
            status.error = Some(format!("Index updated, but saving failed: {error}"));
        }
    }
}

fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn forward_new_events(app_handle: &AppHandle, snapshots: &[EventSnapshot]) {
    if snapshots.is_empty() {
        return;
    }

    let mut ordered_events: Vec<&EventSnapshot> = snapshots.iter().collect();
    ordered_events.sort_unstable_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });
    let new_events: Vec<RecentEvent> = ordered_events
        .into_iter()
        .map(|event| RecentEvent {
            path: event.path.to_string_lossy().into_owned(),
            flag_bits: event.flag.bits(),
            event_id: event.event_id,
            timestamp: event.timestamp,
        })
        .collect();

    let _ = app_handle.emit("fs_events_batch", new_events);
}
