# Rust Everything for macOS

一个使用 Rust 编写的 macOS 文件搜索工具，交互方式类似 Windows
Everything。程序默认在后台索引当前用户主目录，并根据输入实时查找文件和文件夹，
包括隐藏项目；用户可以在设置中增量加入系统和其他启动磁盘资源。

An Everything-inspired file search application for macOS, written in Rust. It
indexes the current user's home directory by default and searches files,
folders, and hidden entries as you type. System and other startup-disk resources
can be added incrementally from Settings.

---

## 中文说明

### 功能概览

- 默认索引当前用户主目录中的文件和文件夹，避免无意义的系统资源占用。
- 可以在 `Settings / 设置` 中增量加入用户目录之外的系统和资源文件。
- 支持隐藏文件、中文文件名和中文路径。
- 普通输入只匹配文件名，路径输入只匹配完整路径。
- 连续追加字符时使用增量候选缓存，例如 `s → se → ser` 的中间输入只过滤上一轮
  候选；停止输入 500 毫秒后校验最终查询。删除字符、改变筛选条件、排序或索引更新后
  会自动回退到 SQLite 查询。
- 支持按名称、大小和修改时间排序。
- 使用 Finder 原生文件图标。
- 可以打开文件、在 Finder 中定位文件或复制完整路径。
- 程序关闭窗口后继续在后台运行，可用 `Command+Space` 再次唤出。
- 使用 macOS 原生 FSEvents 和持久化事件游标增量更新索引，不需要周期性重新扫描全盘。
- 文件名在 SQLite 中去重存储；文件记录通过 `name_id` 和 `parent_id` 关联，减少重复字符串、FTS 体积和 WAL 写入。

### 运行环境

本项目仅面向 macOS。运行前需要：

- Rust stable 工具链。
- Xcode Command Line Tools。
- 建议为终端或 IDE 授予“完全磁盘访问权限”。

安装 Xcode Command Line Tools：

```bash
xcode-select --install
```

