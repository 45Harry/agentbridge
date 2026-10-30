//! Write back (spec 0001, AC-17 to AC-25): turns made in a continuation are
//! added to the end of the session it continued (its origin), in the
//! origin's own tool, so opening the origin there shows the whole story.
//!
//! A session is a continuation when its own transcript holds a call to
//! agentbridge's `read_session`; the origin is the session it read last. The
//! turns written are the ones after that call's result, without any
//! agentbridge tool call or its output, converted to the origin tool's
//! format and opened by a note turn ("continued in <tool> on <date>").
//!
//! The gates, which are the narrow form of the project rule:
//! - only added at the end: no existing line, record or row is changed;
//! - never while the origin's tool is open (that origin waits for a later run);
//! - the origin's file or database is backed up before the first write of a
//!   run;
//! - every written turn is recorded in a `writeback` manifest row (its id, and
//!   a fingerprint of its content), so a run never writes a turn twice, a
//!   written turn is never taken for new work, and `unsync` removes exactly
//!   those turns.

use crate::connector::Registry;
use crate::index::{discover, Index, IndexEntry};
use crate::mcp::CopyFilter;
use crate::mcp_install::{display_name, OpenCheck};
use crate::model::{Message, Role, Session};
use crate::sync::{LinkRecord, KIND_WRITEBACK};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The most origins a chain is followed up through.
pub const MAX_CHAIN: usize = 10;

/// The field every record agentbridge appends to a JSONL session carries.
pub const MARKER_FIELD: &str = "agentbridge";

/// Fixed namespace for written turn ids. Never changes: the same turn always
/// gets the same id.
const NAMESPACE: uuid::Uuid = uuid::uuid!("3c1a7f52-6d0e-5b8a-9f41-0b7e2d6c8a19");

/// True when `name` is agentbridge's `read_session` as some tool names it:
/// `mcp__agentbridge__read_session` (Claude Code), `agentbridge_read_session`
/// (OpenCode), or Codex's form. Matched loosely on purpose until each tool's
/// exact naming is recorded from a live run (spec 0001 build plan step 12).
pub fn is_read_call(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("agentbridge") && n.ends_with("read_session")
}

/// Any of agentbridge's own MCP tools. Their calls and outputs are never
/// written back: the origin must not grow by its own content.
fn is_agentbridge_tool(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("agentbridge") && (n.ends_with("read_session") || n.ends_with("list_sessions"))
}

/// The session id and tool a `read_session` call names. Claude Code and
/// Codex keep the arguments as the tool input; OpenCode keeps them under the
/// part's `state.input`.
fn read_target(m: &Message) -> Option<(String, Option<String>)> {
    let input = m.tool_input.as_ref()?;
    let args = if input.get("id").is_some() { input } else { input.get("input")? };
    let id = args.get("id")?.as_str()?.trim().to_string();
    let tool = args.get("tool").and_then(|t| t.as_str()).map(str::to_string);
    (!id.is_empty()).then_some((id, tool))
}

/// What a turn says, for telling a written turn apart from new work. Not the
/// time: some stores move a timestamp by a millisecond to keep order.
pub fn fingerprint(m: &Message) -> String {
    format!(
        "{}|{}|{}|{}",
        m.text.as_deref().unwrap_or("").trim(),
        m.tool_name.as_deref().unwrap_or(""),
        m.tool_input.as_ref().map(|v| v.to_string()).unwrap_or_default(),
        m.tool_result.as_ref().map(result_text).unwrap_or_default(),
    )
}

fn result_text(v: &Value) -> String {
    crate::convert::tool_result_text(Some(v))
}

/// A turn worth carrying: something was said or a tool was used.
fn says_something(m: &Message) -> bool {
    let text = m.text.as_deref().unwrap_or("");
    let has_text = !text.trim().is_empty() && !crate::label::is_bookkeeping(text);
    has_text || m.tool_name.is_some() || m.tool_input.is_some() || m.tool_result.is_some()
}

/// One continuation as found in its own transcript.
#[derive(Debug, Clone)]
struct Continuation {
    entry: IndexEntry,
    /// The id (and tool, when given) of the session it read last.
    origin_id: String,
    origin_tool: Option<String>,
    /// Its turns after the last `read_session` result, filtered, in order.
    turns: Vec<Message>,
}

fn key(tool: &str, id: &str) -> String {
    format!("{}:{}", tool, id)
}

