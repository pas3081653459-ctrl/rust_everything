# Rust Everything for macOS

A local, filename-oriented search application inspired by Windows Everything.
It is implemented in Rust and designed specifically for macOS.

## Current features

- Native desktop UI built with `egui`/`eframe`.
- Background indexing of all accessible files and folders on the macOS startup
  disk, rooted at `/`.
- Dotfiles and files with the macOS `UF_HIDDEN` flag are indexed.
- SQLite WAL database with a filename-only FTS5 trigram index. Full paths are
  not duplicated in the FTS index.
- Search-as-you-type with a 120 ms debounce.
- One search box automatically selects its mode: input containing `/`, a
  `file://` URI, or an initial `path:` searches paths only; all other input
  searches filenames only. `~/` is expanded to the current user's home path.
- Unicode search supports Chinese file and folder names. Three or more
  characters use the trigram index; shorter input uses a bounded fallback scan.
  A macOS Chinese system font is installed as an egui fallback so input and
  results render correctly.
- An empty search keeps the result area hidden; enter `*` for the all-results
  view (the current safety limit is 200 rows).
- File sizes and modification times are stored during the metadata scan.
- Clickable Name, Size, and Modified headers with ascending/descending sorting.
- A bordered, responsive result table keeps every column aligned. Long names and
  paths are truncated inside their cells; click a row to see the complete text.
- Native Finder icons are loaded through `NSWorkspace`; vector badges remain as
  the fallback while an icon is loading or cannot be read.
- The result table shrinks to the number of matches instead of drawing empty rows.
- FSEvents-backed change monitoring through `notify`.
- Double-click to reveal a result in Finder.
- Visible Finder and Copy buttons on every result row.
- Context menu actions to open, reveal in Finder, or copy a path.
- Global `Command+Space` brings a running application window to the front and
  focuses the search box.
- Closing the window hides it while indexing and filesystem monitoring continue
  in the background. Use `Command+Q` or the **Quit** button to end the process.
- Search filters:
  - `ext:rs`
  - `type:file`
  - `type:dir`
  - `!target`

Name filters apply in filename mode. Use `path:projects`, paste an absolute
path, or enter a path fragment such as `work/everything/src` to explicitly use
path mode. A quoted leading form such as
`path:"~/Library/Application Support"` preserves spaces as part of the path.

Folder sizes are intentionally left blank: recursively calculating every folder
size would repeat work and significantly increase indexing CPU and disk usage.

## macOS permissions

The application indexes all startup-disk paths that macOS allows the current
process to read. To include protected user data, grant the built application
Full Disk Access in:

`System Settings -> Privacy & Security -> Full Disk Access`

During development, grant Full Disk Access to the terminal or IDE that launches
the process. The application does not bypass filesystem permissions;
inaccessible directories are counted and skipped.

The startup-disk scope intentionally excludes virtual filesystems, APFS backing
paths that would duplicate files already visible under `/`, system maintenance
stores, and `/Volumes`. External disks, network shares, and Time Machine volumes
are therefore not indexed by this version.

## Command+Space shortcut

macOS normally assigns `Command+Space` to Spotlight, so the operating system may
reject this application's registration. When that happens, the application keeps
running and shows a warning with a **Retry ⌘Space** button in the status
area. Change or disable Spotlight's shortcut in:

`System Settings -> Keyboard -> Keyboard Shortcuts -> Spotlight`

Then return to the application and click retry. The red window close button only
hides the window; `Command+Space` restores and focuses it while the process is
running. Use `Command+Q`, the macOS application menu, or the in-app **Quit** button
to stop the resident process and remove the global shortcut.

Filesystem changes are maintained incrementally while the application is open.
The current MVP does not persist the native FSEvents event ID across application
shutdowns; use **Rebuild index** if files changed while the application was not
running and a stale result is observed.

## Index location

The persistent SQLite index is stored under the macOS application data
directory selected by the `directories` crate, in the `RustEverything`
directory as `everything-global.sqlite3`. It is separate from the former
project-only database, so switching scope does not delete that older index. The
data directory is explicitly excluded from scanning and filesystem events so
the index cannot trigger an update loop.

The database records whether a full scan completed. If the application exits
mid-scan, the next launch discards the partial data and starts again. Scan
progress is shown in the bottom status bar, and the **Rebuild index** button is
disabled while a scan is already active.

Existing databases that indexed both names and full paths are migrated once at
startup. The FTS index is rebuilt from the existing `entries` table, so this
does not rescan the filesystem. The UI reports this preparation step. SQLite
can reuse the freed pages, but the database file does not physically shrink
without a separate `VACUUM`; the application intentionally does not run that
expensive operation automatically.

## CPU behavior

The initial scan runs on one background scanner thread and writes records in
1,000-entry transactions with persistence backpressure. Once indexing is
complete, there is no periodic full scan. Filesystem events are debounced for
400 ms (with a 2-second maximum delay) and applied incrementally. If the native
watcher reports dropped events or its pending-path queue becomes excessive, the
application schedules a recovery scan.

Index writes and searches use separate SQLite connections in WAL mode. Search
requests use a one-item latest-value queue; new input replaces a waiting query
and interrupts a query already running on the read-only search connection. This
keeps filename lookup responsive while the global index is still being built.
Absolute path prefixes use the `path_lower` B-tree index. Unanchored path
fragments use a bounded `LIKE` scan and can therefore be slower than filename
search on a multi-million-entry index.

The UI repaints only for input, received events, or a pending debounced query.
The global-shortcut listener blocks on its event channel and requests a repaint
only when the registered shortcut is pressed; it does not add a polling timer.

## Development

The project is intended for macOS and uses Rust edition 2024. Standard Cargo
development commands can be used to format, check, test, and run it when desired.

After source changes, rebuild with Cargo before launching the application so the
binary contains the latest indexing and UI behavior.
