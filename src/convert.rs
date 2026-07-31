use crate::model::{Role, Session};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;

pub trait SessionConverter {
    fn convert(&self, session: &Session, target_dir: &PathBuf) -> Result<PathBuf, String>;

    fn resume_cmd(&self, session_path: &PathBuf) -> Vec<String>;
}

pub struct ClaudeCodeConverter;

impl ClaudeCodeConverter {
    pub fn new() -> Self {
        Self
    }

    fn encode_project_dir(path: &str) -> String {
        path.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect()
    }
}

impl SessionConverter for ClaudeCodeConverter {
    fn convert(&self, session: &Session, target_dir: &PathBuf) -> Result<PathBuf, String> {
        let project_dir_name = Self::encode_project_dir(
            &session.project_path().unwrap_or(session.project_id.clone())
        );
        let project_root = target_dir.join(&project_dir_name);
        std::fs::create_dir_all(&project_root)
            .map_err(|e| format!("failed to create project dir: {}", e))?;

        let filename = format!("{}.jsonl", session.id);
        let out_path = project_root.join(&filename);

        let mut records = Vec::new();

        let start_event = json!({
            "uuid": session.id,
            "type": "conversation_start",
            "cwd": session.project_path().unwrap_or_default(),
            "timestamp": format_timestamp(session.started_at),
            "title": session.title,
        });
        records.push(start_event);

        for msg in &session.messages {
            let record = match msg.role {
                Role::User => json!({
                    "uuid": format!("{}-{}", session.id, msg.ordinal),
                    "type": "user_message",
                    "cwd": session.project_path().unwrap_or_default(),
                    "timestamp": format_timestamp(msg.timestamp),
                    "content": msg.text,
                }),
                Role::Assistant => {
                    if msg.tool_name.is_some() {
                        json!({
                            "uuid": format!("{}-{}", session.id, msg.ordinal),
                            "type": "tool_use",
                            "cwd": session.project_path().unwrap_or_default(),
                            "timestamp": format_timestamp(msg.timestamp),
                            "tool_name": msg.tool_name,
                            "tool_input": msg.tool_input,
                            "content": msg.text,
                        })
                    } else {
                        json!({
                            "uuid": format!("{}-{}", session.id, msg.ordinal),
                            "type": "assistant_message",
                            "cwd": session.project_path().unwrap_or_default(),
                            "timestamp": format_timestamp(msg.timestamp),
                            "content": msg.text,
                        })
                    }
                }
                Role::Tool => json!({
                    "uuid": format!("{}-{}", session.id, msg.ordinal),
                    "type": "tool_result",
                    "cwd": session.project_path().unwrap_or_default(),
                    "timestamp": format_timestamp(msg.timestamp),
                    "tool_name": msg.tool_name,
                    "tool_result": msg.tool_result,
                }),
                Role::System => json!({
                    "uuid": format!("{}-{}", session.id, msg.ordinal),
                    "type": "assistant_message",
                    "cwd": session.project_path().unwrap_or_default(),
                    "timestamp": format_timestamp(msg.timestamp),
                    "content": msg.text,
                }),
            };
            records.push(record);
        }

        let content: Vec<String> = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .collect();
        let content = content.join("\n");

        std::fs::write(&out_path, &content)
            .map_err(|e| format!("failed to write converted session: {}", e))?;

        Ok(out_path)
    }

    fn resume_cmd(&self, session_path: &PathBuf) -> Vec<String> {
        let session_id = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        vec!["claude".to_string(), "--resume".to_string(), session_id]
    }
}

pub struct CodexCliConverter;

impl CodexCliConverter {
    pub fn new() -> Self {
        Self
    }
}