/// The turns of `session` that are new work after its last `read_session`.
/// `None` when it read no session.
fn after_last_read(session: &Session, written: &HashSet<String>) -> Option<(String, Option<String>, Vec<Message>)> {
    let msgs = &session.messages;
    let (call_idx, (origin_id, origin_tool)) = msgs
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| m.tool_name.as_deref().filter(|n| is_read_call(n)).and_then(|_| read_target(m)).map(|t| (i, t)))?;
    // The call's result, when the tool keeps it as its own message.
    let start = msgs[call_idx + 1..]
        .iter()
        .position(|m| m.role == Role::Tool && m.tool_name.as_deref().is_none_or(is_read_call))
        .map(|p| call_idx + 1 + p + 1)
        .unwrap_or(call_idx + 1);
    let turns = msgs[start..]
        .iter()
        .filter(|m| !m.tool_name.as_deref().is_some_and(is_agentbridge_tool))
        .filter(|m| says_something(m))
        .filter(|m| !written.contains(&fingerprint(m)))
        .cloned()
        .collect();
    Some((origin_id, origin_tool, turns))
}

/// Could this session hold a `read_session` call? A cheap look so a run does
/// not load every session on the machine.
fn may_have_read(e: &IndexEntry, opencode_readers: &HashSet<String>) -> bool {
    match e.provider.as_str() {
        "claude-code" | "codex-cli" => fs::read(&e.source_path)
            .map(|b| b.windows(12).any(|w| w == b"read_session"))
            .unwrap_or(false),
        "opencode" => opencode_readers.contains(&e.id),
        // The tool call's arguments are stored as plain JSON text inside the
        // step's protobuf, so the bytes show it.
        // agy keeps its databases in WAL mode, so a recent step may still be
        // in the `-wal` file.
        "antigravity" => {
            let wal = PathBuf::from(format!("{}-wal", e.source_path.display()));
            [e.source_path.clone(), wal]
                .iter()
                .any(|p| fs::read(p).is_ok_and(|b| b.windows(12).any(|w| w == b"read_session")))
        }
        _ => false,
    }
}

/// OpenCode sessions with a part that mentions `read_session`.
fn opencode_readers(index: &Index) -> HashSet<String> {
    let Some(db) = index.entries.iter().find(|e| e.provider == "opencode").map(|e| e.source_path.clone()) else {
        return HashSet::new();
    };
    let Ok(conn) = crate::connectors::open_read_only(&db) else {
        return HashSet::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT DISTINCT session_id FROM part WHERE data LIKE '%read_session%'") else {
        return HashSet::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Rows

/// What a `writeback` row records, kept in its `entry`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Written {
    /// The continuation's file or database, for the report.
    pub continuation_source_path: PathBuf,
    /// The continuation's last ordinal already written; `None` before any.
    pub upto_ordinal: Option<u64>,
    /// Ids of every turn written (JSONL `uuid` or marker id, database row
    /// ids), the note turns included.
    pub written: Vec<String>,
    /// Fingerprints of every turn written, the note turns included.
    #[serde(default)]
    pub keys: Vec<String>,
    /// For a JSONL origin: its length just before the first write back.
    pub bytes_before: Option<u64>,
    /// For a JSONL origin: a newline was added first because the file did
    /// not end with one; `unsync` takes it away again.
    #[serde(default)]
    pub added_newline: bool,
}

fn rows() -> Vec<LinkRecord> {
    crate::sync::read_manifest_kind(KIND_WRITEBACK)
}

fn written_of(r: &LinkRecord) -> Written {
    r.entry.clone().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn find_row<'r>(rows: &'r [LinkRecord], origin: &IndexEntry, cont: &IndexEntry) -> Option<&'r LinkRecord> {
    rows.iter().find(|r| {
        r.target_provider == origin.provider
            && r.session_id == origin.id
            && r.source_provider == cont.provider
            && r.cache.as_os_str() == cont.id.as_str()
    })
}

fn turn_id(origin: &IndexEntry, cont: &IndexEntry, ordinal: u64, note: bool) -> uuid::Uuid {
    let name = format!(
        "{}:{}:{}:{}:{}{}",
        origin.provider,
        origin.id,
        cont.provider,
        cont.id,
        ordinal,
        if note { ":note" } else { "" }
    );
    uuid::Uuid::new_v5(&NAMESPACE, name.as_bytes())
}

/// The note that opens every batch.
fn note_turn(cont: &IndexEntry, first: &Message) -> Message {
    let date = first.timestamp.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "an unknown date".to_string());
    Message {
        session_id: first.session_id.clone(),
        ordinal: first.ordinal,
        role: Role::Assistant,
        timestamp: first.timestamp,
        text: Some(format!("continued in {} on {}", display_name(&cont.provider), date)),
        tool_name: None,
        tool_input: None,
        tool_result: None,
        parent_ordinal: None,
    }
}

// ---------------------------------------------------------------------------
// Planning

