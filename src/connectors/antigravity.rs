//! Antigravity CLI connector.
//!
//! Antigravity ("agy") stores CLI conversations as SQLite databases under
//! `~/.gemini/antigravity-cli/`:
//!
//!   conversations/<uuid>.db      — one row per step in `steps`, payloads are
//!                                 Google cortex protobuf blobs
//!   conversation_summaries.db    — metadata index (preview, workspace URIs,
//!                                 timestamps, step counts)
//!
//! Step payloads (verified against real databases, 2026-07-30):
//!   payload.1        varint step_type (14 = user input, 15 = follow-up user
//!                    input with no text, 17 = model turn, 98 = context)
//!   payload.4        varint status (3 = DONE)
//!   payload.5        StepMetadata — .1.1/.1.2 = created (seconds, nanos)
//!   payload.19.2     user text (step type 14)
//!   payload.24.3.1   model error message (step type 17; the location of
//!                    successful model text is not yet mapped — quota
//!                    failures on the machine that produced the real data
//!                    mean no successful response exists to decode)
//!
//! Reads are strictly read-only: databases open with
//! `SQLITE_OPEN_READ_ONLY` so a live Antigravity process is never blocked.

use crate::connector::{Connector, ConnectorError, ConnectorResult, InjectTarget, SessionStream};
use crate::model::{Message, RawSession, Role, Session, TokenTotals};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

const ANTIGRAVITY_HOME: &str = ".gemini/antigravity-cli";

pub struct AntigravityConnector {
    root: PathBuf,
}

impl AntigravityConnector {
    pub fn new() -> Self {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        Self {
            root: home.join(ANTIGRAVITY_HOME),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn conversations_dir(&self) -> PathBuf {
        self.root.join("conversations")
    }

    fn summaries_db(&self) -> PathBuf {
        self.root.join("conversation_summaries.db")
    }

    fn open_conversation(&self, id: &str) -> ConnectorResult<Connection> {
        Connection::open_with_flags(
            self.conversations_dir().join(format!("{}.db", id)),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ConnectorError::Parse {
            id: id.to_string(),
            path: self.conversations_dir().join(format!("{}.db", id)),
            message: format!("open failed: {}", e),
        })
    }

    /// Look up metadata (preview / workspace / times) in the summaries index.
    fn summary_meta(&self, id: &str) -> Option<(String, Option<String>, Option<DateTime<Utc>>)> {
        let conn = Connection::open_with_flags(
            &self.summaries_db(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT preview, workspace_uris, last_modified_time \
                 FROM conversation_summaries WHERE conversation_id = ?1",
            )
            .ok()?;
        let mut rows = stmt.query_map([id], |row| {
            let preview: Option<String> = row.get(0).ok();
            let uris: Option<String> = row.get(1).ok();
            let modified: Option<String> = row.get(2).ok();
            Ok((preview, uris, modified))
        }).ok()?;
        let row = rows.next()?.ok()?;
        let (preview, uris, modified) = row;
        let project = uris.and_then(|s| parse_workspace_uris(&s));
        let last = modified.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc));
        Some((preview.unwrap_or_default(), project, last))
    }
}

/// `["file:///home/u/proj"]` → `/home/u/proj`
fn parse_workspace_uris(s: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = val.as_array()?;
    let first = arr.iter().filter_map(|v| v.as_str()).next()?;
    first.strip_prefix("file://").map(|p| p.to_string())
}

impl Connector for AntigravityConnector {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn display_name(&self) -> &'static str {
        "Antigravity CLI"
    }

