use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{
    Connection, InterruptHandle, OpenFlags, OptionalExtension, params, params_from_iter,
};

use crate::model::{EntryKind, FileRecord, SearchResult, SortField, SortSpec};
use crate::search::{SearchMode, SearchQuery};

const DATABASE_SCHEMA_VERSION: &str = "3";

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
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let mut database = Self { connection };
        database.ensure_schema()?;
        database.connection.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS removed_name_ids (
                 id INTEGER PRIMARY KEY
             ) WITHOUT ROWID;",
        )?;
        Ok(database)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("unable to open read-only index {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA temp_store = MEMORY;
             PRAGMA busy_timeout = 1000;
             PRAGMA foreign_keys = ON;
             PRAGMA query_only = ON;",
        )?;
        Ok(Self { connection })
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.connection.get_interrupt_handle()
    }

    fn ensure_schema(&mut self) -> Result<()> {
        let schema_version: Option<String> = self.setting("database_schema_version")?;
        if schema_version.as_deref() == Some(DATABASE_SCHEMA_VERSION) {
            self.create_schema_objects()?;
            return Ok(());
        }

        // Version 3 deliberately rebuilds the index. Migrating millions of
        // duplicated path/name rows in-place would temporarily require both
        // schemas and substantially more disk space than a clean rescan.
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS entries_ai;
             DROP TRIGGER IF EXISTS entries_ad;
             DROP TRIGGER IF EXISTS entries_au;
             DROP TRIGGER IF EXISTS names_ai;
             DROP TRIGGER IF EXISTS names_ad;
             DROP TRIGGER IF EXISTS names_au;
             DROP TABLE IF EXISTS entries_fts;
             DROP TABLE IF EXISTS names_fts;
             DROP TABLE IF EXISTS entries;
             DROP TABLE IF EXISTS names;
             CREATE TABLE names (
                 id          INTEGER PRIMARY KEY,
                 name        TEXT NOT NULL UNIQUE,
                 name_lower  TEXT NOT NULL,
                 extension   TEXT
             );
             CREATE TABLE entries (
                 id          INTEGER PRIMARY KEY,
                 path        TEXT NOT NULL UNIQUE,
                 parent_id   INTEGER REFERENCES entries(id) ON DELETE CASCADE,
                 name_id     INTEGER NOT NULL REFERENCES names(id),
                 kind        INTEGER NOT NULL,
                 size        INTEGER,
                 modified_at INTEGER,
                 hidden      INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX idx_names_lower ON names(name_lower);
             CREATE INDEX idx_names_extension ON names(extension);
             CREATE INDEX idx_entries_parent ON entries(parent_id);
             CREATE INDEX idx_entries_name ON entries(name_id);
             CREATE INDEX idx_entries_path_nocase ON entries(path COLLATE NOCASE);
             CREATE INDEX idx_entries_size ON entries(size);
             CREATE INDEX idx_entries_modified ON entries(modified_at);
             CREATE VIRTUAL TABLE names_fts USING fts5(
                 name_lower,
                 content='names',
                 content_rowid='id',
                 tokenize='trigram'
             );
             CREATE TRIGGER names_ai AFTER INSERT ON names BEGIN
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER names_ad AFTER DELETE ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER names_au AFTER UPDATE OF name_lower ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             INSERT INTO settings(key, value) VALUES ('database_schema_version', '3')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;
             INSERT INTO settings(key, value) VALUES ('scan_complete', '0')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;
             INSERT INTO settings(key, value) VALUES ('system_scan_complete', '0')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;
             DELETE FROM settings WHERE key = 'fts_schema_version';",
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn create_schema_objects(&self) -> Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS names (
                 id          INTEGER PRIMARY KEY,
                 name        TEXT NOT NULL UNIQUE,
                 name_lower  TEXT NOT NULL,
                 extension   TEXT
             );
             CREATE TABLE IF NOT EXISTS entries (
                 id          INTEGER PRIMARY KEY,
                 path        TEXT NOT NULL UNIQUE,
                 parent_id   INTEGER REFERENCES entries(id) ON DELETE CASCADE,
                 name_id     INTEGER NOT NULL REFERENCES names(id),
                 kind        INTEGER NOT NULL,
                 size        INTEGER,
                 modified_at INTEGER,
                 hidden      INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_names_lower ON names(name_lower);
             CREATE INDEX IF NOT EXISTS idx_names_extension ON names(extension);
             CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_id);
             CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name_id);
             CREATE INDEX IF NOT EXISTS idx_entries_path_nocase ON entries(path COLLATE NOCASE);
             CREATE INDEX IF NOT EXISTS idx_entries_size ON entries(size);
             CREATE INDEX IF NOT EXISTS idx_entries_modified ON entries(modified_at);
             CREATE VIRTUAL TABLE IF NOT EXISTS names_fts USING fts5(
                 name_lower,
                 content='names',
                 content_rowid='id',
                 tokenize='trigram'
             );
             CREATE TRIGGER IF NOT EXISTS names_ai AFTER INSERT ON names BEGIN
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER IF NOT EXISTS names_ad AFTER DELETE ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER IF NOT EXISTS names_au AFTER UPDATE OF name_lower ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;",
        )?;
        Ok(())
    }

    pub fn ensure_index_root(&mut self, root: &Path) -> Result<()> {
        let root = root.to_string_lossy();
        let stored_root = self.stored_index_root()?;

        if stored_root.as_deref() != Some(root.as_ref()) {
            self.clear()?;
            self.connection.execute(
                "INSERT INTO settings(key, value) VALUES ('index_root', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [root.as_ref()],
            )?;
            self.set_scan_complete(false)?;
            self.set_include_system_files(false)?;
            self.set_system_scan_complete(false)?;
        }
        Ok(())
    }

    pub fn stored_index_root(&self) -> Result<Option<String>> {
        self.setting("index_root")
    }

    pub fn is_scan_complete(&self) -> Result<bool> {
        let value = self.setting("scan_complete")?;
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

    pub fn include_system_files(&self) -> Result<bool> {
        self.bool_setting("include_system_files")
    }

    pub fn set_include_system_files(&self, include: bool) -> Result<()> {
        self.set_bool_setting("include_system_files", include)
    }

    pub fn is_system_scan_complete(&self) -> Result<bool> {
        self.bool_setting("system_scan_complete")
    }

    pub fn set_system_scan_complete(&self, complete: bool) -> Result<()> {
        self.set_bool_setting("system_scan_complete", complete)
    }

    pub fn last_fsevent_id(&self) -> Result<Option<u64>> {
        Ok(self
            .setting("last_fsevent_id")?
            .and_then(|value| value.parse().ok()))
    }

    pub fn set_last_fsevent_id(&self, event_id: u64) -> Result<()> {
        self.set_setting("last_fsevent_id", &event_id.to_string())
    }

    fn bool_setting(&self, key: &str) -> Result<bool> {
        let value = self.setting(key)?;
        Ok(value.as_deref() == Some("1"))
    }

    fn set_bool_setting(&self, key: &str, value: bool) -> Result<()> {
        self.set_setting(key, if value { "1" } else { "0" })
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
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
            "DROP TRIGGER IF EXISTS names_ai;
             DROP TRIGGER IF EXISTS names_ad;
             DROP TRIGGER IF EXISTS names_au;
             INSERT INTO names_fts(names_fts) VALUES ('delete-all');
             DELETE FROM entries;
             DELETE FROM names;
             CREATE TRIGGER names_ai AFTER INSERT ON names BEGIN
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER names_ad AFTER DELETE ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER names_au AFTER UPDATE OF name_lower ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn retain_path_tree(&mut self, root: &Path) -> Result<()> {
        let root = root.to_string_lossy();
        let prefix = format!("{}/", root.trim_end_matches('/'));
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS names_ai;
             DROP TRIGGER IF EXISTS names_ad;
             DROP TRIGGER IF EXISTS names_au;",
        )?;
        transaction.execute(
            "DELETE FROM entries
             WHERE path != ?1
               AND substr(path, 1, length(?2)) != ?2",
            params![root.as_ref(), prefix],
        )?;
        transaction.execute(
            "DELETE FROM names
             WHERE NOT EXISTS (SELECT 1 FROM entries WHERE entries.name_id = names.id)",
            [],
        )?;
        transaction.execute(
            "INSERT INTO names_fts(names_fts) VALUES ('rebuild')",
            [],
        )?;
        transaction.execute_batch(
            "INSERT INTO settings(key, value) VALUES ('include_system_files', '0')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;
             INSERT INTO settings(key, value) VALUES ('system_scan_complete', '0')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
        )?;
        transaction.execute_batch(
            "CREATE TRIGGER names_ai AFTER INSERT ON names BEGIN
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;
             CREATE TRIGGER names_ad AFTER DELETE ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
             END;
             CREATE TRIGGER names_au AFTER UPDATE OF name_lower ON names BEGIN
                 INSERT INTO names_fts(names_fts, rowid, name_lower)
                 VALUES ('delete', old.id, old.name_lower);
                 INSERT INTO names_fts(rowid, name_lower)
                 VALUES (new.id, new.name_lower);
             END;",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn upsert_batch(&mut self, records: &[FileRecord]) -> Result<u64> {
        let transaction = self.connection.transaction()?;
        let mut inserted_count = 0_u64;
        {
            let mut insert_name = transaction.prepare_cached(
                "INSERT INTO names(name, name_lower, extension)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO NOTHING",
            )?;
            let mut select_name =
                transaction.prepare_cached("SELECT id FROM names WHERE name = ?1")?;
            let mut select_parent =
                transaction.prepare_cached("SELECT id FROM entries WHERE path = ?1")?;
            let mut insert_entry = transaction.prepare_cached(
                "INSERT INTO entries (
                    path, parent_id, name_id, kind, size, modified_at, hidden
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO NOTHING",
            )?;
            let mut update_entry = transaction.prepare_cached(
                "UPDATE entries SET
                    parent_id = ?2,
                    name_id = ?3,
                    kind = ?4,
                    size = ?5,
                    modified_at = ?6,
                    hidden = ?7
                 WHERE path = ?1",
            )?;
            let mut name_ids = HashMap::<&str, i64>::new();

            for record in records {
                let name_id = if let Some(id) = name_ids.get(record.name.as_str()) {
                    *id
                } else {
                    insert_name.execute(params![
                        record.name,
                        record.name_lower,
                        record.extension,
                    ])?;
                    let id = select_name.query_row([record.name.as_str()], |row| row.get(0))?;
                    name_ids.insert(record.name.as_str(), id);
                    id
                };
                let parent_id = if record.parent.is_empty() {
                    None
                } else {
                    select_parent
                        .query_row([record.parent.as_str()], |row| row.get::<_, i64>(0))
                        .optional()?
                };
                let inserted = insert_entry.execute(params![
                    record.path,
                    parent_id,
                    name_id,
                    record.kind.as_i64(),
                    record.size,
                    record.modified_at,
                    record.hidden,
                ])?;
                if inserted == 0 {
                    update_entry.execute(params![
                        record.path,
                        parent_id,
                        name_id,
                        record.kind.as_i64(),
                        record.size,
                        record.modified_at,
                        record.hidden,
                    ])?;
                } else {
                    inserted_count = inserted_count.saturating_add(inserted as u64);
                }
            }
        }
        transaction.commit()?;
        Ok(inserted_count)
    }

    pub fn remove_path_tree(&mut self, path: &Path) -> Result<u64> {
        let path = path.to_string_lossy();
        let trimmed_path = path.trim_end_matches('/');
        let prefix = format!("{trimmed_path}/");
        // SQLite's default BINARY collation compares UTF-8 bytes. Replacing
        // the trailing slash with the next ASCII byte gives an exclusive
        // upper bound for every descendant path while keeping the path index
        // usable: `/a/` <= descendants < `/a0`.
        let upper_bound = format!("{trimmed_path}0");
        let transaction = self.connection.transaction()?;

        transaction.execute("DELETE FROM temp.removed_name_ids", [])?;
        transaction.execute(
            "INSERT OR IGNORE INTO temp.removed_name_ids(id)
             SELECT name_id FROM entries WHERE path = ?1",
            [path.as_ref()],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO temp.removed_name_ids(id)
             SELECT name_id FROM entries
             WHERE path >= ?2 AND path < ?3 AND path != ?1",
            params![path.as_ref(), prefix, upper_bound],
        )?;
        let exact_removed = transaction.query_row(
            "SELECT count(*) FROM entries WHERE path = ?1",
            [path.as_ref()],
            |row| row.get::<_, u64>(0),
        )?;
        let descendant_removed = transaction.query_row(
            "SELECT count(*) FROM entries
             WHERE path >= ?2 AND path < ?3 AND path != ?1",
            params![path.as_ref(), prefix, upper_bound],
            |row| row.get::<_, u64>(0),
        )?;
        let removed = exact_removed.saturating_add(descendant_removed);
        transaction.execute(
            "DELETE FROM entries WHERE path = ?1",
            [path.as_ref()],
        )?;
        // The exact-path delete normally cascades through parent_id. The
        // indexed range delete also removes any legacy/orphaned descendants
        // whose parent_id could not be populated.
        transaction.execute(
            "DELETE FROM entries
             WHERE path >= ?2 AND path < ?3 AND path != ?1",
            params![path.as_ref(), prefix, upper_bound],
        )?;
        transaction.execute(
            "DELETE FROM names
             WHERE id IN (SELECT id FROM temp.removed_name_ids)
               AND NOT EXISTS (
                   SELECT 1 FROM entries WHERE entries.name_id = names.id
               )",
            [],
        )?;
        transaction.execute("DELETE FROM temp.removed_name_ids", [])?;
        transaction.commit()?;
        Ok(removed)
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
            "SELECT e.path, n.name, e.kind, e.size, e.modified_at, e.hidden
             FROM names_fts
             JOIN names n ON n.id = names_fts.rowid
             JOIN entries e ON e.name_id = n.id
             WHERE names_fts MATCH ?"
                .to_owned()
        } else {
            "SELECT e.path, n.name, e.kind, e.size, e.modified_at, e.hidden
             FROM names n
             JOIN entries e ON e.name_id = n.id
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

        for term in query
            .terms
            .iter()
            .filter(|term| term.chars().count() < 3)
        {
            sql.push_str(" AND n.name_lower LIKE ? ESCAPE '\\'");
            parameters.push(Value::Text(format!("%{}%", escape_like(term))));
        }
        if let Some(path) = &query.path {
            if query.path_is_anchored {
                sql.push_str(" AND e.path COLLATE NOCASE LIKE ? ESCAPE '\\'");
                parameters.push(Value::Text(format!("{}%", escape_like(path))));
            } else {
                sql.push_str(" AND e.path COLLATE NOCASE LIKE ? ESCAPE '\\'");
                parameters.push(Value::Text(format!("%{}%", escape_like(path))));
            }
        }
        if let Some(extension) = &query.extension {
            sql.push_str(" AND n.extension = ?");
            parameters.push(Value::Text(extension.clone()));
        }
        if let Some(kind) = query.kind {
            sql.push_str(" AND e.kind = ?");
            parameters.push(Value::Integer(kind.as_i64()));
        }
        for excluded in &query.excluded {
            sql.push_str(" AND n.name_lower NOT LIKE ? ESCAPE '\\'");
            parameters.push(Value::Text(format!("%{}%", escape_like(excluded))));
        }
        if !has_linear_filter {
            sql.push(' ');
            sql.push_str(sql_order_by(sort));
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

fn sql_order_by(sort: SortSpec) -> &'static str {
    match (sort.field, sort.ascending) {
        (SortField::Name, true) => "ORDER BY n.name_lower ASC, e.path ASC",
        (SortField::Name, false) => "ORDER BY n.name_lower DESC, e.path DESC",
        (SortField::Size, true) => {
            "ORDER BY e.size IS NULL ASC, e.size ASC, n.name_lower ASC"
        }
        (SortField::Size, false) => {
            "ORDER BY e.size IS NULL ASC, e.size DESC, n.name_lower ASC"
        }
        (SortField::Modified, true) => {
            "ORDER BY e.modified_at IS NULL ASC, e.modified_at ASC, n.name_lower ASC"
        }
        (SortField::Modified, false) => {
            "ORDER BY e.modified_at IS NULL ASC, e.modified_at DESC, n.name_lower ASC"
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
