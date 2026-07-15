use crate::model::{EntryKind, SearchResult};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Name,
    Path,
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub mode: SearchMode,
    pub terms: Vec<String>,
    pub excluded: Vec<String>,
    pub extension: Option<String>,
    pub path: Option<String>,
    pub path_is_anchored: bool,
    pub kind: Option<EntryKind>,
}

impl SearchQuery {
    pub fn parse(input: &str) -> Self {
        let input = input.trim();
        if let Some(path) = explicit_path_value(input) {
            return Self::for_path(path);
        }
        if looks_like_path(input) {
            return Self::for_path(input);
        }

        let mut query = Self::default();
        for token in input.split_whitespace() {
            let token_lower = token.to_lowercase();
            if let Some(value) = token_lower.strip_prefix("ext:") {
                if !value.is_empty() {
                    query.extension = Some(value.trim_start_matches('.').to_owned());
                }
            } else if token_lower == "type:file" {
                query.kind = Some(EntryKind::File);
            } else if token_lower == "type:dir" || token_lower == "type:folder" {
                query.kind = Some(EntryKind::Directory);
            } else if let Some(value) = token_lower.strip_prefix('!') {
                if !value.is_empty() {
                    query.excluded.push(value.to_owned());
                }
            } else if !token_lower.is_empty() {
                query.terms.push(token_lower);
            }
        }

        query
    }

    fn for_path(value: &str) -> Self {
        let path = normalize_path_query(value);
        Self {
            mode: SearchMode::Path,
            path_is_anchored: path.starts_with('/'),
            path: (!path.is_empty()).then_some(path),
            ..Self::default()
        }
    }

    pub fn matches(&self, result: &SearchResult) -> bool {
        let path = result.path.to_lowercase();
        let name = result.name.to_lowercase();

        match self.mode {
            SearchMode::Name => {
                if self.terms.iter().any(|term| !name.contains(term)) {
                    return false;
                }
                if self.excluded.iter().any(|term| name.contains(term)) {
                    return false;
                }
            }
            SearchMode::Path => {
                if self.path.as_ref().is_some_and(|term| {
                    if self.path_is_anchored {
                        !path.starts_with(term)
                    } else {
                        !path.contains(term)
                    }
                }) {
                    return false;
                }
            }
        }

        if self.kind.is_some_and(|kind| result.kind != kind) {
            return false;
        }
        if let Some(extension) = &self.extension {
            let actual = result
                .name
                .rsplit_once('.')
                .map(|(_, value)| value.to_lowercase());
            if actual.as_deref() != Some(extension.as_str()) {
                return false;
            }
        }

        true
    }

    pub fn supports_incremental_filtering(&self) -> bool {
        match self.mode {
            SearchMode::Name => {
                self.terms.len() == 1
                    && self.excluded.is_empty()
                    && self.extension.is_none()
                    && self.kind.is_none()
            }
            SearchMode::Path => self.path.is_some(),
        }
    }

    pub fn is_strict_refinement_of(&self, previous: &Self) -> bool {
        if !self.supports_incremental_filtering()
            || !previous.supports_incremental_filtering()
            || self.mode != previous.mode
        {
            return false;
        }

        match self.mode {
            SearchMode::Name => {
                let current = &self.terms[0];
                let previous = &previous.terms[0];
                current.len() > previous.len() && current.starts_with(previous)
            }
            SearchMode::Path => match (&self.path, &previous.path) {
                (Some(current), Some(previous_path)) => {
                    self.path_is_anchored == previous.path_is_anchored
                        && current.len() > previous_path.len()
                        && current.starts_with(previous_path)
                }
                _ => false,
            },
        }
    }

