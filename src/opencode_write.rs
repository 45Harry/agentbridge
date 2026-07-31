//! Materializing sessions **into** OpenCode.
//!
//! OpenCode stores sessions as rows in a live SQLite database, so unlike the
//! file-based tools there is nothing to hardlink — presence means `INSERT`.
//! This is the only place agentbridge writes into another tool's real data,
//! so every operation here is gated (`DESIGN.md` §5):
//!
//! 1. The database is backed up before the first write of a run.
//! 2. Every inserted row is tagged in the unused `metadata` column, so
//!    removal can target exactly agentbridge's rows and nothing else.
//! 3. Writing is refused while OpenCode is running.
//! 4. `--dry-run` renders the statements without executing them.
//!
//! Deleting a tagged session cascades to its messages and parts through the
//! schema's existing `ON DELETE CASCADE` foreign keys.

use crate::model::{Role, Session};
use rusqlite::{params, Connection};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Written into `session.metadata`. No OpenCode-authored row uses that column
/// (verified: 0 of 147 on a real database), so its presence unambiguously
/// identifies a row agentbridge created.
pub const MARKER: &str = "agentbridge";

/// OpenCode requires a non-null `project_id` referencing `project`. `global`
/// is OpenCode's own catch-all row and is present on a stock install.
const PROJECT_ID: &str = "global";

const VERSION: &str = "1.17.15";

#[derive(Debug)]
pub enum WriteError {
    OpenCodeRunning,
    Sql(String),
    Backup(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::OpenCodeRunning => write!(
                f,
                "OpenCode is running — refusing to write to its database. Quit it and retry."
            ),
            WriteError::Sql(e) => write!(f, "sqlite: {}", e),
            WriteError::Backup(e) => write!(f, "backup failed: {}", e),
        }
    }
}

/// True when an `opencode` process is live. Writing under a running instance
/// risks racing its own writes and having it serve stale cached state.
pub fn is_opencode_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "opencode"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Gate every write behind this. It lives at the call site rather than inside
/// the write primitives so the primitives stay testable on a machine where
/// OpenCode happens to be running.
pub fn ensure_safe_to_write() -> Result<(), WriteError> {
    if is_opencode_running() {
        return Err(WriteError::OpenCodeRunning);
    }
    Ok(())
}

/// Copy the database next to itself before the first write of a run.
pub fn backup(db: &Path) -> Result<PathBuf, WriteError> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let dest = db.with_extension(format!("agentbridge-backup-{}.db", stamp));
    std::fs::copy(db, &dest).map_err(|e| WriteError::Backup(e.to_string()))?;
    Ok(dest)
}

/// Deterministic OpenCode-shaped id for a foreign session, so re-running is
/// idempotent rather than inserting a duplicate every time.
pub fn derive_id(source_provider: &str, source_id: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}", source_provider, source_id).as_bytes(),
    );
    format!("ses_ab{}", ns.simple())
}

fn slug_of(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "agentbridge-session".to_string()
    } else {
        s.chars().take(60).collect()
    }
}

/// The statements that would materialize `session` into `directory`.
/// Rendered for `--dry-run`; the executed path uses bound parameters.
pub fn plan(session: &Session, directory: &str) -> Vec<String> {
    let id = derive_id(&session.provider, &session.id);
    let mut out = vec![format!(
        "INSERT OR REPLACE INTO session (id, project_id, slug, directory, title, version, \
         time_created, time_updated, metadata) VALUES ('{}', '{}', '{}', '{}', '{}', '{}', …, \
         '{{\"{}\":true}}');",
        id,
        PROJECT_ID,
        slug_of(session.title.as_deref().unwrap_or("")),
        directory,
        session.title.as_deref().unwrap_or("(untitled)"),
        VERSION,
        MARKER
    )];
    out.push(format!(
        "-- + {} message rows and their part rows",
        session.messages.len()
    ));
    out
}

