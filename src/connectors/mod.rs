//! Registration point for every connector. This is the **one** file (besides
//! the new connector's own file) that changes when a provider is added — see
//! `crate::connector` for the interface contract.

use crate::connector::{Connector, Registry};

pub(crate) mod antigravity;
pub(crate) mod claude_code;
pub(crate) mod codex_cli;
pub(crate) mod opencode;

/// How long a read of another tool's database waits for a lock before giving up.
///
/// rusqlite's default is 5 seconds, and `load` opens a fresh connection per
/// session, so one long-held lock would stall a sync for minutes. A short wait
/// turns it into an ordinary per-session error the sync reports and moves past.
pub(crate) const READ_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Open another tool's SQLite database strictly read-only. Never with
/// `immutable=1`: that tells SQLite the file cannot change, which risks stale or
/// torn reads while the tool is running (and ignores its WAL).
pub(crate) fn open_read_only(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(READ_BUSY_TIMEOUT)?;
    Ok(conn)
}

/// How much of the end of a JSONL file `scan()` looks at, to learn what the
/// head cannot: the true last event time and any later rename.
const TAIL_BYTES: u64 = 32 * 1024;

/// The JSON records in the last `TAIL_BYTES` of a JSONL file, oldest first.
///
/// `scan()` must stay cheap (no full-body read), but stopping at the first
/// record made `last_event_at` the session's *start* time and hid any rename
/// made later, which breaks "rank by the last event inside the file" (SPEC §5).
/// A line cut by the start of the window, and a final line still being
/// written, are dropped rather than guessed at. A single record bigger than the
/// window yields nothing, in which case callers keep what the head told them.
pub(crate) fn tail_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.take(TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    lines
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

pub fn all() -> Registry {
    let connectors: Vec<Box<dyn Connector>> = vec![
        Box::new(antigravity::AntigravityConnector::new()),
        Box::new(claude_code::ClaudeCodeConnector::new()),
        Box::new(codex_cli::CodexCliConnector::new()),
        Box::new(opencode::OpenCodeConnector::new()),
    ];
    Registry::new(connectors)
}

#[cfg(test)]
pub(crate) fn all_for_testing(fixture_root: &std::path::Path) -> Registry {
    let mut connectors: Vec<Box<dyn Connector>> = vec![];

    let cc_root = fixture_root.join("claude-code");
    if cc_root.exists() {
        connectors.push(Box::new(claude_code::TestClaudeCode::new(cc_root)));
    }

    let cx_root = fixture_root.join("codex-cli");
    if cx_root.exists() {
        connectors.push(Box::new(codex_cli::TestCodexCli::new(cx_root)));
    }

    Registry::new(connectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn fixture_root() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests");
        p.push("fixtures");
        p
    }

    #[test]
    fn test_claude_code_scan_finds_all_sessions() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let results: Vec<_> = cc.scan().unwrap().filter_map(|r| r.ok()).collect();

        let names: std::collections::BTreeSet<&str> = results
            .iter()
            .map(|r| r.id.as_str())
            .collect();

        assert!(names.contains("normal-multi-turn"), "should find normal-multi-turn");
        assert!(names.contains("tool-calls-large-output"), "should find tool-calls-large-output");
        assert!(names.contains("compacted-session"), "should find compacted-session");
        assert!(names.contains("embedded-secret"), "should find embedded-secret");
        assert!(names.contains("legacy-timestamp"), "should find legacy-timestamp");
        assert!(names.contains("non-utf8"), "should find non-utf8");
        assert!(names.contains("truncated-final-line"), "should find truncated-final-line");
    }

    #[test]
    fn test_claude_code_scan_skips_empty_file() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let results: Vec<_> = cc.scan().unwrap().filter_map(|r| r.ok()).collect();
        let has_empty = results.iter().any(|r| r.id == "empty-file");
        assert!(!has_empty, "empty file should be skipped");
    }

    #[test]
    fn test_claude_code_load_normal_session() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("normal-multi-turn").unwrap();

        assert_eq!(session.id, "normal-multi-turn");
        assert_eq!(session.provider, "claude-code");
        assert!(session.messages.len() >= 5, "should have at least 5 messages, got {}", session.messages.len());

        let first_user = &session.messages[0];
        assert_eq!(first_user.role, Role::User);

        let last_msg = session.messages.last().unwrap();
        assert_eq!(last_msg.role, Role::Tool);
    }

    #[test]
    fn test_claude_code_load_tool_calls() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("tool-calls-large-output").unwrap();

        let tool_msgs: Vec<_> = session.messages.iter().filter(|m| m.tool_name.is_some()).collect();
        assert!(tool_msgs.len() >= 2, "should have tool calls, got {}", tool_msgs.len());

        let tool_result = session.messages.iter().find(|m| m.role == Role::Tool);
        assert!(tool_result.is_some(), "should have tool result messages");
    }

    #[test]
    fn test_claude_code_load_legacy_timestamps() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("legacy-timestamp").unwrap();

        assert!(session.started_at.is_some(), "should parse epoch timestamp");
        assert!(session.messages.len() >= 2, "should have messages, got {}", session.messages.len());
    }

    #[test]
    fn test_claude_code_load_truncated_handles_gracefully() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("truncated-final-line").unwrap();

        assert!(session.messages.len() >= 2, "should parse messages before truncation, got {}",
            session.messages.len());
    }

    /// `-n/--name` and in-session rename write dedicated `custom-title`/
    /// `agent-name` records, not a field on a turn — `load()` (full session,
    /// what sync/pull actually use) must see the last rename, not the
    /// original `conversation_start` title.
    ///
    /// `scan()` is a different story: it stops at the first record carrying
    /// `cwd` (RawSession's contract is "cheap, no full-file read" — see
    /// `model.rs`), so a rename recorded *after* that point is invisible to
    /// it until `load()` is used. That only affects `agentbridge list`'s
    /// display, not sync/pull-back, which always goes through `load()`.
    #[test]
    fn test_claude_code_title_prefers_last_custom_title_record() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();

        let results: Vec<_> = cc.scan().unwrap().filter_map(|r| r.ok()).collect();
        let raw = results.iter().find(|r| r.id == "renamed-session").expect("should find renamed-session");
        assert_eq!(
            raw.title.as_deref(),
            Some("Final rename"),
            "scan() reads the tail too, so a rename made after the first cwd record shows in `ls`"
        );

        let session = cc.load("renamed-session").unwrap();
        assert_eq!(session.title.as_deref(), Some("Final rename"), "load() reads the whole file, so it sees the rename");
    }

    #[test]
    fn test_codex_cli_scan_finds_sessions() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let results: Vec<_> = cx.scan().unwrap().filter_map(|r| r.ok()).collect();

        assert!(results.len() >= 5, "should find at least 5 codex sessions, got {}", results.len());

        let has_tool_session = results.iter().any(|r| r.title.as_deref() == Some("Refactor database layer"));
        assert!(has_tool_session, "should find the refactor session");
    }

    #[test]
    fn test_codex_cli_load_session() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let session = cx.load("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();

        assert_eq!(session.provider, "codex-cli");
        assert!(session.messages.len() >= 3, "should have messages, got {}", session.messages.len());
    }

    #[test]
    fn test_codex_cli_load_legacy_timestamps() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let session = cx.load("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();

        assert!(session.started_at.is_some(), "should parse epoch timestamp");
        assert!(!session.messages.is_empty(), "should have messages");
    }

    #[test]
    fn test_codex_cli_scan_skips_empty() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let results: Vec<_> = cx.scan().unwrap().filter_map(|r| r.ok()).collect();

        let has_empty = results.iter().any(|r| r.id == "empty-session");
        assert!(!has_empty, "empty file should be skipped");
    }

    #[test]
    fn test_codex_cli_load_not_found() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let result = cx.load("nonexistent-id-12345");
        assert!(result.is_err(), "should error on unknown id");
    }

    #[test]
    fn test_detect_on_fixtures() {
        let registry = all_for_testing(&fixture_root());
        let detected: Vec<&str> = registry.detected().map(|c| c.id()).collect();
        assert!(detected.contains(&"claude-code"), "claude-code should be detected");
        assert!(detected.contains(&"codex-cli"), "codex-cli should be detected");
    }

    #[test]
    fn test_scan_err_does_not_abort_whole_scan() {
        use crate::connector::Connector;
        let cx_root = fixture_root().join("codex-cli");
        let connector = codex_cli::TestCodexCli::new(cx_root);
        let results: Vec<_> = connector.scan().unwrap().collect();
        let err_count = results.iter().filter(|r| r.is_err()).count();
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert!(ok_count >= 5, "should have successes despite any errors: {}", ok_count);
        assert_eq!(err_count, 0, "should have no errors in scan");
    }

    // ---- scan() reads the tail: real last-event time and late renames ----

    fn scan_one_claude(dir: &std::path::Path, id: &str) -> crate::model::RawSession {
        let c = crate::connectors::claude_code::TestClaudeCode::new(dir.to_path_buf());
        c.scan().unwrap().filter_map(|r| r.ok()).find(|r| r.id == id).expect("scanned")
    }

    fn claude_rec(i: usize, ts: &str, pad: usize) -> String {
        serde_json::json!({
            "type": "user", "cwd": "/work/p", "timestamp": ts,
            "uuid": format!("u{i}"), "message": {"role": "user", "content": "x".repeat(pad)},
        })
        .to_string()
    }

    #[test]
    fn test_scan_last_event_is_the_end_of_the_file_not_the_start() {
        let tmp = tempfile::tempdir().unwrap();
        // A long session: ~600 KB, far larger than the tail window, so the
        // last timestamp is only reachable by reading the end.
        let mut body = String::new();
        for i in 0..600 {
            let ts = format!("2026-07-01T12:{:02}:{:02}.000Z", i / 60, i % 60);
            body.push_str(&claude_rec(i, &ts, 1000));
            body.push('\n');
        }
        std::fs::write(tmp.path().join("long.jsonl"), body).unwrap();

        let raw = scan_one_claude(tmp.path(), "long");
        let started = raw.started_at.unwrap();
        let last = raw.last_event_at.unwrap();
        assert_eq!(started.to_rfc3339(), "2026-07-01T12:00:00+00:00");
        assert_eq!(last.to_rfc3339(), "2026-07-01T12:09:59+00:00", "should be the final record's time");
    }

    #[test]
    fn test_scan_ignores_a_final_line_that_is_still_being_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::new();
        body.push_str(&claude_rec(0, "2026-07-01T12:00:00.000Z", 10));
        body.push('\n');
        body.push_str(&claude_rec(1, "2026-07-01T12:05:00.000Z", 10));
        body.push('\n');
        // A tool is mid-append: no closing brace, no newline.
        body.push_str("{\"type\":\"user\",\"timestamp\":\"2026-07-01T13:00:00.000Z\",\"mess");
        std::fs::write(tmp.path().join("live.jsonl"), body).unwrap();

        let raw = scan_one_claude(tmp.path(), "live");
        assert_eq!(raw.last_event_at.unwrap().to_rfc3339(), "2026-07-01T12:05:00+00:00");
    }

    #[test]
    fn test_scan_sees_a_rename_made_late_in_a_long_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::new();
        body.push_str(&serde_json::json!({"type":"custom-title","customTitle":"Old name","sessionId":"s"}).to_string());
        body.push('\n');
        for i in 0..400 {
            body.push_str(&claude_rec(i, "2026-07-01T12:00:00.000Z", 1000));
            body.push('\n');
        }
        body.push_str(&serde_json::json!({"type":"custom-title","customTitle":"New name","sessionId":"s"}).to_string());
        body.push('\n');
        std::fs::write(tmp.path().join("renamed.jsonl"), body).unwrap();

        assert_eq!(scan_one_claude(tmp.path(), "renamed").title.as_deref(), Some("New name"));
    }

    #[test]
    fn test_scan_survives_a_single_record_larger_than_the_tail_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = claude_rec(0, "2026-07-01T12:00:00.000Z", 10);
        body.push('\n');
        // One huge tool result as the last record: nothing parseable in the
        // window, so the head's answer must stand.
        body.push_str(&claude_rec(1, "2026-07-01T12:30:00.000Z", 200_000));
        body.push('\n');
        std::fs::write(tmp.path().join("huge.jsonl"), body).unwrap();

        let raw = scan_one_claude(tmp.path(), "huge");
        assert_eq!(raw.started_at.unwrap().to_rfc3339(), "2026-07-01T12:00:00+00:00");
        assert!(raw.last_event_at.is_some(), "must still report a time");
    }

    #[test]
    fn test_codex_scan_last_event_follows_the_rollout_tail() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let raws: Vec<_> = cx.scan().unwrap().filter_map(|r| r.ok()).collect();
        // Real multi-turn rollouts: the last event is never before the start,
        // and for at least one it is strictly later (it used to equal it).
        assert!(raws.iter().all(|r| r.last_event_at >= r.started_at));
        assert!(
            raws.iter().any(|r| r.last_event_at > r.started_at),
            "no rollout reported an end later than its start"
        );
    }

    // ---- readers understand the real record shapes, not just our fixtures ----

    fn write_lines(dir: &std::path::Path, name: &str, lines: &[serde_json::Value]) -> std::path::PathBuf {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Hand-written in the shape real Claude Code produces: one assistant API
    /// message split over several records, tool calls as `tool_use` blocks, and
    /// tool output as a *user* record of `tool_result` blocks.
    #[test]
    fn test_claude_reader_understands_real_tool_use_and_tool_result_blocks() {
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let sid = "5b0e0000-0000-4000-8000-000000000001";
        let path = write_lines(tmp.path(), &format!("{sid}.jsonl"), &[
            json!({"type":"user","cwd":"/w","sessionId":sid,"timestamp":"2026-07-01T10:00:00Z",
                   "message":{"role":"user","content":"list the files"}}),
            json!({"type":"assistant","cwd":"/w","timestamp":"2026-07-01T10:00:01Z",
                   "message":{"role":"assistant","content":[
                       {"type":"thinking","thinking":"private reasoning","signature":"x"},
                       {"type":"text","text":"I'll look."},
                       {"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls -la"}}]}}),
            json!({"type":"user","cwd":"/w","timestamp":"2026-07-01T10:00:02Z",
                   "message":{"role":"user","content":[
                       {"type":"tool_result","tool_use_id":"toolu_01",
                        "content":[{"type":"text","text":"a.txt\nb.txt"}]}]}}),
            // The model's private reasoning is written as its own record,
            // holding nothing but a `thinking` block. It is not a turn.
            json!({"type":"assistant","cwd":"/w","timestamp":"2026-07-01T10:00:02.5Z",
                   "message":{"role":"assistant","content":[
                       {"type":"thinking","thinking":"more private reasoning","signature":"y"}]}}),
            json!({"type":"assistant","cwd":"/w","timestamp":"2026-07-01T10:00:03Z",
                   "message":{"role":"assistant","content":[{"type":"text","text":"Two files."}]}}),
        ]);
        let s = crate::connectors::claude_code::load_file(&path, sid).unwrap();
        let view: Vec<_> = s
            .messages
            .iter()
            .map(|m| (m.role, m.text.clone(), m.tool_name.clone()))
            .collect();
        assert_eq!(view, vec![
            (Role::User, Some("list the files".to_string()), None),
            (Role::Assistant, Some("I'll look.".to_string()), None),
            (Role::Assistant, None, Some("Bash".to_string())),
            (Role::Tool, None, Some("Bash".to_string())),
            (Role::Assistant, Some("Two files.".to_string()), None),
        ], "a tool call is its own turn, its result is a tool turn (not an empty user message), \
           and a thinking-only record is no turn at all");
        assert_eq!(s.messages[2].tool_input, Some(json!({"command":"ls -la"})));
        assert_eq!(s.messages[3].tool_result, Some(json!("a.txt\nb.txt")));
        assert!(
            !s.messages.iter().any(|m| m.text.as_deref().is_some_and(|t| t.contains("private reasoning"))),
            "thinking blocks must not leak into the transcript"
        );
    }

    /// Hand-written in the shape real Codex rollouts have: messages, function
    /// calls paired with outputs by `call_id`, and encrypted reasoning.
    #[test]
    fn test_codex_reader_understands_real_function_calls_and_skips_reasoning() {
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let id = "5b0e0000-0000-4000-8000-0000000000c1";
        let path = write_lines(tmp.path(), &format!("rollout-2026-07-01T10-00-00-{id}.jsonl"), &[
            json!({"timestamp":"2026-07-01T10:00:00.000Z","type":"session_meta",
                   "payload":{"id":id,"cwd":"/w","timestamp":"2026-07-01T10:00:00.000Z"}}),
            json!({"timestamp":"2026-07-01T10:00:01.000Z","type":"response_item",
                   "payload":{"type":"message","role":"user",
                              "content":[{"type":"input_text","text":"run the tests"}]}}),
            json!({"timestamp":"2026-07-01T10:00:02.000Z","type":"response_item",
                   "payload":{"type":"reasoning","summary":[],"encrypted_content":"gAAAA..."}}),
            json!({"timestamp":"2026-07-01T10:00:03.000Z","type":"response_item",
                   "payload":{"type":"function_call","name":"shell","call_id":"call_9",
                              "arguments":"{\"command\":[\"cargo\",\"test\"]}"}}),
            json!({"timestamp":"2026-07-01T10:00:04.000Z","type":"response_item",
                   "payload":{"type":"function_call_output","call_id":"call_9","output":"all passed"}}),
            json!({"timestamp":"2026-07-01T10:00:05.000Z","type":"response_item",
                   "payload":{"type":"message","role":"assistant",
                              "content":[{"type":"output_text","text":"Tests pass."}]}}),
        ]);
        let s = crate::connectors::codex_cli::load_file(&path, id).unwrap();
        let view: Vec<_> = s
            .messages
            .iter()
            .map(|m| (m.role, m.text.clone(), m.tool_name.clone()))
            .collect();
        assert_eq!(view, vec![
            (Role::User, Some("run the tests".to_string()), None),
            (Role::Assistant, None, Some("shell".to_string())),
            (Role::Tool, None, Some("shell".to_string())),
            (Role::Assistant, Some("Tests pass.".to_string()), None),
        ], "reasoning is not a turn; the call and its output are paired by call_id");
        assert_eq!(s.messages[1].tool_input, Some(json!({"command":["cargo","test"]})));
        assert_eq!(s.messages[2].tool_result, Some(json!("all passed")));
    }

    /// Whatever a connector lists it must be able to load. Two Codex fixtures
    /// broke this (file name and in-file id disagreed) and nothing noticed.
    #[test]
    fn test_every_scanned_session_can_be_loaded_by_its_id() {
        let registry = all_for_testing(&fixture_root());
        for c in registry.all() {
            for raw in c.scan().unwrap().filter_map(|r| r.ok()) {
                if !raw.body_available {
                    continue;
                }
                let s = c
                    .load(&raw.id)
                    .unwrap_or_else(|e| panic!("{} listed {} but cannot load it: {e}", c.id(), raw.id));
                assert_eq!(s.id, raw.id, "{}: loaded a different session than listed", c.id());
            }
        }
    }

    /// Index and load while a tool is appending to the transcript, including
    /// writes that stop mid-line. No panic, no half-parsed turn, and the final
    /// state is complete.
    #[test]
    fn test_scan_and_load_while_a_writer_appends_to_the_transcript() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let sid = "5b0e0000-0000-4000-8000-0000000000aa";
        let path = tmp.path().join(format!("{sid}.jsonl"));
        std::fs::write(&path, format!("{}\n", claude_rec(0, "2026-07-01T12:00:00.000Z", 50))).unwrap();

        let total = 400usize;
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new().append(true).open(&writer_path).unwrap();
            for i in 1..total {
                let line = format!("{}\n", claude_rec(i, "2026-07-01T12:00:01.000Z", 400));
                // Split every third write so readers see a torn final line.
                if i % 3 == 0 {
                    let (a, b) = line.split_at(line.len() / 2);
                    f.write_all(a.as_bytes()).unwrap();
                    f.flush().unwrap();
                    std::thread::yield_now();
                    f.write_all(b.as_bytes()).unwrap();
                } else {
                    f.write_all(line.as_bytes()).unwrap();
                }
                f.flush().unwrap();
            }
        });

        let mut last_seen = 0usize;
        let c = crate::connectors::claude_code::TestClaudeCode::new(tmp.path().to_path_buf());
        while !writer.is_finished() {
            let raw = c.scan().unwrap().filter_map(|r| r.ok()).count();
            assert_eq!(raw, 1, "the session must stay listed while it is written");
            let s = c.load(sid).expect("load must tolerate a torn final line");
            for m in &s.messages {
                assert!(m.text.as_deref().is_some_and(|t| t.starts_with('x')), "half-parsed turn: {:?}", m.text);
            }
            assert!(s.messages.len() >= last_seen, "turns disappeared: {} -> {}", last_seen, s.messages.len());
            last_seen = s.messages.len();
        }
        writer.join().unwrap();
        assert_eq!(c.load(sid).unwrap().messages.len(), total, "final state must be complete");
    }

    // ---- tests against real data / real scale (run with --ignored) ----

    /// The decoding bugs this project has had all passed "it is not empty" while
    /// real sessions lost their tool calls. This asserts properties of the
    /// *content* over whatever real Claude Code and Codex sessions the machine
    /// has. Read-only; prints only counts, never transcript text.
    #[test]
    #[ignore = "reads the operator's real Claude Code / Codex sessions"]
    fn test_real_sessions_decode_tool_calls_and_do_not_invent_empty_turns() {
        for id in ["claude-code", "codex-cli"] {
            let registry = all();
            let c = registry.by_id(id).unwrap();
            if !c.detect() {
                eprintln!("skipping {id}: not present on this machine");
                continue;
            }
            let (mut sessions, mut turns, mut calls, mut results, mut empty, mut failed) =
                (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
            for raw in c.scan().unwrap().filter_map(|r| r.ok()).filter(|r| r.body_available).take(300) {
                // Everything listed must load, or sync silently skips it.
                let Ok(s) = c.load(&raw.id) else {
                    failed += 1;
                    continue;
                };
                sessions += 1;
                for m in &s.messages {
                    turns += 1;
                    if m.tool_name.is_some() && m.role == Role::Assistant {
                        calls += 1;
                    }
                    if m.role == Role::Tool {
                        results += 1;
                    }
                    let blank = m.text.as_deref().is_none_or(|t| t.trim().is_empty());
                    if blank && m.tool_name.is_none() && m.tool_result.is_none() {
                        empty += 1;
                    }
                }
            }
            eprintln!(
                "{id}: {sessions} sessions, {turns} turns, {calls} tool calls, {results} tool results, \
                 {empty} empty turns, {failed} failed to load"
            );
            assert_eq!(failed, 0, "{id}: sessions that scan lists but load cannot read");
            if sessions == 0 {
                continue;
            }
            assert!(
                calls > 0,
                "{id}: {sessions} real sessions and not one tool call decoded; the reader is dropping them"
            );
            assert!(
                results > 0,
                "{id}: tool calls but no tool results; results are being misread"
            );
            // A transcript is not made of blank turns. A few are real (an
            // interrupted prompt); a large share means a record type is being
            // read as a message.
            assert!(
                empty * 20 <= turns.max(1),
                "{id}: {empty} of {turns} turns are empty (>5%): some record type is read as a message"
            );
        }
    }

    /// SPEC §7: a 100 MB session. Generated here (the committed fixture named
    /// "100mb" is 2.7 MB). Run: cargo test --release -- --ignored test_perf
    #[test]
    #[ignore = "performance: writes ~100 MB; run explicitly with --release"]
    fn test_perf_a_100mb_session_scans_fast_and_loads_with_bounded_work() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let sid = "5b0e0000-0000-4000-8000-0000000000bb";
        let path = tmp.path().join(format!("{sid}.jsonl"));
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut i = 0usize;
        while f.get_ref().metadata().map(|m| m.len()).unwrap_or(0) < 100 * 1024 * 1024 {
            let ts = format!("2026-07-01T{:02}:{:02}:{:02}.000Z", (i / 3600) % 24, (i / 60) % 60, i % 60);
            writeln!(f, "{}", claude_rec(i, &ts, 4000)).unwrap();
            i += 1;
            if i.is_multiple_of(2000) {
                f.flush().unwrap();
            }
        }
        f.flush().unwrap();
        drop(f);
        let size = std::fs::metadata(&path).unwrap().len();

        let c = crate::connectors::claude_code::TestClaudeCode::new(tmp.path().to_path_buf());
        let t = std::time::Instant::now();
        let raw = c.scan().unwrap().filter_map(|r| r.ok()).next().unwrap();
        let scan = t.elapsed();
        assert!(raw.last_event_at > raw.started_at, "scan must find the end of the file");
        assert!(
            scan < std::time::Duration::from_millis(500),
            "scan of a {} MB session took {scan:?}; it must not read the body",
            size / (1024 * 1024)
        );

        let t = std::time::Instant::now();
        let s = c.load(sid).unwrap();
        let load = t.elapsed();
        eprintln!(
            "{} MB, {} turns: scan {scan:?}, full load {load:?}",
            size / (1024 * 1024),
            s.messages.len()
        );
        assert_eq!(s.messages.len(), i, "every record must load");
        assert!(load < std::time::Duration::from_secs(60), "full load took {load:?}");
    }
}
