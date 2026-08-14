//! Materializing sessions **into** Codex CLI's index (`state_5.sqlite`).
//!
//! The resume picker in codex 0.146 lists rollout **files** from
//! `~/.codex/sessions/` whose `session_meta.cwd` matches the launch
//! directory (verified in source: `thread-store/src/local/list_threads.rs`,
//! `rollout/src/recorder.rs`); `threads` is a side effect read-repaired from
//! those files. The files are materialized by `convert.rs` (`convert_multi`,
//! one rollout per directory); what lives here is the `threads` rows that
//! follow those files, written with the same gates OpenCode's database gets:
//!
//! 1. The database is backed up before the first write of a run that
//!    actually inserts something new — idempotent refreshes of rows
//!    agentbridge itself wrote take no backup.
//! 2. Every inserted row is tagged in the `thread_source` column (real rows
//!    use "user"), so removal can target exactly agentbridge's rows.
//! 3. Writing is refused while Codex is running.
//! 4. `--dry-run` renders the statement without executing it.
//!
//! Rows are keyed on `id` — the deterministic UUID v5 of the source session
//! and directory (`session_uuid_for_dir`) — so a row that already exists (a
//! genuine Codex session, or a previous run of ours) is never duplicated.

use crate::convert::CODEX_CLI_VERSION;
use crate::model::{Role, Session};
use rusqlite::{params, Connection};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Written into `threads.thread_source`. Real Codex rows use "user"
/// (verified on a real database); our value unambiguously identifies rows
/// agentbridge created.
pub const MARKER: &str = "agentbridge";

/// Mirrors what Codex writes for its own sessions on the operator's machine.
const MODEL_PROVIDER: &str = "openai";
const APPROVAL_MODE: &str = "on-request";

/// The picker truncates long titles; keep preview strings modest.
const PREVIEW_MAX: usize = 120;

#[derive(Debug)]
pub enum WriteError {
    CodexRunning,
    Sql(String),
    Backup(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::CodexRunning => write!(
                f,
                "Codex is running — refusing to write to its index. Quit it and retry."
            ),
            WriteError::Sql(e) => write!(f, "sqlite: {}", e),
            WriteError::Backup(e) => write!(f, "backup failed: {}", e),
        }
    }
}

/// True when a `codex` process is live. Writing under a running instance
/// risks racing its own writes and having it serve stale cached state.
pub fn is_codex_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "codex"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Gate every write behind this. It lives at the call site rather than inside
/// the write primitives so the primitives stay testable on a machine where
/// Codex happens to be running.
pub fn ensure_safe_to_write() -> Result<(), WriteError> {
    if is_codex_running() {
        return Err(WriteError::CodexRunning);
    }
    Ok(())
}

/// The live index. `None` when Codex has never run on this machine.
pub fn state_db() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(home).join(".codex").join("state_5.sqlite");
    p.exists().then_some(p)
}

/// Copy the database next to itself before the first write of a run.
pub fn backup(db: &Path) -> Result<PathBuf, WriteError> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let dest = db.with_extension(format!("agentbridge-backup-{}.db", stamp));
    std::fs::copy(db, &dest).map_err(|e| WriteError::Backup(e.to_string()))?;
    Ok(dest)
}

/// Whether agentbridge already owns a `threads` row for this id. A re-sync
/// that would only refresh our own rows (idempotent upserts of data
/// agentbridge wrote) does not need a database backup; an INSERT of a new
/// row does. Used by the sync loop to decide whether to take the one-per-run
/// backup.
pub fn thread_row_exists(db: &Path, sid: &str) -> Result<bool, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM threads WHERE id = ?1 AND thread_source = ?2 LIMIT 1",
            params![sid, MARKER],
            |_| Ok(()),
        )
        .is_ok())
}

