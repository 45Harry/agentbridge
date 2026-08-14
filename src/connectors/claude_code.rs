use crate::connector::{Connector, ConnectorError, ConnectorResult, InjectTarget, SessionStream};
use crate::model::{Message, RawSession, Role, Session, TokenTotals};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use walkdir::WalkDir;

static CLAUDE_CONFIG_DIR: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(default_claude_dir)
});

fn default_claude_dir() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".claude").join("projects"))
}

/// The base config directory (`CLAUDE_CONFIG_DIR` override, or `~/.claude`
/// by default) — always the parent of `projects/`, regardless of source.
fn config_base() -> Option<PathBuf> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".claude")))
}

/// Where materialized session copies must be written for the real Claude
/// Code binary's `projects/<encoded-dir>/<uuid>.jsonl` convention to find
/// them, honoring `CLAUDE_CONFIG_DIR` the same way reads do. Unlike
/// `CLAUDE_CONFIG_DIR`/`roots()` (which point at the raw override and rely
/// on `scan()`'s recursive walk to find `projects/` underneath it), this
/// always appends `projects` explicitly — used by `sync::live_root`, never
/// by `scan()`.
pub(crate) fn write_root() -> Option<PathBuf> {
    config_base().map(|b| b.join("projects"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[derive(Debug, Default)]
pub struct ClaudeCodeConnector;

impl ClaudeCodeConnector {
    pub fn new() -> Self {
        Self
    }
}

impl Connector for ClaudeCodeConnector {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self) -> bool {
        CLAUDE_CONFIG_DIR
            .as_ref()
            .map(|d| d.exists())
            .unwrap_or(false)
    }

    fn roots(&self) -> Vec<PathBuf> {
        CLAUDE_CONFIG_DIR
            .as_ref()
            .cloned()
            .into_iter()
            .collect()
    }

    fn scan(&self) -> ConnectorResult<SessionStream<'_>> {
        let dirs: Vec<PathBuf> = self.roots();
        let jsonl_files: Vec<PathBuf> = dirs
            .into_iter()
            .flat_map(|root| {
                if root.exists() {
                    WalkDir::new(root)
                        .into_iter()
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_file())
                        .map(|e| e.path().to_path_buf())
                        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            })
            .collect();

        let stream = ClaudeCodeScanIter {
            files: jsonl_files,
            idx: 0,
        };

        Ok(Box::new(stream))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        let dirs = self.roots();
        for root in &dirs {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "jsonl") {
                        let found_id = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("");
                        if found_id == id {
                            return load_from_path(path, id);
                        }
                    }
                }
            }
        }
        Err(ConnectorError::NotFound(id.to_string()))
    }

    fn resume_cmd(&self, session: &crate::model::Session) -> Option<Vec<String>> {
        Some(vec!["claude".to_string(), "--resume".to_string(), session.id.clone()])
    }

    fn inject(&self, brief: &str, dry_run: bool) -> ConnectorResult<InjectTarget> {
        let target = find_inject_target()?;
        if !dry_run {
            let marker_begin = "<!-- agentbridge:brief -->\n";
            let marker_end = "\n<!-- /agentbridge:brief -->";
            let content = format!("{}{}{}", marker_begin, brief, marker_end);
            let start = compute_fence_start(&target)?;
            fs::write(&target, &content).map_err(|e| ConnectorError::Io {
                path: target.clone(),
                source: e,
            })?;
            let end = start + content.len();
            Ok(InjectTarget {
                path: target,
                fenced_range: Some((start, end)),
            })
        } else {
            Ok(InjectTarget {
                path: target,
                fenced_range: None,
            })
        }
    }
}

struct ClaudeCodeScanIter {
    files: Vec<PathBuf>,
    idx: usize,
}

impl Iterator for ClaudeCodeScanIter {
    type Item = ConnectorResult<RawSession>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let file = self.files.get(self.idx)?;
            self.idx += 1;