如果尚未安装 Rust，可以通过 [rustup](https://rustup.rs/) 安装。

### 编译和运行

进入项目目录：

```bash
cd /path/to/everything
```

开发模式运行：

```bash
cargo run
```

推荐使用 release 模式。大量文件索引和搜索时，release 构建的性能明显更好：

```bash
cargo run --release
```

只生成可执行文件：

```bash
cargo build --release
```

生成的程序位于：

```text
target/release/rust-everything
```

### 首次启动

首次启动后，程序只扫描当前用户主目录。底部状态栏会显示：

- 已扫描路径数量。
- 已写入索引数量。
- 无法访问或跳过的数量。
- 当前是否仍在建立索引。

索引过程中仍然可以搜索，但结果只包含当前已经写入数据库的项目。用户文件较多时，
首次索引可能需要较长时间。

如果打开的是旧版本数据库，程序会升级到名称去重的索引结构。由于旧表为每个文件
重复保存名称和 lowercase 路径，升级会清空旧索引并自动重新扫描，但会保留“是否索引
系统文件”的用户设置。底部状态栏会持续显示扫描进度。

### 索引范围设置

点击工具栏中的 `Settings`，可以选择：

- 默认关闭“索引和搜索系统文件”：只扫描、监控和搜索当前用户主目录。
- 启用系统文件：保留现有用户索引，并增量扫描 `/` 中用户主目录之外的内容。
- 再次关闭：停止系统范围监控，移除系统索引条目，并恢复为只搜索用户文件。

系统扫描不会重复扫描已经完成的用户主目录。外部磁盘、网络卷、Time Machine、虚拟
文件系统和 APFS 重复路径仍然保持排除。

### 完全磁盘访问权限

macOS 会阻止普通应用读取部分用户数据和系统目录。如果希望获得尽可能完整的结果，
请打开：

```text
系统设置 → 隐私与安全性 → 完全磁盘访问权限
```

开发时，请为启动程序的 Terminal、iTerm、VS Code 或其他 IDE 授权。打包为 `.app`
后，请直接为该应用授权。

程序不会绕过 macOS 权限。状态栏中的 `inaccessible/skipped` 表示路径无权访问、
已经消失，或属于程序主动排除的虚拟文件系统、APFS 重复路径、外部卷和系统维护目录。

### 如何搜索

#### 1. 文件名搜索

不包含 `/` 的普通输入只匹配文件名，不会因为父目录名称相同而返回无关文件。

```text
database
报告.pdf
数据库设计
测试
```

三个及以上字符使用 FTS5 trigram 索引。一到两个字符（包括一到两个中文字）使用
有界 `LIKE` 查询，因此短词搜索可能比长词稍慢。

#### 2. 路径搜索

以下输入会自动切换为路径模式，并且只匹配路径：

```text
/Users/Zhuanz/Documents
~/Library/Application Support
work/everything/src
file:///Users/Zhuanz/Documents/report.pdf
```

也可以使用 `path:` 强制进行路径搜索：

```text
path:Downloads
path:"~/Library/Application Support"
```

绝对路径使用前缀查询，速度较快。类似 `work/everything/src` 的非绝对路径片段需要
进行有界扫描，在几百万条索引上可能比文件名搜索慢。

#### 3. 显示全部结果

搜索框为空时不显示结果。输入下面的内容可以显示当前索引中的全部项目，最多显示
200 条：

```text
*
```

#### 4. 文件名过滤器

过滤器用于文件名模式：

```text
report ext:pdf
main type:file
project type:dir
report !archive
```

支持的过滤器：

- `ext:rs`：只显示指定扩展名。
- `type:file`：只显示文件。
- `type:dir` 或 `type:folder`：只显示文件夹。
- `!target`：排除名称中包含指定文字的项目。

### 结果操作

- 单击结果：在表格上方显示完整名称和完整路径。
- 双击结果：在 Finder 中定位该项目。
- `Finder`：在 Finder 中定位该项目。
- `Copy`：复制完整文件路径。
- 右键结果：可以打开、在 Finder 中定位或复制路径。
- 点击 `Name`、`Size`、`Modified` 表头：切换排序字段和升降序。
- `Hidden`：控制是否显示隐藏项目。
- 后台文件变化后，已有结果会标记为 `Results may be outdated`。窗口可见时，多次变化
  最多每 5 秒合并自动刷新一次，也可以点击 `Refresh` 立即刷新；窗口隐藏时不会自动搜索。
- `Rebuild index`：按当前设置重建索引；始终先扫描用户目录，启用系统文件时再增量
  扫描其余启动磁盘内容。

文件夹大小留空是正常行为。递归计算每个文件夹大小会重复遍历大量文件，显著增加
CPU 和磁盘负载。

### 后台运行和 Command+Space

点击窗口左上角红色关闭按钮只会隐藏窗口，索引线程和文件监控仍然运行。按下
`Command+Space` 可以重新显示窗口并聚焦搜索框。

macOS 默认把 `Command+Space` 分配给 Spotlight。如果程序提示快捷键不可用，请前往：

```text
系统设置 → 键盘 → 键盘快捷键 → Spotlight
```

修改或关闭 Spotlight 快捷键，然后点击程序状态区域中的 `Retry ⌘Space`。

要彻底退出后台进程，请使用以下任一方式：

- 点击程序中的 `Quit`。
- 按下 `Command+Q`。
- 使用 macOS 应用菜单中的退出命令。

### 索引数据库位置

SQLite 索引通常位于：

```text
~/Library/Application Support/dev.RustEverything.RustEverything/everything-global.sqlite3
```

数据库使用 WAL 模式，因此运行时可能同时看到：

```text
everything-global.sqlite3
everything-global.sqlite3-wal
everything-global.sqlite3-shm
```

旧 schema 升级后，SQLite 会复用释放的页面，但数据库文件不会自动缩小。程序不会
在启动时自动执行耗时且需要额外磁盘空间的 `VACUUM`。

### 打包为 macOS 应用

可以使用 `cargo-bundle` 生成 `.app`：

```bash
cargo install cargo-bundle --locked
cargo bundle --release
```

默认输出位置：

```text
target/release/bundle/osx/rust-everything.app
```

对外分发时，还需要使用 Apple Developer ID 对应用签名，并通过 `notarytool` 完成
Apple 公证。

---

## English

### Features

- Indexes the current user's home directory by default to avoid unnecessary
  system-wide background load.
- Can incrementally add system and resource files outside the home directory
  from `Settings`.
- Includes dotfiles and entries marked with the macOS hidden flag.
- Supports Unicode and Chinese filenames and paths.
- Searches filenames by default and switches to path-only matching for path
  input.
- Uses an incremental candidate cache while characters are appended, so the
  intermediate steps in `s → se → ser` filter previous candidates. The final
  query is verified after 500 ms of idle time. Deleting characters, changing
  filters or sorting, or updating the index falls back to SQLite.
- Sorts results by name, size, or modification time.
- Displays native Finder icons through `NSWorkspace`.
- Opens files, reveals results in Finder, and copies complete paths.
- Keeps running after the window is closed and can be recalled with
  `Command+Space`.
- Uses native macOS FSEvents with a persisted event cursor to apply filesystem
  changes incrementally instead of periodically rescanning the complete disk.
- Deduplicates filenames in SQLite and links file rows through `name_id` and
  `parent_id`, reducing repeated strings, FTS size, and WAL writes.

### Requirements

This project targets macOS only. You need:

- The stable Rust toolchain.
- Xcode Command Line Tools.
- Full Disk Access for the terminal or IDE is recommended.

Install Xcode Command Line Tools with:

```bash
xcode-select --install
```

Install Rust from [rustup](https://rustup.rs/) if it is not already available.

### Build and run

Enter the project directory:

```bash
cd /path/to/everything
```

Run a development build:

```bash
cargo run
```

Release mode is recommended because it performs significantly better during a
large scan and search:

```bash
cargo run --release
```

Build without launching the application:

```bash
cargo build --release
```

The executable is generated at:

```text
target/release/rust-everything
```

### First launch

On first launch, the application scans the current user's home directory only.
The bottom status bar reports:

- Paths discovered.
- Entries stored in the index.
- Inaccessible or skipped paths.
- Whether indexing is still active.

Search remains available during indexing, but results only include entries that
have already been stored. The first scan can take a while on systems with many
files.

When an older database is detected, the application upgrades to the normalized
filename schema. Because the old table duplicated names and lowercase paths for
every entry, the old index is cleared and scanned again. The system-file scope
preference is preserved, and the status bar reports rebuild progress.

### Index scope settings

Open `Settings` from the toolbar to choose the index scope:

- “Index and search system files” is disabled by default. Only the current
  user's home directory is scanned, watched, and searched.
- Enabling it preserves the user index and incrementally scans content outside
  the home directory under `/`.
- Disabling it again stops system-wide monitoring, removes system entries, and
  returns search to user files only.

The system scan does not repeat the completed home-directory scan. External
drives, network volumes, Time Machine, virtual filesystems, and duplicate APFS
paths remain excluded.

### Full Disk Access

macOS prevents ordinary applications from reading some user data and protected
system locations. For the most complete index, open:

```text
System Settings → Privacy & Security → Full Disk Access
```

During development, grant access to the Terminal, iTerm, VS Code, or other IDE
that launches the process. After packaging, grant access directly to the `.app`.

The application never bypasses macOS permissions. `inaccessible/skipped` can
mean that a path was denied, disappeared during scanning, or belongs to an
intentionally excluded virtual filesystem, duplicate APFS path, external
volume, or system maintenance directory.

### Searching

#### 1. Filename search

Ordinary input without `/` matches filenames only. A term found only in a
parent directory does not return unrelated files.

```text
database
report.pdf
数据库设计
测试
```

Terms of three or more characters use the FTS5 trigram index. One- and
two-character terms, including Chinese input, use a bounded `LIKE` query and
may be slower than longer terms.

#### 2. Path search

The following forms automatically select path mode and match paths only:

```text
/Users/Zhuanz/Documents
~/Library/Application Support
work/everything/src
file:///Users/Zhuanz/Documents/report.pdf
```

Use `path:` to explicitly request path mode:

```text
path:Downloads
path:"~/Library/Application Support"
```

Absolute paths use an indexed prefix query. Unanchored fragments such as
`work/everything/src` require a bounded scan and can be slower on a
multi-million-entry database.

#### 3. Show all indexed entries

An empty search box hides the result area. Enter `*` to show all indexed entries
up to the current 200-row result limit:

```text
*
```

#### 4. Filename filters

Filters are available in filename mode:

```text
report ext:pdf
main type:file
project type:dir
report !archive
```

Supported filters:

- `ext:rs`: require an extension.
- `type:file`: show files only.
- `type:dir` or `type:folder`: show folders only.
- `!target`: exclude entries whose filename contains the term.

### Result actions

- Click a row to display its complete filename and path above the table.
- Double-click a row to reveal it in Finder.
- Use `Finder` to reveal the result in Finder.
- Use `Copy` to copy the complete path.
- Right-click a row to open it, reveal it, or copy its path.
- Click the `Name`, `Size`, or `Modified` header to change sorting and direction.
- Use `Hidden` to show or hide hidden entries.
- Background file changes mark existing results as `Results may be outdated`.
  While the window is visible, changes are coalesced into at most one automatic
  refresh every five seconds; `Refresh` updates immediately. No query is run
  automatically while the window is hidden.
- Use `Rebuild index` to rebuild the configured scope. The home directory is
  scanned first; when system files are enabled, the rest of the startup disk is
  added incrementally.

Folder sizes are intentionally blank. Recursively calculating every directory
size would repeatedly traverse files and significantly increase CPU and disk
usage.

### Background mode and Command+Space

The red window close button hides the window while indexing and filesystem
monitoring continue in the background. Press `Command+Space` to show the window
and focus the search box again.

macOS assigns `Command+Space` to Spotlight by default. If registration fails,
open:

```text
System Settings → Keyboard → Keyboard Shortcuts → Spotlight
```

Change or disable the Spotlight shortcut, then click `Retry ⌘Space` in the
application status area.

To terminate the resident process completely:

- Click `Quit` in the application.
- Press `Command+Q`.
- Use Quit from the macOS application menu.

### Index database

The SQLite index is normally stored at:

```text
~/Library/Application Support/dev.RustEverything.RustEverything/everything-global.sqlite3
```

The database uses WAL mode, so these files can coexist while the application is
running:

```text
everything-global.sqlite3
everything-global.sqlite3-wal
everything-global.sqlite3-shm
```

After the old schema is upgraded, SQLite can reuse freed pages but does not
automatically shrink the database file. The application intentionally does not
run the expensive and disk-space-intensive `VACUUM` operation during startup.

### Package as a macOS application

Use `cargo-bundle` to create an `.app` bundle:

```bash
cargo install cargo-bundle --locked
cargo bundle --release
```

The default output is:

```text
target/release/bundle/osx/rust-everything.app
```

Public distribution additionally requires Developer ID signing and Apple
notarization with `notarytool`.
