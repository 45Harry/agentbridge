//! `agentbridge mcp`: an MCP server over stdio (spec 0001).
//!
//! JSON-RPC 2.0, one compact JSON object per line on stdin and stdout. Two
//! tools, `list_sessions` and `read_session`, both read only: they discover
//! sessions live from each tool's own store on every call and never open
//! anything for writing. Nothing but protocol messages goes to stdout; logs
//! go to stderr.
//!
//! The protocol is written in house (no SDK, no async runtime); the supported
//! subset is `initialize`, `notifications/initialized`, `ping`, `tools/list`
//! and `tools/call`.

use crate::connector::{ConnectorError, Registry};
use crate::index::{discover, IndexEntry};
use crate::model::{Message, Role, Session};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Protocol versions this server speaks. A client asking for another one gets
/// the first (newest).
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub const LIST_DEFAULT_LIMIT: u64 = 20;
pub const LIST_MAX_LIMIT: u64 = 100;
pub const PAGE_DEFAULT_SIZE: u64 = 40;
pub const PAGE_MAX_SIZE: u64 = 200;

/// JSON-RPC error codes used here.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const NOT_INITIALIZED: i64 = -32002;

/// Where sessions come from. `connectors::all()` in the binary; fixture
/// connectors in tests. Called on every tool call so env overrides and new
/// sessions are always seen (no cache).
pub type RegistryFn = dyn Fn() -> Registry;

/// One server process's state.
pub struct Server<'a> {
    registry: &'a RegistryFn,
    /// The folder the tool started the server in: the "current folder" that
    /// `list_sessions` puts first.
    cwd: PathBuf,
    initialized: bool,
    /// Run once, right after the first `initialize` reply has been written
    /// (write back starts a thread here, so startup is never delayed).
    on_initialized: Option<Box<dyn FnOnce() + 'a>>,
    /// Run at end of input, before the process exits (the tool is closing).
    on_end: Option<Box<dyn FnOnce() + 'a>>,
}

impl<'a> Server<'a> {
    pub fn new(registry: &'a RegistryFn, cwd: PathBuf) -> Self {
        Self { registry, cwd, initialized: false, on_initialized: None, on_end: None }
    }

    /// Set the start and end hooks (see the fields).
    pub fn with_hooks(mut self, on_initialized: Box<dyn FnOnce() + 'a>, on_end: Box<dyn FnOnce() + 'a>) -> Self {
        self.on_initialized = Some(on_initialized);
        self.on_end = Some(on_end);
        self
    }

    /// Answer one input line. `None` means no reply is sent (a notification,
    /// or a blank line).
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        if line.trim().is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error_reply(Value::Null, PARSE_ERROR, &format!("parse error: {}", e))),
        };
        let Some(obj) = msg.as_object() else {
            let why = if msg.is_array() { "batches are not supported" } else { "a request must be a JSON object" };
            return Some(error_reply(Value::Null, INVALID_REQUEST, why));
        };
        // A notification has no `id` and never gets a reply, even on error.
        let id = obj.get("id").cloned();
        let reply_err = |code: i64, m: &str| id.clone().map(|id| error_reply(id, code, m));

        if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
            return reply_err(INVALID_REQUEST, "jsonrpc must be \"2.0\"");
        }
        let Some(method) = obj.get("method").and_then(|v| v.as_str()) else {
            return reply_err(INVALID_REQUEST, "missing method");
        };
        let params = obj.get("params");

        let result = match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" if !self.initialized => {
                Err((NOT_INITIALIZED, "not initialized: send initialize first".to_string()))
            }
            "tools/call" => self.call_tool(params),
            m if m.starts_with("notifications/") => return None,
            m => Err((METHOD_NOT_FOUND, format!("method not found: {}", m))),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => error_reply(id, code, &m),
        })
    }

    fn initialize(&mut self, params: Option<&Value>) -> Result<Value, (i64, String)> {
        let params = match params {
            None | Some(Value::Null) => None,
            Some(Value::Object(p)) => Some(p),
            Some(_) => return Err((INVALID_PARAMS, "params must be an object".to_string())),
        };
        let asked = params.and_then(|p| p.get("protocolVersion")).and_then(|v| v.as_str());
        let version = asked
            .filter(|v| PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        self.initialized = true;
        Ok(json!({
            "protocolVersion": version,
            "serverInfo": { "name": "agentbridge", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "tools": {} },
        }))
    }

    fn call_tool(&self, params: Option<&Value>) -> Result<Value, (i64, String)> {
        let Some(p) = params.and_then(|p| p.as_object()) else {
            return Err((INVALID_PARAMS, "params must be an object".to_string()));
        };
        let Some(name) = p.get("name").and_then(|v| v.as_str()) else {
            return Err((INVALID_PARAMS, "missing tool name".to_string()));
        };
        let empty = Map::new();
        let args = match p.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".to_string())),
        };
        let registry = (self.registry)();
        let outcome = match name {
            "list_sessions" => list_sessions(&registry, args, &self.cwd),
            "read_session" => read_session(&registry, args),
            other => return Err((INVALID_PARAMS, format!("unknown tool: {}", other))),
        };
        Ok(match outcome {
            Ok(v) => tool_result(v),
            Err(m) => tool_error(&m),
        })
    }
}

