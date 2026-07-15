use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub field: SortField,
    pub ascending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            field: SortField::Name,
            ascending: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    pub fn as_i64(self) -> i64 {
        match self {
            Self::File => 0,
            Self::Directory => 1,
            Self::Symlink => 2,
            Self::Other => 3,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::File,
            1 => Self::Directory,
            2 => Self::Symlink,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Directory => "Folder",
            Self::Symlink => "Link",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub parent: String,
    pub name: String,
    pub name_lower: String,
    pub extension: Option<String>,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified_at: Option<i64>,
    pub hidden: bool,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified_at: Option<i64>,
    pub hidden: bool,
    pub score: i64,
}

impl FileRecord {
    pub fn from_path(path: &Path) -> Option<Self> {
        let metadata = path.symlink_metadata().ok()?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        let name = path.file_name()?.to_string_lossy().into_owned();
        let path_text = path.to_string_lossy().into_owned();
        let parent = path
            .parent()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_lowercase());
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_secs() as i64);

        #[cfg(target_os = "macos")]
        let system_hidden = {
            use std::os::macos::fs::MetadataExt;
            const UF_HIDDEN: u32 = 0x0000_8000;
            metadata.st_flags() & UF_HIDDEN != 0
        };

        #[cfg(not(target_os = "macos"))]
        let system_hidden = false;

        let dot_hidden = path.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with('.')
        });

        Some(Self {
            name_lower: name.to_lowercase(),
            hidden: dot_hidden || system_hidden,
            path: path_text,
            parent,
            name,
            extension,
            kind,
            size: (kind == EntryKind::File).then_some(metadata.len()),
            modified_at,
        })
    }
}
