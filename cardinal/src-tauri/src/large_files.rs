use crate::{
    commands::{NodeInfoRequest, SearchState},
    lifecycle::{APP_QUIT, AppLifecycleState, load_app_state},
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use base64::{Engine as _, engine::general_purpose};
use parking_lot::Mutex;
use rayon::prelude::*;
use search_cache::{SearchCache, SearchResultNode, SlabIndex, SlabNodeMetadataCompact};
use serde::Serialize;
use std::{
    sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}},
    time::Duration,
};
use tauri::State;

const MINIMUM_SIZE: u64 = 1024 * 1024;
const BATCH_SIZE: usize = 256;
const PAGE_SIZE: usize = 100;

type Batch = (u64, Vec<(SlabIndex, SearchResultNode)>);

#[derive(Debug, Clone)]
pub enum CacheRequest {
    Candidates(Sender<(u64, Vec<SlabIndex>)>),
    Batch {
        generation: u64,
        indices: Vec<SlabIndex>,
        reply: Sender<Result<Batch, String>>,
    },
    Store {
        revision: u64,
        entries: Vec<(SlabIndex, SlabNodeMetadataCompact)>,
    },
}

pub fn handle_cache_request(
    request: CacheRequest,
    cache: &mut SearchCache,
    revision: u64,
    generation: u64,
) {
    match request {
        CacheRequest::Candidates(reply) => {
            let _ = reply.send((generation, cache.large_file_candidates()));
        }
        CacheRequest::Batch { generation: requested, indices, reply } => {
            let result = if requested == generation {
                Ok((revision, cache.large_file_batch(&indices, MINIMUM_SIZE)))
            } else {
                Err("The index was rebuilt or its scope changed. Refresh the large-file search.".into())
            };
            let _ = reply.send(result);
        }
        CacheRequest::Store { revision: observed, entries } => {
            // An FSEvent/rescan may have invalidated or reused nodes during disk I/O.
            if observed == revision {
                for (index, metadata) in entries {
                    cache.cache_large_file_metadata(index, metadata);
                }
            }
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    phase: &'static str,
    checked: usize,
    total: usize,
    matched: usize,
    skipped: usize,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct LargeFileRow {
    path: String,
    size: u64,
}

#[derive(Serialize)]
pub struct LargeFilePageRow {
    #[serde(flatten)]
    file: LargeFileRow,
    icon: Option<String>,
}

struct ResultSet {
    rows: Vec<LargeFileRow>,
    order: Vec<usize>,
}

impl ResultSet {
    fn page(&self, offset: usize, ascending: bool) -> Vec<LargeFileRow> {
        (offset..offset.saturating_add(PAGE_SIZE).min(self.order.len()))
            .map(|position| {
                let position = if ascending { self.order.len() - 1 - position } else { position };
                self.rows[self.order[position]].clone()
            })
            .collect()
    }
}

struct Job {
    id: u64,
    cancelled: Arc<AtomicBool>,
    progress: Progress,
    results: Option<ResultSet>,
}

#[derive(Default)]
pub struct LargeFileState {
    next_id: AtomicU64,
    job: Arc<Mutex<Option<Job>>>,
}

fn stopped(cancelled: &AtomicBool) -> bool {
    cancelled.load(Ordering::Relaxed) || APP_QUIT.load(Ordering::Relaxed)
}

fn receive<T>(rx: Receiver<T>, cancelled: &AtomicBool) -> Result<T, String> {
    loop {
        if stopped(cancelled) {
            return Err("Cancelled".into());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("Index worker disconnected".into()),
        }
    }
}

fn send(tx: &Sender<NodeInfoRequest>, request: CacheRequest) -> Result<(), String> {
    tx.send(NodeInfoRequest::LargeFiles(request)).map_err(|error| error.to_string())
}

fn update(job: &Mutex<Option<Job>>, id: u64, progress: &Progress) {
    if let Some(job) = job.lock().as_mut()
        && job.id == id && !stopped(&job.cancelled)
    {
        job.progress = progress.clone();
    }
}

/// Stable radix ordering of compact row offsets: no paths are cloned or compared.
/// Checks cancellation throughout, including while distributing a very large bucket.
fn size_order(rows: &[LargeFileRow], cancelled: &AtomicBool) -> Result<Vec<usize>, String> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    let mut scratch = vec![0; rows.len()];
    for shift in (0..64).step_by(8) {
        let mut counts = [0usize; 256];
        for (position, &index) in order.iter().enumerate() {
            if position % 4096 == 0 && stopped(cancelled) {
                return Err("Cancelled".into());
            }
            counts[((!rows[index].size >> shift) & 255) as usize] += 1;
        }
        let mut offset = 0;
        for count in &mut counts {
            let length = *count;
            *count = offset;
            offset += length;
        }
        for (position, &index) in order.iter().enumerate() {
            if position % 4096 == 0 && stopped(cancelled) {
                return Err("Cancelled".into());
            }
            let bucket = ((!rows[index].size >> shift) & 255) as usize;
            scratch[counts[bucket]] = index;
            counts[bucket] += 1;
        }
        std::mem::swap(&mut order, &mut scratch);
    }
    Ok(order)
}

fn collect_files(
    tx: &Sender<NodeInfoRequest>,
    job: &Mutex<Option<Job>>,
    id: u64,
    cancelled: &AtomicBool,
    progress: &mut Progress,
) -> Result<ResultSet, String> {
    let (reply, rx) = bounded(1);
    send(tx, CacheRequest::Candidates(reply))?;
    let (generation, indices) = receive(rx, cancelled)?;
    progress.total = indices.len();
    update(job, id, progress);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()
        .map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    for indices in indices.chunks(BATCH_SIZE) {
        if stopped(cancelled) { return Err("Cancelled".into()); }
        let (reply, rx) = bounded(1);
        send(tx, CacheRequest::Batch { generation, indices: indices.to_vec(), reply })?;
        let (revision, mut batch) = receive(rx, cancelled)??;
        let updates: Vec<_> = pool.install(|| batch.par_iter_mut().filter_map(|(index, node)| {
            if stopped(cancelled) || node.metadata.is_some() { return None; }
            node.metadata = std::fs::symlink_metadata(&node.path)
                .map(|metadata| SlabNodeMetadataCompact::some(metadata.into()))
                .unwrap_or_else(|_| SlabNodeMetadataCompact::unaccessible());
            Some((*index, node.metadata))
        }).collect());
        if stopped(cancelled) { return Err("Cancelled".into()); }
        if !updates.is_empty() {
            send(tx, CacheRequest::Store { revision, entries: updates })?;
        }
        for (_, node) in batch {
            match node.metadata.as_ref() {
                Some(meta) if meta.r#type() == fswalk::NodeFileType::File
                    && meta.size() > MINIMUM_SIZE as i64 => {
                    rows.push(LargeFileRow {
                        path: node.path.to_string_lossy().into_owned(),
                        size: meta.size() as u64,
                    });
                }
                None => progress.skipped += 1,
                _ => {}
            }
        }
        progress.checked += indices.len();
        progress.matched = rows.len();
        update(job, id, progress);
    }
    // Also detects a rebuild that happened while reading the final batch.
    let (reply, rx) = bounded(1);
    send(tx, CacheRequest::Batch { generation, indices: Vec::new(), reply })?;
    receive(rx, cancelled)??;
    progress.phase = "sorting";
    update(job, id, progress);
    let order = size_order(&rows, cancelled)?;
    Ok(ResultSet { rows, order })
}

#[tauri::command(async)]
pub fn start_large_file_search(
    request_id: u64,
    state: State<'_, LargeFileState>,
    search: State<'_, SearchState>,
) -> Result<u64, String> {
    if load_app_state() != AppLifecycleState::Ready {
        return Err("Wait for indexing to finish before searching large files.".into());
    }
    let id = request_id;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut progress = Progress {
        phase: "scanning", checked: 0, total: 0, matched: 0, skipped: 0, error: None,
    };
    {
        let mut current = state.job.lock();
        // Commands may execute out of order (including React StrictMode remounts).
        if id <= state.next_id.load(Ordering::Relaxed) {
            return Err("Superseded by a newer large-file search.".into());
        }
        state.next_id.store(id, Ordering::Relaxed);
        if let Some(old) = current.as_ref() { old.cancelled.store(true, Ordering::Relaxed); }
        *current = Some(Job { id, cancelled: cancelled.clone(), progress: progress.clone(), results: None });
    }
    let job = state.job.clone();
    let tx = search.node_info_tx.clone();
    let spawn = std::thread::Builder::new().name("large-file-search".into()).spawn(move || {
        let result = collect_files(&tx, &job, id, &cancelled, &mut progress);
        let mut current = job.lock();
        if let Some(current) = current.as_mut()
            && current.id == id && !stopped(&cancelled)
        {
            match result {
                Ok(results) => { progress.phase = "ready"; current.results = Some(results); }
                Err(error) => { progress.phase = "error"; progress.error = Some(error); }
            }
            current.progress = progress;
        }
    });
    if let Err(error) = spawn {
        let mut job = state.job.lock();
        if job.as_ref().is_some_and(|job| job.id == id) { *job = None; }
        return Err(error.to_string());
    }
    Ok(id)
}

#[tauri::command(async)]
pub fn get_large_file_progress(id: u64, state: State<'_, LargeFileState>) -> Result<Progress, String> {
    state.job.lock().as_ref().filter(|job| job.id == id)
        .map(|job| job.progress.clone()).ok_or_else(|| "Search expired. Refresh to retry.".into())
}

#[tauri::command(async)]
pub fn get_large_file_page(
    id: u64, offset: usize, ascending: bool, state: State<'_, LargeFileState>,
) -> Result<Vec<LargeFilePageRow>, String> {
    let (rows, cancelled) = {
        let job = state.job.lock();
        let job = job.as_ref().filter(|job| job.id == id)
            .ok_or_else(|| "Search expired. Refresh to retry.".to_string())?;
        let results = job.results.as_ref()
            .ok_or_else(|| "Results are not ready. Refresh to retry.".to_string())?;
        (results.page(offset, ascending), job.cancelled.clone())
    };
    // Extract native icons only for this page, outside the job lock. Keep the
    // full result set compact and allow cancellation while AppKit does its work.
    rows.into_iter().map(|file| {
        if stopped(&cancelled) {
            return Err("Cancelled".into());
        }
        let icon = fs_icon::icon_of_path_ns(&file.path).map(|data| {
            format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(data))
        });
        Ok(LargeFilePageRow { file, icon })
    }).collect()
}

#[tauri::command(async)]
pub fn cancel_large_file_search(id: u64, state: State<'_, LargeFileState>) {
    if let Some(job) = state.job.lock().as_mut()
        && job.id == id
    {
        job.cancelled.store(true, Ordering::Relaxed);
        job.progress.phase = "cancelled";
        job.results = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_orders_full_u64_sizes_stably() {
        let rows: Vec<_> = [0, u64::MAX, 1024 * 1024 + 1, 256, 256, 1 << 40]
            .into_iter().map(|size| LargeFileRow { path: String::new(), size }).collect();
        assert_eq!(size_order(&rows, &AtomicBool::new(false)).unwrap(), vec![1, 5, 2, 3, 4, 0]);
        assert!(size_order(&rows, &AtomicBool::new(true)).is_err());
        assert!(size_order(&[], &AtomicBool::new(false)).unwrap().is_empty());
    }

    #[test]
    fn all_matches_are_sorted_and_paged_past_twenty_thousand() {
        let rows: Vec<_> = (0..20003u64).map(|index| LargeFileRow {
            path: index.to_string(),
            size: MINIMUM_SIZE + 1 + (index * 7919) % 20003,
        }).collect();
        let order = size_order(&rows, &AtomicBool::new(false)).unwrap();
        let results = ResultSet { rows, order };
        let sizes: Vec<_> = (0..20003).step_by(PAGE_SIZE)
            .flat_map(|offset| results.page(offset, false))
            .map(|row| row.size).collect();
        let mut expected: Vec<_> = results.rows.iter().map(|row| row.size).collect();
        expected.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, expected);
        assert_eq!(results.page(20000, false).len(), 3);
        assert_eq!(results.page(0, true)[0].size, *expected.last().unwrap());
        assert!(results.page(20003, true).is_empty());
        assert!(results.page(usize::MAX, false).is_empty());
    }
}