/// The `sandbox_policy` value Codex writes for a session rooted at `cwd`:
/// managed sandbox, root read-only, the working tree writable, and the
/// usual temp/codex exceptions.
fn sandbox_policy(cwd: &str) -> String {
    json!({
        "type": "managed",
        "file_system": {
            "type": "restricted",
            "entries": [
                { "path": { "type": "special", "value": { "kind": "root" } }, "access": "read" },
                { "path": { "type": "path", "path": cwd }, "access": "write" },
                { "path": { "type": "special", "value": { "kind": "slash_tmp" } }, "access": "write" },
                { "path": { "type": "special", "value": { "kind": "tmpdir" } }, "access": "write" },
                { "path": { "type": "path", "path": format!("{cwd}/.git") }, "access": "read", "missing_path_behavior": "skip" },
                { "path": { "type": "path", "path": format!("{cwd}/.agents") }, "access": "read", "missing_path_behavior": "skip" }
            ]
        },
        "network": "restricted"
    })
    .to_string()
}

fn first_user_message(session: &Session) -> String {
    session
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.text.clone())
        .unwrap_or_default()
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Anchor timestamp shared with the Codex converter, so the row agrees with
/// the rollout filename and records.
fn anchor(session: &Session) -> chrono::DateTime<chrono::Utc> {
    session
        .started_at
        .or(session.last_event_at)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
}

/// Directory-stable row id for a session surfaced in `directory`. The picker
/// keys visibility on `cwd`, so a session carries one row per directory it
/// should appear in (`/resume` filters `threads.cwd` with an exact match,
/// `threads.cwd IN (...)`, verified in codex 0.146 source). Resuming reads
/// `rollout_path` from the row, so any id works.
pub fn session_uuid_for_dir(session_id: &str, dir: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("agentbridge:{}:{}", session_id, dir).as_bytes(),
    );
    ns.simple().to_string()
}

