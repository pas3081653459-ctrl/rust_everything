mod database;
mod scanner;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use directories::ProjectDirs;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::model::{FileRecord, SearchResult, SortSpec};
use database::IndexDatabase;
use scanner::spawn_scan;

const RESULT_LIMIT: usize = 200;
const MAX_PENDING_PATHS: usize = 50_000;
const GLOBAL_INDEX_ROOT: &str = "/";
// Index the macOS startup disk while avoiding virtual filesystems, APFS backing
// paths that mirror the same data, mounted external/network volumes, and noisy
// system-maintenance stores. Hidden files in all other locations are included.
const GLOBAL_EXCLUDED_PATHS: &[&str] = &[
    "/.DocumentRevisions-V100",
    "/.Spotlight-V100",
    "/.TemporaryItems",
    "/.Trashes",
    "/.fseventsd",
    "/.vol",
    "/dev",
    "/home",
    "/net",
    "/Network",
    "/private/var/run",
    "/private/var/vm",
    "/System/Volumes",
    "/Volumes",
];

#[derive(Debug)]
enum Command {
    IndexBatch {
        records: Vec<FileRecord>,
        persisted: Sender<()>,
    },
    ScanProgress { discovered: u64, skipped: u64 },
    ScanFinished { indexed: u64, skipped: u64 },
    ScanFailed(String),
    FileEvent(Event),
    WatcherFailed(String),
    RescanRequired,
    Rebuild,
    Shutdown,
}

#[derive(Debug)]
struct SearchCommand {
    id: u64,
    text: String,
    sort: SortSpec,
}

#[derive(Default)]
struct SearchMailboxState {
    pending: Option<SearchCommand>,
    shutdown: bool,
}

#[derive(Default)]
struct SearchMailbox {
    state: Mutex<SearchMailboxState>,
    ready: Condvar,
}

impl SearchMailbox {
    fn replace(&self, request: SearchCommand) {
        if let Ok(mut state) = self.state.lock() {
            if !state.shutdown {
                state.pending = Some(request);
                self.ready.notify_one();
            }
        }
    }

    fn receive(&self) -> Option<SearchCommand> {
        let mut state = self.state.lock().ok()?;
        while state.pending.is_none() && !state.shutdown {
            state = self.ready.wait(state).ok()?;
        }
        if state.shutdown {
            None
        } else {
            state.pending.take()
        }
    }

    fn take_pending(&self) -> Option<SearchCommand> {
        self.state.lock().ok()?.pending.take()
    }

    fn is_shutdown(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.shutdown)
            .unwrap_or(true)
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.pending = None;
            self.ready.notify_one();
        }
    }
}

#[derive(Debug)]
pub enum IndexEvent {
    PreparingSearchIndex,
    Resetting { entries: u64 },
    Ready { indexed: u64, root: PathBuf },
    ScanStarted,
    ScanProgress { discovered: u64, skipped: u64 },
    ScanFinished { indexed: u64, skipped: u64 },
    ScanFailed(String),
    SearchResults { id: u64, results: Vec<SearchResult> },
    SearchFailed { id: u64, message: String },
    IndexChanged { indexed: u64 },
    WatcherWarning(String),
    Warning(String),
}

pub struct IndexService {
    command_tx: Sender<Command>,
    search_mailbox: Arc<SearchMailbox>,
    search_interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
    event_rx: Receiver<IndexEvent>,
    root: PathBuf,
}

impl IndexService {
    pub fn start(repaint_context: eframe::egui::Context) -> Result<Self> {
        let root = PathBuf::from(GLOBAL_INDEX_ROOT);
        let project_dirs = ProjectDirs::from("dev", "RustEverything", "RustEverything")
            .context("unable to locate the application data directory")?;
        let data_dir = project_dirs.data_local_dir();
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("unable to create {}", data_dir.display()))?;
        // Use a separate database so changing from the former project-only
        // scope never blocks startup while deleting that index. The database
        // directory itself is excluded from the global scan and file watcher.
        let database_path = data_dir.join("everything-global.sqlite3");
        let excluded_roots = global_excluded_roots(data_dir);