/// One batch to write: these turns of `cont` into `origin`.
#[derive(Debug, Clone)]
pub struct Batch {
    pub origin: IndexEntry,
    pub cont: IndexEntry,
    /// The note first, then the turns.
    pub turns: Vec<(Message, uuid::Uuid)>,
    /// The highest continuation ordinal in this batch.
    pub upto: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub batches: Vec<Batch>,
    /// Continuations whose origin could not be resolved, and why.
    pub skipped: Vec<String>,
}

fn plan(registry: &Registry) -> Plan {
    let mut out = Plan::default();
    let index = discover(registry);
    let copies = CopyFilter::load();
    let natives: Vec<IndexEntry> = index.entries.iter().filter(|e| !copies.is_copy(e)).cloned().collect();
    let rows = rows();
    // Turns agentbridge wrote, never new work of the session they sit in.
    let written: HashSet<String> = rows.iter().flat_map(|r| written_of(r).keys).collect();
    let oc_readers = opencode_readers(&index);

    let mut conts: HashMap<String, Continuation> = HashMap::new();
    for e in natives.iter().filter(|e| may_have_read(e, &oc_readers)) {
        let Ok(session) = crate::mcp::load_entry(registry, e) else {
            continue;
        };
        if let Some((origin_id, origin_tool, turns)) = after_last_read(&session, &written) {
            conts.insert(key(&e.provider, &e.id), Continuation { entry: e.clone(), origin_id, origin_tool, turns });
        }
    }

    // Each continuation's direct origin, resolved the way read_session
    // resolves an id.
    let mut origin_of: HashMap<String, IndexEntry> = HashMap::new();
    for (k, c) in &conts {
        match crate::mcp::resolve_entry(&index.entries, &c.origin_id, c.origin_tool.as_deref()) {
            Ok(o) if key(&o.provider, &o.id) != *k => {
                origin_of.insert(k.clone(), o.clone());
            }
            Ok(_) => {}
            Err(why) => out.skipped.push(format!("{} {}: origin not resolved: {}", c.entry.provider, c.entry.id, why)),
        }
    }

    let mut keys: Vec<&String> = conts.keys().collect();
    keys.sort();
    for k in keys {
        let c = &conts[k];
        if c.turns.is_empty() {
            continue;
        }
        // Up the chain: the direct origin first, then its origin, and so on.
        let mut seen: HashSet<String> = HashSet::from([k.clone()]);
        let mut at = k.clone();
        for _ in 0..MAX_CHAIN {
            let Some(origin) = origin_of.get(&at) else { break };
            let ok = key(&origin.provider, &origin.id);
            if !seen.insert(ok.clone()) {
                break;
            }
            let upto = find_row(&rows, origin, &c.entry).and_then(|r| written_of(r).upto_ordinal);
            let new: Vec<&Message> = c.turns.iter().filter(|m| upto.is_none_or(|u| m.ordinal > u)).collect();
            if let Some(first) = new.first() {
                let mut turns = vec![(note_turn(&c.entry, first), turn_id(origin, &c.entry, first.ordinal, true))];
                turns.extend(new.iter().map(|m| ((*m).clone(), turn_id(origin, &c.entry, m.ordinal, false))));
                out.batches.push(Batch {
                    origin: origin.clone(),
                    cont: c.entry.clone(),
                    turns,
                    upto: new.iter().map(|m| m.ordinal).max().unwrap_or(0),
                });
            }
            at = ok;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running

#[derive(Debug, Default)]
pub struct WritebackReport {
    pub dry_run: bool,
    /// (origin tool, origin id, continuation tool, continuation id, turns
    /// written including the note, destination).
    pub written: Vec<(String, String, String, String, usize, PathBuf)>,
    /// Origins left for a later run, and why.
    pub waiting: Vec<String>,
    pub errors: Vec<String>,
    /// Another agentbridge run held `sync.lock`; this one did nothing.
    pub busy: bool,
}

/// Write every continuation's new turns into its origin chain.
pub fn run(registry: &Registry, dry_run: bool, is_open: &OpenCheck) -> WritebackReport {
    let mut report = WritebackReport { dry_run, ..Default::default() };
    // Nothing to do: touch nothing, not even the lock.
    if plan(registry).batches.is_empty() {
        return report;
    }
    let _lock = if dry_run {
        None
    } else {
        match crate::sync::acquire_run_lock() {
            Some(l) => Some(l),
            None => {
                report.busy = true;
                return report;
            }
        }
    };
    // Planned again under the lock, so a run that just finished is seen.
    let plan = plan(registry);
    report.waiting.extend(plan.skipped);
    let mut rows = rows();
    let mut backed_up: HashSet<PathBuf> = HashSet::new();

    for batch in plan.batches {
        let o = &batch.origin;
        let label = format!("{} {} (continued in {} {})", o.provider, o.id, batch.cont.provider, batch.cont.id);
        if is_open(&o.provider) {
            report.waiting.push(format!("{}: {} is open; written on a later run", label, display_name(&o.provider)));
            continue;
        }
        let row = find_row(&rows, o, &batch.cont).cloned();
        let jsonl = matches!(o.provider.as_str(), "claude-code" | "codex-cli");
        if jsonl
            && let Some(r) = &row
            && r.inode != 0
            && crate::sync::file_identity(&o.source_path) != r.inode
        {
            report.waiting.push(format!("{}: {} was replaced since the last write back; skipped", label, o.source_path.display()));
            continue;
        }
        let dest = o.source_path.clone();
        if dry_run {
            report.written.push((o.provider.clone(), o.id.clone(), batch.cont.provider.clone(), batch.cont.id.clone(), batch.turns.len(), dest));
            continue;
        }
        if !backed_up.contains(&dest) {
            if let Err(e) = backup(&o.provider, &dest) {
                report.errors.push(format!("{}: backup failed, nothing written: {}", label, e));
                continue;
            }
            backed_up.insert(dest.clone());
        }
        let mut w = row.as_ref().map(written_of).unwrap_or_default();
        w.continuation_source_path = batch.cont.source_path.clone();
        let tag = key(&batch.cont.provider, &batch.cont.id);
        let result = match o.provider.as_str() {
            "claude-code" => append_claude(&dest, o, &batch, &tag),
            "codex-cli" => append_codex(&dest, &batch, &tag),
            "opencode" => append_opencode(&dest, o, &batch, &tag),
            "antigravity" => append_antigravity(&dest, &batch),
            other => Err(format!("write back into {} is not supported", other)),
        };
        match result {
            Ok(done) => {
                if w.bytes_before.is_none() {
                    w.bytes_before = done.bytes_before;
                    w.added_newline = done.added_newline;
                }
                w.upto_ordinal = Some(batch.upto);
                w.written.extend(done.ids);
                w.keys.extend(batch.turns.iter().map(|(m, _)| fingerprint(m)));
                rows.retain(|r| !(r.target_provider == o.provider && r.session_id == o.id && r.source_provider == batch.cont.provider && r.cache.as_os_str() == batch.cont.id.as_str()));
                rows.push(LinkRecord {
                    dest: dest.clone(),
                    cache: PathBuf::from(&batch.cont.id),
                    session_id: o.id.clone(),
                    source_provider: batch.cont.provider.clone(),
                    target_provider: o.provider.clone(),
                    project: o.project_path.clone().unwrap_or_default(),
                    inode: if jsonl { crate::sync::file_identity(&dest) } else { 0 },
                    message_count: 0,
                    title: None,
                    kind: KIND_WRITEBACK.to_string(),
                    entry: serde_json::to_value(&w).ok(),
                    created_file: false,
                    created_parent: false,
                    blank_before: false,
                    undo: None,
                });
                // Right after each batch, so a crash later never loses track.
                if let Err(e) = crate::sync::write_manifest_kind(KIND_WRITEBACK, &rows) {
                    report.errors.push(format!("{}: written, but the manifest could not be updated: {}", label, e));
                }
                report.written.push((o.provider.clone(), o.id.clone(), batch.cont.provider.clone(), batch.cont.id.clone(), batch.turns.len(), dest));
            }
            Err(e) => {
                let e = if e.to_lowercase().contains("locked") || e.to_lowercase().contains("busy") {
                    format!("{} is busy writing; written on a later run", display_name(&o.provider))
                } else {
                    e
                };
                report.errors.push(format!("{}: {}", label, e));
            }
        }
    }
    report
}

fn backup(tool: &str, path: &Path) -> Result<PathBuf, String> {
    match tool {
        "opencode" => crate::opencode_write::backup(path).map_err(|e| e.to_string()),
        "antigravity" => crate::antigravity_write::backup(path).map_err(|e| e.to_string()),
        _ => backup_session_file(path).map_err(|e| e.to_string()),
    }
}

/// Copy a JSONL session next to itself as `<name>.jsonl.agentbridge-backup-
/// <stamp>`. The name must not end in `.jsonl`: Claude Code and Codex list
/// every `.jsonl` in their session folders, so a backup named like a session
/// shows up as a second copy of it (found live on a copy of a real Codex
/// store, 2026-10-30).
pub fn backup_session_file(path: &Path) -> std::io::Result<PathBuf> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f");
    let mut dest = path.with_file_name(format!("{}.agentbridge-backup-{}", name, stamp));
    let mut n = 1;
    while dest.exists() {
        dest = path.with_file_name(format!("{}.agentbridge-backup-{}-{}", name, stamp, n));
        n += 1;
    }
    fs::copy(path, &dest)?;
    Ok(dest)
}

/// What one append did.
struct Done {
    ids: Vec<String>,
    bytes_before: Option<u64>,
    added_newline: bool,
}

/// Read a JSONL origin for appending: its bytes, and whether a newline has to
/// be added first so the new records start on their own line.
fn open_jsonl(path: &Path) -> Result<(Vec<u8>, bool), String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let needs_newline = !bytes.is_empty() && !bytes.ends_with(b"\n");
    Ok((bytes, needs_newline))
}

/// Append whole lines, checking the file did not change since it was read.
fn append_lines(path: &Path, before: &[u8], needs_newline: bool, lines: &[Value]) -> Result<(), String> {
    use std::io::Write;
    let now = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if now != before.len() as u64 {
        return Err(format!("{} changed while agentbridge was writing; written on a later run", path.display()));
    }
    let mut body = String::new();
    if needs_newline {
        body.push('\n');
    }
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap_or_default());
        body.push('\n');
    }
    let mut f = fs::OpenOptions::new().append(true).open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    f.write_all(body.as_bytes()).map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

