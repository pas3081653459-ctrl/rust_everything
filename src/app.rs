use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui;
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{CMD_OR_CTRL, Code, HotKey},
};

use crate::index::{IndexEvent, IndexService, ScanScope};
use crate::model::{EntryKind, SearchResult, SortField, SortSpec};
use crate::native_icon::NativeIconCache;
use crate::platform;
use crate::search::SearchQuery;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
const INCREMENTAL_VERIFY_DELAY: Duration = Duration::from_millis(500);
const INDEX_REFRESH_COALESCE: Duration = Duration::from_secs(5);
const HEADER_HEIGHT: f32 = 32.0;
const RESULT_ROW_HEIGHT: f32 = 30.0;
const TYPE_WIDTH: f32 = 72.0;
const SIZE_WIDTH: f32 = 88.0;
const MODIFIED_WIDTH: f32 = 144.0;
const ACTIONS_WIDTH: f32 = 126.0;
const NAME_MIN_WIDTH: f32 = 140.0;
const PATH_MIN_WIDTH: f32 = 140.0;
const CELL_HORIZONTAL_PADDING: f32 = 8.0;
const FILE_ICON_SIZE: f32 = 18.0;

#[cfg(target_os = "macos")]
fn configure_chinese_font(context: &egui::Context) {
    const FONT_CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
    ];

    let Some(font_bytes) = FONT_CANDIDATES
        .iter()
        .find_map(|path| std::fs::read(path).ok())
    else {
        return;
    };

    let font_name = "macos-chinese".to_owned();
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        font_name.clone(),
        Arc::new(egui::FontData::from_owned(font_bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(fallbacks) = fonts.families.get_mut(&family) {
            fallbacks.push(font_name.clone());
        }
    }
    context.set_fonts(fonts);
}