        let (command_tx, command_rx) = unbounded();
        let search_mailbox = Arc::new(SearchMailbox::default());
        let search_interrupt = Arc::new(Mutex::new(None));
        let (database_ready_tx, database_ready_rx) = bounded(1);
        let (event_tx, event_rx) = unbounded();
        let event_sink = EventSink {
            sender: event_tx,
            repaint_context,
        };
        let search_database_path = database_path.clone();
        let search_event_sink = event_sink.clone();
        let search_error_sink = event_sink.clone();
        let worker_search_mailbox = Arc::clone(&search_mailbox);
        let worker_search_interrupt = Arc::clone(&search_interrupt);

        thread::Builder::new()
            .name("index-search".to_owned())
            .spawn(move || {
                if let Err(error) = run_search_worker(
                    search_database_path,
                    database_ready_rx,
                    worker_search_mailbox,
                    search_event_sink,
                    worker_search_interrupt,
                ) {
                    let _ = search_error_sink.send(IndexEvent::SearchFailed {
                        id: 0,
                        message: format!("Search service error: {error:#}"),
                    });
                }
            })
            .context("unable to start the search worker")?;

        let worker_command_tx = command_tx.clone();
        let worker_root = root.clone();
        let error_sink = event_sink.clone();

        thread::Builder::new()
            .name("index-database".to_owned())
            .spawn(move || {
                if let Err(error) = run_worker(
                    database_path,
                    worker_root,
                    excluded_roots,
                    worker_command_tx,
                    command_rx,
                    event_sink,
                    database_ready_tx,
                ) {
                    let _ = error_sink.send(IndexEvent::ScanFailed(format!(
                        "Index error: {error:#}"
                    )));
                }
            })
            .context("unable to start the index worker")?;

        Ok(Self {
            command_tx,
            search_mailbox,
            search_interrupt,
            event_rx,
            root,
        })
    }

    pub fn search(&self, id: u64, text: String, sort: SortSpec) {
        if let Ok(interrupt) = self.search_interrupt.lock() {
            if let Some(interrupt) = interrupt.as_ref() {
                interrupt.interrupt();
            }
        }

        self.search_mailbox
            .replace(SearchCommand { id, text, sort });
    }

    pub fn rebuild(&self) {
        let _ = self.command_tx.send(Command::Rebuild);
    }

    pub fn events(&self) -> &Receiver<IndexEvent> {
        &self.event_rx
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Clone)]
struct EventSink {
    sender: Sender<IndexEvent>,
    repaint_context: eframe::egui::Context,
}

impl EventSink {
    fn send(&self, event: IndexEvent) -> Result<()> {
        self.sender
            .send(event)
            .context("the UI event receiver was disconnected")?;
        self.repaint_context.request_repaint();
        Ok(())
    }
}

impl Drop for IndexService {
    fn drop(&mut self) {
        if let Ok(interrupt) = self.search_interrupt.lock() {
            if let Some(interrupt) = interrupt.as_ref() {
                interrupt.interrupt();
            }
        }
        self.search_mailbox.shutdown();
        let _ = self.command_tx.send(Command::Shutdown);
    }
}

fn run_search_worker(
    database_path: PathBuf,
    database_ready_rx: Receiver<()>,
    search_mailbox: Arc<SearchMailbox>,
    event_tx: EventSink,
    interrupt_slot: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
) -> Result<()> {
    database_ready_rx
        .recv()
        .context("the index database stopped before search became ready")?;
    let database = IndexDatabase::open_read_only(&database_path)?;
    if let Ok(mut interrupt) = interrupt_slot.lock() {
        *interrupt = Some(database.interrupt_handle());
    }

    while let Some(mut request) = search_mailbox.receive() {
        // A newer request may have arrived between waking this thread and
        // starting SQLite. Only execute the newest queued input.
        while let Some(newer) = search_mailbox.take_pending() {
            request = newer;
        }
        if search_mailbox.is_shutdown() {
            break;
        }

        match database.search(&request.text, RESULT_LIMIT, request.sort) {
            Ok(results) => {
                event_tx.send(IndexEvent::SearchResults {
                    id: request.id,
                    results,
                })?;
            }
            Err(error) if is_interrupted_search(&error) => {}
            Err(error) => {
                event_tx.send(IndexEvent::SearchFailed {
                    id: request.id,
                    message: format!("Search failed: {error:#}"),
                })?;
            }
        }
    }

    if let Ok(mut interrupt) = interrupt_slot.lock() {
        *interrupt = None;
    }
    Ok(())
}

