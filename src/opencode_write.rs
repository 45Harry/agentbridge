//! Materializing sessions **into** OpenCode.
//!
//! OpenCode stores sessions as rows in a live SQLite database, so unlike the
//! file-based tools there is nothing to hardlink — presence means `INSERT`.
//! This is the only place agentbridge writes into another tool's real data,
//! so every operation here is gated (`DESIGN.md` §5):
//!
//! 1. The database is backed up before the first write of a run that
//!    actually inserts something new — idempotent refreshes of rows
//!    agentbridge itself wrote take no backup.
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
/// is OpenCode's own catch-all row and is present on a stock install; any
/// other directory that is a known project worktree uses that project's id.
const PROJECT_ID: &str = "global";

/// Resolve the `project` id OpenCode's picker filters on for `directory`:
/// the worktree's project row, or the catch-all `global` project (which is
/// what OpenCode itself uses for sessions outside any worktree).
fn resolve_project_id(db: &Connection, directory: &str) -> Result<String, WriteError> {
    match db.query_row(
        "SELECT id FROM project WHERE worktree = ?1 LIMIT 1",
        params![directory],
        |r| r.get::<_, String>(0),
    ) {
        Ok(id) => Ok(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(PROJECT_ID.to_string()),
        Err(e) => Err(WriteError::Sql(e.to_string())),
    }
}

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

/// Deterministic OpenCode-shaped id for a foreign session **in one project**,
/// so re-running is idempotent rather than inserting a duplicate every time.
///
/// Keyed on the project, not the directory: OpenCode's picker filters by
/// `project_id` (verified live — a session whose `directory` is `$HOME` lists
/// from any other directory that also resolves to `global`), while `id` is the
/// table's primary key. One row per project is therefore both the minimum for
/// visibility everywhere and the maximum before the picker lists the same
/// session twice.
pub fn derive_id(source_provider: &str, source_id: &str, project_id: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}:{}", source_provider, source_id, project_id).as_bytes(),
    );
    format!("ses_ab{}", ns.simple())
}

/// The pre-0.3.4 id: one row per session for the whole machine. Rows under it
/// are agentbridge's own and are replaced by the per-project scheme.
fn legacy_id(source_provider: &str, source_id: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}", source_provider, source_id).as_bytes(),
    );
    format!("ses_ab{}", ns.simple())
}

/// Whether agentbridge already owns the row for `id`. A re-sync that would
/// only refresh our own rows (idempotent delete-then-reinsert of data
/// agentbridge wrote) does not need a database backup; an INSERT of a new row
/// does. Used by the sync loop to decide whether to take the one-per-run
/// backup.
pub fn session_row_exists(db: &Path, id: &str) -> Result<bool, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM session WHERE id = ?1 AND metadata LIKE ?2 LIMIT 1",
            params![id, format!("%{}%", MARKER)],
            |_| Ok(()),
        )
        .is_ok())
}

