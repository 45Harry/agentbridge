//! Antigravity connector ("agy").
//!
//! Antigravity ships as several surfaces (CLI, IDE) that each keep their own
//! store under `~/.gemini/`. Every one of them is scanned, because a session
//! recorded by the IDE is the same kind of object as one recorded by the CLI
//! and a user asking "where are my agy sessions" means all of them:
//!
//!   antigravity-cli/     headless CLI runs
//!   antigravity-ide/     IDE runs
//!   antigravity/         desktop app
//!   antigravity-backup/  a previous install's store
//!
//! Within a home:
//!
//!   conversations/<uuid>.db      — one row per step in `steps`, payloads are
//!                                 Google cortex protobuf blobs. Readable.
//!   conversations/<uuid>.pb      — encrypted (measured byte entropy 8.0/8.0);
//!                                 no key is available to us, so these are
//!                                 skipped rather than reported as corrupt.
//!   conversation_summaries.db    — optional metadata index (title, preview,
//!                                 workspace URIs, timestamps, step counts).
//!                                 Only `antigravity-cli` has one; the IDE
//!                                 keeps the equivalent per-conversation in
//!                                 `trajectory_metadata_blob`.
//!
//! Step payloads (verified against real databases, 2026-08-19):
//!   payload.1        varint step_type (14 = user input, 15 = model turn,
//!                    17 = model turn carrying an error, 21 = tool/command,
//!                    23 = model summary, 98/99 = context)
//!   payload.4        varint status (3 = DONE)
//!   payload.5        StepMetadata — .1.1/.1.2 = created (seconds, nanos)
//!   payload.19.2     user text (step type 14)
//!   payload.20.1     model response text (step type 15) — `.20.8` repeats it
//!                    and `.20.3` is the model's private reasoning, which is
//!                    deliberately not surfaced as the turn's text
//!   payload.23.1     model turn text on the alternate encoding
//!   payload.24.3.1   model error message (step type 17)
//!   payload.30.4     model summary text (step type 23)
//!
//! Per-conversation metadata blob (`trajectory_metadata_blob.data`):
//!   .1.1             workspace URI (`file:///...`) — the project path
//!   .2.1/.2.2        created (seconds, nanos)
//!   .18              project id (`default-cli-project` for headless runs,
//!                    which legitimately have no workspace)
//!
//! Reads are strictly read-only: databases open with
//! `SQLITE_OPEN_READ_ONLY` so a live Antigravity process is never blocked.

use crate::connector::{Connector, ConnectorError, ConnectorResult, InjectTarget, SessionStream};
use crate::model::{Message, RawSession, Role, Session, TokenTotals};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Every Antigravity surface that keeps sessions, in the order they are
/// scanned. `antigravity-cli` is first because it is the one agentbridge
/// writes into (see `crate::antigravity_write`).
const ANTIGRAVITY_HOMES: &[&str] = &[
    ".gemini/antigravity-cli",
    ".gemini/antigravity-ide",
    ".gemini/antigravity",
    ".gemini/antigravity-backup",
];

/// The home agentbridge materializes sessions into: the CLI store, which is
/// the only surface with a `conversation_summaries.db` index to register a
/// session in. Honors `ANTIGRAVITY_HOME` so tests and operators can redirect
/// it, mirroring `CODEX_HOME`/`CLAUDE_CONFIG_DIR` handling in the other
/// connectors.
pub(crate) fn write_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ANTIGRAVITY_HOME") {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(ANTIGRAVITY_HOMES[0]))
}

pub struct AntigravityConnector {
    /// Every home that exists on this machine, scanned in order.
    roots: Vec<PathBuf>,
}

impl AntigravityConnector {
    pub fn new() -> Self {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        let mut roots: Vec<PathBuf> = ANTIGRAVITY_HOMES.iter().map(|h| home.join(h)).collect();
        // An `ANTIGRAVITY_HOME` override must also be scanned, or a session
        // agentbridge just wrote would be invisible to the next read.
        if let Some(w) = write_home()
            && !roots.contains(&w)
        {
            roots.insert(0, w);
        }
        Self { roots }
    }