fn marker(tag: &str, id: &str) -> Value {
    json!({ "writeback_of": tag, "id": id })
}

fn append_claude(path: &Path, origin: &IndexEntry, batch: &Batch, tag: &str) -> Result<Done, String> {
    let (before, needs_newline) = open_jsonl(path)?;
    let tail = crate::connectors::tail_records(path);
    let mut parent: Option<String> = tail.iter().find_map(|r| r.get("uuid").and_then(|u| u.as_str()).map(str::to_string));
    let last = |field: &str| tail.iter().find_map(|r| r.get(field).cloned());
    let cwd = last("cwd").unwrap_or_else(|| json!(origin.project_path.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()));
    let version = last("version").unwrap_or(json!("2.1.220"));
    let git_branch = last("gitBranch").unwrap_or(json!(""));
    let model = tail
        .iter()
        .find_map(|r| r.get("message").and_then(|m| m.get("model")).cloned())
        .unwrap_or(json!("unknown"));
    let mut pending_tool: Option<String> = None;
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    for (m, id) in &batch.turns {
        let uuid = id.to_string();
        let ts = crate::convert::format_timestamp_z(m.timestamp);
        let mut rec = json!({
            "parentUuid": parent,
            "isSidechain": false,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": origin.id,
            "version": version,
            "gitBranch": git_branch,
            "uuid": uuid,
            "timestamp": ts,
            MARKER_FIELD: marker(tag, &uuid),
        });
        let simple = id.simple().to_string();
        let extra = match m.role {
            Role::User => json!({ "type": "user", "message": { "role": "user", "content": m.text.clone().unwrap_or_default() } }),
            Role::Tool => json!({ "type": "user", "message": { "role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": pending_tool.take().unwrap_or_else(|| format!("toolu_{}", simple)),
                "content": crate::convert::tool_result_text(m.tool_result.as_ref()),
            }] } }),
            _ => {
                let content = match &m.tool_name {
                    Some(tool) => {
                        let tool_id = format!("toolu_{}", simple);
                        pending_tool = Some(tool_id.clone());
                        json!([{ "type": "tool_use", "id": tool_id, "name": tool, "input": m.tool_input.clone().unwrap_or(json!({})) }])
                    }
                    None => json!([{ "type": "text", "text": m.text.clone().unwrap_or_default() }]),
                };
                json!({
                    "type": "assistant",
                    "requestId": format!("req_{}", simple),
                    "message": {
                        "id": format!("msg_{}", simple), "type": "message", "role": "assistant", "model": model,
                        "content": content, "stop_reason": Value::Null, "stop_sequence": Value::Null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                })
            }
        };
        if let (Some(r), Some(x)) = (rec.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                r.insert(k.clone(), v.clone());
            }
        }
        parent = Some(uuid.clone());
        ids.push(uuid);
        lines.push(rec);
    }
    append_lines(path, &before, needs_newline, &lines)?;
    Ok(Done { ids, bytes_before: Some(before.len() as u64), added_newline: needs_newline })
}

