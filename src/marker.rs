//! A durable "agentbridge wrote this" marker inside generated session files.
//!
//! The manifest records what agentbridge created, but if it is lost (deleted
//! data dir, restored backup, copied machine) the generated files are
//! indistinguishable from real sessions: discovery re-ingests them as new
//! sources and multiplies every copy on the next sync. A marker inside the file
//! itself means the answer never depends on a separate file surviving.
//!
//! Where it lives, per tool (OpenCode, Codex `threads` and Antigravity rows
//! already carry their own tag column and need nothing here):
//!
//! * Claude Code: an extra `agentbridge` key on the leading `mode` record.
//! * Codex CLI: an extra `agentbridge` key inside the `session_meta` payload.
//!
//! An unknown extra key is the least intrusive place: neither tool needs it,
//! and both already carry assorted fields they ignore. **Not verified against a
//! real `claude`/`codex` binary here** (none installed on the dev machine) —
//! see HANDOFF §4's zero-cost `claude --resume` / `codex delete` signals before
//! trusting it on a real machine. If a tool ever strips the key when it rewrites
//! a file, the manifest is still the primary record; the marker is the fallback.

use crate::model::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// JSON key added to the carrying record.
pub const KEY: &str = "agentbridge";
const VERSION: u32 = 1;

/// How much of a file to inspect. The marker sits in the first few records, so
/// this bounds the cost of checking a multi-hundred-megabyte session.
const HEAD_LINES: usize = 16;
const HEAD_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub v: u32,
    /// Provider the session originally came from.
    pub origin_provider: String,
    pub origin_id: String,
    /// Turns agentbridge wrote. More than this on disk later means the tool
    /// appended new work, so the file is not safe to delete as an orphan.
    pub messages: usize,
}

/// The value to place under [`KEY`] for a session about to be written.
pub fn value(session: &Session) -> Value {
    json!({
        "v": VERSION,
        "origin_provider": session.provider,
        "origin_id": session.id,
        "messages": session.messages.len(),
    })
}

/// Read the marker from the head of a session file, if it has one. Never
/// fails: an unreadable, empty or non-JSON file simply has no marker.
pub fn read(path: &Path) -> Option<Marker> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file.take(HEAD_BYTES));
    for line in reader.lines().take(HEAD_LINES) {
        // A non-UTF-8 or over-long line ends the scan; the marker is never
        // that far in.
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let found = v.get(KEY).or_else(|| v.pointer(&format!("/payload/{KEY}")));
        if let Some(m) = found
            && let Ok(marker) = serde_json::from_value::<Marker>(m.clone())
        {
            return Some(marker);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{ClaudeCodeConverter, CodexCliConverter, SessionConverter};
    use crate::model::{Message, Role, TokenTotals};

    fn session(provider: &str, id: &str, turns: usize) -> Session {
        Session {
            id: id.into(),
            provider: provider.into(),
            project_id: "/work/proj".into(),
            started_at: chrono::DateTime::from_timestamp(1_780_000_000, 0),
            last_event_at: chrono::DateTime::from_timestamp(1_780_000_100, 0),
            model: None,
            title: Some("A session".into()),
            token_totals: TokenTotals::default(),
            source_path: "/x".into(),
            raw_payload: Value::Null,
            body_available: true,
            messages: (0..turns)
                .map(|i| Message {
                    session_id: id.into(),
                    ordinal: i as u64,
                    role: if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    timestamp: chrono::DateTime::from_timestamp(1_780_000_000 + i as i64, 0),
                    text: Some(format!("turn {i}")),
                    tool_name: None,
                    tool_input: None,
                    tool_result: None,
                    parent_ordinal: None,
                })
                .collect(),
            artifacts: vec![],
        }
    }

    #[test]
    fn test_claude_copy_carries_a_readable_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session("codex-cli", "origin-1", 4);
        let out = ClaudeCodeConverter::new().convert(&s, tmp.path()).unwrap();
        let m = read(&out).expect("generated claude file must carry the marker");
        assert_eq!(m.origin_provider, "codex-cli");
        assert_eq!(m.origin_id, "origin-1");
        assert_eq!(m.messages, 4);
    }

    #[test]
    fn test_codex_copy_carries_a_readable_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session("claude-code", "origin-2", 6);
        let out = CodexCliConverter::new()
            .convert_multi(&s, tmp.path(), &["/work/proj".to_string()])
            .unwrap();
        let m = read(&out[0]).expect("generated codex rollout must carry the marker");
        assert_eq!(m.origin_provider, "claude-code");
        assert_eq!(m.messages, 6);
    }

    #[test]
    fn test_marked_files_still_load_as_ordinary_sessions() {
        // The marker is an extra key; our own readers must not trip over it.
        let tmp = tempfile::tempdir().unwrap();
        let s = session("codex-cli", "origin-3", 4);
        let out = ClaudeCodeConverter::new().convert(&s, tmp.path()).unwrap();
        let id = out.file_stem().unwrap().to_string_lossy().to_string();
        let loaded = crate::connectors::claude_code::load_file(&out, &id).unwrap();
        assert_eq!(loaded.messages.len(), 4);
    }

    #[test]
    fn test_plain_and_broken_files_have_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &[u8]| {
            let p = tmp.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        assert!(read(&write("empty.jsonl", b"")).is_none());
        assert!(
            read(&write(
                "plain.jsonl",
                b"{\"type\":\"user\",\"cwd\":\"/a\"}\n"
            ))
            .is_none()
        );
        assert!(read(&write("junk.jsonl", b"not json at all\n\xff\xfe\n")).is_none());
        assert!(read(&tmp.path().join("missing.jsonl")).is_none());
        // A look-alike key with the wrong shape is not a marker.
        assert!(read(&write("odd.jsonl", b"{\"agentbridge\":\"yes\"}\n")).is_none());
    }

    #[test]
    fn test_marker_beyond_the_head_is_ignored() {
        // Bounded cost on huge files: only the first records are inspected.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("deep.jsonl");
        let mut body = String::new();
        for _ in 0..(HEAD_LINES + 4) {
            body.push_str("{\"type\":\"user\"}\n");
        }
        body.push_str(&format!(
            "{{\"agentbridge\":{}}}\n",
            value(&session("codex-cli", "x", 1))
        ));
        std::fs::write(&p, body).unwrap();
        assert!(read(&p).is_none());
    }
}