    #[cfg(test)]
    pub fn with_root(root: PathBuf) -> Self {
        Self { roots: vec![root] }
    }

    #[cfg(test)]
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Homes that actually exist, cheaply — no parsing.
    fn live_roots(&self) -> impl Iterator<Item = &PathBuf> {
        self.roots.iter().filter(|r| r.join("conversations").is_dir())
    }

    /// Locate the body for `id` across every home. Returns the home too, so
    /// the matching summaries index can be consulted.
    fn find_body(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        // Reject ids that could escape the conversations dir — `id` reaches
        // here from the CLI and must never be pasted into a path unchecked.
        if !is_safe_id(id) {
            return None;
        }
        self.live_roots().find_map(|root| {
            let body = root.join("conversations").join(format!("{}.db", id));
            body.is_file().then(|| (root.clone(), body))
        })
    }

    fn open_body(&self, id: &str, path: &Path) -> ConnectorResult<Connection> {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ConnectorError::Parse {
            id: id.to_string(),
            path: path.to_path_buf(),
            message: format!("open failed: {}", e),
        })
    }

    /// Look up metadata (title / preview / workspace / times) in one home's
    /// summaries index. Absent for the IDE store, which is why every caller
    /// falls back to the per-conversation metadata blob.
    fn summary_meta(&self, root: &Path, id: &str) -> Option<SummaryRow> {
        let db = root.join("conversation_summaries.db");
        if !db.is_file() {
            return None;
        }
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .ok()?;
        // `title` is the user's explicit rename and must win over `preview`,
        // which is only the first words of the opening message. Reading
        // `preview` as the title made a rename invisible, so a session
        // renamed inside agy looked unchanged to write-back.
        let mut stmt = conn
            .prepare(
                "SELECT title, preview, workspace_uris, last_modified_time \
                 FROM conversation_summaries WHERE conversation_id = ?1",
            )
            .ok()?;
        let mut rows = stmt
            .query_map([id], |row| {
                let title: Option<String> = row.get(0).ok();
                let preview: Option<String> = row.get(1).ok();
                let uris: Option<String> = row.get(2).ok();
                let modified: Option<String> = row.get(3).ok();
                Ok((title, preview, uris, modified))
            })
            .ok()?;
        let (title, preview, uris, modified) = rows.next()?.ok()?;
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| {
                preview
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
            });
        Some(SummaryRow {
            title,
            project: uris.and_then(|s| parse_workspace_uris(&s)),
            last_modified: modified.as_deref().and_then(parse_agy_time),
        })
    }

    /// Metadata carried inside the conversation body itself. This is the only
    /// source for IDE sessions, and fills in the project path the summaries
    /// index lacks.
    fn blob_meta(&self, conn: &Connection) -> Option<BlobMeta> {
        blob_meta_of(conn)
    }
}

/// One row of the summaries index, already normalized.
#[derive(Debug, Clone, Default)]
struct SummaryRow {
    title: Option<String>,
    project: Option<String>,
    last_modified: Option<DateTime<Utc>>,
}

/// Metadata read from a conversation's own `trajectory_metadata_blob`.
#[derive(Debug, Clone, Default)]
struct BlobMeta {
    project: Option<String>,
    created: Option<DateTime<Utc>>,
}