fn append_codex(path: &Path, batch: &Batch, tag: &str) -> Result<Done, String> {
    let (before, needs_newline) = open_jsonl(path)?;
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    let mut pending_call: Option<String> = None;
    for (m, id) in &batch.turns {
        let id_s = id.to_string();
        let ts = crate::convert::format_timestamp_z(m.timestamp);
        let mark = marker(tag, &id_s);
        let said = m.text.as_deref().filter(|t| !t.trim().is_empty());
        // The screen's view, for turns that say something: without it the
        // turn would not show when the session is opened in Codex.
        match (m.role, said, m.tool_name.is_some()) {
            (Role::User, Some(text), _) => lines.push(json!({ "timestamp": ts, "type": "event_msg", MARKER_FIELD: mark,
                "payload": { "type": "user_message", "message": text, "images": [], "local_images": [], "text_elements": [] } })),
            (Role::Assistant, Some(text), false) => lines.push(json!({ "timestamp": ts, "type": "event_msg", MARKER_FIELD: mark,
                "payload": { "type": "agent_message", "message": text } })),
            _ => {}
        }
        let call_id = format!("call_{}", id.simple());
        let payload = match m.role {
            Role::User => json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": m.text.clone().unwrap_or_default() }] }),
            Role::Tool => json!({ "type": "function_call_output", "call_id": pending_call.take().unwrap_or(call_id),
                "output": crate::convert::tool_result_text(m.tool_result.as_ref()) }),
            _ => match &m.tool_name {
                Some(tool) => {
                    pending_call = Some(call_id.clone());
                    json!({ "type": "function_call", "name": tool, "call_id": call_id,
                        "arguments": serde_json::to_string(&m.tool_input.clone().unwrap_or(json!({}))).unwrap_or_default() })
                }
                None => json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": m.text.clone().unwrap_or_default() }] }),
            },
        };
        lines.push(json!({ "timestamp": ts, "type": "response_item", MARKER_FIELD: mark, "payload": payload }));
        ids.push(id_s);
    }
    append_lines(path, &before, needs_newline, &lines)?;
    // So Codex lists the session as recently active.
    if let Some(home) = crate::connectors::codex_cli::config_home() {
        let db = home.join("state_5.sqlite");
        if db.exists() {
            let last = batch.turns.iter().filter_map(|(m, _)| m.timestamp).max().unwrap_or_else(chrono::Utc::now);
            if let Ok(conn) = rusqlite::Connection::open(&db) {
                let p = path.to_string_lossy().to_string();
                // By path, or by thread id: Codex's own threads use the
                // session id as their id.
                let thread = &batch.origin.id;
                let both = conn.execute(
                    "UPDATE threads SET updated_at = MAX(updated_at, ?1), updated_at_ms = MAX(COALESCE(updated_at_ms, 0), ?2) WHERE rollout_path = ?3 OR id = ?4",
                    rusqlite::params![last.timestamp(), last.timestamp_millis(), p, thread],
                );
                if both.is_err() {
                    let _ = conn.execute(
                        "UPDATE threads SET updated_at = MAX(updated_at, ?1) WHERE rollout_path = ?2 OR id = ?3",
                        rusqlite::params![last.timestamp(), p, thread],
                    );
                }
            }
        }
    }
    Ok(Done { ids, bytes_before: Some(before.len() as u64), added_newline: needs_newline })
}