/// Insert one `threads` row per directory in `dirs` for a materialized
/// rollout, so the session shows up in `codex /resume` from any of them.
///
/// The picker lists threads filtered by `cwd` (exact match against the
/// launch directory), so one row alone only helps the directory it names.
/// agentbridge writes rows for the sync project and `$HOME`; any other
/// launch directory still sees them via the picker's `All` filter.
///
/// Safety (all preserved from before):
/// - a row Codex itself authored for the same rollout *and directory* is
///   never duplicated;
/// - rows agentbridge owns for this rollout are rewritten (delete-then-
///   reinsert), so re-syncing with a different directory set cannot leave
///   stale duplicates behind;
/// - the `UPSERT` update branch is scoped to `thread_source = 'agentbridge'`,
///   so a genuine Codex row can never be modified through a coincidental
///   id collision.
pub fn ensure_thread_rows(
    db: &Path,
    session: &Session,
    rollout_path: &Path,
    dirs: &[String],
) -> Result<ThreadRowReport, WriteError> {
    let mut report = ThreadRowReport::default();
    let mut conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let tx = conn.transaction().map_err(|e| WriteError::Sql(e.to_string()))?;

    // Rows agentbridge owns for this rollout in directories that are no
    // longer in the requested set (e.g. written before the per-directory
    // scheme, or after a directory set change) must not survive; rows for
    // current directories are refreshed by the upsert below instead.
    if !dirs.is_empty() {
        let placeholders = std::iter::repeat_n("?", dirs.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM threads WHERE rollout_path = ?1 AND thread_source = ?2 \
             AND cwd NOT IN ({placeholders})"
        );
        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| WriteError::Sql(e.to_string()))?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(rollout_path.to_string_lossy().to_string()), Box::new(MARKER)];
        for d in dirs {
            params_vec.push(Box::new(d.clone()));
        }
        stmt.execute(rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())))
            .map_err(|e| WriteError::Sql(e.to_string()))?;
    }

    let anchor = anchor(session);
    let secs = anchor.timestamp();
    let ms = anchor.timestamp_millis();
    let first_user = first_user_message(session);
    // An explicit title (real rename, or one recovered from another tool via
    // the title overlay — see sync.rs's fold_overlay) always wins: it was
    // deliberately set. Only a session that has never been named falls back
    // to a message preview, matching Codex's own default before any rename.
    let title = match &session.title {
        Some(t) if !t.is_empty() => t.clone(),
        _ if !first_user.is_empty() => clip(&first_user, PREVIEW_MAX),
        _ => "New conversation".to_string(),
    };
    report.title = title.clone();
    let has_user = if session.messages.iter().any(|m| m.role == Role::User) { 1 } else { 0 };

    for cwd in dirs {
        if cwd.is_empty() || !std::path::Path::new(cwd).is_absolute() {
            continue;
        }

        // Invariant 2: Codex's own row already covers this rollout *in this
        // directory* — nothing to do here (other directories are still ours
        // to surface).
        let native = tx
            .query_row(
                "SELECT 1 FROM threads WHERE rollout_path = ?1 AND cwd = ?2 AND thread_source <> ?3 LIMIT 1",
                params![rollout_path.to_string_lossy(), cwd, MARKER],
                |_| Ok(()),
            )
            .is_ok();
        if native {
            continue;
        }

        let sid = session_uuid_for_dir(&session.id, cwd);
        // Rows we own for this rollout and directory under any other id
        // (e.g. written by pre-0.3.2 syncs keyed on the plain session id)
        // must not survive next to the per-directory row.
        tx.execute(
            "DELETE FROM threads WHERE rollout_path = ?1 AND thread_source = ?2 AND cwd = ?3 AND id <> ?4",
            params![rollout_path.to_string_lossy(), MARKER, cwd, sid],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
        // A row we own under this id means the UPSERT below is an update
        // (refreshing an existing row), not an insert — changes() alone
        // cannot tell them apart (both report 1).
        let refresh = tx
            .query_row(
                "SELECT 1 FROM threads WHERE id = ?1 AND thread_source = ?2 LIMIT 1",
                params![sid, MARKER],
                |_| Ok(()),
            )
            .is_ok();

        let changed = tx
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, source, model_provider,
                    cwd, title, sandbox_policy, approval_mode, cli_version,
                    first_user_message, preview, created_at_ms, updated_at_ms,
                    recency_at, recency_at_ms, thread_source, has_user_event
                 ) VALUES (
                    ?1, ?2, ?3, ?3, 'cli', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?12, ?12, ?13, ?14
                 )
                 ON CONFLICT(id) DO UPDATE SET
                    cwd = excluded.cwd,
                    rollout_path = excluded.rollout_path,
                    title = excluded.title,
                    preview = excluded.preview,
                    first_user_message = excluded.first_user_message,
                    updated_at = excluded.updated_at,
                    updated_at_ms = excluded.updated_at_ms,
                    recency_at = excluded.recency_at,
                    recency_at_ms = excluded.recency_at_ms
                 WHERE threads.thread_source = ?13",
                params![
                    sid,
                    rollout_path.to_string_lossy(),
                    secs,
                    MODEL_PROVIDER,
                    cwd,
                    title,
                    sandbox_policy(cwd),
                    APPROVAL_MODE,
                    CODEX_CLI_VERSION,
                    clip(&first_user, 4000),
                    clip(&first_user, PREVIEW_MAX),
                    ms,
                    MARKER,
                    has_user,
                ],
            )
            .map_err(|e| WriteError::Sql(e.to_string()))?;

        match (changed, refresh) {
            (0, _) => {}
            (_, true) => report.updated += 1,
            (_, false) => report.inserted += 1,
        }
    }

    tx.commit().map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(report)
}

/// Remove agentbridge-owned `threads` rows for `session` in `dirs`. Used when
/// a directory's variant is skipped because the tool already lists the
/// session there natively — the row would otherwise point at a file that no
/// longer matches the directory it claims.
pub fn remove_thread_rows_for(
    db: &Path,
    session: &Session,
    dirs: &[String],
) -> Result<usize, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let mut removed = 0usize;
    for dir in dirs {
        let sid = session_uuid_for_dir(&session.id, dir);
        removed += conn
            .execute(
                "DELETE FROM threads WHERE id = ?1 AND thread_source = ?2",
                params![sid, MARKER],
            )
            .map_err(|e| WriteError::Sql(e.to_string()))?;
    }
    Ok(removed)
}