/// Insert (or refresh) `session` so it appears in OpenCode's own picker for
/// `directory`. Returns the OpenCode session id used.
pub fn write_session(
    db: &Path,
    session: &Session,
    directory: &str,
) -> Result<String, WriteError> {
    let mut conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let tx = conn.transaction().map_err(|e| WriteError::Sql(e.to_string()))?;

    let id = derive_id(&session.provider, &session.id);
    let created = session.started_at.map(|t| t.timestamp_millis()).unwrap_or(0);
    let updated = session
        .last_event_at
        .or(session.started_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or(created);
    let title = session
        .title
        .clone()
        .unwrap_or_else(|| format!("{} session {}", session.provider, &session.id));
    let metadata = json!({
        MARKER: true,
        "source_provider": session.provider,
        "source_id": session.id,
    })
    .to_string();

    // Replace wholesale so a re-run refreshes rather than duplicating. The
    // cascade clears the old messages/parts first.
    tx.execute(
        "DELETE FROM session WHERE id = ?1 AND metadata LIKE ?2",
        params![id, format!("%{}%", MARKER)],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;

    tx.execute(
        "INSERT INTO session (id, project_id, slug, directory, title, version, \
         time_created, time_updated, metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            id,
            PROJECT_ID,
            slug_of(&title),
            directory,
            title,
            VERSION,
            created,
            updated,
            metadata
        ],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;

    for (i, m) in session.messages.iter().enumerate() {
        let msg_id = format!("{}_m{:06}", id, i);
        let role = match m.role {
            Role::User => "user",
            Role::System => "system",
            _ => "assistant",
        };
        let ts = m.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created);

        tx.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                msg_id,
                id,
                ts,
                ts,
                json!({ "role": role, "time": { "created": ts } }).to_string()
            ],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;

        let part = if let Some(tool) = &m.tool_name {
            json!({ "type": "tool", "tool": tool, "state": m.tool_input.clone() })
        } else {
            json!({ "type": "text", "text": m.text.clone().unwrap_or_default() })
        };

        tx.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                format!("{}_p0", msg_id),
                msg_id,
                id,
                ts,
                ts,
                part.to_string()
            ],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    }

    tx.commit().map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(id)
}

/// Remove every session agentbridge inserted — matched by the marker, so rows
/// OpenCode authored are never touched. Messages and parts go with them via
/// the schema's cascade.
pub fn remove_all(db: &Path) -> Result<usize, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    conn.execute(
        "DELETE FROM session WHERE metadata LIKE ?1",
        params![format!("%{}%", MARKER)],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))
}