/// True when materializing `session` into `dirs` would INSERT at least one row
/// that is not already there — the sync loop's cue to back the database up
/// first. Unreadable database ⇒ assume yes: a backup is the cheap side.
pub fn will_insert(db: &Path, session: &Session, dirs: &[String]) -> bool {
    let Ok(conn) = Connection::open(db) else {
        return true;
    };
    for dir in dirs {
        let Ok(project_id) = resolve_project_id(&conn, dir) else {
            return true;
        };
        let id = derive_id(&session.provider, &session.id, &project_id);
        let exists = conn
            .query_row(
                "SELECT 1 FROM session WHERE id = ?1 AND metadata LIKE ?2 LIMIT 1",
                params![id, format!("%{}%", MARKER)],
                |_| Ok(()),
            )
            .is_ok();
        if !exists {
            return true;
        }
    }
    false
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
    let id = derive_id(&session.provider, &session.id, PROJECT_ID);
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

/// One materialized row: which directory asked for it, which project it landed
/// in, the OpenCode id it got, and how many messages were written.
#[derive(Debug, Clone)]
pub struct RowWritten {
    pub directory: String,
    pub project_id: String,
    pub id: String,
    pub messages: usize,
    /// The title actually persisted into the row — `session.title` when set,
    /// else the fallback `write_session` derives. Callers must use this, not
    /// `session.title`, when recording what was written: round-tripping the
    /// fallback back through `load_from_db` makes it indistinguishable from
    /// a real title, so treating a bare `session.title` (often `None`) as
    /// "what we wrote" makes every untitled session look renamed on the next
    /// `pull` (regression caught 2026-08-14 live-testing outside the sandbox).
    pub title: String,
}

/// Materialize `session` once per *project* that `dirs` resolve to, so it is
/// listed in OpenCode's picker from every one of them. Directories sharing a
/// project collapse to a single row — two rows in one project would list the
/// same conversation twice.
///
/// Errors are per-row: one unwritable project must not cost the others.
pub fn write_sessions(
    db: &Path,
    session: &Session,
    dirs: &[String],
) -> (Vec<RowWritten>, Vec<String>) {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut done: Vec<String> = Vec::new();

    for dir in dirs {
        let project_id = match Connection::open(db)
            .map_err(|e| WriteError::Sql(e.to_string()))
            .and_then(|c| resolve_project_id(&c, dir))
        {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!("{} ({}): {}", session.id, dir, e));
                continue;
            }
        };
        if done.contains(&project_id) {
            continue;
        }
        match write_session(db, session, dir) {
            Ok((id, messages, title)) => {
                done.push(project_id.clone());
                rows.push(RowWritten {
                    directory: dir.clone(),
                    project_id,
                    id,
                    messages,
                    title,
                });
            }
            Err(e) => errors.push(format!("{} ({}): {}", session.id, dir, e)),
        }
    }

    (rows, errors)
}

