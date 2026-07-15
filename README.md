# Rust Everything for macOS

一个使用 Rust 编写的 macOS 文件搜索工具，交互方式类似 Windows
Everything。程序会在后台建立本机启动磁盘索引，并根据输入实时查找文件和文件夹，
包括隐藏项目。

An Everything-inspired file search application for macOS, written in Rust. It
builds a background index of the startup disk and searches files, folders, and
hidden entries as you type.

---

## 中文说明

### 功能概览

- 全局索引 macOS 启动磁盘中当前用户有权限访问的文件和文件夹。
- 支持隐藏文件、中文文件名和中文路径。
- 普通输入只匹配文件名，路径输入只匹配完整路径。
- 支持按名称、大小和修改时间排序。
- 使用 Finder 原生文件图标。
- 可以打开文件、在 Finder 中定位文件或复制完整路径。
- 程序关闭窗口后继续在后台运行，可用 `Command+Space` 再次唤出。
- 文件系统发生变化时增量更新索引，不需要周期性重新扫描全盘。

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

推荐使用 release 模式。全盘索引和搜索时，release 构建的性能明显更好：

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

首次启动后，程序会从 `/` 开始建立全局索引。底部状态栏会显示：

- 已扫描路径数量。
- 已写入索引数量。
- 无法访问或跳过的数量。
- 当前是否仍在建立索引。

索引过程中仍然可以搜索，但结果只包含当前已经写入数据库的项目。全盘文件较多时，
首次索引可能需要较长时间。

如果打开的是旧版本数据库，程序会先把原来同时包含名称和路径的 FTS 索引迁移为
仅包含文件名的索引。迁移直接使用已有数据库记录，不会重新扫描文件系统，界面会显示
`Preparing the filename search index...`。

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
- `Rebuild index`：清空数据库并重新扫描启动磁盘。

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

全局 SQLite 索引通常位于：

```text
~/Library/Application Support/dev.RustEverything.RustEverything/everything-global.sqlite3
```

数据库使用 WAL 模式，因此运行时可能同时看到：

```text
everything-global.sqlite3
everything-global.sqlite3-wal
everything-global.sqlite3-shm
```

旧 FTS 迁移完成后，SQLite 会复用释放的页面，但数据库文件不会自动缩小。程序不会
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

- Indexes all accessible files and folders on the macOS startup disk.
- Includes dotfiles and entries marked with the macOS hidden flag.
- Supports Unicode and Chinese filenames and paths.
- Searches filenames by default and switches to path-only matching for path
  input.
- Sorts results by name, size, or modification time.
- Displays native Finder icons through `NSWorkspace`.
- Opens files, reveals results in Finder, and copies complete paths.
- Keeps running after the window is closed and can be recalled with
  `Command+Space`.
- Applies filesystem changes incrementally instead of periodically rescanning
  the complete disk.

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
global scan and search:

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

On first launch, the application starts a global scan rooted at `/`. The bottom
status bar reports:

- Paths discovered.
- Entries stored in the index.
- Inaccessible or skipped paths.
- Whether indexing is still active.

Search remains available during indexing, but results only include entries that
have already been stored. The first scan can take a while on systems with many
files.

When an older database is detected, the application migrates the former
name-and-path FTS index to a filename-only index. It rebuilds FTS from existing
database rows without rescanning the filesystem and displays
`Preparing the filename search index...` while doing so.

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
- Use `Rebuild index` to clear the database and scan the startup disk again.

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

After the old FTS index is migrated, SQLite can reuse freed pages but does not
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