/// Run the server until end of input. Returns the process exit code: 0 at
/// end of input, 1 when stdout cannot be written (the client has gone).
pub fn serve(input: impl BufRead, mut output: impl Write, server: &mut Server) -> i32 {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            // Bytes that are not UTF-8 are a line we cannot parse.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let reply = error_reply(Value::Null, PARSE_ERROR, "parse error: input is not UTF-8");
                if write_reply(&mut output, &reply).is_err() {
                    return 1;
                }
                continue;
            }
            Err(_) => return 0,
        };
        if let Some(reply) = server.handle_line(&line)
            && write_reply(&mut output, &reply).is_err()
        {
            return 1;
        }
        if server.initialized
            && let Some(hook) = server.on_initialized.take()
        {
            hook();
        }
    }
    if let Some(hook) = server.on_end.take() {
        hook();
    }
    0
}

fn write_reply(output: &mut impl Write, reply: &Value) -> std::io::Result<()> {
    // Compact output never holds a raw newline: one message per line.
    let mut line = serde_json::to_string(reply).unwrap_or_default();
    line.push('\n');
    output.write_all(line.as_bytes())?;
    output.flush()
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A successful tool result: the object as `structuredContent`, and again as
/// pretty JSON text for clients that ignore `structuredContent`.
fn tool_result(v: Value) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": v,
        "isError": false,
    })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// The two tools and their input schemas.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_sessions",
            "description": "List AI coding sessions from every tool on this machine (Claude Code, \
Codex, OpenCode, Antigravity), from every folder. Sessions of the current folder come first, \
then all others, each group newest first. Use `query` to search titles, first messages, \
folders, tool ids and session id prefixes. Then call read_session with a session's id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words that must all match (ignoring case) the title, first message, folder or tool; or a session id prefix." },
                    "tool": { "type": "string", "enum": ["claude-code", "codex-cli", "opencode", "antigravity"], "description": "Only sessions from this tool." },
                    "project": { "type": "string", "description": "Only sessions from this folder (absolute path). \"all\" lists every folder with no current folder first." },
                    "since": { "type": "string", "description": "Only sessions active at or after this time: YYYY-MM-DD or an RFC 3339 time." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": LIST_MAX_LIMIT, "default": LIST_DEFAULT_LIMIT },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "read_session",
            "description": "Read one session's messages, page by page, straight from the tool that \
saved it. Page 1 is the newest messages (in time order inside the page); page 2 the ones \
before those, and so on. Each message has its role, time, full text, and for tool calls the \
tool name, input and output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The session id from list_sessions." },
                    "tool": { "type": "string", "enum": ["claude-code", "codex-cli", "opencode", "antigravity"], "description": "Which tool's session, when the same id exists in two tools." },
                    "page": { "type": "integer", "minimum": 1, "default": 1, "description": "1 = newest messages." },
                    "page_size": { "type": "integer", "minimum": 1, "maximum": PAGE_MAX_SIZE, "default": PAGE_DEFAULT_SIZE }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Arguments

