use crate::{commands::{NodeInfoRequest, SearchState}, lifecycle::AppLifecycleState};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::LazyLock;
use tauri::{AppHandle, Emitter, State};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringStatus {
    pub enabled: bool,
    pub refreshing: bool,
    pub needs_initial_scan: bool,
    pub needs_refresh: bool,
    pub error: Option<String>,
    pub revision: u64,
}

impl Default for MonitoringStatus {
    fn default() -> Self {
        Self { enabled: false, refreshing: false, needs_initial_scan: true,
            needs_refresh: true, error: None, revision: 0 }
    }
}

static STATUS: LazyLock<Mutex<MonitoringStatus>> = LazyLock::new(|| Mutex::new(MonitoringStatus::default()));

pub fn publish(app: &AppHandle, status: &mut MonitoringStatus) {
    let mut current = STATUS.lock();
    status.revision = current.revision + 1;
    *current = status.clone();
    let _ = app.emit("monitoring_status", status.clone());
}

#[tauri::command]
pub fn get_monitoring_status() -> MonitoringStatus {
    STATUS.lock().clone()
}

#[tauri::command(async)]
pub fn set_event_monitoring(enabled: bool, state: State<'_, SearchState>) -> Result<(), String> {
    if crate::lifecycle::load_app_state() == AppLifecycleState::Initializing && enabled {
        return Err("Wait for initialization to finish.".into());
    }
    // Allocate when requested, not when dequeued, so Stop also cancels pending work.
    let token = search_cancel::CancellationToken::new_scan();
    state.node_info_tx.send(NodeInfoRequest::SetMonitoring { enabled, token })
        .map_err(|error| error.to_string())
}