    pub fn rank(&self, result: &SearchResult) -> i64 {
        let name = result.name.to_lowercase();
        let path = result.path.to_lowercase();
        let mut score = if result.kind == EntryKind::Directory { 20 } else { 0 };

        match self.mode {
            SearchMode::Name => {
                for term in &self.terms {
                    if name == *term {
                        score += 1_000;
                    } else if name.starts_with(term) {
                        score += 700;
                    } else if name.contains(term) {
                        score += 500;
                    }
                }
            }
            SearchMode::Path => {
                if let Some(term) = &self.path {
                    if path == *term {
                        score += 1_000;
                    } else if path.starts_with(term) {
                        score += 700;
                    } else if path.contains(term) {
                        score += 500;
                    }
                }
            }
        }

        score - path.matches('/').count() as i64
    }
}

fn explicit_path_value(input: &str) -> Option<&str> {
    input
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("path:"))
        .map(|_| input[5..].trim())
}

fn looks_like_path(input: &str) -> bool {
    input.contains('/')
        || input
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
}

fn normalize_path_query(value: &str) -> String {
    let value = strip_wrapping_quotes(value.trim());
    let is_file_url = value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"));
    let value = if is_file_url { &value[7..] } else { value };
    let decoded = if is_file_url {
        percent_decode(value)
    } else {
        value.to_owned()
    };

    let expanded = if decoded.starts_with("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{}/{}", home.trim_end_matches('/'), &decoded[2..]),
            Err(_) => decoded,
        }
    } else {
        decoded
    };
    expanded.to_lowercase()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn strip_wrapping_quotes(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(path: &str, name: &str) -> SearchResult {
        SearchResult {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: EntryKind::File,
            size: None,
            modified_at: None,
            hidden: false,
            score: 0,
        }
    }

    #[test]
    fn parses_name_filters() {
        let query = SearchQuery::parse("report ext:pdf type:file !archive");
        assert_eq!(query.mode, SearchMode::Name);
        assert_eq!(query.terms, ["report"]);
        assert_eq!(query.extension.as_deref(), Some("pdf"));
        assert_eq!(query.kind, Some(EntryKind::File));
        assert_eq!(query.excluded, ["archive"]);
    }

    #[test]
    fn detects_automatic_and_explicit_paths() {
        let automatic = SearchQuery::parse("work/everything/src");
        assert_eq!(automatic.mode, SearchMode::Path);
        assert_eq!(automatic.path.as_deref(), Some("work/everything/src"));
        assert!(!automatic.path_is_anchored);

        let explicit = SearchQuery::parse("path:\"/Users/Test/Application Support\"");
        assert_eq!(explicit.mode, SearchMode::Path);
        assert_eq!(
            explicit.path.as_deref(),
            Some("/users/test/application support")
        );
        assert!(explicit.path_is_anchored);

        let file_url = SearchQuery::parse("file:///Users/Test/Application%20Support");
        assert_eq!(
            file_url.path.as_deref(),
            Some("/users/test/application support")
        );
    }

    #[test]
    fn name_mode_never_matches_only_a_parent_path() {
        let query = SearchQuery::parse("数据库");
        assert!(query.matches(&result("/tmp/other/数据库.txt", "数据库.txt")));
        assert!(!query.matches(&result("/tmp/数据库/main.rs", "main.rs")));
    }

    #[test]
    fn path_mode_only_matches_the_path() {
        let query = SearchQuery::parse("project/src");
        assert!(query.matches(&result("/tmp/project/src/main.rs", "main.rs")));
        assert!(!query.matches(&result("/tmp/other/project-src.txt", "project/src")));
    }

    #[test]
    fn detects_incremental_name_and_path_refinements() {
        assert!(SearchQuery::parse("ser")
            .is_strict_refinement_of(&SearchQuery::parse("se")));
        assert!(SearchQuery::parse("数据库")
            .is_strict_refinement_of(&SearchQuery::parse("数据")));
        assert!(SearchQuery::parse("work/everything")
            .is_strict_refinement_of(&SearchQuery::parse("work/every")));
        assert!(!SearchQuery::parse("se")
            .is_strict_refinement_of(&SearchQuery::parse("ser")));
        assert!(!SearchQuery::parse("ser ext:rs")
            .is_strict_refinement_of(&SearchQuery::parse("ser")));
    }
}
