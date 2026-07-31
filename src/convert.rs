use crate::model::{Role, Session};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Version string stamped into generated Claude Code records. Kept close to
/// the real client version the format was verified against (2.1.220).
const CLAUDE_CODE_VERSION: &str = "2.1.220";

/// Version stamped into generated Codex CLI `session_meta` records.
const CODEX_CLI_VERSION: &str = "0.146.0";

/// Fixed namespace for deriving stable UUIDs from foreign session ids.
/// Must never change: it is what makes a given source session map to the same
/// target id on every machine and every run.
const AGENTBRIDGE_NAMESPACE: uuid::Uuid =
    uuid::uuid!("6ba7b814-9dad-11d1-80b4-00c04fd430c8");

pub trait SessionConverter {
    fn convert(&self, session: &Session, target_dir: &PathBuf) -> Result<PathBuf, String>;

    fn resume_cmd(&self, session_path: &PathBuf) -> Vec<String>;
}

pub struct ClaudeCodeConverter;

impl ClaudeCodeConverter {
    pub fn new() -> Self {
        Self
    }

    /// Encode an absolute path the way Claude Code names its project dirs:
    /// every non-alphanumeric char becomes `-`, and **case is preserved**.
    /// Verified against real dirs: `/Users/harry/Documents/bankNotes-OCR`
    /// → `-Users-harry-Documents-bankNotes-OCR`.
    ///
    /// Note: this encoding is lossy and is only ever used to *write* into the
    /// directory Claude Code will look in. The project path itself is always
    /// read from the `cwd` field inside records, never decoded from here.
    fn encode_project_dir(path: &str) -> String {
        path.chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect()
    }