    fn detect(&self) -> bool {
        self.conversations_dir().exists()
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.conversations_dir(), self.summaries_db()]
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        let dir = self.conversations_dir();
        let mut dbs: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "db"))
                .collect(),
            Err(e) => return Err(ConnectorError::Io { path: dir, source: e }),
        };
        dbs.sort();

        let meta: std::collections::HashMap<String, (String, Option<String>, Option<DateTime<Utc>>)> = {
            let mut m = std::collections::HashMap::new();
            for name in dbs.iter().filter_map(|p| p.file_stem().and_then(|s| s.to_str())) {
                if let Some(row) = self.summary_meta(name) {
                    m.insert(name.to_string(), row);
                }
            }
            m
        };

        Ok(Box::new(AntigravityScanIter {
            connector: self,
            dbs,
            idx: 0,
            meta,
        }))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        let conn = self.open_conversation(id)?;
        let mut stmt = conn
            .prepare("SELECT idx, step_type, status, step_payload FROM steps ORDER BY idx")
            .map_err(|e| ConnectorError::Parse {
                id: id.to_string(),
                path: self.conversations_dir().join(format!("{}.db", id)),
                message: format!("prepare failed: {}", e),
            })?;

        let mut messages = Vec::new();
        let mut started_at: Option<DateTime<Utc>> = None;
        let mut last_event_at: Option<DateTime<Utc>> = None;

        let rows = stmt
            .query_map([], |row| {
                let _idx: i64 = row.get(0)?;
                let step_type: i64 = row.get(1)?;
                let status: i64 = row.get(2)?;
                let payload: Vec<u8> = row.get(3).unwrap_or_default();
                Ok((step_type, status, payload))
            })
            .map_err(|e| ConnectorError::Parse {
                id: id.to_string(),
                path: self.conversations_dir().join(format!("{}.db", id)),
                message: format!("query failed: {}", e),
            })?;

        for row in rows {
            let (step_type, status, payload) = match row {
                Ok(r) => r,
                Err(e) => {
                    return Err(ConnectorError::Parse {
                        id: id.to_string(),
                        path: self.conversations_dir().join(format!("{}.db", id)),
                        message: format!("row failed: {}", e),
                    })
                }
            };
            if status != 3 {
                continue;
            }
            let ts = step_time(&payload);
            if ts.is_some() {
                if started_at.is_none() {
                    started_at = ts;
                }
                last_event_at = ts;
            }
            let role = match step_type {
                14 => Role::User,
                17 => Role::Assistant,
                _ => continue,
            };
            let text = match role {
                Role::User => step_text(&payload).or_else(|| user_input_fallback(&payload)),
                _ => model_error(&payload),
            };
            messages.push(Message {
                session_id: id.to_string(),
                ordinal: messages.len() as u64,
                role,
                timestamp: ts,
                text,
                tool_name: None,
                tool_input: None,
                tool_result: None,
                parent_ordinal: None,
            });
        }

        let (title, project, _) = self.summary_meta(id).unwrap_or_default();

        Ok(Session {
            id: id.to_string(),
            provider: "antigravity".to_string(),
            project_id: project.unwrap_or_default(),
            started_at,
            last_event_at,
            model: None,
            title: if title.is_empty() { None } else { Some(title) },
            token_totals: TokenTotals::default(),
            source_path: self.conversations_dir().join(format!("{}.db", id)),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages,
            artifacts: vec![],
        })
    }

    fn resume_cmd(&self, _session: &Session) -> Option<Vec<String>> {
        None
    }

    fn inject(&self, _brief: &str, _dry_run: bool) -> ConnectorResult<InjectTarget> {
        Err(ConnectorError::Other(anyhow::anyhow!(
            "inject not yet implemented for Antigravity"
        )))
    }
}

struct AntigravityScanIter<'a> {
    connector: &'a AntigravityConnector,
    dbs: Vec<PathBuf>,
    idx: usize,
    meta: std::collections::HashMap<String, (String, Option<String>, Option<DateTime<Utc>>)>,
}