fn is_interrupted_search(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<rusqlite::Error>()
        .and_then(rusqlite::Error::sqlite_error_code)
        == Some(rusqlite::ErrorCode::OperationInterrupted)
}

fn run_worker(
    database_path: PathBuf,
    root: PathBuf,
    excluded_roots: Arc<[PathBuf]>,
    command_tx: Sender<Command>,
    command_rx: Receiver<Command>,
    event_tx: EventSink,
    database_ready_tx: Sender<()>,
) -> Result<()> {
    event_tx.send(IndexEvent::PreparingSearchIndex)?;
    let mut database = IndexDatabase::open(&database_path)?;
    database.ensure_index_root(&root)?;
    let mut initial_count = database.count()?;
    let scan_required = initial_count == 0 || !database.is_scan_complete()?;
    if scan_required {
        database.set_scan_complete(false)?;
        if initial_count > 0 {
            event_tx.send(IndexEvent::Resetting {
                entries: initial_count,
            })?;
            database.clear()?;
            initial_count = 0;
        }
    }
    event_tx.send(IndexEvent::Ready {
        indexed: initial_count,
        root: root.clone(),
    })?;
    let _ = database_ready_tx.send(());
    let mut indexed_count = initial_count;

    let watch_tx = command_tx.clone();
    let callback_excluded_roots = Arc::clone(&excluded_roots);
    let watcher_error_reported = Arc::new(AtomicBool::new(false));
    let callback_watcher_error_reported = Arc::clone(&watcher_error_reported);
    let watcher_result: notify::Result<RecommendedWatcher> =
        notify::recommended_watcher(move |result: notify::Result<Event>| match result {
            Ok(mut event) => {
                if event.need_rescan() {
                    let _ = watch_tx.send(Command::RescanRequired);
                    return;
                }
                event.paths.retain(|path| {
                    !is_excluded_path(path, callback_excluded_roots.as_ref())
                });
                if !event.paths.is_empty() {
                    let _ = watch_tx.send(Command::FileEvent(event));
                }
            }
            Err(error) => {
                if !callback_watcher_error_reported.swap(true, Ordering::AcqRel) {
                    let _ = watch_tx.send(Command::WatcherFailed(error.to_string()));
                }
            }
        });
    let watcher = match watcher_result {
        Ok(mut watcher) => match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => Some(watcher),
            Err(error) => {
                event_tx.send(IndexEvent::WatcherWarning(format!(
                    "Global file watching is unavailable ({error}); search will use the current scan."
                )))?;
                None
            }
        },
        Err(error) => {
            event_tx.send(IndexEvent::WatcherWarning(format!(
                "Could not start global file watching ({error}); search will use the current scan."
            )))?;
            None
        }
    };

    let scanning = Arc::new(AtomicBool::new(false));
    if scan_required {
        begin_scan(
            &root,
            &excluded_roots,
            &command_tx,
            &event_tx,
            &scanning,
        );
    }

    let mut pending_paths = HashMap::<PathBuf, bool>::new();
    let mut last_file_event = Instant::now();
    let mut first_pending_at = None::<Instant>;
    let mut rescan_after_scan = false;

    loop {
        match command_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Command::IndexBatch { records, persisted }) => {
                let batch_len = records.len() as u64;
                database.upsert_batch(&records)?;
                indexed_count = indexed_count.saturating_add(batch_len);
                event_tx.send(IndexEvent::IndexChanged {
                    indexed: indexed_count,
                })?;
                let _ = persisted.send(());
            }
            Ok(Command::ScanProgress {
                discovered,
                skipped,
            }) => {
                event_tx.send(IndexEvent::ScanProgress {
                    discovered,
                    skipped,
                })?;
            }
            Ok(Command::ScanFinished { indexed, skipped }) => {
                if !rescan_after_scan {
                    database.set_scan_complete(true)?;
                }
                scanning.store(false, Ordering::Release);
                indexed_count = indexed;
                event_tx.send(IndexEvent::ScanFinished {
                    indexed: indexed_count,
                    skipped,
                })?;
                if rescan_after_scan {
                    rescan_after_scan = false;
                    pending_paths.clear();
                    first_pending_at = None;
                    let _ = rebuild_index(
                        &mut database,
                        &root,
                        &excluded_roots,
                        &command_tx,
                        &event_tx,
                        &scanning,
                        &mut indexed_count,
                    )?;
                }
            }
            Ok(Command::ScanFailed(message)) => {
                scanning.store(false, Ordering::Release);
                rescan_after_scan = false;
                event_tx.send(IndexEvent::ScanFailed(message))?;
            }
            Ok(Command::WatcherFailed(message)) => {
                event_tx.send(IndexEvent::WatcherWarning(format!(
                    "Global file watching reported an error ({message}); a recovery scan was queued."
                )))?;
                if scanning.load(Ordering::Acquire) {
                    rescan_after_scan = true;
                } else {
                    pending_paths.clear();
                    first_pending_at = None;
                    let _ = rebuild_index(
                        &mut database,
                        &root,
                        &excluded_roots,
                        &command_tx,
                        &event_tx,
                        &scanning,
                        &mut indexed_count,
                    )?;
                }
            }
            Ok(Command::RescanRequired) => {
                if scanning.load(Ordering::Acquire) {
                    rescan_after_scan = true;
                    event_tx.send(IndexEvent::Warning(
                        "File-system events were dropped; another global scan is queued."
                            .to_owned(),
                    ))?;
                } else {
                    pending_paths.clear();
                    first_pending_at = None;
                    let _ = rebuild_index(
                        &mut database,
                        &root,
                        &excluded_roots,
                        &command_tx,
                        &event_tx,
                        &scanning,
                        &mut indexed_count,
                    )?;
                }
            }
            Ok(Command::FileEvent(event)) => {
                let rescan_tree = matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
                );
                let pending_was_empty = pending_paths.is_empty();
                for path in event
                    .paths
                    .into_iter()
                    .filter(|path| {
                        !is_excluded_path(path, excluded_roots.as_ref())
                    })
                {
                    pending_paths
                        .entry(path)
                        .and_modify(|current| *current |= rescan_tree)
                        .or_insert(rescan_tree);
                }
                let now = Instant::now();
                if pending_was_empty && !pending_paths.is_empty() {
                    first_pending_at = Some(now);
                }
                if pending_paths.len() > MAX_PENDING_PATHS {
                    pending_paths.clear();
                    first_pending_at = None;
                    let _ = command_tx.send(Command::RescanRequired);
                }
                last_file_event = now;
            }
            Ok(Command::Rebuild) => {
                if rebuild_index(
                    &mut database,
                    &root,
                    &excluded_roots,
                    &command_tx,
                    &event_tx,
                    &scanning,
                    &mut indexed_count,
                )? {
                    pending_paths.clear();
                    first_pending_at = None;
                } else {
                    event_tx.send(IndexEvent::Warning(
                        "An index scan is already running.".to_owned(),
                    ))?;
                }
            }
            Ok(Command::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }

        let pending_too_old = first_pending_at
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(2));
        if !pending_paths.is_empty()
            && (last_file_event.elapsed() >= Duration::from_millis(400) || pending_too_old)
            && !scanning.load(Ordering::Acquire)
        {
            let paths: Vec<_> = pending_paths.drain().collect();
            first_pending_at = None;
            indexed_count = refresh_paths(
                &mut database,
                paths,
                excluded_roots.as_ref(),
                &event_tx,
            )?;
        }
    }

    drop(watcher);
    Ok(())
}