/// Outcome of an `ensure_thread_rows` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadRowReport {
    /// Rows created (each is one directory the session is now visible from).
    pub inserted: usize,
    /// Rows refreshed (values rewritten, same id — not new visibility).
    pub updated: usize,
    /// The title actually persisted into every row this call touched (same
    /// value for all of them — computed once, before the per-directory
    /// loop). Callers recording "what we wrote" (sync.rs's `LinkRecord`)
    /// must use this, not `session.title`: this always has a value (falling
    /// back to a message preview or "New conversation" for an untitled
    /// session), and round-tripping that fallback back through
    /// `load_materialized` makes it indistinguishable from a real title —
    /// recording a bare `session.title` here would make every untitled
    /// session look renamed on the next `pull` (regression caught
    /// 2026-08-14 live-testing outside the sandbox, alongside the identical
    /// OpenCode gap — see `opencode_write::RowWritten::title`).
    pub title: String,
}

/// Remove every row agentbridge inserted — matched by the marker, so
/// Codex's own sessions are never touched. Returns the number removed.
pub fn remove_all(db: &Path) -> Result<usize, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let removed = conn
        .execute("DELETE FROM threads WHERE thread_source = ?1", [MARKER])
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(removed)
}

/// How many rows agentbridge owns (for `status`).
pub fn count_written(db: &Path) -> usize {
    Connection::open(db)
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM threads WHERE thread_source = ?1",
                [MARKER],
                |r| r.get(0),
            )
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::ClaudeCodeConverter;
    use crate::model::{Message, Role, Session, TokenTotals};
    use chrono::{TimeZone, Utc};
    use rusqlite::Connection;

    /// The exact schema Codex 0.146.0 created, captured from a real
    /// `state_5.sqlite` (CONNECTORS.md §2). Tests pin to it so a vendor
    /// migration that breaks the writer fails here first.
    const REAL_SCHEMA: &str = "CREATE TABLE threads (
        id TEXT PRIMARY KEY,
        rollout_path TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        source TEXT NOT NULL,
        model_provider TEXT NOT NULL,
        cwd TEXT NOT NULL,
        title TEXT NOT NULL,
        sandbox_policy TEXT NOT NULL,
        approval_mode TEXT NOT NULL,
        tokens_used INTEGER NOT NULL DEFAULT 0,
        has_user_event INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        archived_at INTEGER,
        git_sha TEXT,
        git_branch TEXT,
        git_origin_url TEXT
    , cli_version TEXT NOT NULL DEFAULT '', first_user_message TEXT NOT NULL DEFAULT '', agent_nickname TEXT, agent_role TEXT, memory_mode TEXT NOT NULL DEFAULT 'enabled', model TEXT, reasoning_effort TEXT, agent_path TEXT, created_at_ms INTEGER, updated_at_ms INTEGER, thread_source TEXT, preview TEXT NOT NULL DEFAULT '', recency_at INTEGER NOT NULL DEFAULT 0, recency_at_ms INTEGER NOT NULL DEFAULT 0, history_mode TEXT NOT NULL DEFAULT 'legacy', name TEXT, is_pinned INTEGER NOT NULL DEFAULT 0)";

    fn db_with_schema() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(REAL_SCHEMA).unwrap();
        (tmp, db)
    }

    fn a_session() -> Session {
        Session {
            id: "codex-session-1".to_string(),
            provider: "codex-cli".to_string(),
            project_id: "/home/harry/work".to_string(),
            started_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 8, 0, 54).unwrap()),
            last_event_at: Some(Utc.with_ymd_and_hms(2026, 7, 30, 8, 1, 0).unwrap()),
            model: None,
            title: None,
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/home/harry/.codex/sessions/2026/07/30/original.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![
                Message {
                    session_id: "codex-session-1".into(),
                    ordinal: 0,
                    role: Role::User,
                    timestamp: Some(Utc.with_ymd_and_hms(2026, 7, 30, 8, 0, 54).unwrap()),
                    text: Some("build a python program".to_string()),
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                },
                Message {
                    session_id: "codex-session-1".into(),
                    ordinal: 1,
                    role: Role::Assistant,
                    timestamp: Some(Utc.with_ymd_and_hms(2026, 7, 30, 8, 1, 0).unwrap()),
                    text: Some("ok".to_string()),
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                },
            ],
            artifacts: vec![],
        }
    }

    #[test]
    fn test_insert_creates_a_picker_visible_row() {
        let (_tmp, db) = db_with_schema();
        let report = ensure_thread_rows(
            &db,
            &a_session(),
            Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl"),
            &["/home/harry/work".to_string()],
        )
        .unwrap();
        assert_eq!(report.inserted, 1);
        let conn = Connection::open(&db).unwrap();
        let (title, preview, src, cwd, path): (String, String, String, String, String) = conn
            .query_row("SELECT title, preview, thread_source, cwd, rollout_path FROM threads", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap();
        assert_eq!(title, "build a python program");
        assert_eq!(preview, "build a python program");
        assert_eq!(src, MARKER);
        assert_eq!(cwd, "/home/harry/work");
        assert_eq!(path, "/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl");
        assert_eq!(count_written(&db), 1);
        // Deterministic, directory-stable id.
        assert_eq!(
            session_uuid_for_dir(&a_session().id, "/home/harry/work"),
            session_uuid_for_dir(&a_session().id, "/home/harry/work")
        );
    }

    /// Regression: an explicit title (a real rename, or one recovered from
    /// another tool via sync.rs's title overlay) must win over the
    /// first-user-message preview `threads.title` otherwise falls back to —
    /// previously the preview always won whenever the session had any user
    /// message at all, silently discarding every rename.
    #[test]
    fn test_explicit_title_beats_first_message_preview() {
        let (_tmp, db) = db_with_schema();
        let mut s = a_session();
        s.title = Some("renamed-session".to_string());
        let report = ensure_thread_rows(
            &db,
            &s,
            Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl"),
            &["/home/harry/work".to_string()],
        )
        .unwrap();
        assert_eq!(report.inserted, 1);
        let conn = Connection::open(&db).unwrap();
        let title: String = conn
            .query_row("SELECT title FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "renamed-session");
    }

    /// Two directories means two rows: the session is visible from both.
    #[test]
    fn test_one_row_per_directory() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let rollout = Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl");
        let dirs = vec!["/home/harry/work".to_string(), "/home/harry".to_string()];
        let report = ensure_thread_rows(&db, &s, rollout, &dirs).unwrap();
        assert_eq!(report.inserted, 2);
        assert_eq!(count_written(&db), 2);

        let conn = Connection::open(&db).unwrap();
        let cwds: Vec<String> = conn
            .prepare("SELECT cwd FROM threads ORDER BY cwd")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(cwds, vec!["/home/harry", "/home/harry/work"]);
        // Distinct ids, both deterministic.
        let (id_a, id_b): (String, String) = conn
            .query_row(
                "SELECT (SELECT id FROM threads WHERE cwd='/home/harry/work'), \
                        (SELECT id FROM threads WHERE cwd='/home/harry')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_ne!(id_a, id_b);
        assert_eq!(id_a, session_uuid_for_dir(&s.id, "/home/harry/work"));
        assert_eq!(id_b, session_uuid_for_dir(&s.id, "/home/harry"));
    }

    /// Re-syncing for a different directory set must replace the old rows,
    /// never accumulate them.
    #[test]
    fn test_reshuffle_directories_replaces_old_rows() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let rollout = Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl");

        ensure_thread_rows(
            &db,
            &s,
            rollout,
            &["/home/harry/work".to_string(), "/home/harry".to_string()],
        )
        .unwrap();
        assert_eq!(count_written(&db), 2);

        let report = ensure_thread_rows(&db, &s, rollout, &["/home/harry".to_string()]).unwrap();
        assert_eq!(report.inserted, 0);
        assert_eq!(report.updated, 1, "the home row already exists, just refreshed");
        assert_eq!(count_written(&db), 1, "work row must be gone");
        let conn = Connection::open(&db).unwrap();
        let cwd: String = conn
            .query_row("SELECT cwd FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cwd, "/home/harry");
    }

    #[test]
    fn test_insert_is_idempotent_and_never_overwrites_genuine_rows() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        // A genuine Codex row occupying the same id as our directory row must
        // survive untouched (the UPSERT update branch is scoped to our
        // marker).
        let id = session_uuid_for_dir(&s.id, "/home/harry/work");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, thread_source)
             VALUES (?1, '/genuine.jsonl', 1, 1, 'cli', 'openai', '/home/harry/work', 'genuine', '{}', 'on-request', 'user')",
            [&id],
        )
        .unwrap();
        drop(conn);

        let report = ensure_thread_rows(
            &db,
            &s,
            Path::new("/ours.jsonl"),
            &["/home/harry/work".to_string()],
        )
        .unwrap();
        assert_eq!(report.inserted, 0, "genuine row occupies that id");
        let conn = Connection::open(&db).unwrap();
        let (path, src, title): (String, String, String) = conn
            .query_row("SELECT rollout_path, thread_source, title FROM threads WHERE id = ?1", [&id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(path, "/genuine.jsonl");
        assert_eq!(src, "user");
        assert_eq!(title, "genuine");
        assert_eq!(count_written(&db), 0);
    }

    #[test]
    fn test_second_insert_for_same_session_creates_nothing() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let p = Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl");
        let dirs = vec!["/home/harry/work".to_string()];
        let first = ensure_thread_rows(&db, &s, p, &dirs).unwrap();
        assert_eq!(first.inserted, 1);
        let second = ensure_thread_rows(&db, &s, p, &dirs).unwrap();
        assert_eq!(second.inserted, 0);
        assert_eq!(second.updated, 1);
        assert_eq!(count_written(&db), 1);
    }

    /// A row Codex itself authored for the same rollout *and directory* must
    /// never be duplicated: that directory already shows the session. Other
    /// directories are still ours to surface.
    #[test]
    fn test_native_row_is_never_duplicated_in_its_own_directory() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let rollout = Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-native.jsonl");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, thread_source)
             VALUES ('native-id', ?1, 1, 1, 'cli', 'openai', '/home/harry', 'agentbridge-test', '{}', 'on-request', 'user')",
            [&rollout.to_string_lossy().to_string()],
        )
        .unwrap();
        drop(conn);

        let dirs = vec!["/home/harry".to_string(), "/home/harry/work".to_string()];
        let report = ensure_thread_rows(&db, &s, rollout, &dirs).unwrap();
        // Home is covered natively; only the work directory gets our row.
        assert_eq!(report.inserted, 1);
        assert_eq!(count_written(&db), 1);
        let conn = Connection::open(&db).unwrap();
        let cwd: String = conn
            .query_row("SELECT cwd FROM threads WHERE thread_source=?1", [MARKER], |r| r.get(0))
            .unwrap();
        assert_eq!(cwd, "/home/harry/work");
    }

    #[test]
    fn test_remove_all_leaves_genuine_sessions_alone() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let genuine = ClaudeCodeConverter::session_uuid("genuine-session");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, thread_source)
             VALUES (?1, '/genuine.jsonl', 1, 1, 'cli', 'openai', '/home/harry', 'genuine', '{}', 'on-request', 'user')",
            [&genuine],
        )
        .unwrap();
        drop(conn);

        let dirs = vec!["/home/harry".to_string(), "/home/harry/work".to_string()];
        ensure_thread_rows(&db, &s, Path::new("/ours.jsonl"), &dirs).unwrap();
        assert_eq!(remove_all(&db).unwrap(), 2);
        let conn = Connection::open(&db).unwrap();
        let remaining: usize = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn test_backup_copies_the_database() {
        let (_tmp, db) = db_with_schema();
        let b = backup(&db).unwrap();
        assert!(b.exists());
        let conn = Connection::open(&b).unwrap();
        let n: usize = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_thread_row_exists_sees_only_our_rows() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let dirs = vec!["/home/harry/work".to_string()];
        ensure_thread_rows(&db, &s, Path::new("/ours.jsonl"), &dirs).unwrap();
        let sid = session_uuid_for_dir(&s.id, "/home/harry/work");
        assert!(thread_row_exists(&db, &sid).unwrap());
        assert!(!thread_row_exists(&db, "missing-id").unwrap());
    }

    #[test]
    fn test_sandbox_policy_carries_the_cwd() {
        let p = sandbox_policy("/home/harry/work");
        assert!(p.contains("/home/harry/work"));
        assert!(p.contains("\"network\":\"restricted\""));
    }
}