fn check_args(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown argument `{}`; allowed: {}", k, allowed.join(", "))),
        None => Ok(()),
    }
}

fn arg_str<'v>(args: &'v Map<String, Value>, key: &str) -> Result<Option<&'v str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("`{}` must be a string", key)),
    }
}

fn arg_int(args: &Map<String, Value>, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let v = match args.get(key) {
        None | Some(Value::Null) => return Ok(default),
        Some(v) => v,
    };
    match v.as_i64() {
        Some(n) if n >= min as i64 && n <= max as i64 => Ok(n as u64),
        Some(n) => Err(format!("`{}` is {}; it must be from {} to {}", key, n, min, max)),
        None => Err(format!("`{}` must be a whole number", key)),
    }
}

fn arg_tool<'v>(args: &'v Map<String, Value>, registry: &Registry) -> Result<Option<&'v str>, String> {
    let tool = arg_str(args, "tool")?;
    if let Some(t) = tool
        && registry.by_id(t).is_none()
    {
        let known: Vec<&str> = registry.all().iter().map(|c| c.id()).collect();
        return Err(format!("unknown tool `{}`; use one of: {}", t, known.join(", ")));
    }
    Ok(tool)
}

/// `since` as an instant: `YYYY-MM-DD` (00:00 UTC) or an RFC 3339 time.
fn parse_since(s: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|_| format!("`since` is \"{}\"; use YYYY-MM-DD or an RFC 3339 time like 2026-10-01T09:00:00Z", s))
}

/// A folder path as `project` compares it: absolute, trailing slashes
/// removed, symlinks left as they are.
fn normalize_folder(p: &Path, cwd: &Path) -> String {
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let s = abs.to_string_lossy().to_string();
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
}

fn time_str(t: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    t.map(|t| Value::String(t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)))
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Copies

/// Tells agentbridge's own copies apart from sessions a tool saved itself.
/// Built once per call from the manifest; a copy never appears in a list and
/// never wins an id clash.
pub(crate) struct CopyFilter {
    /// (target tool, file) for file based copies.
    files: std::collections::HashSet<(String, PathBuf)>,
    /// (target tool, row id) for copies inside a shared database (OpenCode).
    rows: std::collections::HashSet<(String, String)>,
}

impl CopyFilter {
    pub(crate) fn load() -> Self {
        let mut files = std::collections::HashSet::new();
        let mut rows = std::collections::HashSet::new();
        for r in crate::sync::read_manifest().into_iter().filter(|r| r.is_session_copy()) {
            if r.target_provider == "opencode" {
                rows.insert((r.target_provider, r.cache.to_string_lossy().to_string()));
            } else {
                files.insert((r.target_provider, r.dest));
            }
        }
        // OpenCode retitles sessions on its own, so a copy can lose its label,
        // and old races lost manifest rows. The marker agentbridge put in the
        // row's own metadata survives both (found live on a copy of a real
        // database: 16 of the 25 newest sessions were copies, 12 of them
        // listed as the user's own).
        let db = crate::connectors::opencode::default_db_path();
        if db.exists() {
            for id in crate::opencode_write::written_ids(&db) {
                rows.insert(("opencode".to_string(), id));
            }
        }
        Self { files, rows }
    }

    pub(crate) fn is_copy(&self, e: &IndexEntry) -> bool {
        // OpenCode keeps every session in one database, so the path says
        // nothing there; the row id does.
        let in_manifest = if e.provider == "opencode" {
            self.rows.contains(&(e.provider.clone(), e.id.clone()))
        } else {
            self.files.contains(&(e.provider.clone(), e.source_path.clone()))
        };
        in_manifest
            || e.title.as_deref().is_some_and(|t| crate::label::parse(t).is_some())
            || is_tagged_codex_copy(e)
    }
}