/// A turn as plain text, for the stores where agentbridge writes tool calls
/// as words rather than in the tool's own tool shape.
fn as_text(m: &Message) -> String {
    match (m.role, &m.tool_name) {
        (Role::Tool, name) => format!(
            "{} output:\n{}",
            name.as_deref().unwrap_or("tool"),
            crate::convert::tool_result_text(m.tool_result.as_ref())
        ),
        (_, Some(name)) => format!(
            "called {}: {}",
            name,
            m.tool_input.as_ref().map(|v| v.to_string()).unwrap_or_default()
        ),
        _ => m.text.clone().unwrap_or_default(),
    }
}

fn append_opencode(db: &Path, origin: &IndexEntry, batch: &Batch, tag: &str) -> Result<Done, String> {
    let mut conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
    conn.busy_timeout(crate::connectors::BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    // OpenCode picks a session's latest message by comparing ids. Every new
    // id extends the current highest one plus a counter, so the batch sorts
    // after every message there now, in its own order, and before any id
    // OpenCode makes later (those differ earlier).
    let (base, mut last_ts): (String, i64) = tx
        .query_row(
            "SELECT id, time_created FROM message WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            [&origin.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("cannot read session {}: {}", origin.id, e))?;
    let directory = origin.project_path.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mut ids = Vec::new();
    let mut parent: Option<String> = None;
    for (i, (m, id)) in batch.turns.iter().enumerate() {
        let msg_id = format!("{}_ab{:04}{}", base, i, &id.simple().to_string()[..8]);
        let part_id = format!("prt_{}_p0", msg_id.trim_start_matches("msg_"));
        let ts = m.timestamp.map(|t| t.timestamp_millis()).unwrap_or(last_ts).max(last_ts + 1);
        last_ts = ts;
        let user = m.role == Role::User;
        let mut data = if user {
            json!({ "role": "user", "time": { "created": ts }, "agent": "build" })
        } else {
            json!({ "role": "assistant", "mode": "build", "agent": "build", "path": { "cwd": directory, "root": directory },
                "cost": 0, "tokens": { "total": 0, "input": 0, "output": 0, "reasoning": 0, "cache": { "write": 0, "read": 0 } },
                "time": { "created": ts, "completed": ts }, "finish": "end-turn" })
        };
        if let Some(obj) = data.as_object_mut() {
            obj.insert(MARKER_FIELD.to_string(), marker(tag, &msg_id));
            if !user && let Some(p) = &parent {
                obj.insert("parentID".to_string(), json!(p));
            }
        }
        tx.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1,?2,?3,?3,?4)",
            rusqlite::params![msg_id, origin.id, ts, data.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1,?2,?3,?4,?4,?5)",
            rusqlite::params![part_id, msg_id, origin.id, ts, json!({ "type": "text", "text": as_text(m) }).to_string()],
        )
        .map_err(|e| e.to_string())?;
        if user {
            parent = Some(msg_id.clone());
        }
        ids.push(msg_id);
    }
    tx.execute(
        "UPDATE session SET time_updated = MAX(time_updated, ?1) WHERE id = ?2",
        rusqlite::params![last_ts, origin.id],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(Done { ids, bytes_before: None, added_newline: false })
}

fn append_antigravity(db: &Path, batch: &Batch) -> Result<Done, String> {
    let mut conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
    conn.busy_timeout(crate::connectors::BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let next: i64 = tx
        .query_row("SELECT COALESCE(MAX(idx), -1) + 1 FROM steps", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    let mut last_secs = 0i64;
    for (i, (m, _)) in batch.turns.iter().enumerate() {
        let idx = next + i as i64;
        let step_type = if m.role == Role::User { crate::antigravity_write::STEP_USER } else { crate::antigravity_write::STEP_MODEL };
        let ts = m.timestamp.unwrap_or_else(chrono::Utc::now);
        let secs = ts.timestamp().max(last_secs);
        last_secs = secs;
        let payload = crate::antigravity_write::encode_step(step_type, &as_text(m), secs, ts.timestamp_subsec_nanos());
        tx.execute(
            "INSERT INTO steps (idx, step_type, status, step_payload, step_format) VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![idx, step_type, crate::antigravity_write::STATUS_DONE, payload],
        )
        .map_err(|e| e.to_string())?;
        ids.push(idx.to_string());
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(Done { ids, bytes_before: None, added_newline: false })
}

// ---------------------------------------------------------------------------
// unsync

#[derive(Debug, Default)]
pub struct WritebackUnsyncReport {
    /// Origins every written turn was taken out of.
    pub cleaned: Vec<PathBuf>,
    /// Origins left as they are for now (tool open, or a write failed).
    pub kept: Vec<(PathBuf, String)>,
}

/// Take every written turn back out of its origin, exactly: the records or
/// rows whose ids a `writeback` row lists, nothing else.
pub fn unsync(dry_run: bool, is_open: &OpenCheck) -> WritebackUnsyncReport {
    let mut report = WritebackUnsyncReport::default();
    let all = rows();
    if all.is_empty() {
        return report;
    }
    // Rows for one origin store are undone together, so a file is rewritten
    // once and the newline rule sees every turn gone.
    let mut by_dest: Vec<(PathBuf, Vec<LinkRecord>)> = Vec::new();
    for r in all {
        match by_dest.iter_mut().find(|(d, _)| *d == r.dest) {
            Some((_, v)) => v.push(r),
            None => by_dest.push((r.dest.clone(), vec![r])),
        }
    }
    let mut keep: Vec<LinkRecord> = Vec::new();
    for (dest, group) in by_dest {
        let tool = group[0].target_provider.clone();
        if is_open(&tool) {
            report.kept.push((dest, format!("{} is open; run unsync again after closing it", display_name(&tool))));
            keep.extend(group);
            continue;
        }
        if dry_run {
            report.cleaned.push(dest);
            keep.extend(group);
            continue;
        }
        let ids: HashSet<String> = group.iter().flat_map(|r| written_of(r).written).collect();
        let result = match tool.as_str() {
            "claude-code" | "codex-cli" => {
                // The earliest write back's length and newline decide what
                // "as it was" means.
                let first = group.iter().map(written_of).filter(|w| w.bytes_before.is_some()).min_by_key(|w| w.bytes_before);
                unsync_jsonl(&dest, &ids, first.as_ref())
            }
            "opencode" => unsync_rows(&dest, "DELETE FROM part WHERE message_id = ?1", "DELETE FROM message WHERE id = ?1", &ids),
            "antigravity" => unsync_rows(&dest, "DELETE FROM steps WHERE idx = CAST(?1 AS INTEGER)", "", &ids),
            other => Err(format!("unknown tool {}", other)),
        };
        match result {
            Ok(()) => report.cleaned.push(dest),
            Err(e) => {
                report.kept.push((dest, e));
                keep.extend(group);
            }
        }
    }
    if !dry_run {
        let _ = crate::sync::write_manifest_kind(KIND_WRITEBACK, &keep);
    }
    report
}

fn unsync_jsonl(path: &Path, ids: &HashSet<String>, first: Option<&Written>) -> Result<(), String> {
    let Ok(bytes) = fs::read(path) else {
        return Ok(()); // the file is gone, and its written turns with it
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = String::with_capacity(bytes.len());
    for piece in text.split_inclusive('\n') {
        let ours = serde_json::from_str::<Value>(piece.trim_end_matches('\n'))
            .ok()
            .and_then(|v| v.get(MARKER_FIELD)?.get("id")?.as_str().map(|id| ids.contains(id)))
            .unwrap_or(false);
        if !ours {
            out.push_str(piece);
        }
    }
    // The newline write back added to a file that did not end in one comes
    // off again when nothing else was added after it.
    if let Some(w) = first
        && w.added_newline
        && let Some(n) = w.bytes_before
        && out.len() as u64 == n + 1
        && out.ends_with('\n')
    {
        out.pop();
    }
    if out.as_bytes() == bytes.as_slice() {
        return Ok(());
    }
    let tmp = path.with_extension("jsonl.agentbridge-tmp");
    fs::write(&tmp, out).map_err(|e| format!("cannot write {}: {}", tmp.display(), e))?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {}", path.display(), e))
}

fn unsync_rows(db: &Path, first_sql: &str, second_sql: &str, ids: &HashSet<String>) -> Result<(), String> {
    if !db.exists() {
        return Ok(());
    }
    let mut conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
    conn.busy_timeout(crate::connectors::BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for id in ids {
        tx.execute(first_sql, [id]).map_err(|e| e.to_string())?;
        if !second_sql.is_empty() {
            tx.execute(second_sql, [id]).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ordinal: u64, role: Role, text: Option<&str>, tool: Option<&str>, input: Option<Value>) -> Message {
        Message {
            session_id: "s".into(),
            ordinal,
            role,
            timestamp: None,
            text: text.map(str::to_string),
            tool_name: tool.map(str::to_string),
            tool_input: input,
            tool_result: None,
            parent_ordinal: None,
        }
    }

    #[test]
    fn test_read_call_names_in_each_tool() {
        assert!(is_read_call("mcp__agentbridge__read_session"));
        assert!(is_read_call("agentbridge_read_session"));
        assert!(is_read_call("agentbridge__read_session"));
        assert!(!is_read_call("mcp__other__read_session"));
        assert!(!is_read_call("mcp__agentbridge__list_sessions"));
    }

    #[test]
    fn test_turns_start_after_the_last_read_and_skip_agentbridge_calls() {
        let read = |o: u64, id: &str| msg(o, Role::Assistant, None, Some("mcp__agentbridge__read_session"), Some(json!({"id": id})));
        let result = |o: u64| msg(o, Role::Tool, None, Some("mcp__agentbridge__read_session"), None);
        let session = Session {
            id: "s".into(),
            provider: "codex-cli".into(),
            project_id: String::new(),
            started_at: None,
            last_event_at: None,
            model: None,
            title: None,
            token_totals: Default::default(),
            source_path: PathBuf::new(),
            raw_payload: Value::Null,
            body_available: true,
            messages: vec![
                msg(0, Role::User, Some("find my old session"), None, None),
                read(1, "first"),
                result(2),
                msg(3, Role::Assistant, Some("between reads"), None, None),
                read(4, "second"),
                result(5),
                msg(6, Role::User, Some("now fix it"), None, None),
                msg(7, Role::Assistant, None, Some("mcp__agentbridge__list_sessions"), Some(json!({}))),
                msg(8, Role::Assistant, Some(""), None, None),
                msg(9, Role::Assistant, Some("fixed"), None, None),
            ],
            artifacts: vec![],
        };
        let (origin, tool, turns) = after_last_read(&session, &HashSet::new()).unwrap();
        assert_eq!(origin, "second", "the session read last is the origin");
        assert_eq!(tool, None);
        let texts: Vec<&str> = turns.iter().map(|m| m.text.as_deref().unwrap()).collect();
        assert_eq!(texts, ["now fix it", "fixed"]);

        let written = HashSet::from([fingerprint(&session.messages[9])]);
        let (_, _, turns) = after_last_read(&session, &written).unwrap();
        assert_eq!(turns.len(), 1, "a turn agentbridge wrote is not new work");
    }

    #[test]
    fn test_opencode_keeps_read_arguments_under_state_input() {
        let m = msg(0, Role::Assistant, None, Some("agentbridge_read_session"),
            Some(json!({"status": "completed", "input": {"id": "abc", "tool": "claude-code"}, "output": "..."})));
        assert_eq!(read_target(&m), Some(("abc".to_string(), Some("claude-code".to_string()))));
    }
}
