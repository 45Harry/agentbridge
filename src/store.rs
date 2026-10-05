//! The persistent index: one SQLite file in agentbridge's own data dir.
//!
//! It exists so search and briefs do not re-read every transcript on every run.
//! It is a **cache, not a system of record**: it can be deleted at any time and
//! rebuilt with `agentbridge index`, and nothing is lost but time.
//!
//! This deliberately amends DESIGN.md Rule 1 ("never copy a session body"): the
//! index holds a *derived* copy of message text, because full-text search needs
//! the text. What keeps it within the spirit of the rule:
//!
//! * the text is **redacted** before it is stored (a secret never lands here),
//! * it is **truncated** per message, so a 100 MB tool dump is a few hundred bytes,
//! * tool output is stored only as a short excerpt, never in full,
//! * it never leaves the data dir, and `unsync` / deleting the file removes it.
//!
//! The schema is versioned with forward-only migrations: an index written by a
//! newer agentbridge is refused rather than guessed at, and an older one is
//! upgraded in a transaction at open.

use crate::model::{Message, Role, Session};
use crate::redact::Redactor;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Longest message text kept per message.
pub const MAX_TEXT_CHARS: usize = 4_000;
/// Longest excerpt kept from a tool result.
pub const MAX_TOOL_RESULT_CHARS: usize = 500;
/// Longest "what the tool was asked to touch" string kept per tool call.
const MAX_TOOL_ARG_CHARS: usize = 500;

/// Ordered, forward-only. Never edit an entry that has shipped; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE sessions (
        id            INTEGER PRIMARY KEY,
        provider      TEXT NOT NULL,
        sid           TEXT NOT NULL,
        project       TEXT,
        title         TEXT,
        started_at    INTEGER,
        last_event_at INTEGER,
        source_path   TEXT NOT NULL,
        fingerprint   TEXT NOT NULL,
        message_count INTEGER NOT NULL,
        indexed_at    INTEGER NOT NULL,
        UNIQUE (provider, sid)
    );
    CREATE INDEX sessions_project ON sessions (project);
    CREATE TABLE messages (
        id        INTEGER PRIMARY KEY,
        session   INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        ordinal   INTEGER NOT NULL,
        role      TEXT NOT NULL,
        ts        INTEGER,
        tool_name TEXT,
        tool_arg  TEXT,
        text      TEXT NOT NULL,
        UNIQUE (session, ordinal)
    );
    CREATE VIRTUAL TABLE messages_fts USING fts5(
        text, tool_name, tool_arg,
        content = 'messages', content_rowid = 'id',
        -- Stemming, so "migration" finds "migrate" and "tests" finds "test".
        tokenize = 'porter unicode61'
    );
    -- Kept in sync by triggers, not by application code, so a partial write can
    -- never leave the search index disagreeing with the table.
    CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts (rowid, text, tool_name, tool_arg)
        VALUES (new.id, new.text, new.tool_name, new.tool_arg);
    END;
    CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts (messages_fts, rowid, text, tool_name, tool_arg)
        VALUES ('delete', old.id, old.text, old.tool_name, old.tool_arg);
    END;
    CREATE TABLE facts (
        id         INTEGER PRIMARY KEY,
        project    TEXT,
        text       TEXT NOT NULL,
        tags       TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE brief_cache (
        key        TEXT PRIMARY KEY,
        content    TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    "#,
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("index database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error(
        "the index at {path} was written by a newer agentbridge (schema {found}, this build \
         understands up to {known}); upgrade agentbridge, or delete the file and run `agentbridge index`"
    )]
    TooNew { path: PathBuf, found: u32, known: u32 },
    #[error("cannot prepare {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type StoreResult<T> = Result<T, StoreError>;

pub struct Store {
    conn: Connection,
}

/// One search result, with enough to cite it: provider, session and ordinal.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub provider: String,
    pub sid: String,
    pub ordinal: u64,
    pub role: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub ts: Option<i64>,
    /// Matched text with `«…»` around the hits.
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub project: Option<String>,
    pub provider: Option<String>,
    /// Only messages at or after this unix time.
    pub since: Option<i64>,
    pub limit: usize,
}

/// A session row, as the brief builder reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub rowid: i64,
    pub provider: String,
    pub sid: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub started_at: Option<i64>,
    pub last_event_at: Option<i64>,
    pub message_count: u64,
}

/// A stored message, as the brief builder reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub ordinal: u64,
    pub role: String,
    pub tool_name: Option<String>,
    pub tool_arg: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: i64,
    pub project: Option<String>,
    pub text: String,
    pub tags: Vec<String>,
    pub created_at: i64,
}

pub fn default_path() -> PathBuf {
    crate::sync::data_dir().join("index.db")
}

