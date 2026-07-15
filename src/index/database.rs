use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{
    Connection, InterruptHandle, OpenFlags, OptionalExtension, params, params_from_iter,
};

use crate::model::{EntryKind, FileRecord, SearchResult, SortField, SortSpec};
use crate::search::{SearchMode, SearchQuery};

const FTS_SCHEMA_VERSION: &str = "2";

pub(super) struct IndexDatabase {
    connection: Connection,
}

impl IndexDatabase {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("unable to open {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA case_sensitive_like = ON;
             CREATE TABLE IF NOT EXISTS entries (
                 id          INTEGER PRIMARY KEY,
                 path        TEXT NOT NULL UNIQUE,
                 parent      TEXT NOT NULL,
                 name        TEXT NOT NULL,
                 name_lower  TEXT NOT NULL,
                 path_lower  TEXT NOT NULL,
                 extension   TEXT,
                 kind        INTEGER NOT NULL,
                 size        INTEGER,
                 modified_at INTEGER,
                 device_id   INTEGER NOT NULL,
                 inode       INTEGER NOT NULL,
                 hidden      INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent);
             CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name_lower);
             CREATE INDEX IF NOT EXISTS idx_entries_path_lower ON entries(path_lower);
             CREATE INDEX IF NOT EXISTS idx_entries_extension ON entries(extension);
             CREATE INDEX IF NOT EXISTS idx_entries_size ON entries(size);
             CREATE INDEX IF NOT EXISTS idx_entries_modified ON entries(modified_at);
             CREATE INDEX IF NOT EXISTS idx_entries_identity ON entries(device_id, inode);
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let mut database = Self { connection };
        database.ensure_name_only_fts()?;
        Ok(database)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("unable to open read-only index {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA temp_store = MEMORY;
             PRAGMA busy_timeout = 1000;
             PRAGMA case_sensitive_like = ON;
             PRAGMA query_only = ON;",
        )?;
        Ok(Self { connection })
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.connection.get_interrupt_handle()
    }

    fn ensure_name_only_fts(&mut self) -> Result<()> {
        let current_schema: Option<String> = self
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'entries_fts'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let recreate_fts = current_schema
            .as_deref()
            .map(|schema| schema.contains("path_lower"))
            .unwrap_or(true);

        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS entries_ai;
             DROP TRIGGER IF EXISTS entries_ad;
             DROP TRIGGER IF EXISTS entries_au;",
        )?;
        if recreate_fts {
            transaction.execute_batch(
                "DROP TABLE IF EXISTS entries_fts;
                 CREATE VIRTUAL TABLE entries_fts USING fts5(
                     name_lower,
                     content='entries',
                     content_rowid='id',
                     tokenize='trigram'
                 );
                 INSERT INTO entries_fts(entries_fts) VALUES ('rebuild');",
            )?;
        }
        transaction.execute_batch(
            "CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
                 INSERT INTO entries_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER entries_ad AFTER DELETE ON entries BEGIN
                 INSERT INTO entries_fts(entries_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER entries_au AFTER UPDATE ON entries BEGIN
                 INSERT INTO entries_fts(entries_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO entries_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;",
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES ('fts_schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [FTS_SCHEMA_VERSION],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn ensure_index_root(&mut self, root: &Path) -> Result<()> {
        let root = root.to_string_lossy();
        let stored_root: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'index_root'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        if stored_root.as_deref() != Some(root.as_ref()) {
            self.clear()?;
            self.connection.execute(
                "INSERT INTO settings(key, value) VALUES ('index_root', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [root.as_ref()],
            )?;
            self.set_scan_complete(false)?;
        }
        Ok(())
    }

    pub fn is_scan_complete(&self) -> Result<bool> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'scan_complete'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.as_deref() == Some("1"))
    }

    pub fn set_scan_complete(&self, complete: bool) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings(key, value) VALUES ('scan_complete', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [if complete { "1" } else { "0" }],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<u64> {
        let count = self
            .connection
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn clear(&mut self) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS entries_ai;
             DROP TRIGGER IF EXISTS entries_ad;
             DROP TRIGGER IF EXISTS entries_au;
             INSERT INTO entries_fts(entries_fts) VALUES ('delete-all');
             DELETE FROM entries;
             CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
                 INSERT INTO entries_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER entries_ad AFTER DELETE ON entries BEGIN
                 INSERT INTO entries_fts(entries_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER entries_au AFTER UPDATE ON entries BEGIN
                 INSERT INTO entries_fts(entries_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO entries_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn upsert_batch(&mut self, records: &[FileRecord]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO entries (
                    path, parent, name, name_lower, path_lower, extension, kind,
                    size, modified_at, device_id, inode, hidden
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(path) DO UPDATE SET
                    parent = excluded.parent,
                    name = excluded.name,
                    name_lower = excluded.name_lower,
                    path_lower = excluded.path_lower,
                    extension = excluded.extension,
                    kind = excluded.kind,
                    size = excluded.size,
                    modified_at = excluded.modified_at,
                    device_id = excluded.device_id,
                    inode = excluded.inode,
                    hidden = excluded.hidden",
            )?;

            for record in records {
                statement.execute(params![
                    record.path,
                    record.parent,
                    record.name,
                    record.name_lower,
                    record.path_lower,
                    record.extension,
                    record.kind.as_i64(),
                    record.size,
                    record.modified_at,
                    record.device_id,
                    record.inode,
                    record.hidden,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_path_tree(&mut self, path: &Path) -> Result<()> {
        let path = path.to_string_lossy();
        let prefix = format!("{}/%", escape_like(&path));
        self.connection.execute(
            "DELETE FROM entries WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![path.as_ref(), prefix],
        )?;
        Ok(())
    }

    pub fn search(
        &self,
        input: &str,
        limit: usize,
        sort: SortSpec,
    ) -> Result<Vec<SearchResult>> {
        let query = SearchQuery::parse(input);
        if query.mode == SearchMode::Path && query.path.is_none() {
            return Ok(Vec::new());
        }
        // FTS5 trigram queries need at least three Unicode characters. Short
        // terms (including one- or two-character Chinese input) use LIKE. Do
        // not ask SQLite to sort an entire multi-million-row LIKE scan before
        // returning anything; collect a bounded candidate set and sort it in
        // Rust below instead.
        let has_linear_filter = query.mode == SearchMode::Path
            || query
                .terms
                .iter()
                .any(|term| term.chars().count() < 3)
            || !query.excluded.is_empty();
        let candidate_limit = if has_linear_filter {
            (limit * 10).max(2_000)
        } else {
            (limit * 50).max(10_000)
        } as i64;
        let fts_terms: Vec<&str> = if query.mode == SearchMode::Name {
            query
                .terms
                .iter()
                .map(String::as_str)
                .filter(|term| term.chars().count() >= 3)
                .collect()
        } else {
            Vec::new()
        };
        let use_fts = !fts_terms.is_empty();

        let mut sql = if use_fts {
            "SELECT e.path, e.name, e.kind, e.size, e.modified_at, e.hidden
             FROM entries_fts
             JOIN entries e ON e.id = entries_fts.rowid
             WHERE entries_fts MATCH ?"
                .to_owned()
        } else {
            "SELECT path, name, kind, size, modified_at, hidden
             FROM entries
             WHERE 1 = 1"
                .to_owned()
        };

        let mut parameters = Vec::<Value>::new();
        if use_fts {
            let expression = fts_terms
                .iter()
                .map(|term| quote_fts(term))
                .collect::<Vec<_>>()
                .join(" AND ");
            parameters.push(Value::Text(expression));
        }

        let column_prefix = if use_fts { "e." } else { "" };
        for term in query
            .terms
            .iter()
            .filter(|term| term.chars().count() < 3)
        {
            sql.push_str(&format!(
                " AND {column_prefix}name_lower LIKE ? ESCAPE '\\'"
            ));
            parameters.push(Value::Text(format!("%{}%", escape_like(term))));
        }
        if let Some(path) = &query.path {
            if query.path_is_anchored {
                sql.push_str(&format!(
                    " AND {column_prefix}path_lower LIKE ? ESCAPE '\\'"
                ));
                parameters.push(Value::Text(format!("{}%", escape_like(path))));
            } else {
                sql.push_str(&format!(
                    " AND {column_prefix}path_lower LIKE ? ESCAPE '\\'"
                ));
                parameters.push(Value::Text(format!("%{}%", escape_like(path))));
            }
        }
        if let Some(extension) = &query.extension {
            sql.push_str(&format!(" AND {column_prefix}extension = ?"));
            parameters.push(Value::Text(extension.clone()));
        }
        if let Some(kind) = query.kind {
            sql.push_str(&format!(" AND {column_prefix}kind = ?"));
            parameters.push(Value::Integer(kind.as_i64()));
        }
        for excluded in &query.excluded {
            sql.push_str(&format!(
                " AND {column_prefix}name_lower NOT LIKE ? ESCAPE '\\'"
            ));
            parameters.push(Value::Text(format!("%{}%", escape_like(excluded))));
        }
        if !has_linear_filter {
            sql.push(' ');
            sql.push_str(sql_order_by(sort, use_fts));
        }
        sql.push_str(" LIMIT ?");
        parameters.push(Value::Integer(candidate_limit));

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters.iter()), |row| {
            Ok(SearchResult {
                path: row.get(0)?,
                name: row.get(1)?,
                kind: EntryKind::from_i64(row.get(2)?),
                size: row.get(3)?,
                modified_at: row.get(4)?,
                hidden: row.get::<_, i64>(5)? != 0,
                score: 0,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            let mut result = row?;
            if query.matches(&result) {
                result.score = query.rank(&result);
                results.push(result);
            }
        }
        sort_results(&mut results, sort);
        results.truncate(limit);
        Ok(results)
    }
}

fn sql_order_by(sort: SortSpec, fts_query: bool) -> &'static str {
    let prefix = if fts_query { "e." } else { "" };
    match (sort.field, sort.ascending, prefix) {
        (SortField::Name, true, "e.") => "ORDER BY e.name_lower ASC, e.path_lower ASC",
        (SortField::Name, false, "e.") => "ORDER BY e.name_lower DESC, e.path_lower DESC",
        (SortField::Size, true, "e.") => {
            "ORDER BY e.size IS NULL ASC, e.size ASC, e.name_lower ASC"
        }
        (SortField::Size, false, "e.") => {
            "ORDER BY e.size IS NULL ASC, e.size DESC, e.name_lower ASC"
        }
        (SortField::Modified, true, "e.") => {
            "ORDER BY e.modified_at IS NULL ASC, e.modified_at ASC, e.name_lower ASC"
        }
        (SortField::Modified, false, "e.") => {
            "ORDER BY e.modified_at IS NULL ASC, e.modified_at DESC, e.name_lower ASC"
        }
        (SortField::Name, true, _) => "ORDER BY name_lower ASC, path_lower ASC",
        (SortField::Name, false, _) => "ORDER BY name_lower DESC, path_lower DESC",
        (SortField::Size, true, _) => {
            "ORDER BY size IS NULL ASC, size ASC, name_lower ASC"
        }
        (SortField::Size, false, _) => {
            "ORDER BY size IS NULL ASC, size DESC, name_lower ASC"
        }
        (SortField::Modified, true, _) => {
            "ORDER BY modified_at IS NULL ASC, modified_at ASC, name_lower ASC"
        }
        (SortField::Modified, false, _) => {
            "ORDER BY modified_at IS NULL ASC, modified_at DESC, name_lower ASC"
        }
    }
}

fn sort_results(results: &mut [SearchResult], sort: SortSpec) {
    results.sort_unstable_by(|left, right| {
        let ordering = match sort.field {
            SortField::Name => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
            SortField::Size => compare_optional(left.size, right.size, sort.ascending),
            SortField::Modified => {
                compare_optional(left.modified_at, right.modified_at, sort.ascending)
            }
        };
        let ordering = if sort.field == SortField::Name && !sort.ascending {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then_with(|| left.path.cmp(&right.path))
    });
}

fn compare_optional<T: Ord>(left: Option<T>, right: Option<T>, ascending: bool) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) if ascending => left.cmp(&right),
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn quote_fts(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