impl SessionConverter for CodexCliConverter {
    fn convert(&self, session: &Session, target_dir: &PathBuf) -> Result<PathBuf, String> {
        let now: DateTime<Utc> = Utc::now();
        let date_str = now.format("%Y/%m/%d").to_string();
        let ts_str = now.format("%Y-%m-%dT%H-%M-%S").to_string();
        let rollout_dir = target_dir.join("sessions").join(&date_str);
        std::fs::create_dir_all(&rollout_dir)
            .map_err(|e| format!("failed to create rollout dir: {}", e))?;

        let filename = format!("rollout-{}-{}.jsonl", ts_str, session.id);
        let out_path = rollout_dir.join(&filename);

        let mut records = Vec::new();
        let cwd = session.project_path().unwrap_or_default();

        let start_event = json!({
            "id": session.id,
            "type": "conversation",
            "cwd": cwd,
            "created_at": format_timestamp(session.started_at),
            "title": session.title,
        });
        records.push(start_event);

        for msg in &session.messages {
            let record = match msg.role {
                Role::User => json!({
                    "id": format!("{}-{}", session.id, msg.ordinal),
                    "type": "user_turn",
                    "cwd": cwd,
                    "created_at": format_timestamp(msg.timestamp),
                    "content": msg.text,
                }),
                Role::Assistant => {
                    if msg.tool_name.is_some() {
                        json!({
                            "id": format!("{}-{}", session.id, msg.ordinal),
                            "type": "tool_use",
                            "cwd": cwd,
                            "created_at": format_timestamp(msg.timestamp),
                            "tool_name": msg.tool_name,
                            "tool_input": msg.tool_input,
                        })
                    } else {
                        json!({
                            "id": format!("{}-{}", session.id, msg.ordinal),
                            "type": "assistant_turn",
                            "cwd": cwd,
                            "created_at": format_timestamp(msg.timestamp),
                            "content": msg.text,
                        })
                    }
                }
                Role::Tool => json!({
                    "id": format!("{}-{}", session.id, msg.ordinal),
                    "type": "tool_result",
                    "cwd": cwd,
                    "created_at": format_timestamp(msg.timestamp),
                    "tool_name": msg.tool_name,
                    "tool_result": msg.tool_result,
                }),
                Role::System => json!({
                    "id": format!("{}-{}", session.id, msg.ordinal),
                    "type": "assistant_turn",
                    "cwd": cwd,
                    "created_at": format_timestamp(msg.timestamp),
                    "content": msg.text,
                }),
            };
            records.push(record);
        }

        let content: Vec<String> = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .collect();
        let content = content.join("\n");

        std::fs::write(&out_path, &content)
            .map_err(|e| format!("failed to write codex converted session: {}", e))?;

        Ok(out_path)
    }

    fn resume_cmd(&self, session_path: &PathBuf) -> Vec<String> {
        let session_id = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        vec!["codex".to_string(), "resume".to_string(), session_id]
    }
}

pub struct OpenCodeConverter;

impl OpenCodeConverter {
    pub fn new() -> Self {
        Self
    }

    #[allow(dead_code)]
    fn generate_ses_id() -> String {
        use uuid::Uuid;
        let suffix = &Uuid::new_v4().to_string()[..12];
        format!("ses_{}", suffix)
    }
}

impl SessionConverter for OpenCodeConverter {
    fn convert(&self, _session: &Session, _target_dir: &PathBuf) -> Result<PathBuf, String> {
        Err("OpenCode SQLite direct insert not yet implemented; use inject instead".to_string())
    }