#[cfg(not(target_os = "macos"))]
fn configure_chinese_font(_context: &egui::Context) {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileIconKind {
    Folder,
    Application,
    Text,
    Code,
    Image,
    Pdf,
    Audio,
    Video,
    Archive,
    Spreadsheet,
    Presentation,
    Document,
    Database,
    Font,
    Package,
    DiskImage,
    Executable,
    Symlink,
    Generic,
}

impl FileIconKind {
    fn for_result(result: &SearchResult) -> Self {
        let lower_name = result.name.to_ascii_lowercase();
        if result.kind == EntryKind::Symlink {
            return Self::Symlink;
        }
        if result.kind == EntryKind::Directory {
            return if lower_name.ends_with(".app") {
                Self::Application
            } else if [".framework", ".bundle", ".plugin"]
                .iter()
                .any(|suffix| lower_name.ends_with(*suffix))
            {
                Self::Package
            } else {
                Self::Folder
            };
        }

        let extension = Path::new(&lower_name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        match extension {
            "txt" | "md" | "markdown" | "rtf" | "log" | "ini" | "conf" | "cfg"
            | "toml" | "yaml" | "yml" | "xml" | "json" | "lock" | "tex" => Self::Text,
            "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "js" | "jsx"
            | "ts" | "tsx" | "java" | "kt" | "kts" | "swift" | "go" | "rb"
            | "php" | "sh" | "bash" | "zsh" | "fish" | "html" | "css" | "scss"
            | "sql" | "lua" | "vue" | "svelte" | "dart" | "cs" | "m" | "mm" | "r"
            | "pl" => Self::Code,
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tif" | "tiff" | "webp"
            | "heic" | "heif" | "svg" | "ico" | "raw" | "dng" | "psd" => Self::Image,
            "pdf" => Self::Pdf,
            "mp3" | "wav" | "aac" | "m4a" | "flac" | "ogg" | "aiff" | "mid"
            | "midi" | "opus" => Self::Audio,
            "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpeg" | "mpg"
            | "flv" => Self::Video,
            "zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tgz" => {
                Self::Archive
            }
            "csv" | "xls" | "xlsx" | "numbers" | "ods" => Self::Spreadsheet,
            "ppt" | "pptx" | "key" | "odp" => Self::Presentation,
            "doc" | "docx" | "pages" | "odt" => Self::Document,
            "db" | "sqlite" | "sqlite3" => Self::Database,
            "ttf" | "otf" | "woff" | "woff2" => Self::Font,
            "pkg" | "deb" | "rpm" | "jar" | "wasm" => Self::Package,
            "dmg" | "iso" | "img" => Self::DiskImage,
            "exe" | "bin" | "command" | "dylib" | "so" | "a" => Self::Executable,
            _ if is_text_dotfile(&lower_name) => Self::Text,
            _ => Self::Generic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Folder => "Folder",
            Self::Application => "App",
            Self::Text => "Text",
            Self::Code => "Code",
            Self::Image => "Image",
            Self::Pdf => "PDF",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Archive => "Archive",
            Self::Spreadsheet => "Sheet",
            Self::Presentation => "Slides",
            Self::Document => "Document",
            Self::Database => "Database",
            Self::Font => "Font",
            Self::Package => "Package",
            Self::DiskImage => "Disk",
            Self::Executable => "Binary",
            Self::Symlink => "Link",
            Self::Generic => "File",
        }
    }

    fn badge(self) -> (&'static str, egui::Color32) {
        match self {
            Self::Application => ("APP", egui::Color32::from_rgb(61, 126, 229)),
            Self::Text => ("TXT", egui::Color32::from_rgb(94, 120, 153)),
            Self::Code => ("<>", egui::Color32::from_rgb(103, 86, 191)),
            Self::Image => ("IMG", egui::Color32::from_rgb(36, 154, 137)),
            Self::Pdf => ("PDF", egui::Color32::from_rgb(213, 59, 57)),
            Self::Audio => ("AUD", egui::Color32::from_rgb(151, 76, 181)),
            Self::Video => ("VID", egui::Color32::from_rgb(219, 113, 48)),
            Self::Archive => ("ZIP", egui::Color32::from_rgb(147, 111, 72)),
            Self::Spreadsheet => ("XLS", egui::Color32::from_rgb(45, 145, 82)),
            Self::Presentation => ("PPT", egui::Color32::from_rgb(218, 102, 48)),
            Self::Document => ("DOC", egui::Color32::from_rgb(55, 111, 204)),
            Self::Database => ("DB", egui::Color32::from_rgb(42, 132, 159)),
            Self::Font => ("Aa", egui::Color32::from_rgb(121, 92, 170)),
            Self::Package => ("PKG", egui::Color32::from_rgb(153, 105, 62)),
            Self::DiskImage => ("DMG", egui::Color32::from_rgb(99, 111, 126)),
            Self::Executable => ("EXE", egui::Color32::from_rgb(56, 66, 78)),
            Self::Symlink => ("LNK", egui::Color32::from_rgb(41, 142, 180)),
            Self::Generic => ("FILE", egui::Color32::from_rgb(92, 126, 166)),
            Self::Folder => ("", egui::Color32::TRANSPARENT),
        }
    }
}

#[derive(Clone, Copy)]
struct TableColumns {
    name: f32,
    kind: f32,
    size: f32,
    modified: f32,
    path: f32,
    actions: f32,
}

impl TableColumns {
    fn for_available_width(available: f32) -> Self {
        let fixed = TYPE_WIDTH + SIZE_WIDTH + MODIFIED_WIDTH + ACTIONS_WIDTH;
        let table_width = available.max(fixed + 120.0);
        let flexible = table_width - fixed;
        let name = if flexible >= NAME_MIN_WIDTH + PATH_MIN_WIDTH {
            (flexible * 0.38).clamp(NAME_MIN_WIDTH, flexible - PATH_MIN_WIDTH)
        } else {
            // Keep every fixed column visible at the minimum window size by
            // letting Name and Path compress proportionally below their ideal
            // widths. Their labels are clipped inside the cells.
            flexible * 0.48
        };

        Self {
            name,
            kind: TYPE_WIDTH,
            size: SIZE_WIDTH,
            modified: MODIFIED_WIDTH,
            path: flexible - name,
            actions: ACTIONS_WIDTH,
        }
    }

    fn total(self) -> f32 {
        self.name + self.kind + self.size + self.modified + self.path + self.actions
    }

    fn divider_offsets(self) -> [f32; 5] {
        [
            self.name,
            self.name + self.kind,
            self.name + self.kind + self.size,
            self.name + self.kind + self.size + self.modified,
            self.name + self.kind + self.size + self.modified + self.path,
        ]
    }
}

struct GlobalShortcut {
    _manager: GlobalHotKeyManager,
    pressed: Arc<AtomicBool>,
    stop_tx: crossbeam_channel::Sender<()>,
    listener: Option<JoinHandle<()>>,
}

impl GlobalShortcut {
    fn register(context: &egui::Context) -> std::result::Result<Self, String> {
        // global-hotkey requires its manager to be constructed on the macOS
        // main thread. SearchApp::new is called by eframe's app creator there.
        let manager = GlobalHotKeyManager::new()
            .map_err(|error| format!("could not create the hotkey manager: {error}"))?;
        let hotkey = HotKey::new(Some(CMD_OR_CTRL), Code::Space);
        let hotkey_id = hotkey.id();
        manager
            .register(hotkey)
            .map_err(|error| format!("could not register Command+Space: {error}"))?;

        let pressed = Arc::new(AtomicBool::new(false));
        let thread_pressed = Arc::clone(&pressed);
        let repaint_context = context.clone();
        let hotkey_events = GlobalHotKeyEvent::receiver().clone();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let listener = thread::Builder::new()
            .name("global-hotkey-listener".to_owned())
            .spawn(move || loop {
                crossbeam_channel::select! {
                    recv(stop_rx) -> _ => break,
                    recv(hotkey_events) -> event => {
                        let Ok(event) = event else {
                            break;
                        };
                        if event.id == hotkey_id && event.state == HotKeyState::Pressed {
                            thread_pressed.store(true, Ordering::Release);
                            repaint_context.request_repaint();
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start the hotkey listener: {error}"))?;

        Ok(Self {
            _manager: manager,
            pressed,
            stop_tx,
            listener: Some(listener),
        })
    }

    fn take_pressed(&self) -> bool {
        self.pressed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for GlobalShortcut {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

pub struct SearchApp {
    index: IndexService,
    query: String,
    results: Vec<SearchResult>,
    indexed: u64,
    discovered: u64,
    scanning: bool,
    skipped: u64,
    status: String,
    pending_search: Option<(Instant, u64, bool)>,
    searching: bool,
    search_cache_valid: bool,
    results_stale: bool,
    stale_refresh_due: Option<Instant>,
    window_visible: bool,
    next_query_id: u64,
    latest_query_id: u64,
    show_hidden: bool,
    sort: SortSpec,
    selected_path: Option<String>,
    global_shortcut: Option<GlobalShortcut>,
    shortcut_warning: Option<String>,
    watcher_warning: Option<String>,
    focus_search_requested: bool,
    refocus_window_next_frame: bool,
    quit_requested: bool,
    settings_open: bool,
    include_system_files: bool,
    settings_include_system_files: bool,
    native_icons: NativeIconCache,
}

impl SearchApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Result<Self> {
        configure_chinese_font(&creation_context.egui_ctx);
        let index = IndexService::start(creation_context.egui_ctx.clone())?;
        let (global_shortcut, shortcut_warning) =
            match GlobalShortcut::register(&creation_context.egui_ctx) {
                Ok(shortcut) => (Some(shortcut), None),
                Err(error) => (
                    None,
                    Some(format!(
                        "⌘Space is unavailable ({error}). It is commonly reserved by Spotlight; change that shortcut in macOS System Settings, then retry."
                    )),
                ),
            };

        Ok(Self {
            status: format!("Opening user-file index for {}", index.root().display()),
            index,
            query: String::new(),
            results: Vec::new(),
            indexed: 0,
            discovered: 0,
            scanning: false,
            skipped: 0,
            pending_search: None,
            searching: false,
            search_cache_valid: false,
            results_stale: false,
            stale_refresh_due: None,
            window_visible: true,
            next_query_id: 1,
            latest_query_id: 0,
            show_hidden: true,
            sort: SortSpec::default(),
            selected_path: None,
            global_shortcut,
            shortcut_warning,
            watcher_warning: None,
            focus_search_requested: true,
            refocus_window_next_frame: false,
            quit_requested: false,
            settings_open: false,
            include_system_files: false,
            settings_include_system_files: false,
            native_icons: NativeIconCache::new(&creation_context.egui_ctx),
        })
    }

    fn receive_events(&mut self, context: &egui::Context) {
        while let Ok(event) = self.index.events().try_recv() {
            match event {
                IndexEvent::PreparingSearchIndex => {
                    self.status =
                        "Preparing the filename search index (the first upgrade may take a while)..."
                            .to_owned();
                }
                IndexEvent::Resetting { entries } => {
                    self.scanning = true;
                    self.status = format!(
                        "Discarding an incomplete index ({entries} entries)..."
                    );
                }
                IndexEvent::Ready {
                    indexed,
                    root,
                    include_system_files,
                } => {
                    self.indexed = indexed;
                    self.include_system_files = include_system_files;
                    self.settings_include_system_files = include_system_files;
                    self.status = if indexed == 0 {
                        format!(
                            "Preparing the user-file index for {}",
                            root.display()
                        )
                    } else if include_system_files {
                        "Watching user and system files on the startup disk".to_owned()
                    } else {
                        format!("Watching user files under {}", root.display())
                    };
                }
                IndexEvent::ScanStarted { scope } => {
                    self.search_cache_valid = false;
                    self.scanning = true;
                    self.discovered = 0;
                    self.skipped = 0;
                    if scope == ScanScope::UserFiles {
                        self.clear_search_results();
                        self.native_icons.clear();
                        self.indexed = 0;
                        self.status = "Building the user-file index...".to_owned();
                    } else {
                        self.cancel_active_search_keep_results();
                        self.mark_results_stale(context);
                        self.status =
                            "Incrementally adding system and resource files...".to_owned();
                    }
                }
                IndexEvent::ScanProgress {
                    scope,
                    discovered,
                    indexed,
                    skipped,
                } => {
                    self.discovered = discovered;
                    self.indexed = indexed;
                    self.skipped = skipped;
                    self.status = match scope {
                        ScanScope::UserFiles => format!(
                            "Indexing user files... {discovered} paths scanned, {} stored",
                            self.indexed
                        ),
                        ScanScope::SystemFiles => format!(
                            "Adding system files... {discovered} paths scanned, {} total stored",
                            self.indexed
                        ),
                    };
                }
                IndexEvent::ScanFinished {
                    scope,
                    indexed,
                    discovered,
                    skipped,
                } => {
                    self.scanning = false;
                    self.indexed = indexed;
                    self.discovered = discovered;
                    self.skipped = skipped;
                    self.mark_results_stale(context);
                    self.status = match scope {
                        ScanScope::UserFiles => format!(
                            "User scan complete: {discovered} paths scanned, {indexed} stored"
                        ),
                        ScanScope::SystemFiles => format!(
                            "System increment complete: {discovered} paths scanned, {indexed} total stored"
                        ),
                    };
                }
                IndexEvent::ScopeChangeStarted {
                    include_system_files,
                } => {
                    self.search_cache_valid = false;
                    self.scanning = true;
                    self.status = if include_system_files {
                        "Preparing to add system and resource files...".to_owned()
                    } else {
                        "Removing system files from the index...".to_owned()
                    };
                }
                IndexEvent::ScopeChanged {
                    include_system_files,
                    indexed,
                } => {
                    self.scanning = false;
                    self.include_system_files = include_system_files;
                    self.settings_include_system_files = include_system_files;
                    self.indexed = indexed;
                    self.status = if include_system_files {
                        "System-file indexing enabled; incremental scan is starting..."
                            .to_owned()
                    } else {
                        "System files removed; watching user files only".to_owned()
                    };
                }
                IndexEvent::ScanFailed(message) => {
                    self.scanning = false;
                    self.searching = false;
                    self.pending_search = None;
                    self.status = message;
                }
                IndexEvent::SearchResults {
                    id,
                    results,
                    incremental,
                } => {
                    if id == self.latest_query_id {
                        self.searching = false;
                        if self.selected_path.as_ref().is_some_and(|selected| {
                            !results.iter().any(|result| &result.path == selected)
                        }) {
                            self.selected_path = None;
                        }
                        self.results = results;
                        self.results_stale = self.stale_refresh_due.is_some();
                        self.search_cache_valid = !self.results_stale;
                        if incremental && !self.results_stale {
                            self.schedule_search_after(
                                context,
                                false,
                                INCREMENTAL_VERIFY_DELAY,
                            );
                        }
                    }
                }
                IndexEvent::SearchFailed { id, message } => {
                    if id == 0 || id == self.latest_query_id {
                        self.searching = false;
                        self.search_cache_valid = false;
                        self.pending_search = None;
                        self.status = message;
                    }
                }
                IndexEvent::IndexChanged { indexed } => {
                    self.indexed = indexed;
                    self.search_cache_valid = false;
                    self.mark_results_stale(context);
                }
                IndexEvent::WatcherWarning(message) => {
                    self.watcher_warning = Some(message);
                }
                IndexEvent::Warning(message) => self.status = message,
            }
            context.request_repaint();
        }
    }

    fn schedule_search(&mut self, context: &egui::Context, allow_incremental: bool) {
        self.schedule_search_after(context, allow_incremental, SEARCH_DEBOUNCE);
    }

    fn schedule_search_after(
        &mut self,
        context: &egui::Context,
        allow_incremental: bool,
        delay: Duration,
    ) {
        let id = self.next_query_id;
        self.next_query_id += 1;
        self.latest_query_id = id;
        self.pending_search = Some((Instant::now() + delay, id, allow_incremental));
        self.searching = true;
        context.request_repaint_after(delay);
    }

    fn has_active_query(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn clear_search_results(&mut self) {
        // Move the accepted generation past any result that may still be in
        // flight, then remove the pending query and visible rows immediately.
        self.latest_query_id = self.next_query_id;
        self.next_query_id += 1;
        self.index.cancel_search();
        self.pending_search = None;
        self.searching = false;
        self.results_stale = false;
        self.stale_refresh_due = None;
        self.results.clear();
        self.selected_path = None;
    }

    fn cancel_active_search_keep_results(&mut self) {
        self.latest_query_id = self.next_query_id;
        self.next_query_id += 1;
        self.index.cancel_search();
        self.pending_search = None;
        self.searching = false;
        self.stale_refresh_due = None;
    }

    fn mark_results_stale(&mut self, context: &egui::Context) {
        if !self.has_active_query() {
            return;
        }
        self.results_stale = true;
        self.schedule_stale_refresh(context);
    }

    fn schedule_stale_refresh(&mut self, context: &egui::Context) {
        if !self.window_visible
            || !self.results_stale
            || !self.has_active_query()
            || self.stale_refresh_due.is_some()
        {
            return;
        }
        self.stale_refresh_due = Some(Instant::now() + INDEX_REFRESH_COALESCE);
        context.request_repaint_after(INDEX_REFRESH_COALESCE);
    }

    fn refresh_stale_results_if_due(&mut self, context: &egui::Context) {
        if !self.window_visible || !self.results_stale || !self.has_active_query() {
            self.stale_refresh_due = None;
            return;
        }
        let Some(deadline) = self.stale_refresh_due else {
            self.schedule_stale_refresh(context);
            return;
        };
        let now = Instant::now();
        if now < deadline {
            context.request_repaint_after(deadline - now);
        } else if !self.searching {
            self.stale_refresh_due = None;
            self.schedule_search(context, false);
        }
    }

    fn refresh_stale_results_now(&mut self, context: &egui::Context) {
        self.stale_refresh_due = None;
        self.schedule_search(context, false);
    }

    fn schedule_search_if_active(&mut self, context: &egui::Context) {
        if self.has_active_query() {
            self.schedule_search(context, false);
        }
    }

    fn dispatch_search_if_due(&mut self, context: &egui::Context) {
        let Some((deadline, id, allow_incremental)) = self.pending_search else {
            return;
        };
        let now = Instant::now();
        if now < deadline {
            context.request_repaint_after(deadline - now);
            return;
        }

        if !self.has_active_query() {
            self.clear_search_results();
            return;
        }

        let database_query = if self.query.trim() == "*" {
            String::new()
        } else {
            self.query.clone()
        };
        self.index
            .search(id, database_query, self.sort, allow_incremental);
        self.pending_search = None;
    }

    fn handle_global_shortcut(&mut self, context: &egui::Context) {
        if self.refocus_window_next_frame {
            context.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.refocus_window_next_frame = false;
        }

        let shortcut_pressed = self
            .global_shortcut
            .as_ref()
            .is_some_and(GlobalShortcut::take_pressed);
        if shortcut_pressed {
            self.window_visible = true;
            context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            context.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.focus_search_requested = true;
            self.refocus_window_next_frame = true;
            self.schedule_stale_refresh(context);
            context.request_repaint();
        }
    }

    fn handle_window_lifecycle(&mut self, context: &egui::Context) {
        let quit_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q);
        if context.input_mut(|input| input.consume_shortcut(&quit_shortcut)) {
            self.request_quit(context);
            return;
        }

        if !self.quit_requested && context.input(|input| input.viewport().close_requested()) {
            // Keep the indexer and global shortcut alive. The explicit Quit
            // action and Command+Q set quit_requested before closing.
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.window_visible = false;
            self.cancel_active_search_keep_results();
            self.results_stale |= self.has_active_query();
            self.focus_search_requested = true;
        }
    }

    fn request_quit(&mut self, context: &egui::Context) {
        self.quit_requested = true;
        context.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn retry_global_shortcut(&mut self, context: &egui::Context) {
        match GlobalShortcut::register(context) {
            Ok(shortcut) => {
                self.global_shortcut = Some(shortcut);
                self.shortcut_warning = None;
            }
            Err(error) => {
                self.shortcut_warning = Some(format!(
                    "⌘Space is unavailable ({error}). Change the Spotlight shortcut in macOS System Settings, then retry."
                ));
            }
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 330.0).max(180.0);
            let search_id = ui.make_persistent_id("main_search_input");
            let previous_query = self.query.clone();
            let search = ui.add_sized(
                [search_width, 32.0],
                egui::TextEdit::singleline(&mut self.query)
                    .id(search_id)
                    .hint_text(
                        "Name or path / 文件名或路径...   / = path   * = all   ext:rs",
                    ),
            );
            if self.focus_search_requested {
                search.request_focus();
                self.focus_search_requested = false;
            }
            if search.changed() {
                if self.has_active_query() {
                    let allow_incremental = self.search_cache_valid
                        && !self.results_stale
                        && SearchQuery::parse(&self.query)
                            .is_strict_refinement_of(&SearchQuery::parse(&previous_query));
                    self.clear_search_results();
                    self.schedule_search(ui.ctx(), allow_incremental);
                } else {
                    self.clear_search_results();
                    self.search_cache_valid = false;
                    ui.ctx().request_repaint();
                }
            }
            if ui.checkbox(&mut self.show_hidden, "Hidden").changed() {
                ui.ctx().request_repaint();
            }
            if ui.small_button("Settings").clicked() {
                self.settings_include_system_files = self.include_system_files;
                self.settings_open = true;
            }
            let rebuild = ui
                .add_enabled(!self.scanning, egui::Button::new("Rebuild index"))
                .on_disabled_hover_text("An index scan is already running");
            if rebuild.clicked() {
                self.search_cache_valid = false;
                self.status = "Preparing to rebuild the index...".to_owned();
                self.index.rebuild();
            }
            if ui
                .small_button("Quit")
                .on_hover_text("Quit the resident background process")
                .clicked()
            {
                self.request_quit(ui.ctx());
            }
        });
    }

    fn draw_settings_window(&mut self, context: &egui::Context) {
        if !self.settings_open {
            return;
        }

        let mut open = self.settings_open;
        let mut apply_scope = None;
        egui::Window::new("Settings / 设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .show(context, |ui| {
                ui.heading("Index scope / 索引范围");
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.settings_include_system_files,
                    "Index and search system files / 索引和搜索系统文件",
                );
                ui.label(
                    "Disabled by default: only files under your home directory are indexed.",
                );
                ui.label("默认关闭：只索引当前用户主目录中的文件。");
                ui.add_space(8.0);
                if self.settings_include_system_files {
                    ui.colored_label(
                        egui::Color32::from_rgb(190, 120, 20),
                        "Enabling this incrementally scans the rest of the startup disk. Full Disk Access is recommended.",
                    );
                    ui.label(
                        "启用后会增量扫描用户目录之外的启动磁盘内容，建议授予完全磁盘访问权限。",
                    );
                } else if self.include_system_files {
                    ui.label(
                        "Applying this removes system entries and returns file monitoring to your home directory.",
                    );
                    ui.label("应用后会移除系统条目，并只监控用户主目录。");
                }
                ui.add_space(10.0);
                let changed =
                    self.settings_include_system_files != self.include_system_files;
                if ui
                    .add_enabled(
                        changed && !self.scanning,
                        egui::Button::new("Apply / 应用"),
                    )
                    .on_disabled_hover_text(if self.scanning {
                        "Wait for the current scan to finish / 请等待当前扫描完成"
                    } else {
                        "No changes / 设置未变化"
                    })
                    .clicked()
                {
                    apply_scope = Some(self.settings_include_system_files);
                }
            });
        self.settings_open = open;

        if let Some(include_system_files) = apply_scope {
            self.search_cache_valid = false;
            self.clear_search_results();
            self.scanning = true;
            self.status = if include_system_files {
                "Preparing the incremental system-file scan...".to_owned()
            } else {
                "Preparing to remove system files from the index...".to_owned()
            };
            self.index
                .set_include_system_files(include_system_files);
        }
    }

    fn change_sort(&mut self, field: SortField, context: &egui::Context) {
        if self.sort.field == field {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort.field = field;
            self.sort.ascending = matches!(field, SortField::Name);
        }
        self.schedule_search_if_active(context);
    }

    fn sort_label(&self, field: SortField, label: &str) -> String {
        if self.sort.field != field {
            return label.to_owned();
        }
        let direction = if self.sort.ascending { "^" } else { "v" };
        format!("{label} {direction}")
    }

    fn draw_table_header(
        &mut self,
        ui: &mut egui::Ui,
        columns: TableColumns,
        grid_stroke: egui::Stroke,
    ) {
        let (row_rect, _) = ui.allocate_exact_size(
            egui::vec2(columns.total(), HEADER_HEIGHT),
            egui::Sense::hover(),
        );
        ui.painter()
            .rect_filled(row_rect, 0.0, ui.visuals().faint_bg_color);

        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("table_header")
                .max_rect(row_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        row_ui.spacing_mut().item_spacing.x = 0.0;
        row_ui.set_clip_rect(row_rect.intersect(ui.clip_rect()));

        let (name_response, _) = draw_text_cell(
            &mut row_ui,
            columns.name,
            HEADER_HEIGHT,
            egui::RichText::new(self.sort_label(SortField::Name, "Name")).strong(),
            egui::Sense::click(),
            "name",
        );
        let _ = draw_text_cell(
            &mut row_ui,
            columns.kind,
            HEADER_HEIGHT,
            egui::RichText::new("Kind").strong(),
            egui::Sense::hover(),
            "type",
        );
        let (size_response, _) = draw_text_cell(
            &mut row_ui,
            columns.size,
            HEADER_HEIGHT,
            egui::RichText::new(self.sort_label(SortField::Size, "Size")).strong(),
            egui::Sense::click(),
            "size",
        );
        let (modified_response, _) = draw_text_cell(
            &mut row_ui,
            columns.modified,
            HEADER_HEIGHT,
            egui::RichText::new(self.sort_label(SortField::Modified, "Modified")).strong(),
            egui::Sense::click(),
            "modified",
        );
        let _ = draw_text_cell(
            &mut row_ui,
            columns.path,
            HEADER_HEIGHT,
            egui::RichText::new("Path").strong(),
            egui::Sense::hover(),
            "path",
        );
        let _ = draw_text_cell(
            &mut row_ui,
            columns.actions,
            HEADER_HEIGHT,
            egui::RichText::new("Actions").strong(),
            egui::Sense::hover(),
            "actions",
        );
        paint_grid_lines(ui, row_rect, columns, grid_stroke);

        let context = ui.ctx().clone();
        if name_response.clicked() {
            self.change_sort(SortField::Name, &context);
        }
        if size_response.clicked() {
            self.change_sort(SortField::Size, &context);
        }
        if modified_response.clicked() {
            self.change_sort(SortField::Modified, &context);
        }
    }

    fn draw_result_row(
        &mut self,
        ui: &mut egui::Ui,
        result: &SearchResult,
        columns: TableColumns,
        grid_stroke: egui::Stroke,
        row_index: usize,
    ) {
        let selected = self.selected_path.as_deref() == Some(result.path.as_str());
        let icon_kind = FileIconKind::for_result(result);
        let (row_rect, row_hover) = ui.allocate_exact_size(
            egui::vec2(columns.total(), RESULT_ROW_HEIGHT),
            egui::Sense::hover(),
        );
        let icon_is_near_viewport = row_rect.intersects(
            ui.clip_rect().expand(RESULT_ROW_HEIGHT * 2.0),
        );
        let native_icon = self.native_icons.texture_for(
            &result.path,
            result.modified_at,
            icon_is_near_viewport,
        );
        let row_fill = if selected {
            ui.visuals().selection.bg_fill
        } else if row_hover.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else if row_index % 2 == 1 {
            ui.visuals().faint_bg_color
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(row_rect, 0.0, row_fill);

        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(&result.path)
                .max_rect(row_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        row_ui.spacing_mut().item_spacing.x = 0.0;
        row_ui.set_clip_rect(row_rect.intersect(ui.clip_rect()));

        let (name_response, _) = draw_name_cell(
            &mut row_ui,
            columns.name,
            RESULT_ROW_HEIGHT,
            result,
            icon_kind,
            native_icon,
        );
        let (kind_response, _) = draw_text_cell(
            &mut row_ui,
            columns.kind,
            RESULT_ROW_HEIGHT,
            icon_kind.label(),
            egui::Sense::click_and_drag(),
            "type",
        );
        let (size_response, _) = draw_text_cell(
            &mut row_ui,
            columns.size,
            RESULT_ROW_HEIGHT,
            format_size(result.size),
            egui::Sense::click_and_drag(),
            "size",
        );
        let (modified_response, _) = draw_text_cell(
            &mut row_ui,
            columns.modified,
            RESULT_ROW_HEIGHT,
            format_modified(result.modified_at),
            egui::Sense::click_and_drag(),
            "modified",
        );
        let (path_response, _) = draw_text_cell(
            &mut row_ui,
            columns.path,
            RESULT_ROW_HEIGHT,
            &result.path,
            egui::Sense::click_and_drag(),
            "path",
        );
        let (_, _, (finder_clicked, copy_clicked)) = draw_table_cell(
            &mut row_ui,
            columns.actions,
            RESULT_ROW_HEIGHT,
            egui::Sense::hover(),
            "actions",
            |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let finder_clicked = ui.small_button("Finder").clicked();
                let copy_clicked = ui.small_button("Copy").clicked();
                (finder_clicked, copy_clicked)
            },
        );

        paint_grid_lines(ui, row_rect, columns, grid_stroke);
        let response = name_response
            .union(kind_response)
            .union(size_response)
            .union(modified_response)
            .union(path_response)
            .on_hover_text("Drag to Finder to copy, or drag to Trash");

        if response.drag_started() {
            self.selected_path = Some(result.path.clone());
            if let Err(message) = platform::begin_file_drag(Path::new(&result.path)) {
                self.status = message;
            }
        } else if response.double_clicked() {
            self.selected_path = Some(result.path.clone());
            let _ = platform::reveal_in_finder(Path::new(&result.path));
        } else if response.clicked() {
            self.selected_path = if selected {
                None
            } else {
                Some(result.path.clone())
            };
        }
        if finder_clicked {
            self.selected_path = Some(result.path.clone());
            let _ = platform::reveal_in_finder(Path::new(&result.path));
        }
        if copy_clicked {
            self.selected_path = Some(result.path.clone());
            platform::copy_to_clipboard(ui.ctx(), Path::new(&result.path));
        }
        response.context_menu(|ui| {
            if ui.button("Open").clicked() {
                let _ = platform::open(Path::new(&result.path));
                ui.close();
            }
            if ui.button("Reveal in Finder").clicked() {
                let _ = platform::reveal_in_finder(Path::new(&result.path));
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                platform::copy_to_clipboard(ui.ctx(), Path::new(&result.path));
                ui.close();
            }
        });
    }

    fn draw_selected_details(&mut self, ui: &mut egui::Ui, result: &SearchResult) {
        let icon_kind = FileIconKind::for_result(result);
        let native_icon =
            self.native_icons
                .texture_for(&result.path, result.modified_at, true);
        let mut copy_clicked = false;
        let mut finder_clicked = false;
        let mut close_clicked = false;
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (icon_rect, _) = ui.allocate_exact_size(
                        egui::vec2(22.0, 22.0),
                        egui::Sense::hover(),
                    );
                    paint_result_icon(
                        ui.painter(),
                        icon_rect,
                        icon_kind,
                        ui.visuals().dark_mode,
                        native_icon,
                    );
                    ui.strong("Selected result — full text");
                    ui.weak(icon_kind.label());
                    finder_clicked = ui.small_button("Finder").clicked();
                    copy_clicked = ui.small_button("Copy full path").clicked();
                    close_clicked = ui.small_button("Hide details").clicked();
                });
                ui.strong("Name");
                ui.add(egui::Label::new(&result.name).wrap().selectable(true));
                ui.strong("Path");
                ui.add(egui::Label::new(&result.path).wrap().selectable(true));
            });

        if finder_clicked {
            let _ = platform::reveal_in_finder(Path::new(&result.path));
        }
        if copy_clicked {
            platform::copy_to_clipboard(ui.ctx(), Path::new(&result.path));
        }
        if close_clicked {
            self.selected_path = None;
        }
    }

    fn draw_results(&mut self, ui: &mut egui::Ui) {
        // An empty input deliberately has no result area. Entering exactly `*`
        // is translated to the database's empty query for the all-results view.
        if !self.has_active_query() {
            return;
        }

        let visible_results: Vec<_> = self
            .results
            .iter()
            .filter(|entry| self.show_hidden || !entry.hidden)
            .cloned()
            .collect();

        if let Some(selected) = self.selected_path.as_ref().and_then(|path| {
            visible_results
                .iter()
                .find(|result| &result.path == path)
                .cloned()
        }) {
            self.draw_selected_details(ui, &selected);
            ui.add_space(6.0);
        }

        if visible_results.is_empty() {
            let message = if !self.show_hidden && !self.results.is_empty() {
                "All matches are hidden. Enable Hidden to display them."
            } else if self.searching {
                "Searching..."
            } else if self.scanning {
                "No matches yet — indexing is still in progress."
            } else {
                "No matching files or folders."
            };
            ui.label(message);
            return;
        }

        let grid_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        egui::Frame::new()
            .stroke(grid_stroke)
            .inner_margin(0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let columns = TableColumns::for_available_width(ui.available_width());
                ui.set_min_width(columns.total());
                self.draw_table_header(ui, columns, grid_stroke);

                egui::ScrollArea::vertical()
                    .id_salt("results_scroll_area")
                    .auto_shrink([false, true])
                    .min_scrolled_height(0.0)
                    .max_height(ui.available_height().max(RESULT_ROW_HEIGHT))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.set_min_width(columns.total());
                        for (row_index, result) in visible_results.iter().enumerate() {
                            self.draw_result_row(
                                ui,
                                result,
                                columns,
                                grid_stroke,
                                row_index,
                            );
                        }
                    });
            });
    }
}

fn draw_table_cell<R>(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    sense: egui::Sense,
    id_salt: &'static str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> (egui::Response, egui::Rect, R) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let inner_rect = rect.shrink2(egui::vec2(CELL_HORIZONTAL_PADDING, 3.0));
    let mut cell_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .max_rect(inner_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    cell_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
    let inner = add_contents(&mut cell_ui);
    (response, rect, inner)
}

fn draw_text_cell(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    text: impl Into<egui::WidgetText>,
    sense: egui::Sense,
    id_salt: &'static str,
) -> (egui::Response, egui::Rect) {
    let (response, rect, ()) =
        draw_table_cell(ui, width, height, sense, id_salt, move |ui| {
            ui.add_sized(
                ui.available_size(),
                egui::Label::new(text)
                    .truncate()
                    .show_tooltip_when_elided(false)
                    .selectable(false),
            );
        });
    (response, rect)
}

fn draw_name_cell(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    result: &SearchResult,
    icon_kind: FileIconKind,
    native_icon: Option<egui::TextureId>,
) -> (egui::Response, egui::Rect) {
    let (response, rect, ()) = draw_table_cell(
        ui,
        width,
        height,
        egui::Sense::click_and_drag(),
        "name",
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (icon_rect, _) = ui.allocate_exact_size(
                egui::vec2(FILE_ICON_SIZE, FILE_ICON_SIZE),
                egui::Sense::hover(),
            );
            let dark_mode = ui.visuals().dark_mode;
            paint_result_icon(
                ui.painter(),
                icon_rect,
                icon_kind,
                dark_mode,
                native_icon,
            );
            ui.add_sized(
                ui.available_size(),
                egui::Label::new(&result.name)
                    .truncate()
                    .show_tooltip_when_elided(false)
                    .selectable(false),
            );
        },
    );
    (response, rect)
}

fn paint_result_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    kind: FileIconKind,
    dark_mode: bool,
    native_icon: Option<egui::TextureId>,
) {
    if let Some(texture_id) = native_icon {
        painter.image(
            texture_id,
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        paint_file_icon(painter, rect, kind, dark_mode);
    }
}

fn paint_file_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    kind: FileIconKind,
    dark_mode: bool,
) {
    let icon_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0));
    let outline = egui::Stroke::new(
        1.0,
        if dark_mode {
            egui::Color32::from_gray(175)
        } else {
            egui::Color32::from_gray(105)
        },
    );

    match kind {
        FileIconKind::Folder => paint_folder_icon(painter, icon_rect, outline),
        FileIconKind::Application => paint_application_icon(painter, icon_rect, outline),
        _ => paint_document_icon(painter, icon_rect, kind, outline, dark_mode),
    }
}

