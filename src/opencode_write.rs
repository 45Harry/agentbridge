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
//!
//! OpenCode's picker lists only sessions of the project it is launched from
//! (project-scoped query plus an optional launch-directory filter). One row
//! can therefore only ever be visible from one directory, so every synced
//! session is materialized once **per target directory** — the sync project,
//! `$HOME`, and every known project worktree — each with its own id derived
//! from `(provider, source id, directory)`. Rows for a project worktree show
//! up from any directory inside that worktree; rows for the `global` project
//! show up from any non-git directory. The launch-directory filter
//! (`session_directory_filter_enabled`) is additionally disabled by
//! [`ensure_any_directory_filter`] so subdirectories and arbitrary non-git
//! directories list the sessions too.

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

/// Deterministic OpenCode-shaped id for a foreign session materialized into
/// `directory`, so re-running is idempotent rather than inserting a duplicate
/// every time, and so the per-directory rows never collide on the primary key.
pub fn derive_id(source_provider: &str, source_id: &str, directory: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}:{}", source_provider, source_id, directory).as_bytes(),
    );
    format!("ses_ab{}", ns.simple())
}

/// The id scheme used before per-directory materialization (v0.3.3 and
/// earlier: one row per session, keyed on `provider:source_id` alone). Those
/// rows must be migrated away — left in place, a legacy row would duplicate
/// the new per-directory row in the same project.
fn legacy_id(source_provider: &str, source_id: &str) -> String {
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}", source_provider, source_id).as_bytes(),
    );
    format!("ses_ab{}", ns.simple())
}

/// Whether agentbridge already owns the row for `id` in `directory`. A
/// re-sync that would only refresh our own rows (idempotent delete-then-
/// reinsert of data agentbridge wrote) does not need a database backup; an
/// INSERT of a new row does. Used by the sync loop to decide whether to take
/// the one-per-run backup.
pub fn session_row_exists(db: &Path, id: &str, directory: &str) -> Result<bool, WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM session WHERE id = ?1 AND directory = ?2 AND metadata LIKE ?3 LIMIT 1",
            params![id, directory, format!("%{}%", MARKER)],
            |_| Ok(()),
        )
        .is_ok())
}

/// The `session_directory_filter_enabled` key OpenCode's session list reads
/// (default `true`): when set, the picker only lists sessions whose directory
/// matches the launch directory, which would hide worktree-root rows from
/// subdirectories and `global` rows from every non-git directory.
pub const DIRECTORY_FILTER_KEY: &str = "session_directory_filter_enabled";

/// The OpenCode config directory (`$XDG_CONFIG_HOME/opencode`, or
/// `~/.config/opencode`).
fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_default()
        })
        .join("opencode")
}