/// Insert (or refresh) `session` so it appears in OpenCode's own picker for
/// `directory`. Returns the OpenCode session id used.
pub fn write_session(
    db: &Path,
    session: &Session,
    directory: &str,
) -> Result<(String, usize, String), WriteError> {
    let mut conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let tx = conn.transaction().map_err(|e| WriteError::Sql(e.to_string()))?;

    let project_id = resolve_project_id(&tx, directory)?;
    let id = derive_id(&session.provider, &session.id, &project_id);
    let created = session.started_at.map(|t| t.timestamp_millis()).unwrap_or(0);
    let updated = session
        .last_event_at
        .or(session.started_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or(created);
    let title = session
        .title
        .clone()
        .unwrap_or_else(|| format!("{} session {}", session.provider, session.id));
    let metadata = json!({
        MARKER: true,
        "source_provider": session.provider,
        "source_id": session.id,
    })
    .to_string();

    // Replace wholesale so a re-run refreshes rather than duplicating. The
    // cascade clears the old messages/parts first. Matched on the id alone,
    // never on the directory: the id already scopes the row to one project,
    // and a project's representative directory changes between runs (sync
    // passes whichever folder you are standing in). Scoping the DELETE by
    // directory left the old row in place and the INSERT then died on
    // `UNIQUE constraint failed: session.id`.
    tx.execute(
        "DELETE FROM session WHERE id = ?1 AND metadata LIKE ?2",
        params![id, format!("%{}%", MARKER)],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;

    // Migration: the pre-0.3.4 row was keyed on the session alone, so it
    // occupied the id a per-project row needs and would list a stale copy
    // beside the new one. It is ours (marker-checked), so it goes.
    let legacy = legacy_id(&session.provider, &session.id);
    if legacy != id {
        tx.execute(
            "DELETE FROM session WHERE id = ?1 AND metadata LIKE ?2",
            params![legacy, format!("%{}%", MARKER)],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    }

    tx.execute(
        "INSERT INTO session (id, project_id, slug, directory, title, version, \
         time_created, time_updated, metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            id,
            project_id,
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

    // OpenCode's message ids encode a time-ordered counter and its session
    // loop picks the "latest" message by comparing id strings. Foreign ids
    // must sort below anything OpenCode generates (`msg_f…`), so the
    // `msg_0` prefix is what keeps continuation working. The hash carries the
    // project too — message ids are primary keys as well, so the same session
    // in two projects must not reuse them.
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}:{}", session.provider, session.id, project_id).as_bytes(),
    );
    let id_hex: String = ns.simple().to_string();

    let mut idx: usize = 0;
    let mut prev_msg_id: Option<String> = None;
    let mut last_ts = created - 1;

    let insert_msg = |tx: &rusqlite::Transaction,
                      data: serde_json::Value,
                      ts: i64,
                      idx: usize|
     -> Result<String, WriteError> {
        let msg_id = format!("msg_0{}_m{:06}", id_hex, idx);
        tx.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5)",
            params![msg_id, id, ts, ts, data.to_string()],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
        Ok(msg_id)
    };

    let insert_part = |tx: &rusqlite::Transaction,
                       msg_id: &str,
                       data: serde_json::Value,
                       ts: i64,
                       idx: usize|
     -> Result<(), WriteError> {
        tx.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                format!("prt_0{}_m{:06}_p0", id_hex, idx),
                msg_id,
                id,
                ts,
                ts,
                data.to_string()
            ],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
        Ok(())
    };

    for m in &session.messages {
        if m.role == Role::System {
            continue;
        }
        let text = m.text.clone().unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        // Timestamps must be strictly increasing so the message stream is in
        // conversation order; tie-broken rows would be scrambled by id.
        let real_ts = m.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created);
        let ts = real_ts.max(last_ts + 1);
        last_ts = ts;

        // The model API rejects histories that start with an assistant turn;
        // insert a placeholder user turn when the first real message isn't one.
        if prev_msg_id.is_none() && m.role != Role::User {
            let msg_id = insert_msg(
                &tx,
                json!({
                    "role": "user",
                    "time": { "created": ts },
                    "agent": "build",
                    "model": {
                        "providerID": "opencode",
                        "modelID": "deepseek-v4-flash-free",
                        "variant": "max"
                    },
                    "summary": { "diffs": [] }
                }),
                ts,
                idx,
            )?;
            insert_part(
                &tx,
                &msg_id,
                json!({
                    "type": "text",
                    "text": "(agentbridge: continuing a previous conversation)"
                }),
                ts,
                idx,
            )?;
            idx += 1;
            prev_msg_id = Some(msg_id);
        }

        let msg_id = match m.role {
            Role::User => insert_msg(
                &tx,
                json!({
                    "role": "user",
                    "time": { "created": ts },
                    "agent": "build",
                    "model": {
                        "providerID": "opencode",
                        "modelID": "deepseek-v4-flash-free",
                        "variant": "max"
                    },
                    "summary": { "diffs": [] }
                }),
                ts,
                idx,
            )?,
            _ => {
                let mut data = json!({
                    "role": "assistant",
                    "mode": "build",
                    "agent": "build",
                    "variant": "max",
                    "path": { "cwd": directory, "root": directory },
                    "cost": 0,
                    "tokens": {
                        "total": 0, "input": 0, "output": 0, "reasoning": 0,
                        "cache": { "write": 0, "read": 0 }
                    },
                    "modelID": "deepseek-v4-flash-free",
                    "providerID": "opencode",
                    "time": { "created": ts, "completed": ts },
                    "finish": "end-turn"
                });
                if let Some(pid) = &prev_msg_id {
                    data.as_object_mut().unwrap().insert("parentID".to_string(), json!(pid));
                }
                insert_msg(&tx, data, ts, idx)?
            }
        };

        let part = if let Some(tool) = &m.tool_name {
            json!({ "type": "tool", "tool": tool, "state": m.tool_input.clone() })
        } else {
            json!({ "type": "text", "text": text })
        };
        insert_part(&tx, &msg_id, part, ts, idx)?;
        idx += 1;
        prev_msg_id = Some(msg_id);
    }

    tx.commit().map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok((id, idx, title))
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
        let id = write_session(&db, &a_session(), "/home/u/p").unwrap().0;

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

    /// The picker filters by project and `session.id` is the primary key, so a
    /// session that must show up in two projects needs two rows. Under the old
    /// directory-independent id the second write died on `UNIQUE constraint
    /// failed: session.id` — found live on 2026-08-03 materializing one
    /// session into a second folder.
    #[test]
    fn test_two_projects_each_get_their_own_row() {
        let (_t, db) = db_with_schema();
        Connection::open(&db)
            .unwrap()
            .execute("INSERT INTO project VALUES ('proj_a','/work/a',0,0,'[]')", [])
            .unwrap();

        let (rows, errors) =
            write_sessions(&db, &a_session(), &["/work/a".into(), "/home/u/p".into()]);

        assert!(errors.is_empty(), "both writes must succeed: {:?}", errors);
        assert_eq!(rows.len(), 2, "one row per project");
        assert_ne!(rows[0].id, rows[1].id, "ids must differ or the PK collides");
        assert_eq!(rows[0].project_id, "proj_a");
        assert_eq!(rows[1].project_id, "global", "unknown worktree ⇒ catch-all");
        assert_eq!(count_written(&db), 2);

        // Each row carries its own messages — message ids are primary keys too.
        let conn = Connection::open(&db).unwrap();
        for r in &rows {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM message WHERE session_id=?1",
                    params![r.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 2, "{} lost its messages", r.id);
        }
    }

    /// Two directories inside one project are one picker entry. Writing a row
    /// each would list the same conversation twice from either of them.
    #[test]
    fn test_directories_sharing_a_project_collapse_to_one_row() {
        let (_t, db) = db_with_schema();

        let (rows, errors) =
            write_sessions(&db, &a_session(), &["/home/u/p".into(), "/home/u".into()]);

        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(rows.len(), 1, "both resolve to `global` ⇒ a single row");
        assert_eq!(count_written(&db), 1);
    }

    /// A machine synced by an older build carries one directory-independent
    /// row per session. It occupies an id the per-project scheme needs, so the
    /// write must reclaim it rather than fail or list a stale copy beside it.
    #[test]
    fn test_legacy_directory_independent_row_is_reclaimed() {
        let (_t, db) = db_with_schema();
        let s = a_session();
        let legacy = legacy_id(&s.provider, &s.id);
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO session (id, project_id, slug, directory, title, version, \
                 time_created, time_updated, metadata) \
                 VALUES (?1,'global','old','/somewhere','Old','1.17.15',1,1,?2)",
                params![legacy, format!("{{\"{}\":true}}", MARKER)],
            )
            .unwrap();

        let (rows, errors) = write_sessions(&db, &s, &["/home/u/p".into()]);

        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].id, legacy);
        assert_eq!(count_written(&db), 1, "the legacy row must not survive");
        let gone: bool = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id=?1",
                params![legacy],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0;
        assert!(gone, "legacy row still present");
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
        let a = derive_id("claude-code", "abc", "global");
        assert_eq!(a, derive_id("claude-code", "abc", "global"));
        assert_ne!(a, derive_id("codex-cli", "abc", "global"));
        assert_ne!(
            a,
            derive_id("claude-code", "abc", "proj_a"),
            "one row per project means one id per project"
        );
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
    fn test_session_row_exists_sees_only_our_rows() {
        let (_t, db) = db_with_schema();
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, \
             time_created, time_updated, metadata) \
             VALUES ('ses_own','global','own','/p','own','1.17.15',0,0,?1)",
            params![format!("{{\"{}\":true}}", MARKER)],
        )
        .unwrap();
        assert!(session_row_exists(&db, "ses_own").unwrap());
        assert!(
            !session_row_exists(&db, "ses_real").unwrap(),
            "OpenCode's own row carries no marker"
        );
        assert!(!session_row_exists(&db, "ses_missing").unwrap());
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

    /// End-to-end: a session born in another tool (Codex) must surface in
    /// OpenCode's own picker and be continuable there.
    ///
    /// Ignored by default because it shells out to the real `opencode` CLI
    /// (requires a network model provider) and writes to a throwaway
    /// XDG_DATA_HOME sandbox, never to the live database.
    #[test]
    #[ignore = "requires the opencode CLI and a live model provider"]
    fn test_codex_session_is_visible_and_continuable_in_opencode() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("data");
        let data_dir = xdg.join("opencode");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db = data_dir.join("opencode.db");

        // Use the real database's schema (the CLI runs its own migrations and
        // will choke on a hand-rolled approximation); the sandbox copy keeps
        // the live database untouched. Auth/storage are copied too so the CLI
        // can reach the model provider.
        let live = crate::connectors::opencode::default_db_path();
        assert!(live.exists(), "no real opencode.db to copy: {}", live.display());
        fn copy_tree(src: &Path, dst: &Path) {
            if src.is_dir() {
                std::fs::create_dir_all(dst).unwrap();
                for entry in std::fs::read_dir(src).unwrap() {
                    let entry = entry.unwrap();
                    copy_tree(&entry.path(), &dst.join(entry.file_name()));
                }
            } else {
                std::fs::copy(src, dst).unwrap();
            }
        }
        for entry in std::fs::read_dir(live.parent().unwrap()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "opencode.db" || name.starts_with("opencode.db-") {
                continue;
            }
            copy_tree(&entry.path(), &data_dir.join(&name));
        }
        std::fs::copy(&live, &db).unwrap();

        // A session authored by Codex (a real Codex rollout transcript).
        let codex_session = Session {
            id: "7adbc643-e0bd-4c49-8432-6ef37c9001fd".into(),
            provider: "codex-cli".into(),
            project_id: "/home/harry/Documents/agentbridge".into(),
            started_at: Utc.timestamp_millis_opt(1_784_315_929_000).single(),
            last_event_at: Utc.timestamp_millis_opt(1_784_316_034_000).single(),
            model: Some("gpt-5.2-codex".into()),
            title: Some("Codex session".into()),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::new(),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![
                Message {
                    session_id: "s".into(), ordinal: 0, role: Role::User,
                    timestamp: Utc.timestamp_millis_opt(1_784_315_929_000).single(),
                    text: Some("Hi there".into()),
                    tool_name: None, tool_input: None, tool_result: None, parent_ordinal: None,
                },
                Message {
                    session_id: "s".into(), ordinal: 1, role: Role::Assistant,
                    timestamp: Utc.timestamp_millis_opt(1_784_315_930_000).single(),
                    text: Some("Hello! How can I help?".into()),
                    tool_name: None, tool_input: None, tool_result: None, parent_ordinal: None,
                },
            ],
            artifacts: vec![],
        };

        let id = write_session(&db, &codex_session, "/home/harry/Documents/agentbridge").unwrap().0;

        // 1. It must appear in OpenCode's own picker.
        let bin = std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
        eprintln!("[e2e] session id: {}", id);
        let list = std::process::Command::new(&bin)
            .arg("session")
            .arg("list")
            .current_dir("/home/harry/Documents/agentbridge")
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .expect("opencode session list");
        assert!(
            list.status.success(),
            "session list failed: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        let out = String::from_utf8_lossy(&list.stdout).to_string();
        assert!(
            out.contains(&id),
            "session {} not listed; got: {}",
            id,
            out
        );

        // 2. It must be continuable: the model must answer the new prompt
        //    with the marker and show it remembers the earlier turn.
        let prompt = "Reply with exactly: CONTINUED-OK";
        let run = std::process::Command::new(&bin)
            .args(["run", "--session", &id, prompt])
            .current_dir("/home/harry/Documents/agentbridge")
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .expect("opencode run");
        let out = String::from_utf8_lossy(&run.stdout).to_string();
        assert!(
            out.contains("CONTINUED-OK"),
            "continuation failed; stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );

        // 3. The new turn must be persisted back into the row we inserted.
        let conn = Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE session_id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(n >= 3, "expected the new user turn to be persisted, got {}", n);
    }
}