fn begin_scan(
    root: &Path,
    excluded_roots: &Arc<[PathBuf]>,
    command_tx: &Sender<Command>,
    event_tx: &EventSink,
    scanning: &Arc<AtomicBool>,
) {
    if !scanning.swap(true, Ordering::AcqRel) {
        let _ = event_tx.send(IndexEvent::ScanStarted);
        spawn_scan(
            root.to_path_buf(),
            Arc::clone(excluded_roots),
            command_tx.clone(),
            scanning.clone(),
        );
    }
}

fn rebuild_index(
    database: &mut IndexDatabase,
    root: &Path,
    excluded_roots: &Arc<[PathBuf]>,
    command_tx: &Sender<Command>,
    event_tx: &EventSink,
    scanning: &Arc<AtomicBool>,
    indexed_count: &mut u64,
) -> Result<bool> {
    if scanning.swap(true, Ordering::AcqRel) {
        return Ok(false);
    }

    event_tx.send(IndexEvent::ScanStarted)?;
    database.set_scan_complete(false)?;
    database.clear()?;
    *indexed_count = 0;
    spawn_scan(
        root.to_path_buf(),
        Arc::clone(excluded_roots),
        command_tx.clone(),
        scanning.clone(),
    );
    Ok(true)
}

fn refresh_paths(
    database: &mut IndexDatabase,
    paths: Vec<(PathBuf, bool)>,
    excluded_roots: &[PathBuf],
    event_tx: &EventSink,
) -> Result<u64> {
    for (path, rescan_tree) in paths {
        if is_excluded_path(&path, excluded_roots) {
            continue;
        }
        if !path.exists() {
            database.remove_path_tree(&path)?;
        } else if path.is_dir() && rescan_tree {
            let mut batch = Vec::with_capacity(1_000);
            let walker = WalkDir::new(&path)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    !is_excluded_path(entry.path(), excluded_roots)
                });
            for entry in walker.flatten() {
                if let Some(record) = FileRecord::from_path(entry.path()) {
                    batch.push(record);
                }
                if batch.len() >= 1_000 {
                    database.upsert_batch(&batch)?;
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                database.upsert_batch(&batch)?;
            }
        } else if path.is_dir() {
            if let Some(record) = FileRecord::from_path(&path) {
                database.upsert_batch(&[record])?;
            }
        } else if let Some(record) = FileRecord::from_path(&path) {
            database.upsert_batch(&[record])?;
        }
    }
    let indexed = database.count()?;
    event_tx.send(IndexEvent::IndexChanged { indexed })?;
    Ok(indexed)
}