    /// Claude Code requires a UUID session id. Foreign ids that already parse
    /// as UUIDs (Codex) are reused so the id stays stable across tools.
    ///
    /// Non-UUID ids (OpenCode `ses_...`) are mapped through UUID **v5**, i.e.
    /// a pure function of the source id — the same session always yields the
    /// same UUID. A random v4 here would mint a new id (and a new filename) on
    /// every run, so sync could never be idempotent and the same session would
    /// pile up as duplicates.
    fn session_uuid(id: &str) -> String {
        match uuid::Uuid::parse_str(id) {
            Ok(u) => u.to_string(),
            Err(_) => uuid::Uuid::new_v5(&AGENTBRIDGE_NAMESPACE, id.as_bytes()).to_string(),
        }
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

        let sid = Self::session_uuid(&session.id);
        let filename = format!("{}.jsonl", sid);
        let out_path = project_root.join(&filename);

        let cwd = session.project_path().unwrap_or_default();
        let version = CLAUDE_CODE_VERSION;
        let mut records = Vec::new();

        // Claude Code's own sessions always open with these two control
        // records; its resume path expects them before any turn records.
        records.push(json!({
            "type": "mode",
            "mode": "normal",
            "sessionId": sid,
        }));
        records.push(json!({
            "type": "permission-mode",
            "permissionMode": "default",
            "sessionId": sid,
        }));

        // Records form a linked list via parentUuid -> previous record's uuid.
        let mut parent: Option<String> = None;
        let mut last_user_text: Option<String> = None;
        let mut last_uuid: Option<String> = None;
        // A tool_result block must carry the tool_use_id of the call it
        // answers, otherwise the pair can't be reassociated on read.
        let mut pending_tool_id: Option<String> = None;

        for msg in &session.messages {
            let uuid = uuid::Uuid::new_v4().to_string();
            let ts = format_timestamp_z(msg.timestamp);

            // Fields every turn record carries, regardless of role.
            let base = json!({
                "parentUuid": parent,
                "isSidechain": false,
                "userType": "external",
                "entrypoint": "cli",
                "cwd": cwd,
                "sessionId": sid,
                "version": version,
                "gitBranch": "",
                "uuid": uuid,
                "timestamp": ts,
            });

            let record = match msg.role {
                Role::User => {
                    let text = msg.text.clone().unwrap_or_default();
                    last_user_text = Some(text.clone());
                    merge(base, json!({
                        "type": "user",
                        "message": { "role": "user", "content": text },
                    }))
                }
                Role::Assistant => {
                    // Tool calls are content blocks inside an assistant
                    // message, not a distinct record type.
                    let content = if let Some(tool) = &msg.tool_name {
                        let tool_id = format!("toolu_{}", uuid.replace('-', ""));
                        pending_tool_id = Some(tool_id.clone());
                        json!([{
                            "type": "tool_use",
                            "id": tool_id,
                            "name": tool,
                            "input": msg.tool_input.clone().unwrap_or(json!({})),
                        }])
                    } else {
                        json!([{ "type": "text", "text": msg.text.clone().unwrap_or_default() }])
                    };
                    merge(base, json!({
                        "type": "assistant",
                        "requestId": format!("req_{}", uuid.replace('-', "")),
                        "message": {
                            "id": format!("msg_{}", uuid.replace('-', "")),
                            "type": "message",
                            "role": "assistant",
                            "model": session.model.clone().unwrap_or_else(|| "unknown".to_string()),
                            "content": content,
                            "stop_reason": Value::Null,
                            "stop_sequence": Value::Null,
                            "usage": { "input_tokens": 0, "output_tokens": 0 },
                        },
                    }))
                }
                // Tool results come back as a user-role record carrying a
                // tool_result content block — same as Claude Code writes them.
                Role::Tool => merge(base, json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": pending_tool_id
                                .take()
                                .unwrap_or_else(|| format!("toolu_{}", uuid.replace('-', ""))),
                            "content": tool_result_text(msg.tool_result.as_ref()),
                        }],
                    },
                })),
                Role::System => merge(base, json!({
                    "type": "system",
                    "isMeta": true,
                    "subtype": "info",
                    "content": msg.text.clone().unwrap_or_default(),
                })),
            };

            parent = Some(uuid.clone());
            last_uuid = Some(uuid);
            records.push(record);
        }

        // Trailing pointer record: tells Claude Code which leaf to resume from.
        if let Some(leaf) = last_uuid {
            records.push(json!({
                "type": "last-prompt",
                "lastPrompt": last_user_text.unwrap_or_default(),
                "leafUuid": leaf,
                "sessionId": sid,
            }));
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
        // Derive the rollout path from the session's own start time, never
        // from `now`: the same session must always map to the same filename,
        // or sync can never be idempotent (it would create a new rollout on
        // every run). Falls back to a fixed epoch when the source has no
        // timestamp, so the result stays deterministic.
        let anchor: DateTime<Utc> = session
            .started_at
            .or(session.last_event_at)
            .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
        let date_str = anchor.format("%Y/%m/%d").to_string();
        let ts_str = anchor.format("%Y-%m-%dT%H-%M-%S").to_string();
        let rollout_dir = target_dir.join("sessions").join(&date_str);
        std::fs::create_dir_all(&rollout_dir)
            .map_err(|e| format!("failed to create rollout dir: {}", e))?;

        let sid = ClaudeCodeConverter::session_uuid(&session.id);
        let filename = format!("rollout-{}-{}.jsonl", ts_str, sid);
        let out_path = rollout_dir.join(&filename);

        let mut records = Vec::new();
        let cwd = session.project_path().unwrap_or_default();
        let meta_ts = format_timestamp_z(session.started_at);

        // Codex identifies a session by the leading session_meta record; the
        // real format nests everything under `payload`.
        records.push(json!({
            "timestamp": meta_ts,
            "type": "session_meta",
            "payload": {
                "session_id": sid,
                "id": sid,
                "timestamp": meta_ts,
                "cwd": cwd,
                "originator": "codex-cli",
                "cli_version": CODEX_CLI_VERSION,
                "source": "cli",
                "thread_source": "user",
            },
        }));

        // Codex pairs a call with its output by matching call_id, so a
        // tool result must reuse the id of the call it answers.
        let mut pending_call_id: Option<String> = None;

        for msg in &session.messages {
            let ts = format_timestamp_z(msg.timestamp);
            let payload = match msg.role {
                Role::User => json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": msg.text.clone().unwrap_or_default() }],
                }),
                Role::Assistant => {
                    if let Some(tool) = &msg.tool_name {
                        let call_id = format!("call_{}", msg.ordinal);
                        pending_call_id = Some(call_id.clone());
                        json!({
                            "type": "function_call",
                            "name": tool,
                            "arguments": serde_json::to_string(
                                &msg.tool_input.clone().unwrap_or(json!({}))
                            ).unwrap_or_default(),
                            "call_id": call_id,
                        })
                    } else {
                        json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": msg.text.clone().unwrap_or_default() }],
                        })
                    }
                }
                Role::Tool => json!({
                    "type": "function_call_output",
                    "call_id": pending_call_id
                        .take()
                        .unwrap_or_else(|| format!("call_{}", msg.ordinal)),
                    "output": tool_result_text(msg.tool_result.as_ref()),
                }),
                Role::System => json!({
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "input_text", "text": msg.text.clone().unwrap_or_default() }],
                }),
            };

            records.push(json!({
                "timestamp": ts,
                "type": "response_item",
                "payload": payload,
            }));
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
        // Stem is `rollout-<timestamp>-<uuid>`; the trailing 5 hyphen-separated
        // groups are the UUID. Taking the whole stem would pass a bogus id.
        let stem = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let parts: Vec<&str> = stem.split('-').collect();
        let session_id = if parts.len() >= 5 {
            parts[parts.len() - 5..].join("-")
        } else {
            stem.to_string()
        };
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

    /// Valid UUID: Claude Code rejects non-UUID ids outright, so the
    /// fixture must use a real one to exercise the id-preserving path.
    const TEST_SID: &str = "11111111-2222-4333-8444-555555555555";

    fn test_session() -> Session {
        Session {
            id: TEST_SID.to_string(),
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
                    session_id: TEST_SID.to_string(),
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
                    session_id: TEST_SID.to_string(),
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
                    session_id: TEST_SID.to_string(),
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
                    session_id: TEST_SID.to_string(),
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

    /// Parse a converted file into records, failing loudly on bad JSON.
    fn records(path: &PathBuf) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every line must be valid JSON"))
            .collect()
    }

    /// Locks the *real* Claude Code on-disk schema, verified against a live
    /// 2.1.220 session. The previous version of this test asserted the
    /// converter's own invented shape, so it passed while `claude --resume`
    /// rejected every file the converter produced — see CONNECTORS.md §6.
    #[test]
    fn test_claude_code_converter_emits_real_schema() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let path = ClaudeCodeConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .expect("conversion should succeed");

        assert!(path.extension().is_some_and(|e| e == "jsonl"));
        // Filename stem must be the bare session UUID — that's how Claude
        // Code resolves `--resume <id>`.
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), session.id);

        let recs = records(&path);

        // Control records, in order, before any turn records.
        assert_eq!(recs[0]["type"], "mode");
        assert_eq!(recs[0]["mode"], "normal");
        assert_eq!(recs[1]["type"], "permission-mode");
        assert_eq!(recs[1]["permissionMode"], "default");

        // Every record must carry sessionId, or resume cannot match the file.
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(
                r["sessionId"], session.id.as_str(),
                "record {i} ({}) is missing/wrong sessionId", r["type"]
            );
        }

        // Turn records use real type names and nest the payload under
        // `message`, with content as a string (user) or block array (assistant).
        let user = recs.iter().find(|r| r["type"] == "user").expect("a user record");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(user["message"]["content"], "Hello");
        assert!(user["cwd"].is_string(), "turn records need cwd");
        assert!(user["uuid"].is_string(), "turn records need uuid");

        let asst = recs
            .iter()
            .find(|r| r["type"] == "assistant" && r["message"]["content"][0]["type"] == "text")
            .expect("a text assistant record");
        assert_eq!(asst["message"]["role"], "assistant");
        assert_eq!(asst["message"]["content"][0]["text"], "Hi, how can I help?");

        // Tool calls are content blocks on an assistant message, not their
        // own record type.
        let tool_use = recs
            .iter()
            .find(|r| r["message"]["content"][0]["type"] == "tool_use")
            .expect("a tool_use block");
        assert_eq!(tool_use["type"], "assistant");
        assert_eq!(tool_use["message"]["content"][0]["name"], "Bash");

        // Tool results come back as user-role records.
        let tool_res = recs
            .iter()
            .find(|r| r["message"]["content"][0]["type"] == "tool_result")
            .expect("a tool_result block");
        assert_eq!(tool_res["type"], "user");
        // The result must reference the call it answers.
        assert_eq!(
            tool_res["message"]["content"][0]["tool_use_id"],
            tool_use["message"]["content"][0]["id"],
            "tool_result.tool_use_id must match the preceding tool_use.id"
        );

        // Trailing leaf pointer.
        let last = recs.last().unwrap();
        assert_eq!(last["type"], "last-prompt");
        assert!(last["leafUuid"].is_string());
    }

    /// parentUuid must form an unbroken chain: first turn is null-rooted, each
    /// subsequent turn points at its predecessor's uuid.
    #[test]
    fn test_claude_code_parent_uuid_chain_is_linked() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let path = ClaudeCodeConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .unwrap();

        let turns: Vec<serde_json::Value> = records(&path)
            .into_iter()
            .filter(|r| r.get("uuid").is_some())
            .collect();
        assert_eq!(turns.len(), 4, "one record per message");

        assert!(turns[0]["parentUuid"].is_null(), "first turn is root");
        for pair in turns.windows(2) {
            assert_eq!(
                pair[1]["parentUuid"], pair[0]["uuid"],
                "parentUuid must point at the previous record's uuid"
            );
        }
    }

    /// A non-UUID source id (OpenCode `ses_...`) must be replaced, because
    /// Claude Code rejects ids that don't parse as UUIDs before it even looks
    /// on disk.
    /// Determinism guard: the same source session must always map to the same
    /// target id/path, or sync creates duplicates on every run.
    #[test]
    fn test_session_uuid_is_deterministic_for_non_uuid_ids() {
        let a = ClaudeCodeConverter::session_uuid("ses_8f3a2b1c9d4e");
        let b = ClaudeCodeConverter::session_uuid("ses_8f3a2b1c9d4e");
        assert_eq!(a, b, "same source id must always map to the same uuid");
        assert!(uuid::Uuid::parse_str(&a).is_ok());
        assert_ne!(
            a,
            ClaudeCodeConverter::session_uuid("ses_different"),
            "different sources must not collide"
        );
    }

    /// Codex rollout paths must derive from the session's own start time, not
    /// from `now`, for the same reason.
    #[test]
    fn test_codex_rollout_path_is_deterministic() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let c = CodexCliConverter::new();
        let p1 = c.convert(&session, &tmp.path().to_path_buf()).unwrap();
        let p2 = c.convert(&session, &tmp.path().to_path_buf()).unwrap();
        assert_eq!(p1, p2, "same session must always map to the same rollout path");
    }

    #[test]
    fn test_non_uuid_session_id_is_replaced_with_uuid() {
        let mut session = test_session();
        session.id = "ses_8f3a2b1c9d4e".to_string();

        let tmp = tempfile::tempdir().unwrap();
        let path = ClaudeCodeConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .unwrap();

        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert!(uuid::Uuid::parse_str(stem).is_ok(), "stem must be a UUID, got {stem}");
        assert_ne!(stem, "ses_8f3a2b1c9d4e");
        // and the id must be consistent inside the file
        assert_eq!(records(&path)[0]["sessionId"], stem);
    }

    /// Claude Code preserves case when encoding project dirs — verified
    /// against the real `-Users-harry-Documents-bankNotes-OCR`. Lowercasing
    /// silently works on macOS (case-insensitive FS) but breaks on Linux.
    #[test]
    fn test_encode_project_dir_preserves_case() {
        assert_eq!(
            ClaudeCodeConverter::encode_project_dir("/Users/harry/Documents/bankNotes-OCR"),
            "-Users-harry-Documents-bankNotes-OCR"
        );
        assert_eq!(
            ClaudeCodeConverter::encode_project_dir("/Users/harry/denom. model"),
            "-Users-harry-denom--model"
        );
    }

    /// Locks the *real* Codex CLI rollout schema, verified against a live
    /// 0.146.0 session: a leading `session_meta` record and everything nested
    /// under `payload`.
    #[test]
    fn test_codex_cli_converter_emits_real_schema() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let path = CodexCliConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .expect("conversion should succeed");

        // Date-partitioned path + rollout-<ts>-<uuid> filename.
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert!(stem.starts_with("rollout-"), "got {stem}");
        assert!(stem.ends_with(&session.id), "filename must end with the uuid: {stem}");
        assert!(
            path.parent().unwrap().to_str().unwrap().contains("sessions"),
            "must live under sessions/YYYY/MM/DD"
        );

        let recs = records(&path);

        // Leading session_meta with the id inside payload — this is what
        // Codex matches `resume <id>` against.
        assert_eq!(recs[0]["type"], "session_meta");
        assert_eq!(recs[0]["payload"]["session_id"], session.id.as_str());
        assert_eq!(recs[0]["payload"]["id"], session.id.as_str());
        assert_eq!(recs[0]["payload"]["cwd"], "/home/user/test-project");
        assert!(recs[0]["timestamp"].is_string());

        // Turn records are response_item with a payload wrapper.
        for r in &recs[1..] {
            assert_eq!(r["type"], "response_item");
            assert!(r["payload"].is_object(), "payload wrapper is required");
            assert!(r["timestamp"].is_string());
        }

        let user = recs
            .iter()
            .find(|r| r["payload"]["role"] == "user")
            .expect("a user message");
        assert_eq!(user["payload"]["type"], "message");
        assert_eq!(user["payload"]["content"][0]["type"], "input_text");
        assert_eq!(user["payload"]["content"][0]["text"], "Hello");

        let asst = recs
            .iter()
            .find(|r| r["payload"]["role"] == "assistant")
            .expect("an assistant message");
        assert_eq!(asst["payload"]["content"][0]["type"], "output_text");

        // Tool call / result use Codex's function_call pair, matched by call_id.
        let call = recs
            .iter()
            .find(|r| r["payload"]["type"] == "function_call")
            .expect("a function_call");
        assert_eq!(call["payload"]["name"], "Bash");
        let out = recs
            .iter()
            .find(|r| r["payload"]["type"] == "function_call_output")
            .expect("a function_call_output");
        assert_eq!(out["payload"]["call_id"], call["payload"]["call_id"]);
    }

    /// `codex resume` takes a bare UUID; the stem is `rollout-<ts>-<uuid>`, so
    /// naively using the whole stem passes a bogus id.
    #[test]
    fn test_codex_resume_cmd_extracts_bare_uuid() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let converter = CodexCliConverter::new();
        let path = converter.convert(&session, &tmp.path().to_path_buf()).unwrap();

        let cmd = converter.resume_cmd(&path);
        assert_eq!(cmd[0], "codex");
        assert_eq!(cmd[1], "resume");
        assert_eq!(cmd[2], session.id, "must be the bare uuid, not the rollout stem");
    }

    /// Self-consistency: a file written by the Claude Code converter must be
    /// readable by our own Claude Code connector, with messages preserved.
    /// This is the closest we get to an end-to-end guarantee in a unit test —
    /// real-binary verification is recorded in CONNECTORS.md §6.
    #[test]
    fn test_converted_claude_session_is_readable_by_our_connector() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let path = ClaudeCodeConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .unwrap();

        let loaded = crate::connectors::claude_code::load_for_testing(&path, &session.id)
            .expect("our own connector must be able to read what we wrote");

        assert_eq!(loaded.id, session.id);
        assert_eq!(
            loaded.project_id, "/home/user/test-project",
            "cwd must survive the round trip"
        );
        let texts: Vec<String> = loaded
            .messages
            .iter()
            .filter_map(|m| m.text.clone())
            .collect();
        assert!(texts.iter().any(|t| t.contains("Hello")), "user text preserved");
        assert!(
            texts.iter().any(|t| t.contains("how can I help")),
            "assistant text preserved"
        );
    }

    #[test]
    fn test_build_cross_tool_brief() {
        let mut session = test_session();
        session.messages.push(Message {
            session_id: TEST_SID.to_string(),
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
        assert!(brief.contains(TEST_SID), "should mention session");
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
        let recs = records(&result.unwrap());
        // Control records are still emitted for a body-less session, so the
        // file is a structurally valid (if empty) Claude Code session.
        assert_eq!(recs.len(), 2, "mode + permission-mode only");
        assert_eq!(recs[0]["type"], "mode");
        assert_eq!(recs[1]["type"], "permission-mode");
    }

    /// Self-consistency for the Codex direction.
    #[test]
    fn test_converted_codex_session_is_readable_by_our_connector() {
        let session = test_session();
        let tmp = tempfile::tempdir().unwrap();
        let path = CodexCliConverter::new()
            .convert(&session, &tmp.path().to_path_buf())
            .unwrap();

        let loaded = crate::connectors::codex_cli::load_for_testing(&path, &session.id)
            .expect("our own connector must be able to read what we wrote");

        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.project_id, "/home/user/test-project");
        let texts: Vec<String> = loaded
            .messages
            .iter()
            .filter_map(|m| m.text.clone())
            .collect();
        assert!(texts.iter().any(|t| t.contains("Hello")), "user text preserved");
    }
}

/// Claude Code / Codex write timestamps in the `...Z` millisecond form
/// (`2026-07-15T04:01:37.079Z`), not the `+00:00` offset form that
/// `to_rfc3339()` produces.
fn format_timestamp_z(ts: Option<DateTime<Utc>>) -> Value {
    match ts {
        Some(dt) => Value::String(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        None => Value::String(Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
    }
}

/// Shallow-merge `extra` into `base` (both must be JSON objects).
fn merge(base: Value, extra: Value) -> Value {
    let mut out = base;
    if let (Some(map), Some(add)) = (out.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            map.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Tool results are rendered as text; a raw JSON value is stringified rather
/// than nested, matching how Claude Code stores tool_result content.
fn tool_result_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
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
