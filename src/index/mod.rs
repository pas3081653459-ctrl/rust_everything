mod database;
mod fsevents;
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
use walkdir::WalkDir;

use crate::model::{FileRecord, SearchResult, SortSpec};
use crate::search::SearchQuery;
use database::IndexDatabase;
use fsevents::{NativeWatcher, current_event_id};
use scanner::spawn_scan;

const RESULT_LIMIT: usize = 200;
const INCREMENTAL_CACHE_LIMIT: usize = 1_000;
const MAX_PENDING_PATHS: usize = 50_000;
const GLOBAL_INDEX_ROOT: &str = "/";
const LEGACY_DATABASE_NAMES: &[&str] = &["index.sqlite3", "everything-project.sqlite3"];
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanScope {
    UserFiles,
    SystemFiles,
}

#[derive(Debug)]
enum Command {
    IndexBatch {
        records: Vec<FileRecord>,
        persisted: Sender<()>,
    },
    ScanProgress { discovered: u64, skipped: u64 },
    ScanFinished { indexed: u64, skipped: u64 },
    ScanFailed(String),
    FileEvents {
        changes: Vec<FsChange>,
        last_event_id: u64,
    },
    WatcherAdvanced {
        last_event_id: u64,
    },
    RescanRequired {
        last_event_id: u64,
    },
    Rebuild,
    SetIncludeSystemFiles(bool),
    Shutdown,
}

#[derive(Debug)]
struct FsChange {
    path: PathBuf,
    rescan_tree: bool,
}

#[derive(Debug)]
struct SearchCommand {
    id: u64,
    text: String,
    sort: SortSpec,
    allow_incremental: bool,
}

struct IncrementalSearchCache {
    query: SearchQuery,
    sort: SortSpec,
    results: Vec<SearchResult>,
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