impl Iterator for AntigravityScanIter<'_> {
    type Item = ConnectorResult<RawSession>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let db = self.dbs.get(self.idx)?;
            self.idx += 1;
            let id = match db.file_stem().and_then(|s| s.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let (title, project, last) = self.meta.get(&id).cloned().unwrap_or_default();
            let conn = match self.connector.open_conversation(&id) {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            let (first, latest) = match conn
                .query_row(
                    "SELECT (SELECT step_payload FROM steps ORDER BY idx LIMIT 1), \
                            (SELECT step_payload FROM steps ORDER BY idx DESC LIMIT 1)",
                    [],
                    |row| {
                        let a: Option<Vec<u8>> = row.get(0).ok();
                        let b: Option<Vec<u8>> = row.get(1).ok();
                        Ok((a, b))
                    },
                )
            {
                Ok(v) => v,
                Err(_) => (None, None),
            };
            let started_at = first
                .as_deref()
                .and_then(step_time)
                .or(last);
            let last_event_at = latest
                .as_deref()
                .and_then(step_time)
                .or(last);
            return Some(Ok(RawSession {
                id,
                provider: "antigravity".to_string(),
                project_path: project.map(PathBuf::from),
                started_at,
                last_event_at,
                title: if title.is_empty() { None } else { Some(title) },
                source_path: db.clone(),
                body_available: true,
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal Google protobuf reader — enough to extract the handful of fields
// the antigravity step payloads use. Length-delimited values are kept as raw
// slices (never guessed as strings vs messages); descent is explicit.
// ---------------------------------------------------------------------------

enum ProtoField {
    Varint(u64),
    Bytes(u64, u64),
    F64,
    F32,
}

/// Parse the top-level fields of a message. Length and offsets are validated
/// against the slice; unknown wire types abort the parse.
fn proto_fields(data: &[u8]) -> Option<Vec<(u64, ProtoField)>> {
    let mut out = Vec::new();
    let mut i = 0u64;
    while (i as usize) < data.len() {
        let (tag, n) = proto_varint(data, i as usize)?;
        i = n as u64;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (v, n) = proto_varint(data, i as usize)?;
                i = n as u64;
                out.push((field, ProtoField::Varint(v)));
            }
            2 => {
                let (len, n) = proto_varint(data, i as usize)?;
                i = n as u64;
                let end = i.checked_add(len)?;
                if (end as usize) > data.len() {
                    return None;
                }
                out.push((field, ProtoField::Bytes(i, len)));
                i = end;
            }
            1 => {
                let end = i.checked_add(8)?;
                if (end as usize) > data.len() {
                    return None;
                }
                out.push((field, ProtoField::F64));
                i = end;
            }
            5 => {
                let end = i.checked_add(4)?;
                if (end as usize) > data.len() {
                    return None;
                }
                out.push((field, ProtoField::F32));
                i = end;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn proto_varint(data: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    while i < data.len() && shift < 64 {
        let b = data[i];
        i += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((result, i));
        }
        shift += 7;
    }
    None
}

fn proto_varint_field(data: &[u8], field: u64) -> Option<u64> {
    let fields = proto_fields(data)?;
    let f = fields.iter().rev().find(|(f, _)| *f == field)?;
    match f.1 {
        ProtoField::Varint(v) => Some(v),
        _ => None,
    }
}

/// Descend `path` into length-delimited fields, returning the raw slice of
/// the last message found.
fn proto_descend<'a>(data: &'a [u8], path: &[u64]) -> Option<&'a [u8]> {
    let mut cur = data;
    for field in path {
        let fields = proto_fields(cur)?;
        let f = fields.iter().find(|(f, _)| *f == *field)?;
        match f.1 {
            ProtoField::Bytes(off, len) => {
                cur = cur.get(off as usize..(off + len) as usize)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// The first length-delimited string value of `field` inside `data`.
fn proto_string_field(data: &[u8], field: u64) -> Option<String> {
    let fields = proto_fields(data)?;
    let f = fields.iter().find(|(f, _)| *f == field)?;
    match f.1 {
        ProtoField::Bytes(off, len) => {
            let raw = data.get(off as usize..(off + len) as usize)?;
            String::from_utf8(raw.to_vec()).ok()
        }
        _ => None,
    }
}

/// `payload.5.1` → created (seconds, nanos).
fn step_time(payload: &[u8]) -> Option<DateTime<Utc>> {
    let created = proto_descend(payload, &[5, 1])?;
    let secs = proto_varint_field(created, 1)? as i64;
    let nanos = proto_varint_field(created, 2).unwrap_or(0) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

/// User text for step type 14: `payload.19.2`.
fn step_text(payload: &[u8]) -> Option<String> {
    let f19 = proto_descend(payload, &[19])?;
    proto_string_field(f19, 2)
}

/// Fallback user text at `payload.19.3.1` (the inner user message).
fn user_input_fallback(payload: &[u8]) -> Option<String> {
    let f3 = proto_descend(payload, &[19, 3])?;
    proto_string_field(f3, 1)
}

/// Model error message for step type 17: `payload.24.3.1`.
fn model_error(payload: &[u8]) -> Option<String> {
    let f3 = proto_descend(payload, &[24, 3])?;
    proto_string_field(f3, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_buf(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut x = v;
        loop {
            let mut b = (x & 0x7f) as u8;
            x >>= 7;
            if x != 0 {
                b |= 0x80;
            }
            out.push(b);
            if x == 0 {
                return out;
            }
        }
    }

    fn tag_buf(field: u64, wire: u8) -> Vec<u8> {
        varint_buf(field << 3 | wire as u64)
    }

    fn str_field(field: u64, s: &str) -> Vec<u8> {
        let mut out = tag_buf(field, 2);
        out.extend(varint_buf(s.len() as u64));
        out.extend(s.as_bytes());
        out
    }

    fn msg_field(field: u64, inner: &[u8]) -> Vec<u8> {
        let mut out = tag_buf(field, 2);
        out.extend(varint_buf(inner.len() as u64));
        out.extend(inner);
        out
    }

    fn timestamp_field(secs: i64, nanos: u32) -> Vec<u8> {
        // .5 = metadata; .1 = created; (.1 secs, .2 nanos)
        let mut created = tag_buf(1, 0);
        created.extend(varint_buf(secs as u64));
        created.extend(tag_buf(2, 0));
        created.extend(varint_buf(nanos as u64));
        msg_field(5, &msg_field(1, &created))
    }

    /// A step-type-14 payload: user text "say hi" at .19.2
    fn user_payload(secs: i64) -> Vec<u8> {
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(14));
        out.extend(tag_buf(4, 0));
        out.extend(varint_buf(3));
        out.extend(timestamp_field(secs, 0));
        out.extend(msg_field(19, &str_field(2, "say hi")));
        out
    }

    /// A step-type-17 payload with a model error at .24.3.1
    fn model_payload(secs: i64) -> Vec<u8> {
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(17));
        out.extend(tag_buf(4, 0));
        out.extend(varint_buf(3));
        out.extend(timestamp_field(secs, 0));
        out.extend(msg_field(24, &msg_field(3, &str_field(1, "quota exhausted"))));
        out
    }

    /// A minimal step-type-98 (context) payload with no content fields.
    fn ctx_payload() -> Vec<u8> {
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(98));
        out
    }

    fn make_db(dir: &Path, id: &str) -> PathBuf {
        let path = dir.join(format!("{}.db", id));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER, step_type INTEGER, status INTEGER, \
             has_subtrajectory INTEGER, metadata BLOB, error_details BLOB, \
             permissions BLOB, task_details BLOB, render_info BLOB, \
             step_payload BLOB, step_format INTEGER);",
        )
        .unwrap();
        for (i, (step_type, payload)) in [
            (14, user_payload(1_785_377_882)),
            (17, model_payload(1_785_377_885)),
            (98, ctx_payload()),
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO steps (idx, step_type, status, step_payload, step_format) \
                 VALUES (?1, ?2, 3, ?3, 0)",
                rusqlite::params![i as i64, *step_type, *payload],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn test_walker_extracts_user_text_and_model_error() {
        let user = user_payload(1_785_377_882);
        assert_eq!(step_text(&user).as_deref(), Some("say hi"));
        assert_eq!(step_time(&user), Utc.timestamp_opt(1_785_377_882, 0).single());

        let model = model_payload(1_785_377_885);
        assert_eq!(model_error(&model).as_deref(), Some("quota exhausted"));
        assert_eq!(model_error(&user), None, "no error in a user step");

        let ctx = ctx_payload();
        assert_eq!(step_text(&ctx), None, "context steps carry no text");
    }

    /// Real-data check (ignored): run against the actual antigravity store
    /// when present, proving the protobuf decode matches what the real binary
    /// wrote. HANDOFF §5: unit fixtures can pass while real data fails.
    #[test]
    #[ignore = "requires the operator's real antigravity CLI data"]
    fn test_load_real_antigravity_conversation() {
        let home = std::env::var("HOME").unwrap_or_default();
        let root = PathBuf::from(home).join(ANTIGRAVITY_HOME);
        if !root.join("conversations").exists() {
            eprintln!("skipping: no antigravity data on this machine");
            return;
        }
        let connector = AntigravityConnector::with_root(root);
        let results: Vec<_> = connector.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert!(!results.is_empty(), "real machine has antigravity sessions");
        for r in &results {
            let session = connector.load(&r.id).unwrap();
            eprintln!(
                "{}: {} messages, project {:?}, title {:?}",
                r.id,
                session.messages.len(),
                session.project_id,
                session.title
            );
            assert!(!session.messages.is_empty(), "{} must load turns", r.id);
            assert_eq!(session.messages[0].role, Role::User);
            assert!(
                session.messages[0].text.is_some(),
                "first message must carry the user's request"
            );
        }
    }

    #[test]
    fn test_scan_and_load_handcrafted_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let id = "3ff51afb-4660-47ee-884c-56498f4b0222";
        make_db(&root.join("conversations"), id);

        let connector = AntigravityConnector::with_root(root.clone());
        assert!(connector.detect());

        let results: Vec<_> = connector.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id);
        assert!(results[0].started_at.is_some());

        let session = connector.load(id).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text.as_deref(), Some("say hi"));
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text.as_deref(), Some("quota exhausted"));
        assert_eq!(
            session.messages[1].timestamp,
            Utc.timestamp_opt(1_785_377_885, 0).single()
        );
    }
}