fn clip(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((cut, _)) => s[..cut].to_string(),
        None => s.to_string(),
    }
}

/// The part of a tool call worth remembering: the file it touched or the command
/// it ran. Keys are the ones real tools use (Claude Code: `file_path`,
/// `command`; Codex: `command` as an argv array).
fn tool_arg(input: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    for key in [
        "file_path", "path", "filePath", "notebook_path", "command", "cmd", "pattern", "url",
        // Antigravity's tools (verified on a real conversation).
        "AbsolutePath", "TargetFile", "DirectoryPath", "CommandLine", "SearchPath", "Query",
    ] {
        match input.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(clip(s, MAX_TOOL_ARG_CHARS)),
            Some(Value::Array(parts)) => {
                let argv: Vec<&str> = parts.iter().filter_map(|p| p.as_str()).collect();
                if !argv.is_empty() {
                    return Some(clip(&argv.join(" "), MAX_TOOL_ARG_CHARS));
                }
            }
            _ => {}
        }
    }
    None
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// What to store for one message, already redacted: `(text, tool_arg)`.
fn stored_parts(m: &Message, redactor: &Redactor) -> (String, Option<String>) {
    let mut m = m.clone();
    redactor.message(&mut m);
    let arg = m.tool_input.as_ref().and_then(tool_arg);
    let text = match m.role {
        Role::Tool => {
            let raw = match &m.tool_result {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            clip(&raw, MAX_TOOL_RESULT_CHARS)
        }
        _ => clip(m.text.as_deref().unwrap_or(""), MAX_TEXT_CHARS),
    };
    (text, arg)
}

fn secs(t: Option<chrono::DateTime<chrono::Utc>>) -> Option<i64> {
    t.map(|t| t.timestamp())
}

/// Turn free text into an FTS5 query that cannot be a syntax error: each word
/// becomes a quoted term and all must match. A search box is not a query
/// language, and a stray `"` or `-` or `AND` from a user (or an agent) must not
/// fail the call.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

impl Store {
    /// Open (creating and migrating if needed) the index at `path`.
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        // Other processes (the MCP server, `index`) share this file.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate(path)?;
        Ok(store)
    }

    pub fn open_default() -> StoreResult<Self> {
        Self::open(&default_path())
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let s = Self { conn };
        s.migrate(Path::new(":memory:")).unwrap();
        s
    }

    fn schema_version(&self) -> StoreResult<u32> {
        // `meta` does not exist before migration 1; user_version is the record.
        Ok(self.conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))?)
    }

    fn migrate(&self, path: &Path) -> StoreResult<()> {
        let known = MIGRATIONS.len() as u32;
        let found = self.schema_version()?;
        if found > known {
            return Err(StoreError::TooNew {
                path: path.to_path_buf(),
                found,
                known,
            });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            // Each step and its version bump commit together or not at all.
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as u32)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn version(&self) -> u32 {
        self.schema_version().unwrap_or(0)
    }

    /// `(provider, sid) -> (rowid, fingerprint)` for every indexed session, so a
    /// refresh can skip what has not changed without loading it.
    pub fn fingerprints(&self) -> StoreResult<HashMap<(String, String), (i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT provider, sid, id, fingerprint FROM sessions")?;
        let rows = stmt.query_map([], |r| {
            Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?)))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Insert or replace one session and its messages, atomically. The session's
    /// text is redacted here, so nothing unredacted can reach the file.
    pub fn index_session(
        &mut self,
        session: &Session,
        fingerprint: &str,
        redactor: &Redactor,
    ) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE provider = ?1 AND sid = ?2",
            params![session.provider, session.id],
        )?;
        let title = session.title.as_deref().map(|t| redactor.text(t).0);
        tx.execute(
            "INSERT INTO sessions (provider, sid, project, title, started_at, last_event_at, \
             source_path, fingerprint, message_count, indexed_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                session.provider,
                session.id,
                session.project_path(),
                title,
                secs(session.started_at),
                secs(session.last_event_at),
                session.source_path.to_string_lossy(),
                fingerprint,
                session.messages.len() as i64,
                chrono::Utc::now().timestamp(),
            ],
        )?;
        let rowid = tx.last_insert_rowid();
        {
            let mut ins = tx.prepare(
                "INSERT INTO messages (session, ordinal, role, ts, tool_name, tool_arg, text) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for m in &session.messages {
                let (text, arg) = stored_parts(m, redactor);
                ins.execute(params![
                    rowid,
                    m.ordinal as i64,
                    role_str(m.role),
                    secs(m.timestamp),
                    m.tool_name,
                    arg,
                    text,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove sessions by rowid (those whose source has disappeared).
    pub fn remove_sessions(&mut self, rowids: &[i64]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        for id in rowids {
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn counts(&self) -> StoreResult<(u64, u64)> {
        let s: i64 = self.conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let m: i64 = self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok((s as u64, m as u64))
    }

    /// Full-text search, best match first.
    pub fn search(&self, query: &str, filter: &SearchFilter) -> StoreResult<Vec<Hit>> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let limit = if filter.limit == 0 { 20 } else { filter.limit.min(200) } as i64;
        let mut stmt = self.conn.prepare(
            "SELECT s.provider, s.sid, m.ordinal, m.role, s.project, s.title, m.ts, \
                    snippet(messages_fts, -1, '«', '»', '…', 24) \
             FROM messages_fts \
             JOIN messages m ON m.id = messages_fts.rowid \
             JOIN sessions s ON s.id = m.session \
             WHERE messages_fts MATCH ?1 \
               AND (?2 IS NULL OR s.project LIKE '%' || ?2 || '%') \
               AND (?3 IS NULL OR s.provider = ?3) \
               AND (?4 IS NULL OR m.ts >= ?4) \
             ORDER BY bm25(messages_fts) \
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![q, filter.project, filter.provider, filter.since, limit],
            |r| {
                Ok(Hit {
                    provider: r.get(0)?,
                    sid: r.get(1)?,
                    ordinal: r.get::<_, i64>(2)? as u64,
                    role: r.get(3)?,
                    project: r.get(4)?,
                    title: r.get(5)?,
                    ts: r.get(6)?,
                    snippet: r.get(7)?,
                })
            },
        )?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Sessions whose project path is `project` or inside it, newest first.
    pub fn sessions_for_project(
        &self,
        project: &str,
        since: Option<i64>,
    ) -> StoreResult<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, sid, project, title, started_at, last_event_at, message_count \
             FROM sessions \
             WHERE (project = ?1 OR project LIKE ?1 || '/%') \
               AND (?2 IS NULL OR COALESCE(last_event_at, started_at, 0) >= ?2) \
             ORDER BY COALESCE(last_event_at, started_at, 0) DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![project.trim_end_matches('/'), since], |r| {
            Ok(SessionRow {
                rowid: r.get(0)?,
                provider: r.get(1)?,
                sid: r.get(2)?,
                project: r.get(3)?,
                title: r.get(4)?,
                started_at: r.get(5)?,
                last_event_at: r.get(6)?,
                message_count: r.get::<_, i64>(7)? as u64,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn messages_of(&self, session_rowid: i64) -> StoreResult<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT ordinal, role, tool_name, tool_arg, text FROM messages \
             WHERE session = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![session_rowid], |r| {
            Ok(StoredMessage {
                ordinal: r.get::<_, i64>(0)? as u64,
                role: r.get(1)?,
                tool_name: r.get(2)?,
                tool_arg: r.get(3)?,
                text: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// One session by provider and id (an id prefix of 6+ characters also works).
    pub fn find_session(&self, id: &str) -> StoreResult<Option<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, sid, project, title, started_at, last_event_at, message_count \
             FROM sessions WHERE sid = ?1 OR (length(?1) >= 6 AND sid LIKE ?1 || '%') \
             ORDER BY (sid = ?1) DESC, COALESCE(last_event_at, 0) DESC LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![id], |r| {
                Ok(SessionRow {
                    rowid: r.get(0)?,
                    provider: r.get(1)?,
                    sid: r.get(2)?,
                    project: r.get(3)?,
                    title: r.get(4)?,
                    started_at: r.get(5)?,
                    last_event_at: r.get(6)?,
                    message_count: r.get::<_, i64>(7)? as u64,
                })
            })
            .optional()?)
    }

    // ---- facts recorded by an agent or a person --------------------------

    /// Store a fact. The text is redacted first; a fact is attributable by its
    /// id and creation time, shown in a brief as `[fact:<id>]`.
    pub fn add_fact(
        &mut self,
        project: Option<&str>,
        text: &str,
        tags: &[String],
        redactor: &Redactor,
    ) -> StoreResult<i64> {
        let text = clip(&redactor.text(text).0, 1_000);
        self.conn.execute(
            "INSERT INTO facts (project, text, tags, created_at) VALUES (?1,?2,?3,?4)",
            params![
                project.map(|p| p.trim_end_matches('/')),
                text,
                tags.join(","),
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn facts_for_project(&self, project: &str) -> StoreResult<Vec<Fact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project, text, tags, created_at FROM facts \
             WHERE project IS NULL OR project = ?1 OR ?1 LIKE project || '/%' \
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![project.trim_end_matches('/')], |r| {
            let tags: String = r.get(3)?;
            Ok(Fact {
                id: r.get(0)?,
                project: r.get(1)?,
                text: r.get(2)?,
                tags: tags.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ---- brief cache ------------------------------------------------------

    pub fn cached_brief(&self, key: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT content FROM brief_cache WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn cache_brief(&mut self, key: &str, content: &str) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO brief_cache (key, content, created_at) VALUES (?1,?2,?3)",
            params![key, content, chrono::Utc::now().timestamp()],
        )?;
        // A cache that only grows is a leak: keep the newest few.
        tx.execute(
            "DELETE FROM brief_cache WHERE key NOT IN \
             (SELECT key FROM brief_cache ORDER BY created_at DESC, rowid DESC LIMIT 50)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
}


// ---- filling the index from the tools' own stores --------------------------

/// What one refresh did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Sessions the tools reported.
    pub scanned: usize,
    /// (Re)indexed this run.
    pub indexed: usize,
    /// Already current; not re-read.
    pub unchanged: usize,
    /// Dropped because their source no longer exists.
    pub removed: usize,
    /// agentbridge's own copies, left out so a conversation is indexed once.
    pub skipped_copies: usize,
    /// Claude Code sub-agent transcripts (`agent-*`), left out: they are a tool
    /// working for a conversation, and the parent session already holds the
    /// request and the result.
    pub skipped_subagents: usize,
    pub errors: Vec<String>,
}

/// Tells whether a session changed since it was indexed, without reading it.
/// Files are fingerprinted by size and mtime. A database-backed session shares
/// its file with every other session, so that file's mtime says nothing about
/// *this* one; the session's own last-event time does.
fn fingerprint(raw: &crate::model::RawSession) -> String {
    let file_based = matches!(raw.provider.as_str(), "claude-code" | "codex-cli");
    if file_based && let Ok(meta) = std::fs::metadata(&raw.source_path) {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        return format!("f{}:{}", meta.len(), mtime);
    }
    format!("t{}", secs(raw.last_event_at).unwrap_or(0))
}

/// Bring the index up to date with every detected tool. Incremental: only new or
/// changed sessions are loaded. Read-only on the tools' stores.
pub fn refresh(
    store: &mut Store,
    registry: &crate::connector::Registry,
    redactor: &Redactor,
    only_provider: Option<&str>,
    mut progress: impl FnMut(&RefreshStats),
) -> StoreResult<RefreshStats> {
    let known = store.fingerprints()?;
    let mut stats = RefreshStats::default();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // Providers whose scan completed. A failed scan proves nothing about what
    // is gone, so its rows must not be deleted below.
    let mut scanned_ok: std::collections::HashSet<String> = std::collections::HashSet::new();

    for connector in registry
        .detected()
        .filter(|c| only_provider.is_none_or(|p| p == c.id()))
    {
        let stream = match connector.scan() {
            Ok(s) => s,
            Err(e) => {
                stats.errors.push(format!("{}: scan failed: {e}", connector.id()));
                continue;
            }
        };
        scanned_ok.insert(connector.id().to_string());
        for item in stream {
            let raw = match item {
                Ok(r) => r,
                Err(e) => {
                    stats.errors.push(format!("{}: {e}", connector.id()));
                    continue;
                }
            };
            if !raw.body_available {
                continue;
            }
            let key = (raw.provider.clone(), raw.id.clone());
            if !seen.insert(key.clone()) {
                continue; // the same session listed twice
            }
            stats.scanned += 1;

            if raw.provider == "claude-code" && raw.id.starts_with("agent-") {
                stats.skipped_subagents += 1;
                continue;
            }
            let is_copy = raw
                .title
                .as_deref()
                .is_some_and(|t| crate::label::parse(t).is_some())
                || crate::marker::read(&raw.source_path).is_some();
            if is_copy {
                stats.skipped_copies += 1;
                continue;
            }

            let fp = fingerprint(&raw);
            if known.get(&key).is_some_and(|(_, old)| *old == fp) {
                stats.unchanged += 1;
                continue;
            }
            match connector.load(&raw.id) {
                Ok(session) => match store.index_session(&session, &fp, redactor) {
                    Ok(()) => stats.indexed += 1,
                    Err(e) => stats.errors.push(format!("index {}: {e}", raw.id)),
                },
                Err(e) => stats.errors.push(format!("load {}: {e}", raw.id)),
            }
            if (stats.indexed + stats.unchanged).is_multiple_of(50) {
                progress(&stats);
            }
        }
    }

    let gone: Vec<i64> = known
        .iter()
        .filter(|(k, _)| scanned_ok.contains(&k.0) && !seen.contains(*k))
        .map(|(_, (rowid, _))| *rowid)
        .collect();
    stats.removed = gone.len();
    store.remove_sessions(&gone)?;
    progress(&stats);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TokenTotals;
    use serde_json::json;

    fn msg(ord: u64, role: Role, text: &str) -> Message {
        Message {
            session_id: "s".into(),
            ordinal: ord,
            role,
            timestamp: chrono::DateTime::from_timestamp(1_780_000_000 + ord as i64, 0),
            text: Some(text.into()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        }
    }

    fn session(provider: &str, id: &str, project: &str, msgs: Vec<Message>) -> Session {
        Session {
            id: id.into(),
            provider: provider.into(),
            project_id: project.into(),
            started_at: chrono::DateTime::from_timestamp(1_780_000_000, 0),
            last_event_at: chrono::DateTime::from_timestamp(1_780_000_100, 0),
            model: None,
            title: Some(format!("title of {id}")),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from(format!("/src/{id}")),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: msgs,
            artifacts: vec![],
        }
    }

    fn red() -> Redactor {
        Redactor::defaults()
    }

    #[test]
    fn test_open_creates_the_schema_and_reopening_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/index.db");
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version(), MIGRATIONS.len() as u32);
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version(), MIGRATIONS.len() as u32);
        assert_eq!(s.counts().unwrap(), (0, 0));
    }

    #[test]
    fn test_an_index_from_a_newer_agentbridge_is_refused_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        drop(Store::open(&path).unwrap());
        let future = MIGRATIONS.len() as u32 + 3;
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", future)
            .unwrap();
        match Store::open(&path) {
            Err(StoreError::TooNew { found, known, .. }) => {
                assert_eq!((found, known), (future, MIGRATIONS.len() as u32));
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("a newer index must be refused"),
        }
        // Not downgraded or rewritten.
        let v: u32 = Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, future);
    }

    #[test]
    fn test_migrations_apply_in_order_to_an_older_index() {
        // Simulate an index at version 0 (never migrated): all steps run.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.db");
        Connection::open(&path).unwrap(); // empty file, user_version = 0
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version(), MIGRATIONS.len() as u32);
        assert_eq!(s.counts().unwrap(), (0, 0));
    }

    #[test]
    fn test_search_finds_text_and_cites_where_it_came_from() {
        let mut s = Store::in_memory();
        s.index_session(
            &session("codex-cli", "aaaa1111", "/work/p", vec![
                msg(0, Role::User, "please migrate the database to postgres"),
                msg(1, Role::Assistant, "I will write the migration script"),
            ]),
            "fp1",
            &red(),
        )
        .unwrap();
        s.index_session(
            &session("claude-code", "bbbb2222", "/work/other", vec![msg(0, Role::User, "unrelated chatter")]),
            "fp2",
            &red(),
        )
        .unwrap();

        let hits = s.search("postgres migration", &SearchFilter::default()).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        let h = &hits[0];
        assert_eq!((h.provider.as_str(), h.sid.as_str(), h.ordinal), ("codex-cli", "aaaa1111", 0));
        assert!(h.snippet.contains('«'), "matched words are marked: {}", h.snippet);

        // Filters narrow it.
        let f = |p: Option<&str>, prov: Option<&str>| SearchFilter {
            project: p.map(Into::into),
            provider: prov.map(Into::into),
            ..Default::default()
        };
        // Two messages: one says "migrate", one "migration"; stemming matches both.
        assert_eq!(s.search("migration", &f(Some("/work/p"), None)).unwrap().len(), 2);
        assert!(s.search("migration", &f(Some("/work/other"), None)).unwrap().is_empty());
        assert!(s.search("migration", &f(None, Some("claude-code"))).unwrap().is_empty());
        let late = SearchFilter { since: Some(1_780_000_001), ..Default::default() };
        assert_eq!(s.search("database", &late).unwrap().len(), 0, "ordinal 0 is before `since`");
        assert_eq!(s.search("script", &late).unwrap().len(), 1);
    }

    #[test]
    fn test_search_stems_words() {
        let mut s = Store::in_memory();
        s.index_session(&session("c", "x1", "/p", vec![msg(0, Role::User, "running the tests failed")]), "f", &red()).unwrap();
        for q in ["run", "test", "fail", "running tests"] {
            assert_eq!(s.search(q, &SearchFilter::default()).unwrap().len(), 1, "{q}");
        }
    }

    #[test]
    fn test_hostile_search_text_is_never_a_syntax_error() {
        let mut s = Store::in_memory();
        s.index_session(&session("c", "x1", "/p", vec![msg(0, Role::User, "an AND or NOT thing")]), "f", &red())
            .unwrap();
        for q in ["\"", "AND", "foo OR", "a*", "NEAR(", "-x", "'; DROP TABLE messages; --", "   ", "(((", "text:"] {
            assert!(s.search(q, &SearchFilter::default()).is_ok(), "query {q:?} errored");
        }
        assert_eq!(s.counts().unwrap(), (1, 1), "the index must be intact");
        assert_eq!(s.search("AND OR NOT", &SearchFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn test_reindexing_replaces_a_session_and_its_search_entries() {
        let mut s = Store::in_memory();
        s.index_session(&session("c", "x1", "/p", vec![msg(0, Role::User, "alpha beta")]), "v1", &red()).unwrap();
        s.index_session(&session("c", "x1", "/p", vec![msg(0, Role::User, "gamma delta")]), "v2", &red()).unwrap();

        assert_eq!(s.counts().unwrap(), (1, 1));
        assert!(s.search("alpha", &SearchFilter::default()).unwrap().is_empty(), "stale text still searchable");
        assert_eq!(s.search("gamma", &SearchFilter::default()).unwrap().len(), 1);
        assert_eq!(s.fingerprints().unwrap()[&("c".into(), "x1".into())].1, "v2");
    }

    #[test]
    fn test_removing_a_session_removes_it_from_search() {
        let mut s = Store::in_memory();
        s.index_session(&session("c", "x1", "/p", vec![msg(0, Role::User, "findable words")]), "f", &red()).unwrap();
        let (rowid, _) = s.fingerprints().unwrap()[&("c".into(), "x1".into())].clone();
        s.remove_sessions(&[rowid]).unwrap();
        assert_eq!(s.counts().unwrap(), (0, 0));
        assert!(s.search("findable", &SearchFilter::default()).unwrap().is_empty());
    }

    /// The headline property: a secret in a session never reaches the index file.
    #[test]
    fn test_secrets_never_reach_the_index_file() {
        let secret = "sk-abc123def456ghi789jkl012";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        let mut s = Store::open(&path).unwrap();
        let mut call = msg(1, Role::Assistant, "");
        call.text = None;
        call.tool_name = Some("Bash".into());
        call.tool_input = Some(json!({ "command": format!("export KEY={secret}") }));
        let mut res = msg(2, Role::Tool, "");
        res.tool_result = Some(json!(format!("printed {secret}")));
        let mut sess = session("c", "x1", "/p", vec![msg(0, Role::User, &format!("my key is {secret}")), call, res]);
        sess.title = Some(format!("debugging {secret}"));
        s.index_session(&sess, "f", &red()).unwrap();
        drop(s); // checkpoint the WAL into the main file

        for name in ["index.db", "index.db-wal", "index.db-shm"] {
            if let Ok(bytes) = std::fs::read(tmp.path().join(name)) {
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "secret found in {name}"
                );
            }
        }
        let s = Store::open(&path).unwrap();
        assert!(s.search("abc123def456ghi789jkl012", &SearchFilter::default()).unwrap().is_empty());
        assert!(!s.search("REDACTED", &SearchFilter::default()).unwrap().is_empty(), "the tag is searchable");
    }

    #[test]
    fn test_storage_is_bounded_per_message() {
        let mut s = Store::in_memory();
        let mut res = msg(1, Role::Tool, "");
        res.tool_result = Some(json!("y".repeat(5_000_000)));
        let big = msg(0, Role::User, &"word ".repeat(200_000));
        s.index_session(&session("c", "x1", "/p", vec![big, res]), "f", &red()).unwrap();
        let rowid = s.fingerprints().unwrap()[&("c".into(), "x1".into())].0;
        let stored = s.messages_of(rowid).unwrap();
        assert!(stored[0].text.chars().count() <= MAX_TEXT_CHARS);
        assert!(stored[1].text.chars().count() <= MAX_TOOL_RESULT_CHARS);
        // Clipping must not split a character.
        let mut s2 = Store::in_memory();
        s2.index_session(&session("c", "x2", "/p", vec![msg(0, Role::User, &"é".repeat(MAX_TEXT_CHARS + 50))]), "f", &red()).unwrap();
        let r2 = s2.fingerprints().unwrap()[&("c".into(), "x2".into())].0;
        assert_eq!(s2.messages_of(r2).unwrap()[0].text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn test_tool_calls_keep_the_file_or_command_they_touched() {
        let mut s = Store::in_memory();
        let mk = |ord, name: &str, input| {
            let mut m = msg(ord, Role::Assistant, "");
            m.text = None;
            m.tool_name = Some(name.into());
            m.tool_input = Some(input);
            m
        };
        s.index_session(&session("c", "x1", "/p", vec![
            mk(0, "Edit", json!({"file_path": "/p/src/main.rs", "old_string": "a", "new_string": "b"})),
            mk(1, "shell", json!({"command": ["cargo", "test", "--lib"]})),
        ]), "f", &red()).unwrap();
        let rowid = s.fingerprints().unwrap()[&("c".into(), "x1".into())].0;
        let m = s.messages_of(rowid).unwrap();
        assert_eq!(m[0].tool_arg.as_deref(), Some("/p/src/main.rs"));
        assert_eq!(m[1].tool_arg.as_deref(), Some("cargo test --lib"));
        // And it is searchable by file.
        assert_eq!(s.search("main.rs", &SearchFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn test_sessions_for_project_includes_subdirectories_and_orders_newest_first() {
        let mut s = Store::in_memory();
        let mut old = session("c", "old", "/work/p", vec![msg(0, Role::User, "x")]);
        old.last_event_at = chrono::DateTime::from_timestamp(1_000, 0);
        let mut new = session("c", "new", "/work/p/sub", vec![msg(0, Role::User, "x")]);
        new.last_event_at = chrono::DateTime::from_timestamp(2_000, 0);
        let other = session("c", "other", "/work/pp", vec![msg(0, Role::User, "x")]); // prefix lookalike
        for x in [&old, &new, &other] {
            s.index_session(x, "f", &red()).unwrap();
        }
        let rows = s.sessions_for_project("/work/p", None).unwrap();
        assert_eq!(rows.iter().map(|r| r.sid.as_str()).collect::<Vec<_>>(), ["new", "old"]);
        assert_eq!(s.sessions_for_project("/work/p", Some(1_500)).unwrap().len(), 1);
    }

    #[test]
    fn test_facts_are_redacted_scoped_and_listed_newest_first() {
        let mut s = Store::in_memory();
        let a = s.add_fact(Some("/work/p"), "use tabs", &["style".into()], &red()).unwrap();
        let b = s.add_fact(None, "token sk-abc123def456ghi789jkl012 leaked", &[], &red()).unwrap();
        s.add_fact(Some("/elsewhere"), "not ours", &[], &red()).unwrap();
        let facts = s.facts_for_project("/work/p/sub").unwrap();
        assert_eq!(facts.iter().map(|f| f.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(facts[1].tags, ["style"]);
        assert!(!facts[0].text.contains("sk-abc123"), "{}", facts[0].text);
    }

    #[test]
    fn test_brief_cache_round_trips_and_stays_bounded() {
        let mut s = Store::in_memory();
        assert!(s.cached_brief("k").unwrap().is_none());
        s.cache_brief("k", "one").unwrap();
        s.cache_brief("k", "two").unwrap();
        assert_eq!(s.cached_brief("k").unwrap().as_deref(), Some("two"));
        for i in 0..80 {
            s.cache_brief(&format!("key{i}"), "x").unwrap();
        }
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM brief_cache", [], |r| r.get(0)).unwrap();
        assert!(n <= 50, "cache grew to {n}");
    }

    #[test]
    fn test_find_session_by_full_id_or_prefix() {
        let mut s = Store::in_memory();
        s.index_session(&session("c", "abcdef123456", "/p", vec![msg(0, Role::User, "x")]), "f", &red()).unwrap();
        assert!(s.find_session("abcdef123456").unwrap().is_some());
        assert!(s.find_session("abcdef").unwrap().is_some());
        assert!(s.find_session("abc").unwrap().is_none(), "prefixes under 6 chars are too ambiguous");
        assert!(s.find_session("zzzzzz").unwrap().is_none());
    }

    // ---- refresh ----

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn test_refresh_indexes_the_fixtures_then_skips_everything_unchanged() {
        let mut s = Store::in_memory();
        let registry = crate::connectors::all_for_testing(&fixtures());
        let first = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(first.indexed >= 8, "{first:?}");
        assert_eq!(first.unchanged, 0);
        let (sessions, messages) = s.counts().unwrap();
        assert_eq!(sessions as usize, first.indexed);
        assert!(messages > 20, "{messages}");

        let second = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert_eq!(second.indexed, 0, "nothing changed, nothing may be re-read: {second:?}");
        assert_eq!(second.unchanged, first.indexed);
        assert_eq!(second.removed, 0);
        assert_eq!(s.counts().unwrap(), (sessions, messages));
    }

    #[test]
    fn test_refresh_picks_up_a_changed_file_and_drops_a_deleted_one() {
        let tmp = tempfile::tempdir().unwrap();
        let line = |text: &str| {
            format!("{}\n", json!({"type":"user","cwd":"/w","timestamp":"2026-07-01T10:00:00Z","message":{"role":"user","content":text}}))
        };
        let a = tmp.path().join("a.jsonl");
        let b = tmp.path().join("b.jsonl");
        std::fs::write(&a, line("first version")).unwrap();
        std::fs::write(&b, line("will be deleted")).unwrap();
        let registry = crate::connector::Registry::new(vec![Box::new(
            crate::connectors::claude_code::TestClaudeCode::new(tmp.path().to_path_buf()),
        )]);

        let mut s = Store::in_memory();
        assert_eq!(refresh(&mut s, &registry, &red(), None, |_| {}).unwrap().indexed, 2);

        std::fs::write(&a, format!("{}{}", line("first version"), line("second turn added"))).unwrap();
        std::fs::remove_file(&b).unwrap();
        let r = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert_eq!((r.indexed, r.removed, r.unchanged), (1, 1, 0), "{r:?}");
        assert_eq!(s.counts().unwrap().0, 1);
        assert_eq!(s.search("second turn", &SearchFilter::default()).unwrap().len(), 1);
        assert!(s.search("deleted", &SearchFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn test_refresh_leaves_agentbridges_own_copies_out() {
        let tmp = tempfile::tempdir().unwrap();
        // A copy written by agentbridge: carries the marker and a labelled title.
        let origin = session("codex-cli", "origin-1", "/w", vec![msg(0, Role::User, "real conversation")]);
        let copy_dir = tmp.path().join("claude");
        use crate::convert::SessionConverter;
        let copy = crate::convert::ClaudeCodeConverter::new().convert(&origin, &copy_dir).unwrap();
        assert!(crate::marker::read(&copy).is_some());

        let registry = crate::connector::Registry::new(vec![Box::new(
            crate::connectors::claude_code::TestClaudeCode::new(copy_dir),
        )]);
        let mut s = Store::in_memory();
        let r = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert_eq!((r.scanned, r.indexed, r.skipped_copies), (1, 0, 1), "{r:?}");
        assert_eq!(s.counts().unwrap(), (0, 0));
    }

    /// A tool whose scan fails (locked database, unmounted drive) says nothing
    /// about what exists, so its sessions must stay indexed.
    #[test]
    fn test_a_failed_scan_does_not_delete_that_tools_sessions() {
        struct Broken;
        impl crate::connector::Connector for Broken {
            fn id(&self) -> &'static str { "claude-code" }
            fn detect(&self) -> bool { true }
            fn roots(&self) -> Vec<PathBuf> { vec![] }
            fn scan(&self) -> crate::connector::ConnectorResult<crate::connector::SessionStream<'_>> {
                Err(crate::connector::ConnectorError::Other(anyhow::anyhow!("database is locked")))
            }
            fn load(&self, id: &str) -> crate::connector::ConnectorResult<Session> {
                Err(crate::connector::ConnectorError::NotFound(id.into()))
            }
            fn resume_cmd(&self, _: &Session) -> Option<Vec<String>> { None }
        }
        let mut s = Store::in_memory();
        s.index_session(&session("claude-code", "keep-me", "/p", vec![msg(0, Role::User, "precious")]), "f", &red()).unwrap();
        let registry = crate::connector::Registry::new(vec![Box::new(Broken)]);
        let r = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.errors.iter().any(|e| e.contains("locked")), "{:?}", r.errors);
        assert_eq!(s.counts().unwrap().0, 1, "the session was deleted because a scan failed");
    }

    #[test]
    fn test_refresh_leaves_claude_subagent_transcripts_out() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = json!({"type":"user","cwd":"/w","timestamp":"2026-07-01T10:00:00Z","message":{"role":"user","content":"do the research task"}});
        std::fs::write(tmp.path().join("agent-a1b2c3d.jsonl"), format!("{rec}\n")).unwrap();
        std::fs::write(tmp.path().join("5b0e0000-0000-4000-8000-000000000001.jsonl"), format!("{rec}\n")).unwrap();
        let registry = crate::connector::Registry::new(vec![Box::new(
            crate::connectors::claude_code::TestClaudeCode::new(tmp.path().to_path_buf()),
        )]);
        let mut s = Store::in_memory();
        let r = refresh(&mut s, &registry, &red(), None, |_| {}).unwrap();
        assert_eq!((r.indexed, r.skipped_subagents), (1, 1), "{r:?}");
    }

    #[test]
    fn test_antigravity_tool_arguments_are_recognised() {
        let mut s = Store::in_memory();
        let mk = |ord, name: &str, input| {
            let mut m = msg(ord, Role::Assistant, "");
            m.text = None;
            m.tool_name = Some(name.into());
            m.tool_input = Some(input);
            m
        };
        s.index_session(&session("antigravity", "ag1", "/p", vec![
            mk(0, "view_file", json!({"AbsolutePath": "/p/src/a.rs", "StartLine": 1})),
            mk(1, "run_command", json!({"CommandLine": "cargo build"})),
            mk(2, "list_dir", json!({"DirectoryPath": "/p"})),
        ]), "f", &red()).unwrap();
        let rowid = s.fingerprints().unwrap()[&("antigravity".into(), "ag1".into())].0;
        let m = s.messages_of(rowid).unwrap();
        assert_eq!(m[0].tool_arg.as_deref(), Some("/p/src/a.rs"));
        assert_eq!(m[1].tool_arg.as_deref(), Some("cargo build"));
        assert_eq!(m[2].tool_arg.as_deref(), Some("/p"));
    }
}
