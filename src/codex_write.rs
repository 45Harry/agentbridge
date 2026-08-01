//! Materializing sessions **into** Codex CLI's index (`state_5.sqlite`).
//!
//! Codex lists sessions in `/resume` from the `threads` table, not by
//! scanning `~/.codex/sessions/` (CONNECTORS.md §2). Its disk backfill is a
//! one-time migration — `backfill_state` = 'complete' on the operator's
//! machine, verified 2026-08-01 — so a rollout dropped into `sessions/` is
//! never discovered. Picker presence therefore means `INSERT` into `threads`,
//! the same treatment OpenCode's database gets (`opencode_write.rs`), with
//! the same gates:
//!
//! 1. The database is backed up before the first write of a run.
//! 2. Every inserted row is tagged in the `thread_source` column (real rows
//!    use "user"), so removal can target exactly agentbridge's rows.
//! 3. Writing is refused while Codex is running.
//! 4. `--dry-run` renders the statement without executing it.
//!
//! Inserts are keyed on `id` — the deterministic UUID v5 of the source
//! session, shared with the rollout filename — so a row that already exists
//! (a genuine Codex session, or a previous run of ours) is never touched.

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

/// Insert a `threads` row for a materialized rollout, so it shows up in
/// `codex /resume`. Returns `Inserted(id)` when the row was created,
/// `Updated(id)` when a row agentbridge owns was re-homed to a new
/// directory, and `Unchanged` when the row already exists as-is.
///
/// The picker lists threads filtered by `cwd` (`ResumeCwdMode::current`),
/// so a session surfaced for a directory only appears there when its row
/// carries that cwd. Re-syncing a session for another directory updates the
/// row (an `UPSERT` scoped to `thread_source = 'agentbridge'`): genuine
/// Codex rows are never touched by either branch.
pub fn ensure_thread_row(
    db: &Path,
    session: &Session,
    rollout_path: &Path,
    cwd: &str,
) -> Result<ThreadRowResult, WriteError> {
    let sid = crate::convert::ClaudeCodeConverter::session_uuid(&session.id);
    let anchor = anchor(session);
    let secs = anchor.timestamp();
    let ms = anchor.timestamp_millis();
    let first_user = first_user_message(session);
    let title = if first_user.is_empty() {
        session.title.clone().unwrap_or_else(|| "New conversation".to_string())
    } else {
        clip(&first_user, PREVIEW_MAX)
    };

    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;

    // Invariant 2: a row Codex itself authored already points at this
    // rollout — the session is natively visible, never duplicate it.
    let native = conn
        .query_row(
            "SELECT 1 FROM threads WHERE rollout_path = ?1 AND thread_source <> ?2 LIMIT 1",
            params![rollout_path.to_string_lossy(), MARKER],
            |_| Ok(()),
        )
        .is_ok();
    if native {
        return Ok(ThreadRowResult::Unchanged);
    }

    // A row we own under this id means the UPSERT below is an update
    // (re-homing to a new directory), not an insert — changes() alone
    // cannot tell them apart (both report 1).
    let rehome = conn
        .query_row(
            "SELECT 1 FROM threads WHERE id = ?1 AND thread_source = ?2 LIMIT 1",
            params![sid, MARKER],
            |_| Ok(()),
        )
        .is_ok();

    let changed = conn
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
                if session.messages.iter().any(|m| m.role == Role::User) { 1 } else { 0 },
            ],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;

    Ok(match changed {
        0 => ThreadRowResult::Unchanged,
        _ if rehome => ThreadRowResult::Updated(sid),
        _ => ThreadRowResult::Inserted(sid),
    })
}

/// Outcome of an `ensure_thread_row` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadRowResult {
    /// The row did not exist and was created.
    Inserted(String),
    /// A row agentbridge owns was re-homed (cwd/rollout changed).
    Updated(String),
    /// The row already exists unchanged (or belongs to Codex — untouched).
    Unchanged,
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
        let id = match ensure_thread_row(
            &db,
            &a_session(),
            Path::new("/home/harry/.codex/sessions/2026/07/30/rollout-x.jsonl"),
            "/home/harry/work",
        )
        .unwrap()
        {
            ThreadRowResult::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };
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
        // Same deterministic id as the rollout filename stem.
        let expected = ClaudeCodeConverter::session_uuid("codex-session-1");
        assert_eq!(id, expected);
    }

    #[test]
    fn test_insert_is_idempotent_and_never_overwrites_genuine_rows() {
        let (_tmp, db) = db_with_schema();
        let s = a_session();
        let id = ClaudeCodeConverter::session_uuid(&s.id);
        // A genuine Codex row occupying the same id must survive untouched.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, thread_source)
             VALUES (?1, '/genuine.jsonl', 1, 1, 'cli', 'openai', '/home/harry', 'genuine', '{}', 'on-request', 'user')",
            [&id],
        )
        .unwrap();
        drop(conn);

        assert_eq!(
            ensure_thread_row(&db, &s, Path::new("/ours.jsonl"), "/home/harry/work").unwrap(),
            ThreadRowResult::Unchanged
        );
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
        assert!(matches!(
            ensure_thread_row(&db, &s, p, "/home/harry/work").unwrap(),
            ThreadRowResult::Inserted(_)
        ));
        assert!(matches!(
            ensure_thread_row(&db, &s, p, "/home/harry/work").unwrap(),
            ThreadRowResult::Updated(_) | ThreadRowResult::Unchanged
        ));
        assert_eq!(count_written(&db), 1);
    }

    /// A row Codex itself authored for the same rollout must never be
    /// duplicated: the session is already natively visible.
    #[test]
    fn test_native_row_for_same_rollout_is_never_duplicated() {
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

        assert_eq!(
            ensure_thread_row(&db, &s, rollout, "/home/harry/work").unwrap(),
            ThreadRowResult::Unchanged
        );
        assert_eq!(count_written(&db), 0);
        let conn = Connection::open(&db).unwrap();
        let n: usize = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
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

        ensure_thread_row(&db, &s, Path::new("/ours.jsonl"), "/home/harry/work").unwrap();
        assert_eq!(remove_all(&db).unwrap(), 1);        let conn = Connection::open(&db).unwrap();
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
    fn test_sandbox_policy_carries_the_cwd() {
        let p = sandbox_policy("/home/harry/work");
        assert!(p.contains("/home/harry/work"));
        assert!(p.contains("\"network\":\"restricted\""));
    }
}