    fn resume_cmd(&self, _session_path: &PathBuf) -> Vec<String> {
        vec!["opencode".to_string(), "run".to_string(), "--session".to_string(), "".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, Role, Session, TokenTotals};

    fn test_session() -> Session {
        Session {
            id: "test-session-1".to_string(),
            provider: "claude-code".to_string(),
            project_id: "/home/user/test-project".to_string(),
            started_at: Some(DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z").unwrap().with_timezone(&Utc)),
            last_event_at: Some(DateTime::parse_from_rfc3339("2026-07-01T12:05:00Z").unwrap().with_timezone(&Utc)),
            model: Some("claude-sonnet-4".to_string()),
            title: Some("Test Session".to_string()),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![
                Message {
                    session_id: "test-session-1".to_string(),
                    ordinal: 0,
                    role: Role::User,
                    timestamp: Some(DateTime::parse_from_rfc3339("2026-07-01T12:00:05Z").unwrap().with_timezone(&Utc)),
                    text: Some("Hello".to_string()),
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                },
                Message {
                    session_id: "test-session-1".to_string(),
                    ordinal: 1,
                    role: Role::Assistant,
                    timestamp: Some(DateTime::parse_from_rfc3339("2026-07-01T12:00:10Z").unwrap().with_timezone(&Utc)),
                    text: Some("Hi, how can I help?".to_string()),
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                },
                Message {
                    session_id: "test-session-1".to_string(),
                    ordinal: 2,
                    role: Role::Assistant,
                    timestamp: Some(DateTime::parse_from_rfc3339("2026-07-01T12:00:15Z").unwrap().with_timezone(&Utc)),
                    text: None,
                    tool_name: Some("Bash".to_string()),
                    tool_input: Some(serde_json::json!({"command": "ls"})),
                    tool_result: None,
                    parent_ordinal: None,
                },
                Message {
                    session_id: "test-session-1".to_string(),
                    ordinal: 3,
                    role: Role::Tool,
                    timestamp: Some(DateTime::parse_from_rfc3339("2026-07-01T12:00:16Z").unwrap().with_timezone(&Utc)),
                    text: None,
                    tool_name: Some("Bash".to_string()),
                    tool_input: None,
                    tool_result: Some(serde_json::json!("file1.txt\nfile2.txt\n")),
                    parent_ordinal: None,
                },
            ],
            artifacts: vec![],
        }
    }

    #[test]
    fn test_claude_code_converter_roundtrip() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let converter = ClaudeCodeConverter::new();
        let result = converter.convert(&session, &tmp.path().to_path_buf());
        assert!(result.is_ok(), "conversion should succeed: {:?}", result.err());

        let path = result.unwrap();
        assert!(path.exists(), "converted file should exist");
        assert!(path.extension().is_some_and(|e| e == "jsonl"), "should be .jsonl");

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 4, "should have at least 4 JSONL lines (start + 3 msgs), got {}", lines.len());

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "conversation_start");
        assert_eq!(first["title"], "Test Session");
    }

    #[test]
    fn test_codex_cli_converter_roundtrip() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let converter = CodexCliConverter::new();
        let result = converter.convert(&session, &tmp.path().to_path_buf());
        assert!(result.is_ok(), "conversion should succeed: {:?}", result.err());

        let path = result.unwrap();
        assert!(path.exists(), "converted file should exist");

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 4, "should have at least 4 lines");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "conversation");
        assert_eq!(first["title"], "Test Session");
    }

    #[test]
    fn test_claude_code_to_codex_roundtrip_via_convert() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();

        let cc = ClaudeCodeConverter::new();
        let path_cc = cc.convert(&session, &tmp.path().to_path_buf()).unwrap();
        assert!(path_cc.exists());

        let content = std::fs::read_to_string(&path_cc).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 4);
        assert!(lines[0].contains("conversation_start"));
    }

    #[test]
    fn test_build_cross_tool_brief() {
        let mut session = test_session();
        session.messages.push(Message {
            session_id: "test-session-1".to_string(),
            ordinal: 4,
            role: Role::Assistant,
            timestamp: Some(DateTime::parse_from_rfc3339("2026-07-01T12:05:00Z").unwrap().with_timezone(&Utc)),
            text: Some("I found the issue in the configuration file: the timeout was set too low at 30 seconds, which caused the connection pool to exhaust during peak load.".to_string()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        });
        let sessions = vec![session];
        let brief = build_cross_tool_brief(&sessions);

        assert!(brief.contains("Cross-Tool Session Brief"), "should have header");
        assert!(brief.contains("Tools Used"), "should have tools section");
        assert!(brief.contains("Bash"), "should mention tools used");
        assert!(brief.contains("test-session-1"), "should mention session");
        assert!(brief.contains("configuration"), "should contain insight text");
        assert!(brief.contains("claude-code"), "should mention provider in facts");
    }

    #[test]
    fn test_converter_with_empty_messages() {
        let session = Session {
            id: "empty-msgs".to_string(),
            provider: "claude-code".to_string(),
            project_id: "/tmp".to_string(),
            started_at: None,
            last_event_at: None,
            model: None,
            title: None,
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/tmp/empty.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![],
            artifacts: vec![],
        };

        let tmp = tempfile::tempdir().unwrap();
        let cc = ClaudeCodeConverter::new();
        let result = cc.convert(&session, &tmp.path().to_path_buf());
        assert!(result.is_ok());
        let path = result.unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("conversation_start"), "at least start record");
    }

    #[test]
    fn test_codex_cli_converter_structural_check() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let cx = CodexCliConverter::new();
        let path = cx.convert(&session, &tmp.path().to_path_buf()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(first.get("id").is_some(), "codex format should have 'id'");
        assert!(first.get("created_at").is_some(), "codex format should have 'created_at'");

        let has_tool_use = lines.iter().any(|l| l.contains("tool_use"));
        let has_tool_result = lines.iter().any(|l| l.contains("tool_result"));
        assert!(has_tool_use, "should have tool_use record");
        assert!(has_tool_result, "should have tool_result record");
    }
}