/// A Codex rollout named the way only agentbridge's converter names one:
/// `rollout-<time>-<uuid>-<8 hex folder tag>.jsonl`. Codex's own files end
/// with the uuid. Catches copies whose manifest row was lost to the old race
/// between shell hook runs and that predate the origin marker (found live on
/// a copy of a real store: one session listed twice, 2026-10-30).
fn is_tagged_codex_copy(e: &IndexEntry) -> bool {
    e.provider == "codex-cli"
        && e.source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| crate::convert::strip_dir_tag(stem) != stem)
}

// ---------------------------------------------------------------------------
// list_sessions

/// When an entry was last active, for sorting and `since`.
fn active_at(e: &IndexEntry) -> Option<chrono::DateTime<chrono::Utc>> {
    e.last_event_at.or(e.started_at)
}

fn matches_query(e: &IndexEntry, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    if e.id.to_lowercase().starts_with(&q) {
        return true;
    }
    let fields: Vec<String> = [
        e.title.clone(),
        e.first_prompt.clone(),
        e.project_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        Some(e.provider.clone()),
    ]
    .into_iter()
    .flatten()
    .map(|f| f.to_lowercase())
    .collect();
    q.split_whitespace().all(|w| fields.iter().any(|f| f.contains(w)))
}

pub fn list_sessions(registry: &Registry, args: &Map<String, Value>, cwd: &Path) -> Result<Value, String> {
    check_args(args, &["query", "tool", "project", "since", "limit", "offset"])?;
    let query = arg_str(args, "query")?;
    let tool = arg_tool(args, registry)?;
    let project = arg_str(args, "project")?;
    let since = arg_str(args, "since")?.map(parse_since).transpose()?;
    let limit = arg_int(args, "limit", LIST_DEFAULT_LIMIT, 1, LIST_MAX_LIMIT)?;
    let offset = arg_int(args, "offset", 0, 0, i64::MAX as u64)?;

    let all_folders = project == Some("all");
    let current = match project {
        Some(p) if !all_folders => normalize_folder(Path::new(p), cwd),
        _ => normalize_folder(cwd, cwd),
    };
    let folder_of = |e: &IndexEntry| e.project_path.as_deref().map(|p| normalize_folder(p, cwd));

    let index = discover(registry);
    let copies = CopyFilter::load();
    let mut entries: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|e| !copies.is_copy(e))
        .filter(|e| tool.is_none_or(|t| e.provider == t))
        .filter(|e| all_folders || project.is_none() || folder_of(e).as_deref() == Some(current.as_str()))
        .filter(|e| since.is_none_or(|s| active_at(e).is_some_and(|t| t >= s)))
        .filter(|e| query.is_none_or(|q| matches_query(e, q)))
        .collect();

    let in_current = |e: &IndexEntry| folder_of(e).as_deref() == Some(current.as_str());
    // Current folder first (unless "all"), then newest first with undated
    // last, then a fixed tiebreak so paging is stable between calls.
    entries.sort_by(|a, b| {
        let group = |e: &IndexEntry| !(in_current(e) && !all_folders);
        group(a)
            .cmp(&group(b))
            .then_with(|| match (active_at(a), active_at(b)) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let total = entries.len();
    let sessions: Vec<Value> = entries
        .iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|e| {
            json!({
                "id": e.id,
                "tool": e.provider,
                "title": e.title,
                "folder": e.project_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "started_at": time_str(e.started_at),
                "last_event_at": time_str(e.last_event_at),
                "first_prompt": e.first_prompt,
                "in_current_folder": in_current(e),
            })
        })
        .collect();

    Ok(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "current_folder": if all_folders { Value::Null } else { Value::String(current) },
        "warnings": index.errors.len(),
        "sessions": sessions,
    }))
}

// ---------------------------------------------------------------------------
// read_session