            match scan_file(file) {
                Ok(Some(raw)) => return Some(Ok(raw)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

fn extract_cc_text_flexible(val: &Value) -> Option<String> {
    // Try real format: message.content (string or array)
    if let Some(msg) = val.get("message")
        && let Some(content) = msg.get("content") {
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

fn scan_file(path: &Path) -> ConnectorResult<Option<RawSession>> {
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

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let reader = BufReader::new(file);
    let mut cwd: Option<PathBuf> = None;
    let mut timestamp: Option<DateTime<Utc>> = None;
    let mut title: Option<String> = None;

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

        if event_type == "permission-mode" || event_type == "file-history-snapshot" {
            continue;
        }

        if cwd.is_none() {
            cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
        }
        if timestamp.is_none() {
            timestamp = extract_cc_timestamp(&val);
        }
        // `-n/--name` (and in-session rename) write a dedicated record, not a
        // field on a turn — the last one wins, since a later rename replaces
        // an earlier one (see extract_cc_custom_title).
        if let Some(t) = extract_cc_custom_title(event_type, &val) {
            title = Some(t);
        } else if title.is_none() {
            title = val.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
            if title.is_none() {
                title = extract_cc_title(&val);
            }
        }

        if cwd.is_some() {
            break;
        }
    }

    Ok(Some(RawSession {
        id,
        provider: "claude-code".to_string(),
        project_path: cwd,
        started_at: timestamp,
        last_event_at: timestamp,
        title,
        source: None,
        source_path: path.to_path_buf(),
        body_available: true,
    }))
}

fn extract_cc_timestamp(val: &Value) -> Option<DateTime<Utc>> {
    let ts_val = val.get("timestamp")?;
    if let Some(s) = ts_val.as_str() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }
        return None;
    }
    if let Some(n) = ts_val.as_i64() {
        return Utc.timestamp_opt(n, 0).single();
    }
    if let Some(f) = ts_val.as_f64() {
        let whole = f.trunc() as i64;
        let frac = (f.fract() * 1_000_000_000.0).abs() as u32;
        return Utc.timestamp_opt(whole, frac).single();
    }
    None
}

fn extract_cc_title(val: &Value) -> Option<String> {
    val.get("message")
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("title"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// `-n/--name` and in-session rename write a dedicated record — never a field
/// on a turn — so the generic title extractors above never see it:
/// `{"type":"custom-title","customTitle":"…"}` (preferred) or
/// `{"type":"agent-name","agentName":"…"}` (fallback, same value in practice
/// but a distinct field name).
fn extract_cc_custom_title(event_type: &str, val: &Value) -> Option<String> {
    match event_type {
        "custom-title" => val.get("customTitle").and_then(|t| t.as_str()).map(|s| s.to_string()),
        "agent-name" => val.get("agentName").and_then(|t| t.as_str()).map(|s| s.to_string()),
        _ => None,
    }
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
    let mut model: Option<String> = None;
    let mut title: Option<String> = None;
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

        match event_type {
            "permission-mode" | "file-history-snapshot" => continue,
            _ => {}
        }

        if val.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }

        let timestamp = extract_cc_timestamp(&val);

        if let Some(ts) = timestamp {
            if started_at.is_none() {
                started_at = Some(ts);
            }
            last_event_at = Some(ts);
        }

        if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str())
            && project_path.is_none()
        {
            project_path = Some(cwd.to_string());
        }

        if model.is_none() {
            model = val.get("message")
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }

        // Last one wins: a later rename replaces an earlier one, and unlike
        // `scan_file` this reads the whole file, so a rename anywhere in the
        // session is seen (see extract_cc_custom_title).
        if let Some(t) = extract_cc_custom_title(event_type, &val) {
            title = Some(t);
        } else if title.is_none() {
            title = extract_cc_title(&val);
            if title.is_none() {
                title = val.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }

        match event_type {
            "user" | "user_message" => {
                let text = extract_cc_text_flexible(&val);
                let msg = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::User,
                    timestamp,
                    text,
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: val.get("parentUuid").and_then(|v| v.as_str()).map(|_| 0),
                };
                messages.push(msg);
                ordinal += 1;
            }
            "assistant" | "assistant_message" => {
                let text = extract_cc_text_flexible(&val);
                let msg = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Assistant,
                    timestamp,
                    text,
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: val.get("parentUuid").and_then(|v| v.as_str()).map(|_| 0),
                };
                messages.push(msg);
                ordinal += 1;
            }
            "tool_use" => {
                let tool_name = val.get("tool_name").and_then(|v| v.as_str()).map(|s| s.to_string());
                let tool_input = val.get("tool_input").cloned();
                let text = extract_cc_text_flexible(&val);
                let msg = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Assistant,
                    timestamp,
                    text,
                    tool_name,
                    tool_input,
                    tool_result: None,
                    parent_ordinal: val.get("parentUuid").and_then(|v| v.as_str()).map(|_| 0),
                };
                messages.push(msg);
                ordinal += 1;
            }
            "tool_result" => {
                let tool_name = val.get("tool_name").and_then(|v| v.as_str()).map(|s| s.to_string());
                let tool_result = val.get("tool_result").cloned();
                let msg = Message {
                    session_id: id.to_string(),
                    ordinal,
                    role: Role::Tool,
                    timestamp,
                    text: None,
                    tool_name,
                    tool_input: None,
                    tool_result,
                    parent_ordinal: val.get("parentUuid").and_then(|v| v.as_str()).map(|_| 0),
                };
                messages.push(msg);
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
        provider: "claude-code".to_string(),
        project_id,
        started_at,
        last_event_at,
        model,
        title,
        token_totals: TokenTotals::default(),
        source_path: path.to_path_buf(),
        raw_payload: serde_json::Value::Null,
        body_available: true,
        messages,
        artifacts: vec![],
    })
}

fn find_inject_target() -> ConnectorResult<PathBuf> {
    Err(ConnectorError::Other(anyhow::anyhow!(
        "inject target resolution not yet implemented for Claude Code; \
         requires determining the active project directory"
    )))
}

fn compute_fence_start(_target: &Path) -> ConnectorResult<usize> {
    Ok(0)
}

#[cfg(test)]
pub struct TestClaudeCode {
    root: PathBuf,
}

#[cfg(test)]
impl TestClaudeCode {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[cfg(test)]
impl Connector for TestClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
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
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![]
        };

        Ok(Box::new(ClaudeCodeScanIter {
            files: jsonl_files,
            idx: 0,
        }))
    }

    fn load(&self, id: &str) -> ConnectorResult<Session> {
        for entry in WalkDir::new(&self.root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    let found_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if found_id == id {
                        return load_from_path(path, id);
                    }
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
