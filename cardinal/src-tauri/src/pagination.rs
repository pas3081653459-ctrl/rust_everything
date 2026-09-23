use crate::commands::{self, NodeInfoRequest, SearchOptionsPayload, SearchResponse, SearchState};
use crossbeam_channel::bounded;
use parking_lot::Mutex;
use search_cache::SlabIndex;
use serde::Serialize;
use std::sync::{LazyLock, atomic::{AtomicU64, Ordering}};
use tauri::State;

const PAGE_SIZE: usize = 100;
static VERSION: AtomicU64 = AtomicU64::new(0);
static RESULTS: LazyLock<Mutex<Option<ResultSet>>> = LazyLock::new(|| Mutex::new(None));

struct ResultSet {
    version: u64,
    home: bool,
    results: Vec<SlabIndex>,
    highlights: Vec<String>,
    sorted: Option<(String, Vec<SlabIndex>)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultPage {
    results: Vec<SlabIndex>,
    highlights: Vec<String>,
    status_code: u8,
    total: usize,
    page: usize,
    page_size: usize,
    version: u64,
    root: Option<String>,
}

#[tauri::command]
pub async fn search_first_page(
    directory_query: Option<String>, query: Option<String>,
    options: Option<SearchOptionsPayload>, state: State<'_, SearchState>,
) -> Result<ResultPage, String> {
    if crate::lifecycle::load_app_state() == crate::lifecycle::AppLifecycleState::Initializing {
        return Err("Index not ready; complete permission setup and wait for initialization".into());
    }
    let version = VERSION.fetch_add(1, Ordering::SeqCst) + 1;
    RESULTS.lock().take();
    state.clear_sort_cache();
    let home = query.as_deref().unwrap_or("").trim().is_empty()
        && directory_query.as_deref().unwrap_or("").trim().is_empty();
    let response = if home {
        let _ = search_cancel::CancellationToken::new_search();
        SearchResponse::default()
    } else {
        commands::search(directory_query, query, options, state.clone()).await?
    };
    {
        let mut current = RESULTS.lock();
        if VERSION.load(Ordering::SeqCst) != version || response.status_code != SearchResponse::OK {
            return Err("Search superseded by a newer request".into());
        }
        *current = Some(ResultSet { version, home, results: response.results, highlights: response.highlights, sorted: None });
    }
    get_result_page(version, 0, state)
}

#[tauri::command(async)]
pub fn get_result_page(version: u64, page: usize, state: State<'_, SearchState>) -> Result<ResultPage, String> {
    if crate::lifecycle::load_app_state() == crate::lifecycle::AppLifecycleState::Initializing {
        return Err("Index is initializing".into());
    }
    let current = RESULTS.lock();
    let set = current.as_ref().filter(|set| set.version == version)
        .ok_or("Results expired; refresh the search")?;
    if set.home {
        drop(current);
        let (response_tx, response_rx) = bounded(1);
        state.node_info_tx.send(NodeInfoRequest::RootPage { page, response_tx }).map_err(|e| e.to_string())?;
        let (root, total, page, results) = response_rx.recv().map_err(|e| e.to_string())?;
        return Ok(ResultPage { results, highlights: vec![], status_code: 0, total, page,
            page_size: PAGE_SIZE, version, root: Some(root) });
    }
    let total = set.results.len();
    let page = page.min(total.saturating_sub(1) / PAGE_SIZE);
    let start = page * PAGE_SIZE;
    Ok(ResultPage { results: set.results[start..(start + PAGE_SIZE).min(total)].to_vec(),
        highlights: set.highlights.clone(), status_code: 0, total, page,
        page_size: PAGE_SIZE, version, root: None })
}

// Sorting operates on the complete bounded result set, before slicing a page.
#[tauri::command(async)]
pub fn get_sorted_result_page(version: u64, page: usize, sort: crate::sort::SortStatePayload,
    state: State<'_, SearchState>) -> Result<Vec<SlabIndex>, String> {
    let mut current = RESULTS.lock();
    let set = current.as_mut().filter(|set| set.version == version && !set.home)
        .ok_or("Results expired; refresh the search")?;
    if set.results.len() > 20_000 { return Err("Too many results to sort".into()); }
    let key = format!("{sort:?}");
    if set.sorted.as_ref().is_none_or(|(previous, _)| previous != &key) {
        let ordered = commands::get_sorted_view(set.results.clone(), Some(sort), state);
        set.sorted = Some((key, ordered));
    }
    let ordered = &set.sorted.as_ref().expect("sorted above").1;
    let page = page.min(ordered.len().saturating_sub(1) / PAGE_SIZE);
    let start = page * PAGE_SIZE;
    Ok(ordered[start..(start + PAGE_SIZE).min(ordered.len())].to_vec())
}