/// Decode one conversation body by path, without needing to know which home
/// it belongs to. This is what the sync write-back path re-reads to detect
/// turns another tool appended, so there is exactly one decoder for the format
/// (`sync::load_materialized`).
///
/// Project and creation time come from the body's own
/// `trajectory_metadata_blob`; a caller that also has a summaries row layers
/// the explicit title over the top.
pub(crate) fn load_body(path: &Path, id: &str) -> ConnectorResult<Session> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| ConnectorError::Parse {
        id: id.to_string(),
        path: path.to_path_buf(),
        message: format!("open failed: {}", e),
    })?;

    let mut stmt = conn
        .prepare("SELECT idx, step_type, status, step_payload FROM steps ORDER BY idx")
        .map_err(|e| ConnectorError::Parse {
            id: id.to_string(),
            path: path.to_path_buf(),
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
            path: path.to_path_buf(),
            message: format!("query failed: {}", e),
        })?;

    for row in rows {
        let (step_type, status, payload) = match row {
            Ok(r) => r,
            Err(e) => {
                return Err(ConnectorError::Parse {
                    id: id.to_string(),
                    path: path.to_path_buf(),
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
        // A step type that only carries bookkeeping is skipped; a turn with no
        // recoverable text is still kept, since dropping it would silently
        // renumber the transcript.
        let Some((role, text)) = step_message(step_type, &payload) else {
            continue;
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

    let blob = blob_meta_of(&conn).unwrap_or_default();
    let started_at = started_at.or(blob.created);
    let last_event_at = last_event_at.or(started_at);

    Ok(Session {
        id: id.to_string(),
        provider: "antigravity".to_string(),
        project_id: blob.project.unwrap_or_default(),
        started_at,
        last_event_at,
        model: None,
        title: None,
        token_totals: TokenTotals::default(),
        source_path: path.to_path_buf(),
        raw_payload: serde_json::Value::Null,
        body_available: true,
        messages,
        artifacts: vec![],
    })
}

/// `trajectory_metadata_blob` for an already-open connection.
fn blob_meta_of(conn: &Connection) -> Option<BlobMeta> {
    let data: Vec<u8> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let project = proto_descend(&data, &[1])
        .and_then(|m| proto_string_field(m, 1))
        .and_then(|uri| strip_file_uri(&uri));
    let created = proto_descend(&data, &[2]).and_then(|t| {
        let secs = proto_varint_field(t, 1)? as i64;
        let nanos = proto_varint_field(t, 2).unwrap_or(0) as u32;
        Utc.timestamp_opt(secs, nanos).single()
    });
    Some(BlobMeta { project, created })
}

/// A conversation id is a bare uuid-ish token. Anything with a separator could
/// escape `conversations/` when interpolated into a path.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `file:///home/u/proj` → `/home/u/proj`. A workspace that is not a local
/// file URI has no path we can use as a project.
fn strip_file_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("file://")
        .filter(|p| p.starts_with('/'))
        .map(|p| p.to_string())
}

/// `["file:///home/u/proj"]` → `/home/u/proj`
fn parse_workspace_uris(s: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = val.as_array()?;
    let first = arr.iter().filter_map(|v| v.as_str()).next()?;
    strip_file_uri(first)
}

/// Antigravity writes `last_modified_time` as `2026-07-29 08:54:35.312249+00:00`
/// — a space instead of RFC3339's `T`, which `parse_from_rfc3339` rejects
/// outright. Accept both so the timestamp is not silently dropped.
fn parse_agy_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    let swapped = s.replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&swapped)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

impl Connector for AntigravityConnector {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn display_name(&self) -> &'static str {
        "Antigravity CLI"
    }

    fn detect(&self) -> bool {
        self.live_roots().next().is_some()
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.join("conversations")).collect()
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        // Bodies across every home. `.pb` files are encrypted with a key we do
        // not have, so they are skipped silently rather than surfaced as
        // parse errors on every scan.
        let mut bodies: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for root in self.live_roots() {
            let dir = root.join("conversations");
            let Ok(rd) = std::fs::read_dir(&dir) else {
                // A home that exists but cannot be listed must not abort the
                // scan of the others.
                continue;
            };
            let mut found: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "db"))
                .collect();
            found.sort();
            for body in found {
                let Some(id) = body.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                // The same conversation can exist in more than one home (the
                // IDE store and a backup of it). First home wins, matching the
                // scan order, so a session is never listed twice.
                if seen.insert(id.to_string()) {
                    bodies.push((root.clone(), body));
                }
            }
        }

        Ok(Box::new(AntigravityScanIter {
            connector: self,
            bodies,
            idx: 0,
        }))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        let Some((root, path)) = self.find_body(id) else {
            return Err(ConnectorError::NotFound(id.to_string()));
        };
        let mut session = load_body(&path, id)?;
        // The summaries index only covers the CLI home; the body's own blob is
        // the only project source for IDE sessions.
        if let Some(summary) = self.summary_meta(&root, id) {
            if summary.title.is_some() {
                session.title = summary.title;
            }
            if let Some(p) = summary.project {
                session.project_id = p;
            }
            if session.started_at.is_none() {
                session.started_at = summary.last_modified;
            }
            session.last_event_at = session.last_event_at.or(summary.last_modified);
        }
        Ok(session)
    }

    fn resume_cmd(&self, session: &Session) -> Option<Vec<String>> {
        // `--conversation <id>` is the flag verified against `agy 1.1.8`
        // (CONNECTORS.md §4) — not `--resume`, which the other three tools use.
        // Only offered for sessions in the home agy itself reads, since that is
        // the only store its picker consults.
        let home = write_home()?;
        session.source_path.starts_with(home.join("conversations")).then(|| {
            vec![
                "agy".to_string(),
                "--conversation".to_string(),
                session.id.clone(),
            ]
        })
    }

    fn inject(&self, _brief: &str, _dry_run: bool) -> ConnectorResult<InjectTarget> {
        Err(ConnectorError::Other(anyhow::anyhow!(
            "inject not yet implemented for Antigravity"
        )))
    }
}

