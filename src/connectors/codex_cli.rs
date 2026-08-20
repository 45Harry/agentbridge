use crate::connector::{Connector, ConnectorError, ConnectorResult, InjectTarget, SessionStream};
use crate::model::{Message, RawSession, Role, Session, TokenTotals};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// The resolved codex home (`CODEX_HOME` override or the default), exposed
/// for the sync write path so materialized copies land where this same
/// connector reads from — see `sync::live_root`.
///
/// Resolved on every call rather than cached in a `LazyLock`. Caching it made
/// the value depend on whichever code path happened to read it first in the
/// process: a later `CODEX_HOME` change was ignored for the rest of the run.
/// The sibling connectors (`claude_code::config_root`,
/// `antigravity::write_home`) already re-read their env var each time, and the
/// lookup is a single `getenv`.
pub(crate) fn config_home() -> Option<PathBuf> {
    std::env::var("CODEX_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(default_codex_dir)
}

fn default_codex_dir() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".codex"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[derive(Debug, Default)]
pub struct CodexCliConnector;

impl CodexCliConnector {
    pub fn new() -> Self {
        Self
    }
}

impl Connector for CodexCliConnector {
    fn id(&self) -> &'static str {
        "codex-cli"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn detect(&self) -> bool {
        sessions_dir()
            .map(|d| d.exists())
            .unwrap_or(false)
    }

    fn roots(&self) -> Vec<PathBuf> {
        sessions_dir().into_iter().collect()
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        let dir = sessions_dir();
        let jsonl_files: Vec<PathBuf> = match dir {
            Some(root) if root.exists() => {
                WalkDir::new(root)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.path().to_path_buf())
                    .filter(|p| {
                        p.extension().is_some_and(|ext| ext == "jsonl")
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("rollout-"))
                    })
                    .collect()
            }
            _ => vec![],
        };

        Ok(Box::new(CodexScanIter {
            files: jsonl_files,
            idx: 0,
        }))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        let root = match sessions_dir() {
            Some(r) if r.exists() => r,
            _ => return Err(ConnectorError::NotFound(id.to_string())),
        };

        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl")
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && name.starts_with("rollout-") && name.contains(id)
                {
                    return load_from_path(path, id);
                }
            }
        }
        Err(ConnectorError::NotFound(id.to_string()))
    }

    fn resume_cmd(&self, session: &crate::model::Session) -> Option<Vec<String>> {
        Some(vec![
            "codex".to_string(),
            "resume".to_string(),
            session.id.clone(),
        ])
    }

    fn inject(&self, _brief: &str, _dry_run: bool) -> ConnectorResult<InjectTarget> {
        Err(ConnectorError::Other(anyhow::anyhow!(
            "inject not yet implemented for Codex CLI"
        )))
    }
}

fn sessions_dir() -> Option<PathBuf> {
    config_home().map(|h| h.join("sessions"))
}

struct CodexScanIter {
    files: Vec<PathBuf>,
    idx: usize,
}

impl Iterator for CodexScanIter {
    type Item = ConnectorResult<RawSession>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let file = self.files.get(self.idx)?;
            self.idx += 1;

            match scan_codex_file(file) {
                Ok(Some(raw)) => return Some(Ok(raw)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

fn parse_codex_id_from_filename(name: &str) -> Option<String> {
    let name = name.strip_prefix("rollout-")?;
    let parts: Vec<&str> = name.splitn(2, '-').collect();
    if parts.len() < 2 {
        return None;
    }
    let after_ts = parts[1];
    if let Some(uuid_part) = after_ts.rsplitn(2, '-').last() {
        let uuid_part = uuid_part.strip_suffix(".jsonl").unwrap_or(uuid_part);
        Some(uuid_part.to_string())
    } else {
        None
    }
}

fn parse_codex_timestamp(val: Option<&Value>) -> Option<DateTime<Utc>> {
    match val {
        Some(Value::String(s)) => {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }
            None
        }
        Some(Value::Number(n)) => {
            if let Some(secs) = n.as_i64() {
                Utc.timestamp_opt(secs, 0).single()
            } else if let Some(secs) = n.as_f64() {
                let whole = secs.trunc() as i64;
                let frac = (secs.fract() * 1_000_000_000.0).abs() as u32;
                Utc.timestamp_opt(whole, frac).single()
            } else {
                None
            }
        }
        _ => None,
    }
}

fn scan_codex_file(path: &Path) -> ConnectorResult<Option<RawSession>> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };

    if metadata.len() == 0 || metadata.len() < 2 {
        return Ok(None);
    }

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return Err(ConnectorError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let reader = BufReader::new(file);

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "session_meta" => {
                let payload = val.get("payload");
                let id = payload
                    .and_then(|p| p.get("session_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| parse_codex_id_from_filename(&filename))
                    .unwrap_or_default();

                let cwd = payload
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);

                let timestamp = payload
                    .and_then(|p| parse_codex_timestamp(p.get("timestamp")));
                let source = payload
                    .and_then(|p| p.get("source"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                return Ok(Some(RawSession {
                    id,
                    provider: "codex-cli".to_string(),
                    project_path: cwd,
                    started_at: timestamp,
                    last_event_at: timestamp,
                    title: None,
                    source,
                    source_path: path.to_path_buf(),
                    body_available: true,
                }));
            }
            "conversation" => {
                let id = val
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| parse_codex_id_from_filename(&filename))
                    .unwrap_or_default();

                let cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                let timestamp = parse_codex_timestamp(val.get("created_at"));
                let title = val.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());

                return Ok(Some(RawSession {
                    id,
                    provider: "codex-cli".to_string(),
                    project_path: cwd,
                    started_at: timestamp,
                    last_event_at: timestamp,
                    title,
                    source: None,
                    source_path: path.to_path_buf(),
                    body_available: true,
                }));
            }
            _ => continue,
        }
    }

    Ok(None)
}

#[cfg(test)]
pub struct TestCodexCli {
    root: PathBuf,
}

#[cfg(test)]
impl TestCodexCli {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[cfg(test)]
impl Connector for TestCodexCli {
    fn id(&self) -> &'static str {
        "codex-cli"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn detect(&self) -> bool {
        self.root.exists()
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        let jsonl_files: Vec<PathBuf> = if self.root.exists() {
            WalkDir::new(&self.root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().to_path_buf())
                .filter(|p| {
                    p.extension().is_some_and(|ext| ext == "jsonl")
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("rollout-"))
                })
                .collect()
        } else {
            vec![]
        };

        Ok(Box::new(CodexScanIter {
            files: jsonl_files,
            idx: 0,
        }))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        for entry in WalkDir::new(&self.root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl")
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && name.starts_with("rollout-") && name.contains(id) {
                            return load_from_path(path, id);
                        }
            }
        }
        Err(ConnectorError::NotFound(id.to_string()))
    }

    fn resume_cmd(&self, _session: &Session) -> Option<Vec<String>> {
        None
    }

    fn inject(&self, _brief: &str, _dry_run: bool) -> ConnectorResult<InjectTarget> {
        Err(ConnectorError::Other(anyhow::anyhow!("inject not available in test connector")))
    }
}