/// How many agentbridge-inserted sessions are present.
pub fn count_written(db: &Path) -> usize {
    Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM session WHERE metadata LIKE ?1",
                params![format!("%{}%", MARKER)],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|n| n as usize)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, TokenTotals};
    use chrono::{TimeZone, Utc};

    fn db_with_schema() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        // Mirrors the real schema's NOT NULLs and cascades.
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                sandboxes TEXT NOT NULL);
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                metadata TEXT,
                FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL,
                FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE);
            INSERT INTO project VALUES ('global','/',0,0,'[]');
            -- a session OpenCode itself authored; must never be touched
            INSERT INTO session VALUES
              ('ses_real','global',NULL,'real','/home/u/p','Real one','1.17.15',1,2,NULL);
            INSERT INTO message VALUES ('m_real','ses_real',1,1,'{"role":"user"}');
            "#,
        )
        .unwrap();
        (tmp, path)
    }

    fn a_session() -> Session {
        Session {
            id: "7a65dbea-9780-46be-b7b4-a5e5e948abbf".into(),
            provider: "claude-code".into(),
            project_id: "/home/u/p".into(),
            started_at: Utc.timestamp_millis_opt(1_778_656_792_117).single(),
            last_event_at: Utc.timestamp_millis_opt(1_778_656_799_000).single(),
            model: Some("claude-sonnet-5".into()),
            title: Some("Python programming".into()),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/tmp/x.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![
                Message {
                    session_id: "s".into(), ordinal: 0, role: Role::User,
                    timestamp: Utc.timestamp_millis_opt(1_778_656_792_117).single(),
                    text: Some("how do I sort a list".into()),
                    tool_name: None, tool_input: None, tool_result: None, parent_ordinal: None,
                },
                Message {
                    session_id: "s".into(), ordinal: 1, role: Role::Assistant,
                    timestamp: Utc.timestamp_millis_opt(1_778_656_793_000).single(),
                    text: Some("use sorted()".into()),
                    tool_name: None, tool_input: None, tool_result: None, parent_ordinal: None,
                },
            ],
            artifacts: vec![],
        }
    }

    #[test]
    fn test_written_session_is_visible_and_tagged() {
        let (_t, db) = db_with_schema();
        let id = write_session(&db, &a_session(), "/home/u/p").unwrap();

        let conn = Connection::open(&db).unwrap();
        let (title, dir, meta): (String, String, String) = conn
            .query_row(
                "SELECT title, directory, metadata FROM session WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "Python programming");
        assert_eq!(dir, "/home/u/p", "must be scoped to the requested directory");
        assert!(meta.contains(MARKER), "row must be tagged as ours");

        let msgs: i64 = conn
            .query_row("SELECT COUNT(*) FROM message WHERE session_id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 2);
        let parts: i64 = conn
            .query_row("SELECT COUNT(*) FROM part WHERE session_id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(parts, 2);
    }

    /// Re-running must refresh, never accumulate duplicates.
    #[test]
    fn test_write_is_idempotent() {
        let (_t, db) = db_with_schema();
        write_session(&db, &a_session(), "/home/u/p").unwrap();
        write_session(&db, &a_session(), "/home/u/p").unwrap();

        assert_eq!(count_written(&db), 1, "second write must replace, not duplicate");
        let conn = Connection::open(&db).unwrap();
        let msgs: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 3, "1 real + 2 ours, not 5");
    }

    /// The whole safety case: removal touches only agentbridge's rows.
    #[test]
    fn test_remove_all_leaves_opencodes_own_sessions_alone() {
        let (_t, db) = db_with_schema();
        write_session(&db, &a_session(), "/home/u/p").unwrap();
        assert_eq!(count_written(&db), 1);

        let removed = remove_all(&db).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count_written(&db), 0);

        let conn = Connection::open(&db).unwrap();
        let real: i64 = conn
            .query_row("SELECT COUNT(*) FROM session WHERE id='ses_real'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(real, 1, "OpenCode's own session must survive");
        let real_msg: i64 = conn
            .query_row("SELECT COUNT(*) FROM message WHERE id='m_real'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(real_msg, 1, "and so must its messages");
    }

    #[test]
    fn test_derive_id_is_deterministic_and_shaped_like_opencodes() {
        let a = derive_id("claude-code", "abc");
        assert_eq!(a, derive_id("claude-code", "abc"));
        assert_ne!(a, derive_id("codex-cli", "abc"));
        assert!(a.starts_with("ses_ab"));
    }

    #[test]
    fn test_backup_copies_the_database() {
        let (_t, db) = db_with_schema();
        let b = backup(&db).unwrap();
        assert!(b.exists());
        assert_eq!(
            std::fs::metadata(&b).unwrap().len(),
            std::fs::metadata(&db).unwrap().len()
        );
    }

    #[test]
    fn test_plan_renders_without_touching_the_database() {
        let (_t, db) = db_with_schema();
        let before = count_written(&db);
        let sql = plan(&a_session(), "/home/u/p");
        assert!(sql[0].contains("INSERT"));
        assert!(sql[0].contains(MARKER));
        assert_eq!(count_written(&db), before, "dry run must not write");
    }
}