fn paint_folder_icon(painter: &egui::Painter, rect: egui::Rect, outline: egui::Stroke) {
    let tab = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.0, rect.top() + 2.0),
        egui::pos2(rect.left() + 8.5, rect.top() + 7.0),
    );
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.0, rect.top() + 5.0),
        egui::pos2(rect.right() - 1.0, rect.bottom() - 1.0),
    );
    painter.rect(
        tab,
        1.5,
        egui::Color32::from_rgb(111, 190, 247),
        outline,
        egui::StrokeKind::Inside,
    );
    painter.rect(
        body,
        2.0,
        egui::Color32::from_rgb(67, 158, 232),
        outline,
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [
            egui::pos2(body.left() + 2.0, body.top() + 2.0),
            egui::pos2(body.right() - 2.0, body.top() + 2.0),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)),
    );
}

fn paint_application_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    outline: egui::Stroke,
) {
    let app_rect = rect.shrink(1.0);
    painter.rect(
        app_rect,
        3.5,
        egui::Color32::from_rgb(67, 126, 226),
        outline,
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(
        egui::pos2(app_rect.center().x + 3.5, app_rect.center().y - 3.5),
        3.0,
        egui::Color32::from_white_alpha(45),
    );
    painter.text(
        app_rect.center(),
        egui::Align2::CENTER_CENTER,
        "A",
        egui::FontId::proportional(9.0),
        egui::Color32::WHITE,
    );
}

fn paint_document_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    kind: FileIconKind,
    outline: egui::Stroke,
    dark_mode: bool,
) {
    let left = rect.left() + 2.0;
    let top = rect.top() + 1.0;
    let right = rect.right() - 1.0;
    let bottom = rect.bottom() - 1.0;
    let fold = 4.0;
    let page_points = vec![
        egui::pos2(left, top),
        egui::pos2(right - fold, top),
        egui::pos2(right, top + fold),
        egui::pos2(right, bottom),
        egui::pos2(left, bottom),
    ];
    let page_fill = if dark_mode {
        egui::Color32::from_rgb(216, 222, 231)
    } else {
        egui::Color32::from_rgb(247, 249, 252)
    };
    painter.add(egui::Shape::convex_polygon(
        page_points,
        page_fill,
        outline,
    ));
    painter.line_segment(
        [
            egui::pos2(right - fold, top),
            egui::pos2(right - fold, top + fold),
        ],
        outline,
    );
    painter.line_segment(
        [
            egui::pos2(right - fold, top + fold),
            egui::pos2(right, top + fold),
        ],
        outline,
    );

    let (badge, accent) = kind.badge();
    let badge_rect = egui::Rect::from_min_max(
        egui::pos2(left + 0.8, bottom - 6.0),
        egui::pos2(right - 0.8, bottom - 1.0),
    );
    painter.rect_filled(badge_rect, 1.0, accent);
    painter.text(
        badge_rect.center(),
        egui::Align2::CENTER_CENTER,
        badge,
        egui::FontId::proportional(if badge.len() > 3 { 4.2 } else { 5.2 }),
        egui::Color32::WHITE,
    );
}