fn extract_cx_content_flexible(val: &Value) -> Option<String> {
    // Try real format: payload.content (string or array)
    if let Some(payload) = val.get("payload")
        && let Some(content) = payload.get("content") {
            match content {
                Value::String(s) => return Some(s.clone()),
                Value::Array(arr) => {
                    let texts: Vec<String> = arr
                        .iter()
                        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                        .map(|s| s.to_string())
                        .collect();
                    if !texts.is_empty() {
                        return Some(texts.join("\n"));
                    }
                }
                _ => {}
            }
        }
    // Try fixture format: content at top level
    val.get("content").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Read a session from an explicit path rather than by id.
///
/// Used by write-back to re-read a file agentbridge materialized (which lives
/// under a directory the connector's own id lookup would not search), and by
/// converter tests to prove that what we *write* is readable by the reader for
/// the real format (CONNECTORS.md §6).
pub(crate) fn load_file(path: &Path, id: &str) -> ConnectorResult<Session> {
    load_from_path(path, id)
}

fn load_from_path(path: &Path, id: &str) -> ConnectorResult<Session> {
    let file = fs::File::open(path).map_err(|e| ConnectorError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut last_event_at: Option<DateTime<Utc>> = None;
    let mut project_path: Option<String> = None;
    let mut model_provider: Option<String> = None;
    let mut ordinal: u64 = 0;

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = val.get("payload");
        let timestamp = parse_codex_timestamp(val.get("timestamp"))
            .or_else(|| parse_codex_timestamp(val.get("created_at")));

        if let Some(t) = timestamp {
            if started_at.is_none() {
                started_at = Some(t);
            }
            last_event_at = Some(t);
        }

        if project_path.is_none() {
            if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                project_path = Some(cwd.to_string());
            } else if let Some(p) = payload {
                project_path = p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }

        match event_type {
            "session_meta" => {
                if let Some(p) = payload {
                    if model_provider.is_none() {
                        model_provider = p.get("model_provider").and_then(|v| v.as_str()).map(|s| s.to_string());
                    }
                    if started_at.is_none() {
                        started_at = p.get("timestamp").and_then(|v| v.as_str()).and_then(|s| {
                            DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Utc))
                        });
                    }
                }
            }
            "event_msg" => {
                if let Some(p) = payload {
                    // The head-scan preview echo (`payload.message` as a plain
                    // string, type `user_message`) duplicates the response_item
                    // that carries the real turn — counting it would invent a
                    // phantom message. Only the object form is a real message.
                    if let Some(msg) = p.get("message").filter(|v| v.is_object()) {
                        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        let content = msg.get("content").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let role_enum = if role == "user" { Role::User } else { Role::Assistant };
                        let m = Message {
                            session_id: id.to_string(),
                            ordinal,
                            role: role_enum,
                            timestamp,
                            text: content,
                            tool_name: None,
                            tool_input: None,
                            tool_result: None,
                            parent_ordinal: None,
                        };
                        messages.push(m);
                        ordinal += 1;
                    }
                    if project_path.is_none()
                        && let Some(msg_cwd) = p.get("cwd").and_then(|v| v.as_str()) {
                            project_path = Some(msg_cwd.to_string());
                        }
                }
            }
            "response_item" => {
                if let Some(p) = payload {
                    let role = p.get("role").and_then(|v| v.as_str()).unwrap_or("assistant");
                    let content = extract_cx_content_flexible(&val);
                    let role_enum = if role == "user" { Role::User } else { Role::Assistant };
                    let m = Message {
                        session_id: id.to_string(),
                        ordinal,
                        role: role_enum,
                        timestamp,
                        text: content,
                        tool_name: None,
                        tool_input: None,
                        tool_result: None,
                        parent_ordinal: None,
                    };
                    messages.push(m);
                    ordinal += 1;
                }
            }
            "tool_use" => {
                // Handle both: real (payload.name) and fixture (tool_name at top)
                let tool_name = payload
                    .and_then(|p| p.get("name"))
                    .or_else(|| val.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let tool_input = payload
                    .and_then(|p| p.get("input"))
                    .or_else(|| val.get("tool_input"))
                    .cloned();
                let m = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Assistant,
                    timestamp,
                    text: None,
                    tool_name,
                    tool_input,
                    tool_result: None,
                    parent_ordinal: None,
                };
                messages.push(m);
                ordinal += 1;
            }
            "tool_result" => {
                let tool_name = payload
                    .and_then(|p| p.get("name"))
                    .or_else(|| val.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let tool_result = payload
                    .and_then(|p| p.get("result").or_else(|| p.get("output")))
                    .or_else(|| val.get("tool_result"))
                    .cloned();
                let m = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Tool,
                    timestamp,
                    text: None,
                    tool_name,
                    tool_input: None,
                    tool_result,
                    parent_ordinal: None,
                };
                messages.push(m);
                ordinal += 1;
            }
            // Fixture format types
            "conversation" => {}
            "user_turn" => {
                let text = extract_cx_content_flexible(&val);
                let m = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::User,
                    timestamp,
                    text,
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                };
                messages.push(m);
                ordinal += 1;
            }
            "assistant_turn" => {
                let text = extract_cx_content_flexible(&val);
                let m = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Assistant,
                    timestamp,
                    text,
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                };
                messages.push(m);
                ordinal += 1;
            }
            _ => {}
        }
    }

    let project_id = project_path
        .clone()
        .unwrap_or_else(|| id.to_string());

    Ok(Session {
        id: id.to_string(),
        provider: "codex-cli".to_string(),
        project_id,
        started_at,
        last_event_at,
        model: model_provider,
        title: None,
        token_totals: TokenTotals::default(),
        source_path: path.to_path_buf(),
        raw_payload: serde_json::Value::Null,
        body_available: true,
        messages,
        artifacts: vec![],
    })
}