/// The entry `id` names, resolved the way `read_session` (and write back)
/// resolve it: narrow by `tool`, drop copies, and the one left wins. Several
/// left is an error naming each, so the caller can retry with `tool`.
pub fn resolve_entry<'i>(
    entries: &'i [IndexEntry],
    id: &str,
    tool: Option<&str>,
) -> Result<&'i IndexEntry, String> {
    let copies = CopyFilter::load();
    let named: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| e.id == id && tool.is_none_or(|t| e.provider == t))
        .collect();
    if named.is_empty() {
        let where_ = tool.map(|t| format!(" in {}", t)).unwrap_or_default();
        return Err(format!(
            "no session with id `{}`{}. Call list_sessions (with `query` set to part of the title or the id) to find it.",
            id, where_
        ));
    }
    let natives: Vec<&IndexEntry> = named.iter().copied().filter(|e| !copies.is_copy(e)).collect();
    // Only agentbridge's copies carry this id: read the copy rather than
    // fail, it is the only thing there is.
    let pool = if natives.is_empty() { named } else { natives };
    match pool.as_slice() {
        [one] => Ok(one),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|e| format!("{} ({})", e.provider, e.source_path.display()))
                .collect();
            Err(format!(
                "id `{}` exists in more than one place: {}. Call read_session again with `tool` set to one of them.",
                id,
                list.join("; ")
            ))
        }
    }
}

/// True when this SQLite error means another process holds the database.
fn is_busy(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("database is locked") || m.contains("database table is locked") || m.contains("database is busy")
}

fn load_error(entry: &IndexEntry, e: ConnectorError) -> String {
    let tool = &entry.provider;
    match e {
        ConnectorError::BodyUnavailable(_) => format!(
            "session `{}` from {} has no readable body (only its metadata survives{}). Pick another session.",
            entry.id,
            tool,
            if tool == "antigravity" { "; Antigravity encrypts some conversation bodies" } else { "" }
        ),
        ConnectorError::NotFound(_) => format!(
            "session `{}` from {} is gone from {} since the list was made. Call list_sessions again.",
            entry.id,
            tool,
            entry.source_path.display()
        ),
        other => {
            let m = other.to_string();
            if is_busy(&m) {
                format!("{} is busy writing, try again in a moment", tool)
            } else {
                format!("could not read session `{}` from {}: {}", entry.id, tool, m)
            }
        }
    }
}