/// Every directory a synced session must be visible from: the sync project,
/// `$HOME` (the `global` project, covering every non-git directory), and each
/// project worktree OpenCode already knows about. Duplicates collapse.
pub fn target_dirs(
    db: &Path,
    project_dir: &str,
    home: Option<&str>,
) -> Result<Vec<String>, WriteError> {
    let mut dirs: Vec<String> = Vec::new();
    let mut push = |d: &str| {
        let d = d.trim_end_matches('/').to_string();
        if !d.is_empty() && d != "/" && !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    push(project_dir);
    if let Some(h) = home {
        push(h);
    }
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT worktree FROM project WHERE worktree IS NOT NULL AND worktree != ''")
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    for row in rows {
        push(&row.map_err(|e| WriteError::Sql(e.to_string()))?);
    }
    Ok(dirs)
}

/// Make the picker list sessions from every directory: writes
/// `session_directory_filter_enabled: false` into the OpenCode config when no
/// existing config file already sets the key (a user or TUI choice is left
/// alone). The file OpenCode is already loading is preferred; a fresh
/// `opencode.json` is created otherwise. Returns the file written. Callers
/// gate on [`ensure_safe_to_write`] before invoking.
pub fn ensure_any_directory_filter() -> Result<PathBuf, WriteError> {
    let dir = config_dir();
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    for existing in [&json, &jsonc] {
        if existing.exists()
            && let Ok(txt) = std::fs::read_to_string(existing)
            && txt.contains(DIRECTORY_FILTER_KEY)
        {
            return Ok(existing.clone());
        }
    }
    if jsonc.exists() {
        let txt = std::fs::read_to_string(&jsonc).map_err(|e| WriteError::Sql(e.to_string()))?;
        let mut value: serde_json::Value =
            serde_json::from_str(&txt).map_err(|e| WriteError::Sql(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert(DIRECTORY_FILTER_KEY.to_string(), serde_json::Value::Bool(false));
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
        std::fs::copy(
            &jsonc,
            jsonc.with_extension(format!("agentbridge-backup-{}.jsonc", stamp)),
        )
        .map_err(|e| WriteError::Backup(e.to_string()))?;
        std::fs::write(&jsonc, serde_json::to_string_pretty(&value).map_err(|e| WriteError::Sql(e.to_string()))?)
            .map_err(|e| WriteError::Sql(e.to_string()))?;
        Ok(jsonc)
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| WriteError::Sql(e.to_string()))?;
        let body = format!("{{\n  \"{DIRECTORY_FILTER_KEY}\": false\n}}\n");
        std::fs::write(&json, body).map_err(|e| WriteError::Sql(e.to_string()))?;
        Ok(json)
    }
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
    let id = derive_id(&session.provider, &session.id, directory);
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
) -> Result<(String, usize), WriteError> {
    let mut conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let tx = conn.transaction().map_err(|e| WriteError::Sql(e.to_string()))?;

    let id = derive_id(&session.provider, &session.id, directory);
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

    let project_id = resolve_project_id(&tx, directory)?;

    // Replace wholesale so a re-run refreshes rather than duplicating. The
    // cascade clears the old messages/parts first. Scoped by directory: the
    // same session now has one row per directory it should appear in.
    tx.execute(
        "DELETE FROM session WHERE id = ?1 AND directory = ?2 AND metadata LIKE ?3",
        params![id, directory, format!("%{}%", MARKER)],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;

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

    // Migrate pre-v0.3.4 rows: one row per session keyed on
    // `provider:source_id` alone. Left in place, such a row duplicates this
    // session's per-directory row in the same project's picker, so once the
    // new rows exist the old one is removed (cascade drops its messages).
    let legacy = legacy_id(&session.provider, &session.id);
    if legacy != id {
        tx.execute(
            "DELETE FROM session WHERE id = ?1 AND metadata LIKE ?2",
            params![legacy, format!("%{}%", MARKER)],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    }

    // OpenCode's message ids encode a time-ordered counter and its session
    // loop picks the "latest" message by comparing id strings. Foreign ids
    // must sort below anything OpenCode generates (`msg_f…`), so the
    // `msg_0` prefix is what keeps continuation working.
    let ns = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:{}:{}", session.provider, session.id, directory).as_bytes(),
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
    Ok((id, idx))
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
        let a = derive_id("claude-code", "abc", "/p");
        assert_eq!(a, derive_id("claude-code", "abc", "/p"));
        assert_ne!(a, derive_id("codex-cli", "abc", "/p"));
        assert_ne!(a, derive_id("claude-code", "abc", "/other"));
        assert!(a.starts_with("ses_ab"));
    }

    /// The same session must be materializable into several directories at
    /// once — the whole point of "visible from any directory" — without
    /// colliding on the primary key.
    #[test]
    fn test_write_session_can_materialize_into_multiple_directories() {
        let (_t, db) = db_with_schema();
        write_session(&db, &a_session(), "/home/u/p").unwrap();
        write_session(&db, &a_session(), "/home/u").unwrap();
        write_session(&db, &a_session(), "/home/u/other").unwrap();

        assert_eq!(count_written(&db), 3, "one row per directory");
        let conn = Connection::open(&db).unwrap();
        for dir in ["/home/u/p", "/home/u", "/home/u/other"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session WHERE directory=?1 AND metadata LIKE ?2",
                    params![dir, format!("%{}%", MARKER)],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "exactly one row for {}", dir);
        }
    }

    /// Pre-v0.3.4 syncs wrote one row per session keyed on
    /// `provider:source_id` alone; a refresh must migrate those away or the
    /// same session would appear twice in one project's picker.
    #[test]
    fn test_write_migrates_legacy_single_directory_row() {
        let (_t, db) = db_with_schema();
        let conn = Connection::open(&db).unwrap();
        let legacy = legacy_id("claude-code", "7a65dbea-9780-46be-b7b4-a5e5e948abbf");
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, \
             time_created, time_updated, metadata) VALUES (?1,'global','old','/home/u/p',\
             'Old','1.17.15',1,2,?2)",
            params![
                legacy,
                format!("{{\"{}\":true,\"source_provider\":\"claude-code\",\"source_id\":\"7a65dbea-9780-46be-b7b4-a5e5e948abbf\"}}", MARKER)
            ],
        )
        .unwrap();

        write_session(&db, &a_session(), "/home/u/p").unwrap();
        assert_eq!(
            count_written(&db),
            1,
            "legacy row must be replaced by the per-directory row"
        );
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id=?1",
                params![legacy],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    /// `target_dirs` covers the project, $HOME, and every known worktree,
    /// collapsing duplicates.
    #[test]
    fn test_target_dirs_covers_project_home_and_worktrees() {
        let (_t, db) = db_with_schema();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO project VALUES ('p1','/work/one',0,0,'[]');
            INSERT INTO project VALUES ('p2','/work/one',0,0,'[]');
            INSERT INTO project VALUES ('p3','/',0,0,'[]');
            INSERT INTO project VALUES ('p4','',0,0,'[]');
            "#,
        )
        .unwrap();

        let dirs = target_dirs(&db, "/work/one", Some("/home/u")).unwrap();
        assert_eq!(
            dirs,
            vec!["/work/one", "/home/u"],
            "worktree dedup collapses and '/' is skipped"
        );
        let dirs = target_dirs(&db, "/fresh/project", Some("/home/u")).unwrap();
        assert_eq!(
            dirs,
            vec!["/fresh/project", "/home/u", "/work/one"],
            "sync project first, then home, then worktrees"
        );
    }

    /// The any-directory config write is one-shot: an existing key (user or
    /// TUI choice) is respected and never overwritten.
    #[test]
    fn test_ensure_any_directory_filter_respects_existing_choice() {
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: test-only env mutation, restored before returning.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let result = std::panic::catch_unwind(|| {
            std::fs::create_dir_all(tmp.path().join("opencode")).unwrap();
            let jsonc = tmp.path().join("opencode").join("opencode.jsonc");
            std::fs::write(&jsonc, "{ \"session_directory_filter_enabled\": true }").unwrap();

            let written = ensure_any_directory_filter().unwrap();
            assert_eq!(written, jsonc);
            let txt = std::fs::read_to_string(&jsonc).unwrap();
            assert!(txt.contains("\"session_directory_filter_enabled\": true"));

            // A config dir without any config gets a fresh opencode.json.
            let tmp2 = tempfile::tempdir().unwrap();
            // SAFETY: test-only env mutation.
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", tmp2.path());
            }
            let written = ensure_any_directory_filter().unwrap();
            let txt = std::fs::read_to_string(&written).unwrap();
            assert!(txt.contains("\"session_directory_filter_enabled\": false"));
        });
        // SAFETY: restoring the caller's environment.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        assert!(result.is_ok());
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
        assert!(session_row_exists(&db, "ses_own", "/p").unwrap());
        assert!(!session_row_exists(&db, "ses_own", "/other").unwrap());
        assert!(
            !session_row_exists(&db, "ses_real", "/home/u/p").unwrap(),
            "OpenCode's own row carries no marker"
        );
        assert!(!session_row_exists(&db, "ses_missing", "/p").unwrap());
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
