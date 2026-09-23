mod background;
mod commands;
mod large_files;
mod lifecycle;
mod monitoring;
mod pagination;
mod quicklook;
mod sort;
mod window_controls;

use anyhow::Result;
use monitoring::{get_monitoring_status, set_event_monitoring};
use background::{
    BackgroundLoopChannels, IconPayload, emit_status_bar_update,
    run_background_event_loop,
};
use cardinal_sdk::EventWatcher;
use commands::{
    NodeInfoRequest, SearchJob, SearchState, WatchConfigUpdate, activate_main_window,
    close_quicklook, copy_files_to_clipboard, get_app_status, get_nodes_info, get_sorted_view,
    hide_main_window, normalize_watch_config, open_in_finder, open_path, search,
    set_tray_activation_policy, set_watch_config, start_logic, toggle_main_window,
    toggle_quicklook, trigger_rescan, update_icon_viewport, update_quicklook,
};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use large_files::{
    LargeFileState, cancel_large_file_search, get_large_file_page, get_large_file_progress,
    start_large_file_search,
};
use lifecycle::{
    APP_QUIT, AppLifecycleState, EXIT_REQUESTED, emit_app_state, update_app_state,
};
use once_cell::sync::OnceCell;
use search_cache::{SearchCache, SlabIndex};
use search_cancel::CancellationToken;
use std::{
    path::{Path, PathBuf},
    sync::{Once, atomic::Ordering},
    time::Duration,
};
use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;
use window_controls::{activate_window, hide_window};

static DB_PATH: OnceCell<PathBuf> = OnceCell::new();
pub(crate) static LOGIC_START: OnceCell<Sender<LogicStartConfig>> = OnceCell::new();
pub(crate) const DEFAULT_SYSTEM_IGNORE_PATH: &str = "/System/Volumes/Data";
const FSE_LATENCY_SECS: f64 = 0.1;

#[derive(Debug, Clone)]
pub(crate) struct LogicStartConfig {
    pub watch_root: String,
    pub ignore_paths: Vec<String>,
    pub include_paths: Vec<String>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<()> {
    let builder = tracing_subscriber::fmt();
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        builder.with_env_filter(filter).init();
    } else {
        builder.with_max_level(LevelFilter::INFO).init();
    }

    let (finish_tx, finish_rx) = bounded::<()>(1);
    // The worker owns the only result sender. Never put it inside the finish
    // request queue: an unconsumed request can otherwise keep recv() alive forever.
    let (exit_cache_tx, exit_cache_rx) = bounded::<Option<SearchCache>>(1);
    let (search_tx, search_rx) = unbounded::<SearchJob>();
    let (node_info_tx, node_info_rx) = unbounded::<NodeInfoRequest>();
    let (icon_viewport_tx, icon_viewport_rx) = unbounded::<(u64, Vec<SlabIndex>)>();
    let (rescan_tx, rescan_rx) = unbounded::<CancellationToken>();
    let (watch_config_tx, watch_config_rx) = unbounded::<WatchConfigUpdate>();
    let (icon_update_tx, icon_update_rx) = unbounded::<IconPayload>();
    let (update_window_state_tx, update_window_state_rx) = bounded::<()>(1);
    let (logic_start_tx, logic_start_rx) = bounded(1);
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
    LOGIC_START
        .set(logic_start_tx)
        .expect("LOGIC_START channel already initialized");

    let mut builder = tauri::Builder::default();
    #[cfg(not(feature = "dev"))]
    {
        builder = builder.plugin(tauri_plugin_prevent_default::init());
    }
    let update_window_state_tx_for_window = update_window_state_tx.clone();
    builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_macos_permissions::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .on_window_event(move |window, event| {
            if window.label() != "main" {
                return;
            }

            match event {
                WindowEvent::Focused(_) => {
                    let _ = update_window_state_tx_for_window.try_send(());
                }
                WindowEvent::CloseRequested { api, .. } => {
                    if EXIT_REQUESTED.load(Ordering::Relaxed) {
                        return;
                    }

                    api.prevent_close();

                    let Some(window) = window.get_webview_window("main") else {
                        warn!("Close requested but main window is unavailable");
                        return;
                    };

                    if hide_window(&window) {
                        let _ = update_window_state_tx_for_window.try_send(());
                        info!("Main window hidden; Cardinal keeps running in the background");
                    }
                }
                _ => {}
            }
        });