/// Load the session an index entry points at. File based tools are read from
/// the entry's own file: an id can sit in several files of one tool (a copy
/// in another folder), and a lookup by id alone would return whichever file
/// it walks first.
pub fn load_entry(registry: &Registry, entry: &IndexEntry) -> Result<Session, String> {
    let loaded = match entry.provider.as_str() {
        "claude-code" => crate::connectors::claude_code::load_file(&entry.source_path, &entry.id),
        "codex-cli" => crate::connectors::codex_cli::load_file(&entry.source_path, &entry.id),
        other => match registry.by_id(other) {
            Some(c) => c.load(&entry.id),
            None => return Err(format!("no reader for tool `{}`", other)),
        },
    };
    let session = loaded.map_err(|e| load_error(entry, e))?;
    if session.source_path != entry.source_path {
        return Err(format!(
            "session `{}` from {} resolved to {} instead of {}: another store has the same id. Call read_session again with `tool` set.",
            entry.id,
            entry.provider,
            session.source_path.display(),
            entry.source_path.display()
        ));
    }
    Ok(session)
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

fn message_json(m: &Message, ordinal: u64, recovered: bool) -> Value {
    json!({
        "ordinal": ordinal,
        "role": role_str(m.role),
        "time": time_str(m.timestamp),
        "text": m.text,
        "tool_name": m.tool_name,
        "tool_input": m.tool_input,
        "tool_output": m.tool_result,
        "recovered": recovered,
    })
}

pub fn read_session(registry: &Registry, args: &Map<String, Value>) -> Result<Value, String> {
    check_args(args, &["id", "tool", "page", "page_size"])?;
    let id = match arg_str(args, "id")? {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => return Err("`id` is required: the session id from list_sessions".to_string()),
    };
    let tool = arg_tool(args, registry)?;
    let page = arg_int(args, "page", 1, 1, i64::MAX as u64)?;
    let page_size = arg_int(args, "page_size", PAGE_DEFAULT_SIZE, 1, PAGE_MAX_SIZE)?;

    let index = discover(registry);
    let entry = resolve_entry(&index.entries, id, tool)?;
    let session = load_entry(registry, entry)?;

    // Turns recovered by `pull` are keyed by session id alone, so they are
    // only merged when exactly one session (that is not a copy) has this id.
    let copies = CopyFilter::load();
    let owners = index.entries.iter().filter(|e| e.id == entry.id && !copies.is_copy(e)).count();
    let recovered = if owners == 1 { crate::sync::overlay_messages(&entry.id) } else { Vec::new() };

    let next = session.messages.iter().map(|m| m.ordinal + 1).max().unwrap_or(0);
    let mut all: Vec<Value> = session.messages.iter().map(|m| message_json(m, m.ordinal, false)).collect();
    all.extend(recovered.iter().enumerate().map(|(i, m)| message_json(m, next + i as u64, true)));

    let total = all.len() as u64;
    let total_pages = total.div_ceil(page_size);
    let in_range = page <= total_pages || (total == 0 && page == 1);
    if !in_range {
        return Err(format!(
            "page {} is out of range: session `{}` has {} message(s) in {} page(s) of {}",
            page, entry.id, total, total_pages, page_size
        ));
    }
    // Pages are cut from the end: page 1 is the newest.
    let end = total.saturating_sub((page - 1) * page_size) as usize;
    let start = end.saturating_sub(page_size as usize);
    let messages: Vec<Value> = all.drain(start..end).collect();

    Ok(json!({
        "id": entry.id,
        "tool": entry.provider,
        "title": session.title.clone().or_else(|| entry.title.clone()),
        "folder": entry.project_path.as_ref().map(|p| p.to_string_lossy().to_string()).or_else(|| session.project_path()),
        "started_at": time_str(session.started_at.or(entry.started_at)),
        "page": page,
        "page_size": page_size,
        "total_pages": total_pages,
        "total_messages": total,
        "recovered_from_overlay": recovered.len(),
        "messages": messages,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_registry() -> Registry {
        crate::connectors::all_for_testing(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
    }

    fn server_reply(server: &mut Server, msg: Value) -> Value {
        server.handle_line(&msg.to_string()).expect("a reply")
    }

    fn init(server: &mut Server) {
        let r = server_reply(server, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn test_initialize_falls_back_to_newest_protocol() {
        let reg = fixture_registry;
        let mut s = Server::new(&reg, PathBuf::from("/"));
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"1999-01-01"}}));
        assert_eq!(r["id"], "a", "string ids are echoed as given");
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(r["result"]["serverInfo"]["name"], "agentbridge");
        assert_eq!(r["result"]["capabilities"], json!({"tools": {}}));
    }

    #[test]
    fn test_tools_list_has_exactly_two_tools() {
        let reg = fixture_registry;
        let mut s = Server::new(&reg, PathBuf::from("/"));
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_sessions", "read_session"]);
        for t in r["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn test_protocol_errors() {
        let reg = fixture_registry;
        let mut s = Server::new(&reg, PathBuf::from("/"));
        let r = s.handle_line("not json").unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert_eq!(r["id"], Value::Null);
        let r = s.handle_line("[]").unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = server_reply(&mut s, json!({"jsonrpc":"1.0","id":3,"method":"ping"}));
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":4}));
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_sessions"}}));
        assert_eq!(r["error"]["code"], NOT_INITIALIZED);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":6,"method":"nope"}));
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        // Notifications never get a reply, even a bad one.
        assert!(s.handle_line(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string()).is_none());
        assert!(s.handle_line(&json!({"jsonrpc":"2.0","method":"nope"}).to_string()).is_none());
        // Still answering after all of that.
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":7,"method":"ping"}));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn test_unknown_argument_is_a_tool_error() {
        let reg = fixture_registry;
        let mut s = Server::new(&reg, PathBuf::from("/"));
        init(&mut s);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_sessions","arguments":{"bogus":1}}}));
        assert_eq!(r["result"]["isError"], true);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_sessions","arguments":{"limit":101}}}));
        assert_eq!(r["result"]["isError"], true);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_sessions","arguments":{"tool":"vim"}}}));
        assert_eq!(r["result"]["isError"], true);
        let r = server_reply(&mut s, json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"list_sessions","arguments":{"since":"yesterday"}}}));
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn test_start_hook_runs_after_the_initialize_reply_and_end_hook_at_eof() {
        use std::cell::RefCell;
        let events: RefCell<Vec<String>> = RefCell::new(Vec::new());
        struct Out<'e>(&'e RefCell<Vec<String>>);
        impl Write for Out<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if b.ends_with(b"\n") {
                    self.0.borrow_mut().push("reply".into());
                }
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let reg = fixture_registry;
        let mut server = Server::new(&reg, PathBuf::from("/")).with_hooks(
            Box::new(|| events.borrow_mut().push("start".into())),
            Box::new(|| events.borrow_mut().push("end".into())),
        );
        let input = format!(
            "{}\n{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}),
        );
        let code = serve(std::io::Cursor::new(input), Out(&events), &mut server);
        assert_eq!(code, 0);
        drop(server);
        assert_eq!(events.into_inner(), ["reply", "reply", "start", "reply", "end"]);
    }

    #[test]
    fn test_parse_since_accepts_date_and_rfc3339() {
        assert_eq!(parse_since("2026-10-01").unwrap().to_rfc3339(), "2026-10-01T00:00:00+00:00");
        assert_eq!(parse_since("2026-10-01T09:30:00+02:00").unwrap().to_rfc3339(), "2026-10-01T07:30:00+00:00");
        assert!(parse_since("10/01/2026").is_err());
    }

    #[test]
    fn test_normalize_folder_strips_trailing_slash_and_makes_absolute() {
        let cwd = Path::new("/home/u");
        assert_eq!(normalize_folder(Path::new("/a/b/"), cwd), "/a/b");
        assert_eq!(normalize_folder(Path::new("proj"), cwd), "/home/u/proj");
        assert_eq!(normalize_folder(Path::new("/"), cwd), "/");
    }

    fn entry(id: &str, provider: &str, folder: &str, title: Option<&str>, prompt: Option<&str>) -> IndexEntry {
        IndexEntry {
            id: id.to_string(),
            provider: provider.to_string(),
            project_path: Some(PathBuf::from(folder)),
            source_path: PathBuf::from(format!("/store/{}/{}", provider, id)),
            started_at: None,
            last_event_at: None,
            title: title.map(str::to_string),
            source: None,
            first_prompt: prompt.map(str::to_string),
        }
    }

    #[test]
    fn test_a_codex_rollout_with_a_folder_tag_is_a_copy() {
        let mut e = entry("019fc833-0ea4-7101-bb76-46f6e6eeac0d", "codex-cli", "/w", None, None);
        e.source_path = PathBuf::from("/s/rollout-2026-08-03T21-01-57-019fc833-0ea4-7101-bb76-46f6e6eeac0d.jsonl");
        assert!(!is_tagged_codex_copy(&e), "Codex's own file name");
        e.source_path = PathBuf::from("/s/rollout-2026-08-03T15-17-12-019fc833-0ea4-7101-bb76-46f6e6eeac0d-35c0ff34.jsonl");
        assert!(is_tagged_codex_copy(&e), "agentbridge's per-folder copy");
    }

    #[test]
    fn test_query_needs_every_word_in_some_field_or_an_id_prefix() {
        let e = entry("019fc209-abcd", "codex-cli", "/home/u/app", Some("Fix login bug"), Some("the form rejects emails"));
        assert!(matches_query(&e, "LOGIN emails"), "words can match different fields, ignoring case");
        assert!(matches_query(&e, "codex app"), "tool id and folder count");
        assert!(matches_query(&e, "019FC2"), "id prefix");
        assert!(!matches_query(&e, "login payments"), "every word must match");
        assert!(!matches_query(&e, "abcd"), "an id match must be a prefix");
    }
}