fn format_timestamp(ts: Option<DateTime<Utc>>) -> Value {
    match ts {
        Some(dt) => Value::String(dt.to_rfc3339()),
        None => Value::Null,
    }
}

impl Session {
    pub fn project_path(&self) -> Option<String> {
        if !self.project_id.is_empty() {
            Some(self.project_id.clone())
        } else {
            None
        }
    }

    pub fn format_markdown_brief(&self) -> String {
        let header = format!("# Session: {} ({})", self.id, self.provider);
        let meta = format!(
            "- **Provider:** {}\n- **Model:** {}\n- **Started:** {}\n- **Messages:** {}\n- **Title:** {}\n",
            self.provider,
            self.model.as_deref().unwrap_or("unknown"),
            self.started_at.map(|t| t.to_rfc3339()).unwrap_or_else(|| "unknown".to_string()),
            self.messages.len(),
            self.title.as_deref().unwrap_or("untitled"),
        );

        let mut body = String::new();
        body.push_str(&header);
        body.push('\n');
        body.push_str(&meta);
        body.push('\n');

        for msg in &self.messages {
            let role = format!("{:?}", msg.role);
            let text = msg.text.as_deref().unwrap_or("");
            if !text.is_empty() {
                body.push_str(&format!("**{}:** {}\n\n", role, text));
            }
            if let Some(tool_name) = &msg.tool_name {
                body.push_str(&format!("_Tool: {}_\n", tool_name));
                if let Some(input) = &msg.tool_input {
                    if let Some(s) = input.as_str() {
                        body.push_str(&format!("```\n{}\n```\n\n", s));
                    } else {
                        body.push_str(&format!("```json\n{}\n```\n\n", serde_json::to_string_pretty(input).unwrap_or_default()));
                    }
                }
            }
        }

        body
    }
}

pub fn build_cross_tool_brief(sessions: &[Session]) -> String {
    let mut brief = String::new();
    brief.push_str("# agentbridge Cross-Tool Session Brief\n\n");
    brief.push_str(&format!("Aggregated from {} sessions across all providers.\n\n", sessions.len()));

    let mut facts: Vec<String> = Vec::new();
    let mut tools_used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut files_touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut commands_run: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for session in sessions {
        for msg in &session.messages {
            if let Some(text) = &msg.text {
                if text.len() > 20 && text.len() < 500 {
                    facts.push(format!("- [{}] {}: {}", session.provider, session.id, text.lines().next().unwrap_or("")));
                }
            }
            if let Some(tool_name) = &msg.tool_name {
                tools_used.insert(tool_name.clone());
            }
        }
        for artifact in &session.artifacts {
            match artifact.kind {
                crate::model::ArtifactKind::FileTouched => {
                    files_touched.insert(artifact.path_or_command.clone());
                }
                crate::model::ArtifactKind::CommandRun => {
                    commands_run.insert(artifact.path_or_command.clone());
                }
                _ => {}
            }
        }
    }

    if !tools_used.is_empty() {
        brief.push_str("## Tools Used\n\n");
        for tool in &tools_used {
            brief.push_str(&format!("- {}\n", tool));
        }
        brief.push('\n');
    }

    if !facts.is_empty() {
        brief.push_str(&format!("## Key Insights ({} extracted)\n\n", facts.len()));
        for fact in facts.iter().take(30) {
            brief.push_str(fact);
            brief.push('\n');
        }
        brief.push('\n');
    }

    if !files_touched.is_empty() {
        brief.push_str("## Files Referenced\n\n");
        for f in files_touched.iter().take(20) {
            brief.push_str(&format!("- {}\n", f));
        }
        brief.push('\n');
    }

    if !commands_run.is_empty() {
        brief.push_str("## Commands Run\n\n");
        for c in commands_run.iter().take(20) {
            brief.push_str(&format!("- `{}`\n", c));
        }
        brief.push('\n');
    }

    brief
}