    fn clear_pending(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = None;
        }
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
    Ready {
        indexed: u64,
        root: PathBuf,
        include_system_files: bool,
    },
    ScanStarted { scope: ScanScope },
    ScanProgress {
        scope: ScanScope,
        discovered: u64,
        indexed: u64,
        skipped: u64,
    },
    ScanFinished {
        scope: ScanScope,
        indexed: u64,
        discovered: u64,
        skipped: u64,
    },
    ScopeChangeStarted { include_system_files: bool },
    ScopeChanged {
        include_system_files: bool,
        indexed: u64,
    },
    ScanFailed(String),
    SearchResults {
        id: u64,
        results: Vec<SearchResult>,
        incremental: bool,
    },
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
        let root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("unable to locate the current user's home directory")?;
        let project_dirs = ProjectDirs::from("dev", "RustEverything", "RustEverything")
            .context("unable to locate the application data directory")?;
        let data_dir = project_dirs.data_local_dir();
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("unable to create {}", data_dir.display()))?;
        let legacy_cleanup_warnings = cleanup_legacy_database_files(data_dir);
        // The home directory is the persistent base scope. System files can be
        // added incrementally later without rebuilding the user's index.
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
        if !legacy_cleanup_warnings.is_empty() {
            let _ = event_sink.send(IndexEvent::Warning(format!(
                "Unable to remove some legacy index files: {}",
                legacy_cleanup_warnings.join("; ")
            )));
        }
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

    pub fn search(
        &self,
        id: u64,
        text: String,
        sort: SortSpec,
        allow_incremental: bool,
    ) {
        if let Ok(interrupt) = self.search_interrupt.lock() {
            if let Some(interrupt) = interrupt.as_ref() {
                interrupt.interrupt();
            }
        }

        self.search_mailbox
            .replace(SearchCommand {
                id,
                text,
                sort,
                allow_incremental,
            });
    }

    pub fn cancel_search(&self) {
        self.search_mailbox.clear_pending();
        if let Ok(interrupt) = self.search_interrupt.lock() {
            if let Some(interrupt) = interrupt.as_ref() {
                interrupt.interrupt();
            }
        }
    }

    pub fn rebuild(&self) {
        let _ = self.command_tx.send(Command::Rebuild);
    }

    pub fn set_include_system_files(&self, include: bool) {
        self.cancel_search();
        let _ = self
            .command_tx
            .send(Command::SetIncludeSystemFiles(include));
    }

    pub fn events(&self) -> &Receiver<IndexEvent> {
        &self.event_rx
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn cleanup_legacy_database_files(data_dir: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for database_name in LEGACY_DATABASE_NAMES {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let path = data_dir.join(format!("{database_name}{suffix}"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warnings.push(format!("{} ({error})", path.display())),
            }
        }
    }
    warnings
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

    let mut incremental_cache = None::<IncrementalSearchCache>;
    while let Some(mut request) = search_mailbox.receive() {
        // A newer request may have arrived between waking this thread and
        // starting SQLite. Only execute the newest queued input.
        while let Some(newer) = search_mailbox.take_pending() {
            request = newer;
        }
        if search_mailbox.is_shutdown() {
            break;
        }

        let query = SearchQuery::parse(&request.text);
        let can_filter_cache = request.allow_incremental
            && incremental_cache.as_ref().is_some_and(|cache| {
                cache.sort == request.sort && query.is_strict_refinement_of(&cache.query)
            });

        if can_filter_cache {
            let cache = incremental_cache
                .as_mut()
                .expect("incremental cache was checked above");
            cache.results.retain(|result| query.matches(result));
            for result in &mut cache.results {
                result.score = query.rank(result);
            }
            cache.query = query;
            event_tx.send(IndexEvent::SearchResults {
                id: request.id,
                results: cache.results.iter().take(RESULT_LIMIT).cloned().collect(),
                incremental: true,
            })?;
            continue;
        }

        let cacheable = query.supports_incremental_filtering();
        let limit = if cacheable {
            INCREMENTAL_CACHE_LIMIT
        } else {
            RESULT_LIMIT
        };
        match database.search(&request.text, limit, request.sort) {
            Ok(results) => {
                let visible_results = results.iter().take(RESULT_LIMIT).cloned().collect();
                incremental_cache = cacheable.then_some(IncrementalSearchCache {
                    query,
                    sort: request.sort,
                    results,
                });
                event_tx.send(IndexEvent::SearchResults {
                    id: request.id,
                    results: visible_results,
                    incremental: false,
                })?;
            }
            Err(error) if is_interrupted_search(&error) => {}
            Err(error) => {
                incremental_cache = None;
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
    let stored_root = database.stored_index_root()?;
    let root_text = root.to_string_lossy();
    if stored_root.as_deref() != Some(root_text.as_ref()) {
        let entries = database.count()?;
        if entries > 0 {
            event_tx.send(IndexEvent::Resetting { entries })?;
        }
    }
    database.ensure_index_root(&root)?;
    let mut include_system_files = database.include_system_files()?;
    let mut initial_count = database.count()?;
    let user_scan_required = initial_count == 0 || !database.is_scan_complete()?;
    let system_scan_required =
        include_system_files && !database.is_system_scan_complete()?;
    if user_scan_required {
        database.set_scan_complete(false)?;
        database.set_system_scan_complete(false)?;
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
        include_system_files,
    })?;
    let _ = database_ready_tx.send(());
    let mut indexed_count = initial_count;

    let mut watched_root = if include_system_files {
        PathBuf::from(GLOBAL_INDEX_ROOT)
    } else {
        root.clone()
    };
    let watcher_since = if user_scan_required || system_scan_required {
        current_event_id()
    } else {
        database
            .last_fsevent_id()?
            .unwrap_or_else(current_event_id)
    };
    database.set_last_fsevent_id(watcher_since)?;
    let mut watcher = match NativeWatcher::start(
        &watched_root,
        watcher_since,
        excluded_roots.as_ref(),
        command_tx.clone(),
    ) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            event_tx.send(IndexEvent::WatcherWarning(format!(
                "Could not start native FSEvents watching ({error:#}); search will use the current scan."
            )))?;
            None
        }
    };

    let scanning = Arc::new(AtomicBool::new(false));
    let system_excluded_roots = system_scan_excluded_roots(&excluded_roots, &root);
    let mut active_scan = None::<ScanScope>;
    let mut scan_system_after_user = include_system_files && user_scan_required;
    if user_scan_required {
        if begin_scan(
            ScanScope::UserFiles,
            &root,
            &excluded_roots,
            &command_tx,
            &event_tx,
            &scanning,
        ) {
            active_scan = Some(ScanScope::UserFiles);
        }
    } else if system_scan_required
        && begin_scan(
            ScanScope::SystemFiles,
            Path::new(GLOBAL_INDEX_ROOT),
            &system_excluded_roots,
            &command_tx,
            &event_tx,
            &scanning,
        )
    {
        active_scan = Some(ScanScope::SystemFiles);
    }

    let mut pending_paths = HashMap::<PathBuf, bool>::new();
    let mut pending_event_id = None::<u64>;
    let mut last_file_event = Instant::now();
    let mut first_pending_at = None::<Instant>;
    let mut rescan_after_scan = false;

    loop {
        match command_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Command::IndexBatch { records, persisted }) => {
                let inserted = database.upsert_batch(&records)?;
                indexed_count = indexed_count.saturating_add(inserted);
                let _ = persisted.send(());
            }
            Ok(Command::ScanProgress {
                discovered,
                skipped,
            }) => {
                event_tx.send(IndexEvent::ScanProgress {
                    scope: active_scan.unwrap_or(ScanScope::UserFiles),
                    discovered,
                    indexed: indexed_count,
                    skipped,
                })?;
            }
            Ok(Command::ScanFinished {
                indexed: scanned,
                skipped,
            }) => {
                let scope = active_scan.take().unwrap_or(ScanScope::UserFiles);
                match scope {
                    ScanScope::UserFiles => database.set_scan_complete(true)?,
                    ScanScope::SystemFiles => database.set_system_scan_complete(true)?,
                }
                scanning.store(false, Ordering::Release);
                event_tx.send(IndexEvent::ScanFinished {
                    scope,
                    indexed: indexed_count,
                    discovered: scanned + skipped,
                    skipped,
                })?;
                if scope == ScanScope::UserFiles && scan_system_after_user {
                    scan_system_after_user = false;
                    database.set_system_scan_complete(false)?;
                    if begin_scan(
                        ScanScope::SystemFiles,
                        Path::new(GLOBAL_INDEX_ROOT),
                        &system_excluded_roots,
                        &command_tx,
                        &event_tx,
                        &scanning,
                    ) {
                        active_scan = Some(ScanScope::SystemFiles);
                    }
                } else if rescan_after_scan {
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
                        &mut active_scan,
                        &mut scan_system_after_user,
                        include_system_files,
                    )?;
                }
            }
            Ok(Command::ScanFailed(message)) => {
                scanning.store(false, Ordering::Release);
                rescan_after_scan = false;
                event_tx.send(IndexEvent::ScanFailed(message))?;
            }
            Ok(Command::WatcherAdvanced { last_event_id }) => {
                advance_event_id(&mut pending_event_id, last_event_id);
            }
            Ok(Command::RescanRequired { last_event_id }) => {
                advance_event_id(&mut pending_event_id, last_event_id);
                if scanning.load(Ordering::Acquire) {
                    rescan_after_scan = true;
                    event_tx.send(IndexEvent::Warning(
                        "File-system events were dropped; a configured-scope rebuild is queued."
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
                        &mut active_scan,
                        &mut scan_system_after_user,
                        include_system_files,
                    )?;
                }
            }
            Ok(Command::FileEvents {
                changes,
                last_event_id,
            }) => {
                advance_event_id(&mut pending_event_id, last_event_id);
                let pending_was_empty = pending_paths.is_empty();
                for change in changes.into_iter().filter(|change| {
                    !is_excluded_path(&change.path, excluded_roots.as_ref())
                        && (include_system_files || change.path.starts_with(&root))
                }) {
                    insert_pending_path(&mut pending_paths, change);
                }
                let now = Instant::now();
                if pending_was_empty && !pending_paths.is_empty() {
                    first_pending_at = Some(now);
                }
                if pending_paths.len() > MAX_PENDING_PATHS {
                    pending_paths.clear();
                    pending_paths.insert(watched_root.clone(), true);
                    first_pending_at = None;
                    let _ = command_tx.send(Command::RescanRequired { last_event_id });
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
                    &mut active_scan,
                    &mut scan_system_after_user,
                    include_system_files,
                )? {
                    pending_paths.clear();
                    first_pending_at = None;
                } else {
                    event_tx.send(IndexEvent::Warning(
                        "An index scan is already running.".to_owned(),
                    ))?;
                }
            }
            Ok(Command::SetIncludeSystemFiles(include)) => {
                if include == include_system_files {
                    event_tx.send(IndexEvent::ScopeChanged {
                        include_system_files,
                        indexed: indexed_count,
                    })?;
                } else if scanning.load(Ordering::Acquire) {
                    event_tx.send(IndexEvent::Warning(
                        "Wait for the current scan to finish before changing the index scope."
                            .to_owned(),
                    ))?;
                    event_tx.send(IndexEvent::ScopeChanged {
                        include_system_files,
                        indexed: indexed_count,
                    })?;
                } else {
                    event_tx.send(IndexEvent::ScopeChangeStarted {
                        include_system_files: include,
                    })?;
                    pending_paths.clear();
                    first_pending_at = None;
                    let new_watch_root = if include {
                        Path::new(GLOBAL_INDEX_ROOT)
                    } else {
                        root.as_path()
                    };
                    update_watched_root(
                        &mut watcher,
                        &mut watched_root,
                        new_watch_root,
                        excluded_roots.as_ref(),
                        &command_tx,
                        &mut database,
                        &event_tx,
                    )?;
                    pending_event_id = None;

                    if include {
                        include_system_files = true;
                        database.set_include_system_files(true)?;
                        database.set_system_scan_complete(false)?;
                        event_tx.send(IndexEvent::ScopeChanged {
                            include_system_files: true,
                            indexed: indexed_count,
                        })?;
                        if begin_scan(
                            ScanScope::SystemFiles,
                            Path::new(GLOBAL_INDEX_ROOT),
                            &system_excluded_roots,
                            &command_tx,
                            &event_tx,
                            &scanning,
                        ) {
                            active_scan = Some(ScanScope::SystemFiles);
                        }
                    } else {
                        database.retain_path_tree(&root)?;
                        include_system_files = false;
                        indexed_count = database.count()?;
                        event_tx.send(IndexEvent::ScopeChanged {
                            include_system_files: false,
                            indexed: indexed_count,
                        })?;
                    }
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
            refresh_paths(
                &mut database,
                paths,
                excluded_roots.as_ref(),
                &mut indexed_count,
                &event_tx,
            )?;
        }

        if !scanning.load(Ordering::Acquire) && pending_paths.is_empty() {
            if let Some(event_id) = pending_event_id.take() {
                database.set_last_fsevent_id(event_id)?;
            }
        }
    }

    drop(watcher);
    Ok(())
}

fn begin_scan(
    scope: ScanScope,
    root: &Path,
    excluded_roots: &Arc<[PathBuf]>,
    command_tx: &Sender<Command>,
    event_tx: &EventSink,
    scanning: &Arc<AtomicBool>,
) -> bool {
    if !scanning.swap(true, Ordering::AcqRel) {
        let _ = event_tx.send(IndexEvent::ScanStarted { scope });
        spawn_scan(
            root.to_path_buf(),
            Arc::clone(excluded_roots),
            command_tx.clone(),
            scanning.clone(),
        );
        true
    } else {
        false
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
    active_scan: &mut Option<ScanScope>,
    scan_system_after_user: &mut bool,
    include_system_files: bool,
) -> Result<bool> {
    if scanning.swap(true, Ordering::AcqRel) {
        return Ok(false);
    }

    event_tx.send(IndexEvent::ScanStarted {
        scope: ScanScope::UserFiles,
    })?;
    database.set_scan_complete(false)?;
    database.set_system_scan_complete(false)?;
    database.clear()?;
    *indexed_count = 0;
    *active_scan = Some(ScanScope::UserFiles);
    *scan_system_after_user = include_system_files;
    spawn_scan(
        root.to_path_buf(),
        Arc::clone(excluded_roots),
        command_tx.clone(),
        scanning.clone(),
    );
    Ok(true)
}

fn update_watched_root(
    watcher: &mut Option<NativeWatcher>,
    watched_root: &mut PathBuf,
    new_root: &Path,
    excluded_roots: &[PathBuf],
    command_tx: &Sender<Command>,
    database: &mut IndexDatabase,
    event_tx: &EventSink,
) -> Result<()> {
    if watched_root.as_path() == new_root {
        return Ok(());
    }
    let since_event_id = current_event_id();
    match NativeWatcher::start(
        new_root,
        since_event_id,
        excluded_roots,
        command_tx.clone(),
    ) {
        Ok(new_watcher) => {
            // Start the replacement before dropping the previous stream. The
            // persisted cursor makes any overlap harmless and avoids a gap.
            *watcher = Some(new_watcher);
            *watched_root = new_root.to_path_buf();
            database.set_last_fsevent_id(since_event_id)?;
        }
        Err(error) => {
            event_tx.send(IndexEvent::WatcherWarning(format!(
                "Could not watch {} with FSEvents ({error:#}); the previous watch scope remains active.",
                new_root.display()
            )))?;
        }
    }
    Ok(())
}

fn refresh_paths(
    database: &mut IndexDatabase,
    paths: Vec<(PathBuf, bool)>,
    excluded_roots: &[PathBuf],
    indexed_count: &mut u64,
    event_tx: &EventSink,
) -> Result<()> {
    for (path, rescan_tree) in paths {
        if is_excluded_path(&path, excluded_roots) {
            continue;
        }
        if !path.exists() {
            let removed = database.remove_path_tree(&path)?;
            *indexed_count = (*indexed_count).saturating_sub(removed);
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
                    let inserted = database.upsert_batch(&batch)?;
                    *indexed_count = (*indexed_count).saturating_add(inserted);
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                let inserted = database.upsert_batch(&batch)?;
                *indexed_count = (*indexed_count).saturating_add(inserted);
            }
        } else if path.is_dir() {
            if let Some(record) = FileRecord::from_path(&path) {
                let inserted = database.upsert_batch(&[record])?;
                *indexed_count = (*indexed_count).saturating_add(inserted);
            }
        } else if let Some(record) = FileRecord::from_path(&path) {
            let inserted = database.upsert_batch(&[record])?;
            *indexed_count = (*indexed_count).saturating_add(inserted);
        }
    }
    event_tx.send(IndexEvent::IndexChanged {
        indexed: *indexed_count,
    })?;
    Ok(())
}

fn global_excluded_roots(data_dir: &Path) -> Arc<[PathBuf]> {
    let mut roots: Vec<PathBuf> = GLOBAL_EXCLUDED_PATHS
        .iter()
        .map(|path| PathBuf::from(*path))
        .collect();
    roots.push(data_dir.to_path_buf());
    roots.into()
}

fn system_scan_excluded_roots(
    excluded_roots: &Arc<[PathBuf]>,
    user_root: &Path,
) -> Arc<[PathBuf]> {
    let mut roots = excluded_roots.as_ref().to_vec();
    roots.push(user_root.to_path_buf());
    roots.into()
}

pub(super) fn is_excluded_path(path: &Path, excluded_roots: &[PathBuf]) -> bool {
    excluded_roots.iter().any(|root| path.starts_with(root))
}

fn advance_event_id(current: &mut Option<u64>, event_id: u64) {
    *current = Some(current.map_or(event_id, |value| value.max(event_id)));
}

fn insert_pending_path(pending: &mut HashMap<PathBuf, bool>, change: FsChange) {
    if pending.iter().any(|(path, rescan_tree)| {
        *rescan_tree
            && change.path.as_path() != path.as_path()
            && change.path.starts_with(path)
    }) {
        return;
    }
    if change.rescan_tree {
        pending.retain(|path, _| path == &change.path || !path.starts_with(&change.path));
    }
    pending
        .entry(change.path)
        .and_modify(|rescan_tree| *rescan_tree |= change.rescan_tree)
        .or_insert(change.rescan_tree);
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
