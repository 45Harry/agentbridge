//! OpenCode connector.
//!
//! The odd one out: OpenCode keeps sessions as rows in a SQLite database
//! (`~/.local/share/opencode/opencode.db`) rather than JSONL files, so this
//! connector reads a real relational schema instead of parsing a transcript.
//!
//! Schema, verified against 1.17.15 (see `CONNECTORS.md` §3):
//!   session(id TEXT PK `ses_…`, directory TEXT — plain unencoded cwd,
//!           title, time_created/time_updated INTEGER epoch millis, …)
//!   message(id, session_id, time_created, data JSON — `{role, time, model}`)
//!   part(id, message_id, session_id, data JSON — `{type: text|tool|…, text}`)
//!
//! **Reads are strictly read-only.** The database is opened with
//! `SQLITE_OPEN_READ_ONLY` so a live OpenCode process is never blocked and
//! this can never mutate the operator's real data.

use crate::connector::{Connector, ConnectorError, ConnectorResult, InjectTarget, SessionStream};
use crate::model::{Message, RawSession, Role, Session, TokenTotals};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct OpenCodeConnector {
    db_path: PathBuf,
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        Self {
            db_path: default_db_path(),
        }
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Open read-only. Never opens for writing, and never with `immutable=1`
    /// — that would tell SQLite the file cannot change and risk reading torn
    /// or stale data while OpenCode is running.
    fn open(&self) -> ConnectorResult<Connection> {
        Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ConnectorError::Parse {
            id: "opencode".to_string(),
            path: self.db_path.clone(),
            message: format!("open failed: {}", e),
        })
    }
}

pub fn default_db_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("opencode").join("opencode.db");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

/// OpenCode stores epoch **milliseconds**, not seconds.
fn millis_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

impl Connector for OpenCodeConnector {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn detect(&self) -> bool {
        self.db_path.exists()
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.db_path.clone()]
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, directory, title, time_created, time_updated \
                 FROM session ORDER BY time_updated DESC",
            )
            .map_err(|e| ConnectorError::Parse {
                id: "opencode".to_string(),
                path: self.db_path.clone(),
                message: format!("prepare failed: {}", e),
            })?;

        // Collected rather than streamed: the statement borrows the
        // connection, which cannot outlive this function. Session metadata is
        // small (147 rows on a real machine) so this stays cheap — bodies are
        // still only read on `load`.
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let directory: Option<String> = row.get(1).ok();
                let title: Option<String> = row.get(2).ok();
                let created: Option<i64> = row.get(3).ok();
                let updated: Option<i64> = row.get(4).ok();
                Ok(RawSession {
                    id,
                    provider: "opencode".to_string(),
                    // Stored verbatim — no lossy directory encoding here.
                    project_path: directory.map(PathBuf::from),
                    started_at: created.and_then(millis_to_dt),
                    last_event_at: updated.and_then(millis_to_dt),
                    title,
                    source_path: self.db_path.clone(),
                    body_available: true,
                })
            })
            .map_err(|e| ConnectorError::Parse {
                id: "opencode".to_string(),
                path: self.db_path.clone(),
                message: format!("query failed: {}", e),
            })?
            .collect::<Vec<_>>();

        let out: Vec<ConnectorResult<RawSession>> = rows
            .into_iter()
            .map(|r| {
                r.map_err(|e| ConnectorError::Parse {
                    id: "opencode".to_string(),
                    path: self.db_path.clone(),
                    message: format!("row failed: {}", e),
                })
            })
            .collect();

        Ok(Box::new(out.into_iter()))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        let conn = self.open()?;

        let meta = conn.query_row(
            "SELECT directory, title, time_created, time_updated FROM session WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        );

        let (directory, title, created, updated) = match meta {
            Ok(m) => m,
            Err(_) => return Err(ConnectorError::NotFound(id.to_string())),
        };

        // One row per part, ordered by message then part, so a message's text
        // and tool parts arrive together and in order.
        let mut stmt = conn
            .prepare(
                "SELECT m.id, m.data, m.time_created, p.data \
                 FROM message m LEFT JOIN part p ON p.message_id = m.id \
                 WHERE m.session_id = ?1 \
                 ORDER BY m.time_created, m.id, p.id",
            )
            .map_err(|e| ConnectorError::Parse {
                id: id.to_string(),
                path: self.db_path.clone(),
                message: format!("prepare failed: {}", e),
            })?;

        let rows = stmt
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| ConnectorError::Parse {
                id: id.to_string(),
                path: self.db_path.clone(),
                message: format!("query failed: {}", e),
            })?;

        let mut messages: Vec<Message> = Vec::new();
        let mut model: Option<String> = None;
        let mut current_msg_id = String::new();

        for row in rows.flatten() {
            let (msg_id, msg_data, msg_time, part_data) = row;

            let mdata: Value = serde_json::from_str(&msg_data).unwrap_or(Value::Null);
            let role = match mdata.get("role").and_then(|r| r.as_str()) {
                Some("user") => Role::User,
                Some("system") => Role::System,
                _ => Role::Assistant,
            };
            if model.is_none() {
                model = mdata
                    .get("model")
                    .and_then(|m| m.get("modelID"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            let timestamp = msg_time.and_then(millis_to_dt);

            // Each part becomes its own normalized message; a message with no
            // parts still yields one entry so the turn is not lost.
            let (text, tool_name, tool_input) = match part_data
                .as_deref()
                .and_then(|d| serde_json::from_str::<Value>(d).ok())
            {
                Some(p) => match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("reasoning") => (
                        p.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        None,
                        None,
                    ),
                    Some("tool") => (
                        None,
                        p.get("tool").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        p.get("state").cloned(),
                    ),
                    // step-start / step-finish / patch / file carry no
                    // conversational content — skip rather than emit blanks.
                    _ => continue,
                },
                None if msg_id != current_msg_id => (None, None, None),
                None => continue,
            };

            if text.is_none() && tool_name.is_none() {
                continue;
            }

            current_msg_id = msg_id;
            messages.push(Message {
                session_id: id.to_string(),
                ordinal: messages.len() as u64,
                role: if tool_name.is_some() && role == Role::Assistant {
                    Role::Assistant
                } else {
                    role
                },
                timestamp,
                text,
                tool_name,
                tool_input,
                tool_result: None,
                parent_ordinal: None,
            });
        }

        Ok(Session {
            id: id.to_string(),
            provider: "opencode".to_string(),
            project_id: directory.unwrap_or_default(),
            started_at: created.and_then(millis_to_dt),
            last_event_at: updated.and_then(millis_to_dt),
            model,
            title,
            token_totals: TokenTotals::default(),
            source_path: self.db_path.clone(),
            raw_payload: Value::Null,
            body_available: true,
            messages,
            artifacts: Vec::new(),
        })
    }

    fn resume_cmd(&self, session: &Session) -> Option<Vec<String>> {
        Some(vec![
            "opencode".to_string(),
            "run".to_string(),
            "--session".to_string(),
            session.id.clone(),
        ])
    }

    fn inject(&self, _brief: &str, _dry_run: bool) -> ConnectorResult<InjectTarget> {
        Err(ConnectorError::Other(anyhow::anyhow!(
            "opencode inject not implemented"
        )))
    }
}