fn is_text_dotfile(name: &str) -> bool {
    matches!(
        name,
        ".gitignore"
            | ".gitattributes"
            | ".gitmodules"
            | ".editorconfig"
            | ".zshrc"
            | ".bashrc"
            | ".profile"
            | "makefile"
            | "dockerfile"
            | "readme"
            | "license"
    ) || name == ".env"
        || name.starts_with(".env.")
}

fn paint_grid_lines(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    columns: TableColumns,
    stroke: egui::Stroke,
) {
    for offset in columns.divider_offsets() {
        let x = row_rect.left() + offset;
        ui.painter().line_segment(
            [egui::pos2(x, row_rect.top()), egui::pos2(x, row_rect.bottom())],
            stroke,
        );
    }
    ui.painter().line_segment(
        [
            egui::pos2(row_rect.left(), row_rect.bottom()),
            egui::pos2(row_rect.right(), row_rect.bottom()),
        ],
        stroke,
    );
}

impl eframe::App for SearchApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_window_lifecycle(context);
        self.handle_global_shortcut(context);
        self.receive_events(context);
        self.native_icons.drain(context);
        self.dispatch_search_if_due(context);
        self.refresh_stale_results_if_due(context);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(6.0);
            self.draw_toolbar(ui);
            ui.add_space(6.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.scanning {
                    ui.spinner();
                }
                ui.label(&self.status);
                ui.separator();
                ui.label(format!("{} indexed", self.indexed));
                if self.scanning {
                    ui.separator();
                    ui.label(format!("{} scanned", self.discovered));
                }
                if self.has_active_query() {
                    let visible_count = self
                        .results
                        .iter()
                        .filter(|result| self.show_hidden || !result.hidden)
                        .count();
                    ui.separator();
                    ui.label(format!("{visible_count} results"));
                }
                if self.results_stale && self.has_active_query() {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(190, 120, 20),
                        "Results may be outdated",
                    );
                    if ui
                        .add_enabled(!self.searching, egui::Button::new("Refresh"))
                        .clicked()
                    {
                        self.refresh_stale_results_now(ui.ctx());
                    }
                }
                ui.separator();
                ui.label(if self.include_system_files {
                    "User + System"
                } else {
                    "User files only"
                });
                if self.skipped > 0 {
                    ui.separator();
                    ui.label(format!("{} inaccessible/skipped", self.skipped));
                }
                if self.global_shortcut.is_some() {
                    ui.separator();
                    ui.label("⌘Space ready");
                }
            });
            if let Some(warning) = self.shortcut_warning.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(190, 120, 20), warning);
                    if ui.small_button("Retry ⌘Space").clicked() {
                        self.retry_global_shortcut(ui.ctx());
                    }
                });
            }
            if let Some(warning) = self.watcher_warning.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(190, 120, 20), warning);
                    if ui.small_button("Dismiss").clicked() {
                        self.watcher_warning = None;
                    }
                });
            }
        });

        egui::CentralPanel::default().show(ui, |ui| self.draw_results(ui));
        self.draw_settings_window(ui.ctx());
    }
}

fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return String::new();
    };
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1_024.0 && unit < UNITS.len() - 1 {
        value /= 1_024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_modified(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|value| DateTime::from_timestamp(value, 0))
        .map(|value| value.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}