struct AntigravityScanIter<'a> {
    connector: &'a AntigravityConnector,
    /// (home, body path) pairs, deduped by conversation id.
    bodies: Vec<(PathBuf, PathBuf)>,
    idx: usize,
}

impl Iterator for AntigravityScanIter<'_> {
    type Item = ConnectorResult<RawSession>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (root, db) = self.bodies.get(self.idx)?.clone();
            self.idx += 1;
            let id = match db.file_stem().and_then(|s| s.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let summary = self.connector.summary_meta(&root, &id).unwrap_or_default();
            let conn = match self.connector.open_body(&id, &db) {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            let blob = self.connector.blob_meta(&conn).unwrap_or_default();
            let first = conn
                .query_row(
                    "SELECT step_payload FROM steps ORDER BY idx LIMIT 1",
                    [],
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                )
                .ok()
                .flatten();
            let latest = conn
                .query_row(
                    "SELECT step_payload FROM steps ORDER BY idx DESC LIMIT 1",
                    [],
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                )
                .ok()
                .flatten();
            let started_at = first
                .as_deref()
                .and_then(step_time)
                .or(blob.created)
                .or(summary.last_modified);
            let last_event_at = latest
                .as_deref()
                .and_then(step_time)
                .or(summary.last_modified)
                .or(started_at);
            let project = summary.project.or(blob.project);
            return Some(Ok(RawSession {
                id,
                provider: "antigravity".to_string(),
                project_path: project.map(PathBuf::from),
                started_at,
                last_event_at,
                title: summary.title,
                source: None,
                source_path: db,
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

/// The role and text for one step, or `None` for step types that carry no
/// conversational turn (context snapshots, tool bookkeeping, telemetry).
///
/// Step types are the provider's own vocabulary, verified against real CLI and
/// IDE databases:
///   14  user input
///   15  model turn — the common case, text at `.20.1`
///   17  model turn that failed, message at `.24.3.1`
///   23  model summary, text at `.30.4`
///   5/7/8/9/21/90/98/99/101/132/138  context, tool calls, telemetry
fn step_message(step_type: i64, payload: &[u8]) -> Option<(Role, Option<String>)> {
    match step_type {
        14 => Some((Role::User, user_text(payload))),
        15 => Some((Role::Assistant, model_text(payload))),
        17 => Some((Role::Assistant, model_error(payload))),
        23 => {
            // A summary step with no text is pure bookkeeping; only surface it
            // when it actually carries prose.
            let text = model_summary(payload)?;
            Some((Role::Assistant, Some(text)))
        }
        _ => None,
    }
}

/// User text for step type 14: `payload.19.2`, falling back to the inner
/// user message at `payload.19.3`.
fn user_text(payload: &[u8]) -> Option<String> {
    let f19 = proto_descend(payload, &[19])?;
    non_empty(proto_string_field(f19, 2))
        .or_else(|| non_empty(proto_string_field(f19, 3)))
        .or_else(|| {
            // `.19.3.1` on the older encoding.
            let f3 = proto_descend(payload, &[19, 3])?;
            non_empty(proto_string_field(f3, 1))
        })
}

/// Model response for step type 15: `payload.20.1`.
///
/// `.20.3` is the model's private reasoning ("**Defining the Role**\n\nI'm
/// currently focused on...") and `.20.8` repeats `.20.1` verbatim. Only `.20.1`
/// is the turn the user actually saw, so reasoning is never presented as the
/// response — it would leak chain-of-thought into every brief and diff.
fn model_text(payload: &[u8]) -> Option<String> {
    let f20 = proto_descend(payload, &[20])?;
    non_empty(proto_string_field(f20, 1)).or_else(|| non_empty(proto_string_field(f20, 8)))
}

/// Model error message for step type 17: `payload.24.3.1`.
fn model_error(payload: &[u8]) -> Option<String> {
    let f3 = proto_descend(payload, &[24, 3])?;
    non_empty(proto_string_field(f3, 1))
}

/// Model summary for step type 23: `payload.30.4`.
fn model_summary(payload: &[u8]) -> Option<String> {
    let f30 = proto_descend(payload, &[30])?;
    non_empty(proto_string_field(f30, 4)).or_else(|| non_empty(proto_string_field(f30, 1)))
}

/// Protobuf reads yield `Some("")` for an absent-but-present string field;
/// treat that as absent so callers' fallbacks fire.
fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
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

    /// A step-type-15 payload: the real shape of a successful model turn.
    /// `.20.1` is the response, `.20.3` the private reasoning, `.20.8` a
    /// verbatim repeat of `.20.1`.
    fn model_turn_payload(secs: i64, answer: &str, reasoning: &str) -> Vec<u8> {
        let mut inner = str_field(1, answer);
        inner.extend(str_field(3, reasoning));
        inner.extend(str_field(8, answer));
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(15));
        out.extend(tag_buf(4, 0));
        out.extend(varint_buf(3));
        out.extend(timestamp_field(secs, 0));
        out.extend(msg_field(20, &inner));
        out
    }

    /// A step-type-23 payload: a model summary at `.30.4`.
    fn summary_payload(secs: i64, text: &str) -> Vec<u8> {
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(23));
        out.extend(tag_buf(4, 0));
        out.extend(varint_buf(3));
        out.extend(timestamp_field(secs, 0));
        out.extend(msg_field(30, &str_field(4, text)));
        out
    }

    /// A minimal step-type-98 (context) payload with no content fields.
    fn ctx_payload() -> Vec<u8> {
        let mut out = tag_buf(1, 0);
        out.extend(varint_buf(98));
        out
    }

    /// A `trajectory_metadata_blob` carrying a workspace URI and created time,
    /// the way the IDE store records them.
    fn metadata_blob(workspace: &str, secs: i64) -> Vec<u8> {
        let mut out = msg_field(1, &str_field(1, workspace));
        let mut created = tag_buf(1, 0);
        created.extend(varint_buf(secs as u64));
        out.extend(msg_field(2, &created));
        out
    }

    /// Build a conversation body. `blob` populates `trajectory_metadata_blob`
    /// when present, matching the IDE store.
    fn make_db_with(
        dir: &std::path::Path,
        id: &str,
        steps: &[(i64, Vec<u8>)],
        blob: Option<Vec<u8>>,
    ) -> PathBuf {
        let path = dir.join(format!("{}.db", id));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER, step_type INTEGER, status INTEGER, \
             has_subtrajectory INTEGER, metadata BLOB, error_details BLOB, \
             permissions BLOB, task_details BLOB, render_info BLOB, \
             step_payload BLOB, step_format INTEGER); \
             CREATE TABLE trajectory_metadata_blob (id TEXT DEFAULT \"main\", data BLOB);",
        )
        .unwrap();
        for (i, (step_type, payload)) in steps.iter().enumerate() {
            conn.execute(
                "INSERT INTO steps (idx, step_type, status, step_payload, step_format) \
                 VALUES (?1, ?2, 3, ?3, 0)",
                rusqlite::params![i as i64, *step_type, *payload],
            )
            .unwrap();
        }
        if let Some(b) = blob {
            conn.execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                rusqlite::params![b],
            )
            .unwrap();
        }
        path
    }

    fn make_db(dir: &std::path::Path, id: &str) -> PathBuf {
        make_db_with(
            dir,
            id,
            &[
                (14, user_payload(1_785_377_882)),
                (17, model_payload(1_785_377_885)),
                (98, ctx_payload()),
            ],
            None,
        )
    }

    /// A summaries index, with `title` and `preview` distinct so precedence is
    /// observable.
    fn make_summaries(root: &std::path::Path, rows: &[(&str, &str, &str, &str)]) {
        let conn = Connection::open(root.join("conversation_summaries.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (conversation_id TEXT, \
             title TEXT NOT NULL DEFAULT \"\", preview TEXT NOT NULL DEFAULT \"\", \
             step_count INTEGER NOT NULL DEFAULT 0, last_modified_time DATETIME, \
             workspace_uris TEXT NOT NULL DEFAULT \"\", status TEXT NOT NULL DEFAULT \"\", \
             source TEXT NOT NULL DEFAULT \"\", project_id TEXT NOT NULL DEFAULT \"\", \
             agent_name TEXT NOT NULL DEFAULT \"\", app_data_dir TEXT NOT NULL DEFAULT \"\", \
             PRIMARY KEY (conversation_id));",
        )
        .unwrap();
        for (id, title, preview, workspace) in rows {
            conn.execute(
                "INSERT INTO conversation_summaries \
                 (conversation_id, title, preview, last_modified_time, workspace_uris) \
                 VALUES (?1, ?2, ?3, '2026-08-17 11:53:35.312249+00:00', ?4)",
                rusqlite::params![
                    id,
                    title,
                    preview,
                    format!("[\"file://{}\"]", workspace)
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn test_walker_extracts_user_text_and_model_error() {
        let user = user_payload(1_785_377_882);
        assert_eq!(user_text(&user).as_deref(), Some("say hi"));
        assert_eq!(step_time(&user), Utc.timestamp_opt(1_785_377_882, 0).single());

        let model = model_payload(1_785_377_885);
        assert_eq!(model_error(&model).as_deref(), Some("quota exhausted"));
        assert_eq!(model_error(&user), None, "no error in a user step");

        let ctx = ctx_payload();
        assert_eq!(user_text(&ctx), None, "context steps carry no text");
    }

    /// The bug this locks: a successful model turn (step 15) used to decode to
    /// nothing, because only the error field `.24.3.1` was read. Every session
    /// therefore loaded as a single user message with the answer discarded.
    #[test]
    fn test_model_turn_prefers_response_over_reasoning() {
        let p = model_turn_payload(
            1_785_377_890,
            "Here is your standup summary.",
            "**Defining the Role**\n\nI'm currently focused on...",
        );
        assert_eq!(
            model_text(&p).as_deref(),
            Some("Here is your standup summary."),
            "the response at .20.1 is the turn the user saw"
        );
        let (role, text) = step_message(15, &p).expect("step 15 is a turn");
        assert_eq!(role, Role::Assistant);
        assert_eq!(text.as_deref(), Some("Here is your standup summary."));
        assert!(
            !text.unwrap().contains("Defining the Role"),
            "private reasoning at .20.3 must never be surfaced as the response"
        );
    }

    #[test]
    fn test_step_message_classifies_step_types() {
        let summary = summary_payload(1_785_377_900, "Banknote OCR troubleshooting");
        assert_eq!(
            step_message(23, &summary).map(|(r, t)| (r, t.unwrap())),
            Some((Role::Assistant, "Banknote OCR troubleshooting".to_string()))
        );
        // Tool/telemetry steps carry no turn and must not renumber the
        // transcript by appearing as empty messages.
        for t in [5, 7, 8, 9, 21, 90, 98, 99, 101, 132, 138] {
            assert!(
                step_message(t, &ctx_payload()).is_none(),
                "step type {} is not a conversational turn",
                t
            );
        }
    }

    #[test]
    fn test_parse_agy_time_accepts_space_separated_stamps() {
        // The real store writes a space, not RFC3339's `T`. Rejecting it
        // silently dropped every summary timestamp.
        let space = parse_agy_time("2026-07-29 08:54:35.312249+00:00");
        let rfc = parse_agy_time("2026-07-29T08:54:35.312249+00:00");
        assert!(space.is_some(), "space-separated stamps must parse");
        assert_eq!(space, rfc, "both spellings denote the same instant");
        assert_eq!(space.unwrap().to_rfc3339(), "2026-07-29T08:54:35.312249+00:00");
        assert!(parse_agy_time("not a time").is_none());
    }

    #[test]
    fn test_title_column_wins_over_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("antigravity-cli");
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let renamed = "3ff51afb-4660-47ee-884c-56498f4b0222";
        let untitled = "4ff51afb-4660-47ee-884c-56498f4b0222";
        make_db(&root.join("conversations"), renamed);
        make_db(&root.join("conversations"), untitled);
        make_summaries(
            &root,
            &[
                (renamed, "My explicit rename", "say hi", "/tmp/proj"),
                (untitled, "", "say hi", "/tmp/proj"),
            ],
        );

        let connector = AntigravityConnector::with_root(root);
        // An explicit rename must win, or write-back can never see it.
        assert_eq!(
            connector.load(renamed).unwrap().title.as_deref(),
            Some("My explicit rename")
        );
        // With no rename, the preview is the best available name.
        assert_eq!(
            connector.load(untitled).unwrap().title.as_deref(),
            Some("say hi")
        );
        assert_eq!(
            connector.load(renamed).unwrap().project_id,
            "/tmp/proj",
            "workspace_uris supplies the project"
        );
    }

    /// The IDE store has no summaries index at all; the project and creation
    /// time have to come from the conversation's own metadata blob or those
    /// sessions show up as "(unknown)".
    #[test]
    fn test_ide_session_reads_project_from_metadata_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("antigravity-ide");
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let id = "9619d594-45df-4b29-96ee-2b60cb6f3f72";
        make_db_with(
            &root.join("conversations"),
            id,
            &[
                (14, user_payload(1_785_377_882)),
                (15, model_turn_payload(1_785_377_890, "done", "thinking")),
            ],
            Some(metadata_blob("file:///Users/h/bankNotes-OCR", 1_785_823_884)),
        );

        let connector = AntigravityConnector::with_root(root);
        assert!(!root_has_summaries(&connector), "IDE store has no index");

        let raw: Vec<_> = connector.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert_eq!(raw.len(), 1);
        assert_eq!(
            raw[0].project_path.as_deref(),
            Some(std::path::Path::new("/Users/h/bankNotes-OCR"))
        );

        let s = connector.load(id).unwrap();
        assert_eq!(s.project_id, "/Users/h/bankNotes-OCR");
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].text.as_deref(), Some("done"));
    }

    fn root_has_summaries(c: &AntigravityConnector) -> bool {
        c.roots
            .iter()
            .any(|r| r.join("conversation_summaries.db").is_file())
    }

    /// Sessions live in more than one home. Scanning only one hid most of
    /// them; scanning all of them must not list the same id twice.
    #[test]
    fn test_scan_spans_homes_and_dedupes_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("antigravity-cli");
        let ide = tmp.path().join("antigravity-ide");
        std::fs::create_dir_all(cli.join("conversations")).unwrap();
        std::fs::create_dir_all(ide.join("conversations")).unwrap();

        let only_cli = "1ff51afb-4660-47ee-884c-56498f4b0222";
        let only_ide = "2ff51afb-4660-47ee-884c-56498f4b0222";
        let in_both = "3ff51afb-4660-47ee-884c-56498f4b0222";
        make_db(&cli.join("conversations"), only_cli);
        make_db(&cli.join("conversations"), in_both);
        make_db(&ide.join("conversations"), only_ide);
        make_db(&ide.join("conversations"), in_both);

        let connector = AntigravityConnector::with_roots(vec![cli, ide]);
        let ids: Vec<String> = connector
            .scan()
            .unwrap()
            .filter_map(|r| r.ok())
            .map(|r| r.id)
            .collect();
        assert_eq!(ids.len(), 3, "three distinct sessions across two homes");
        for id in [only_cli, only_ide, in_both] {
            assert!(ids.contains(&id.to_string()), "{} must be listed", id);
        }
    }

    /// Encrypted `.pb` bodies (the desktop/backup stores) have no key we can
    /// use. They must be skipped, not reported as corrupt on every scan.
    #[test]
    fn test_encrypted_pb_bodies_are_skipped_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("antigravity");
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        std::fs::write(
            root.join("conversations/010cadf6-7093-4ba1-9057-2e2308309922.pb"),
            [0xbe, 0x49, 0xb3, 0x87, 0x91, 0x1e, 0xff, 0xc1],
        )
        .unwrap();
        let readable = "3ff51afb-4660-47ee-884c-56498f4b0222";
        make_db(&root.join("conversations"), readable);

        let connector = AntigravityConnector::with_root(root);
        let results: Vec<_> = connector.scan().unwrap().collect();
        assert_eq!(results.len(), 1, "only the readable .db is listed");
        assert!(results[0].is_ok(), "no error is raised for the .pb");
        assert_eq!(results[0].as_ref().unwrap().id, readable);
    }

    /// A conversation id reaches `load` straight from the CLI. It must not be
    /// able to walk out of the conversations directory.
    #[test]
    fn test_load_rejects_path_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("antigravity-cli");
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let connector = AntigravityConnector::with_root(root);
        for bad in ["../../etc/passwd", "a/b", "", "with space"] {
            assert!(
                matches!(
                    connector.load(bad),
                    Err(ConnectorError::NotFound(_))
                ),
                "{:?} must be refused",
                bad
            );
        }
    }

    /// Real-data check (ignored): run against the actual antigravity store
    /// when present, proving the protobuf decode matches what the real binary
    /// wrote. HANDOFF §5: unit fixtures can pass while real data fails.
    #[test]
    #[ignore = "requires the operator's real antigravity data"]
    fn test_load_real_antigravity_conversation() {
        let connector = AntigravityConnector::new();
        if !connector.detect() {
            eprintln!("skipping: no antigravity data on this machine");
            return;
        }
        let results: Vec<_> = connector.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert!(!results.is_empty(), "real machine has antigravity sessions");
        let mut assistant_turns = 0usize;
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
            assistant_turns += session
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant && m.text.is_some())
                .count();
        }
        // The decode regression this guards: every session used to load with
        // the model's answers dropped, so a passing "not empty" assertion hid
        // a transcript that was one message long.
        assert!(
            assistant_turns > 0,
            "real sessions must decode model responses, not just user input"
        );
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