/// Read a session from an explicit database path.
pub(crate) fn load_from_db(db: &Path, id: &str) -> ConnectorResult<Session> {
    OpenCodeConnector::with_db(db.to_path_buf()).load(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a miniature database with OpenCode's real schema.
    fn fixture_db() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
                directory TEXT, title TEXT,
                time_created INTEGER, time_updated INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL
            );
            INSERT INTO session VALUES
              ('ses_abc', 'p1', NULL, '/home/u/proj', 'Python work',
               1778656792117, 1778656799000);
            INSERT INTO message VALUES
              ('msg_1','ses_abc',1778656792117,1778656792117,
               '{"role":"user","model":{"modelID":"deepseek-v4"}}'),
              ('msg_2','ses_abc',1778656793000,1778656793000,
               '{"role":"assistant","model":{"modelID":"deepseek-v4"}}');
            INSERT INTO part VALUES
              ('prt_1','msg_1','ses_abc',0,0,'{"type":"text","text":"how do I sort a list"}'),
              ('prt_2','msg_2','ses_abc',0,0,'{"type":"step-start"}'),
              ('prt_3','msg_2','ses_abc',0,0,'{"type":"text","text":"use sorted()"}'),
              ('prt_4','msg_2','ses_abc',0,0,'{"type":"tool","tool":"Bash","state":{"cmd":"ls"}}');
            "#,
        )
        .unwrap();
        (tmp, path)
    }

    #[test]
    fn test_scan_reads_sessions_with_plain_directory() {
        let (_t, db) = fixture_db();
        let c = OpenCodeConnector::with_db(db);
        assert!(c.detect());

        let found: Vec<_> = c.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!(s.id, "ses_abc");
        assert_eq!(s.provider, "opencode");
        // Stored verbatim — no lossy encoding to undo, unlike Claude Code.
        assert_eq!(s.project_path.as_deref(), Some(Path::new("/home/u/proj")));
        assert_eq!(s.title.as_deref(), Some("Python work"));
        assert!(s.started_at.is_some(), "epoch millis must parse");
    }

    #[test]
    fn test_load_joins_messages_and_parts_in_order() {
        let (_t, db) = fixture_db();
        let s = load_from_db(&db, "ses_abc").unwrap();

        assert_eq!(s.project_id, "/home/u/proj");
        assert_eq!(s.model.as_deref(), Some("deepseek-v4"));

        let texts: Vec<&str> = s.messages.iter().filter_map(|m| m.text.as_deref()).collect();
        assert_eq!(texts, vec!["how do I sort a list", "use sorted()"]);

        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);

        let tool = s.messages.iter().find(|m| m.tool_name.is_some()).unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
    }

    /// step-start / step-finish carry no conversation and must not become
    /// empty turns.
    #[test]
    fn test_non_content_parts_are_skipped() {
        let (_t, db) = fixture_db();
        let s = load_from_db(&db, "ses_abc").unwrap();
        assert!(
            s.messages.iter().all(|m| m.text.is_some() || m.tool_name.is_some()),
            "no blank turns"
        );
        assert_eq!(s.messages.len(), 3, "2 text + 1 tool, step-start dropped");
    }

    #[test]
    fn test_load_unknown_session_is_not_found() {
        let (_t, db) = fixture_db();
        assert!(load_from_db(&db, "ses_nope").is_err());
    }

    #[test]
    fn test_missing_db_is_not_detected() {
        let c = OpenCodeConnector::with_db(PathBuf::from("/nonexistent/opencode.db"));
        assert!(!c.detect());
    }
}