fn global_excluded_roots(data_dir: &Path) -> Arc<[PathBuf]> {
    let mut roots: Vec<PathBuf> = GLOBAL_EXCLUDED_PATHS
        .iter()
        .map(|path| PathBuf::from(*path))
        .collect();
    roots.push(data_dir.to_path_buf());
    roots.into()
}

pub(super) fn is_excluded_path(path: &Path, excluded_roots: &[PathBuf]) -> bool {
    excluded_roots.iter().any(|root| path.starts_with(root))
}

pub(super) fn send_batch(sender: &Sender<Command>, records: Vec<FileRecord>) -> bool {
    let (persisted_tx, persisted_rx) = bounded(1);
    if sender
        .send(Command::IndexBatch {
            records,
            persisted: persisted_tx,
        })
        .is_err()
    {
        return false;
    }
    persisted_rx.recv().is_ok()
}

pub(super) fn send_scan_finished(sender: &Sender<Command>, indexed: u64, skipped: u64) {
    let _ = sender.send(Command::ScanFinished { indexed, skipped });
}

pub(super) fn send_scan_progress(sender: &Sender<Command>, discovered: u64, skipped: u64) {
    let _ = sender.send(Command::ScanProgress {
        discovered,
        skipped,
    });
}

pub(super) fn send_scan_failed(sender: &Sender<Command>, message: String) {
    let _ = sender.send(Command::ScanFailed(message));
}