    let app = builder
        .manage(LargeFileState::default())
        .manage(SearchState::new(
            search_tx,
            node_info_tx,
            icon_viewport_tx.clone(),
            rescan_tx.clone(),
            watch_config_tx.clone(),
            update_window_state_tx.clone(),
        ))
        .invoke_handler(tauri::generate_handler![
            get_monitoring_status,
            set_event_monitoring,
            start_large_file_search,
            get_large_file_progress,
            get_large_file_page,
            cancel_large_file_search,
            search,
            pagination::search_first_page,
            pagination::get_result_page,
            pagination::get_sorted_result_page,
            get_nodes_info,
            get_sorted_view,
            update_icon_viewport,
            get_app_status,
            trigger_rescan,
            set_watch_config,
            open_in_finder,
            open_path,
            toggle_quicklook,
            close_quicklook,
            update_quicklook,
            start_logic,
            hide_main_window,
            activate_main_window,
            toggle_main_window,
            set_tray_activation_policy,
            copy_files_to_clipboard,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    let db_path = DB_PATH
        .get_or_try_init(|| app.path().app_config_dir().map(|p| p.join("cardinal.db")))
        .expect("Failed to initialize database path");

    let app_handle = &app.handle().to_owned();
    let channels = BackgroundLoopChannels {
        finish_rx,
        search_rx,
        node_info_rx,
        icon_viewport_rx,
        rescan_rx,
        watch_config_rx,
        icon_update_tx,
        update_window_state_rx,
    };
    emit_app_state(app_handle);
    let icon_update_rx = &icon_update_rx;
    std::thread::scope(move |s| {
        s.spawn(|| {
            while let Ok(icon) = icon_update_rx.recv() {
                let mut icons = vec![icon];
                std::thread::sleep(Duration::from_millis(100));
                icons.extend(icon_update_rx.try_iter());
                info!("emitting {} icons", icons.len());
                app_handle.emit("icon_update", icons).unwrap();
            }
            info!("icon update thread exited");
        });

        let logic_start_rx = logic_start_rx;
        s.spawn(move || {
            let cache = match wait_for_logic_start(logic_start_rx, shutdown_rx) {
                Some(config) => run_logic_thread(app_handle, db_path, channels, config),
                None => {
                    info!("Background thread quitting without Full Disk Access; no cache to save");
                    // Release request receivers and the icon sender even though the
                    // processing loop never started.
                    drop(channels);
                    None
                }
            };
            let _ = exit_cache_tx.send(cache);
        });

        app.run(move |app_handle, event| match event {
            RunEvent::Exit => {
                APP_QUIT.store(true, Ordering::Relaxed);
                let _ = shutdown_tx.try_send(());
                flush_cache_to_file_once(&finish_tx, &exit_cache_rx, db_path);
            }
            RunEvent::ExitRequested { api, code, .. } => {
                let already_requested = EXIT_REQUESTED.swap(true, Ordering::Relaxed);
                APP_QUIT.store(true, Ordering::Relaxed);
                let _ = shutdown_tx.try_send(());
                if !already_requested {
                    info!(
                        "Exit requested (code: {:?}); flushing cache before shutdown",
                        code
                    );
                }

                flush_cache_to_file_once(&finish_tx, &exit_cache_rx, db_path);

                if code.is_none() {
                    api.prevent_exit();
                    app_handle.exit(0);
                }
            }
            RunEvent::Reopen { .. } => {
                // On macOS, clicking the Dock icon should bring the main window back even if the
                // app still "has windows" but they are hidden.
                if let Some(window) = app_handle.get_webview_window("main") {
                    activate_window(&window);
                } else {
                    warn!("Reopen requested but main window is unavailable");
                }
            }
            _ => {}
        });
    });

    Ok(())
}

fn run_logic_thread(
    app_handle: &tauri::AppHandle,
    db_path: &Path,
    channels: BackgroundLoopChannels,
    config: LogicStartConfig,
) -> Option<SearchCache> {
    let Some((watch_root, ignore_paths, include_paths)) = normalize_watch_config(
        &config.watch_root,
        config.ignore_paths,
        config.include_paths,
        Some("/"),
    ) else {
        warn!("Invalid watch root in start config; skipping background startup");
        return None;
    };
    let path = PathBuf::from(&watch_root);
    let ignore_paths: Vec<_> = ignore_paths.into_iter().map(PathBuf::from).collect();
    let include_paths: Vec<_> = include_paths.into_iter().map(PathBuf::from).collect();

    let cache = match SearchCache::try_read_persistent_cache(
        &path,
        db_path,
        &ignore_paths,
        &include_paths,
        &APP_QUIT,
    ) {
        Ok(cached) => {
            info!("Loaded existing cache");
            emit_status_bar_update(app_handle, cached.get_total_files(), 0, 0);
            cached
        }
        Err(e) => {
            info!("No usable snapshot; waiting for manual refresh: {:?}", e);
            SearchCache::noop(path, ignore_paths, include_paths, &APP_QUIT)
        }
    };

    // Loading a snapshot never starts filesystem work. Refresh/monitoring is explicit.
    let event_watcher = EventWatcher::noop();
    update_app_state(app_handle, AppLifecycleState::Ready);

    info!("Started background processing thread");
    // TODO(ldm0): remove this watch_root, use cache's path instead
    let cache = run_background_event_loop(
        app_handle,
        cache,
        event_watcher,
        channels,
        watch_root.to_string(),
        FSE_LATENCY_SECS,
        db_path.to_path_buf(),
    );

    info!("Background thread exited");
    cache
}

fn flush_cache_to_file_once(
    finish_tx: &Sender<()>,
    cache_rx: &Receiver<Option<SearchCache>>,
    db_path: &PathBuf,
) {
    static FLUSH_ONCE: Once = Once::new();
    FLUSH_ONCE.call_once(|| {
        // A worker still awaiting permission is woken by shutdown_tx instead.
        // It explicitly returns None. A failed/exited worker drops the sole
        // result sender, so receiving also terminates without a reply.
        let _ = finish_tx.try_send(());
        match cache_rx.recv() {
            Ok(Some(cache)) => {
                if let Err(error) = cache.flush_to_file(db_path) {
                    warn!("Failed to save index on exit: {error:?}");
                }
            }
            Ok(None) => info!("Exit without an initialized index; keeping saved snapshot"),
            Err(error) => warn!("Index worker exited without returning a snapshot: {error}"),
        }
    });
}

fn wait_for_logic_start(rx: Receiver<LogicStartConfig>, shutdown: Receiver<()>) -> Option<LogicStartConfig> {
    info!("Waiting for Full Disk Access signal from the frontend");
    crossbeam_channel::select! {
        recv(shutdown) -> _ => None,
        recv(rx) -> config => {
            if APP_QUIT.load(Ordering::Relaxed) { None } else { config.ok() }
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn shutdown_wakes_worker_waiting_for_permission() {
        let (_start_tx, start_rx) = bounded::<LogicStartConfig>(1);
        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (result_tx, result_rx) = bounded::<Option<SearchCache>>(1);
        let worker = std::thread::spawn(move || {
            assert!(wait_for_logic_start(start_rx, shutdown_rx).is_none());
            result_tx.send(None).unwrap();
        });
        shutdown_tx.send(()).unwrap();
        assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_none());
        worker.join().unwrap();
    }

    #[test]
    fn queued_finish_request_cannot_keep_worker_result_channel_alive() {
        let (finish_tx, finish_rx) = bounded::<()>(1);
        let (result_tx, result_rx) = bounded::<Option<SearchCache>>(1);
        finish_tx.send(()).unwrap();
        // The worker exits before consuming its finish request. The request
        // contains no reply sender, so keeping finish_tx alive cannot hang recv.
        drop(finish_rx);
        drop(result_tx);
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(2)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
        drop(finish_tx);
    }
}
