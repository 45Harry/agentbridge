//! Write back (spec 0001, AC-17 to AC-24) in a fake `HOME`: a session
//! continued in another tool gets the new turns added to its end, in its own
//! tool's format, exactly once, and `unsync` takes them out again.
//!
//! Every session here is synthetic, built in each tool's real on-disk shape.

#![cfg(unix)]

use agentbridge::writeback;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

const CLAUDE_ID: &str = "c1a0de00-0000-4000-8000-000000000001";
const CODEX_ID: &str = "019fc209-0000-7000-8000-000000000002";
const OC_ID: &str = "ses_0123456789abABCDEFGHIJKLMN";
const AGY_ID: &str = "a9000000-0000-4000-8000-000000000004";

struct Fake {
    tmp: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

fn line(v: Value) -> String {
    v.to_string() + "\n"
}

impl Fake {
    fn new() -> Self {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        unsafe {
            std::env::set_var("HOME", h);
            std::env::set_var("AGENTBRIDGE_DATA_DIR", h.join(".agentbridge"));
            std::env::set_var("CLAUDE_CONFIG_DIR", h.join(".claude"));
            std::env::set_var("CODEX_HOME", h.join(".codex"));
            std::env::set_var("ANTIGRAVITY_HOME", h.join(".gemini/antigravity-cli"));
            std::env::set_var("XDG_DATA_HOME", h.join(".local/share"));
            std::env::set_var("XDG_CONFIG_HOME", h.join(".config"));
        }
        fs::create_dir_all(h.join(".claude/projects")).unwrap();
        fs::create_dir_all(h.join(".codex/sessions")).unwrap();
        Fake { tmp, _guard: guard }
    }

    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn claude_path(&self) -> PathBuf {
        self.home().join(".claude/projects/-work-web").join(format!("{CLAUDE_ID}.jsonl"))
    }

    fn codex_path(&self) -> PathBuf {
        self.home().join(".codex/sessions/2026/10/02").join(format!("rollout-2026-10-02T08-00-00-{CODEX_ID}.jsonl"))
    }

    fn oc_db(&self) -> PathBuf {
        self.home().join(".local/share/opencode/opencode.db")
    }

    /// The origin: a Claude Code session of two turns in `/work/web`.
    fn claude_origin(&self) -> String {
        let rec = |i: u32, kind: &str, content: Value| {
            line(json!({"type": kind, "cwd": "/work/web", "sessionId": CLAUDE_ID, "uuid": format!("cc-{i}"),
                "parentUuid": if i == 0 { Value::Null } else { json!(format!("cc-{}", i - 1)) },
                "timestamp": format!("2026-10-01T09:00:0{i}.000Z"), "version": "2.1.300", "message": {"role": kind, "content": content}}))
        };
        let body = rec(0, "user", json!("make checkout faster")) + &rec(1, "assistant", json!([{"type":"text","text":"cached the cart"}]));
        fs::create_dir_all(self.claude_path().parent().unwrap()).unwrap();
        fs::write(self.claude_path(), &body).unwrap();
        body
    }

    /// A Codex session in `/work/app` that read `origin` and then had `turns`
    /// more turns.
    fn codex_continuation(&self, origin: &str, turns: &[&str]) -> String {
        let r = |ts: &str, payload: Value| line(json!({"timestamp": ts, "type": "response_item", "payload": payload}));
        let mut body = line(json!({"timestamp":"2026-10-02T08:00:00.000Z","type":"session_meta",
            "payload":{"id":CODEX_ID,"session_id":CODEX_ID,"timestamp":"2026-10-02T08:00:00.000Z","cwd":"/work/app","source":"cli"}}));
        body += &r("2026-10-02T08:00:01.000Z", json!({"type":"message","role":"user","content":[{"type":"input_text","text":"continue my checkout work"}]}));
        body += &r("2026-10-02T08:00:02.000Z", json!({"type":"function_call","name":"mcp__agentbridge__read_session",
            "arguments": json!({"id": origin}).to_string(), "call_id":"call_r"}));
        body += &r("2026-10-02T08:00:03.000Z", json!({"type":"function_call_output","call_id":"call_r","output":"{\"messages\":[\"... the whole origin ...\"]}"}));
        for (i, t) in turns.iter().enumerate() {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let kind = if role == "user" { "input_text" } else { "output_text" };
            body += &r(&format!("2026-10-02T08:01:{:02}.000Z", i), json!({"type":"message","role":role,"content":[{"type":kind,"text":t}]}));
        }
        fs::create_dir_all(self.codex_path().parent().unwrap()).unwrap();
        fs::write(self.codex_path(), &body).unwrap();
        body
    }

    fn append_codex_turn(&self, text: &str) {
        let mut body = fs::read_to_string(self.codex_path()).unwrap();
        body += &line(json!({"timestamp":"2026-10-02T09:00:00.000Z","type":"response_item",
            "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}));
        fs::write(self.codex_path(), body).unwrap();
    }

    /// An OpenCode database with one session that read `origin` (a Codex
    /// id), then had two turns of its own.
    fn opencode_continuation(&self, origin: &str) {
        fs::create_dir_all(self.oc_db().parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(self.oc_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute("INSERT INTO session VALUES (?1,'p','','/work/api','Api work',1791300000000,1791300009000)", [OC_ID]).unwrap();
        let msgs = [
            ("msg_f001", "user", json!({"type":"text","text":"pick up the codex session"})),
            ("msg_f002", "assistant", json!({"type":"tool","tool":"agentbridge_read_session","state":{"status":"completed","input":{"id":origin},"output":"..."}})),
            ("msg_f003", "user", json!({"type":"text","text":"add rate limits"})),
            ("msg_f004", "assistant", json!({"type":"text","text":"rate limits added"})),
        ];
        for (i, (id, role, part)) in msgs.iter().enumerate() {
            let ts = 1791300000000i64 + i as i64 * 1000;
            conn.execute("INSERT INTO message VALUES (?1,?2,?3,?3,?4)", rusqlite::params![id, OC_ID, ts, json!({"role": role}).to_string()]).unwrap();
            conn.execute("INSERT INTO part VALUES (?1,?2,?3,?4,?4,?5)", rusqlite::params![format!("prt_{i}"), id, OC_ID, ts, part.to_string()]).unwrap();
        }
    }

    fn run(&self, open: &[&str]) -> writeback::WritebackReport {
        let open: Vec<String> = open.iter().map(|s| s.to_string()).collect();
        writeback::run(&agentbridge::connectors::all(), false, &move |t: &str| open.iter().any(|o| o == t))
    }

    fn records(&self, path: &Path) -> Vec<Value> {
        fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

/// The text of each record agentbridge appended to a Claude Code file.
fn written_texts(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.get("agentbridge").is_some())
        .map(|r| match &r["message"]["content"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a[0]["text"].as_str().unwrap_or_default().to_string(),
            _ => String::new(),
        })
        .collect()
}

#[test]
fn continued_turns_land_at_the_end_of_the_origin_once() {
    let f = Fake::new();
    let original = f.claude_origin();
    f.codex_continuation(CLAUDE_ID, &["now add tests", "added three tests", "ship it"]);

    let dry = writeback::run(&agentbridge::connectors::all(), true, &|_: &str| false);
    assert_eq!(dry.written.len(), 1, "{dry:?}");
    assert_eq!(dry.written[0].4, 4, "note + 3 turns");
    assert_eq!(fs::read_to_string(f.claude_path()).unwrap(), original, "dry run writes nothing");

    let r = f.run(&[]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let after = fs::read_to_string(f.claude_path()).unwrap();
    assert!(after.starts_with(&original), "nothing before the end changed");
    let recs = f.records(&f.claude_path());
    assert_eq!(
        written_texts(&recs),
        ["continued in Codex on 2026-10-02", "now add tests", "added three tests", "ship it"],
        "the note opens the batch, the read itself and its output are left out"
    );
    // A linked list onto the origin's last record, in the origin's session.
    let first = &recs[2];
    assert_eq!(first["parentUuid"], "cc-1");
    assert_eq!(first["sessionId"], CLAUDE_ID);
    assert_eq!(first["cwd"], "/work/web");
    assert_eq!(first["version"], "2.1.300");
    assert_eq!(recs[3]["parentUuid"], recs[2]["uuid"]);
    assert_eq!(first["agentbridge"]["writeback_of"], format!("codex-cli:{CODEX_ID}"));
    assert!(after.ends_with('\n'));
    // The origin was backed up first, under a name no tool lists as a
    // session (regression: a `.jsonl` backup showed up as a second session).
    let backups: Vec<String> = fs::read_dir(f.claude_path().parent().unwrap()).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("agentbridge-backup"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert!(!backups[0].ends_with(".jsonl"), "{}", backups[0]);
    let listed = agentbridge::mcp::list_sessions(&agentbridge::connectors::all(), json!({"tool": "claude-code"}).as_object().unwrap(), Path::new("/"))
        .unwrap();
    assert_eq!(listed["total"], 1, "the backup is not a session: {listed}");

    // Run again: nothing new, nothing written.
    let again = f.run(&[]);
    assert!(again.written.is_empty(), "{:?}", again.written);
    assert_eq!(fs::read_to_string(f.claude_path()).unwrap(), after);

    // One more turn in Codex: only that turn goes, under its own note.
    f.append_codex_turn("also update the changelog");
    let third = f.run(&[]);
    assert_eq!(third.written.len(), 1);
    assert_eq!(third.written[0].4, 2);
    let texts = written_texts(&f.records(&f.claude_path()));
    assert_eq!(texts.len(), 6);
    assert_eq!(texts[4..], ["continued in Codex on 2026-10-02", "also update the changelog"]);

    // The origin, read back through its own reader, now holds the work.
    let read = agentbridge::mcp::read_session(
        &agentbridge::connectors::all(),
        json!({"id": CLAUDE_ID}).as_object().unwrap(),
    )
    .unwrap();
    assert_eq!(read["total_messages"], 8);
}

/// Regression (live run with codex 0.146, 2026-10-30): Codex calls MCP tools
/// from its `exec` code tool and logs the call as `mcp_tool_call_end`. Such a
/// continuation was never detected, and the code tool's output (the whole
/// origin) would have counted as a new turn.
#[test]
fn a_codex_continuation_made_through_its_code_tool_is_detected() {
    let f = Fake::new();
    let original = f.claude_origin();
    let r = |ts: &str, t: &str, payload: Value| line(json!({"timestamp": ts, "type": t, "payload": payload}));
    let mut body = line(json!({"timestamp":"2026-10-02T08:00:00.000Z","type":"session_meta",
        "payload":{"id":CODEX_ID,"timestamp":"2026-10-02T08:00:00.000Z","cwd":"/work/app","source":"exec"}}));
    body += &r("2026-10-02T08:00:01.000Z", "response_item", json!({"type":"message","role":"user","content":[{"type":"input_text","text":"find it"}]}));
    body += &r("2026-10-02T08:00:02.000Z", "response_item", json!({"type":"custom_tool_call","status":"completed","call_id":"c1","name":"exec",
        "input": format!("const r = await tools.mcp__agentbridge__read_session({{id:\"{CLAUDE_ID}\",tool:\"claude-code\"}}); text(r);")}));
    body += &r("2026-10-02T08:00:03.000Z", "event_msg", json!({"type":"mcp_tool_call_end","call_id":"exec-1",
        "invocation":{"server":"agentbridge","tool":"read_session","arguments":{"id":CLAUDE_ID,"tool":"claude-code"}},
        "result":{"Ok":{"content":[{"type":"text","text":"{\"messages\":\"the whole origin\"}"}]}}}));
    body += &r("2026-10-02T08:00:04.000Z", "response_item", json!({"type":"custom_tool_call_output","call_id":"c1","output":"the whole origin"}));
    body += &r("2026-10-02T08:00:05.000Z", "response_item", json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"found it; follow-up CODEX-ON-AZURE"}]}));
    fs::create_dir_all(f.codex_path().parent().unwrap()).unwrap();
    fs::write(f.codex_path(), body).unwrap();

    let report = f.run(&[]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.written.len(), 1, "{report:?}");
    let after = fs::read_to_string(f.claude_path()).unwrap();
    assert!(after.starts_with(&original));
    assert_eq!(
        written_texts(&f.records(&f.claude_path())),
        ["continued in Codex on 2026-10-02", "found it; follow-up CODEX-ON-AZURE"],
        "the code tool's call and output are not written back"
    );
}

#[test]
fn turns_land_after_the_origins_own_new_turns_under_the_note() {
    let f = Fake::new();
    f.claude_origin();
    f.codex_continuation(CLAUDE_ID, &["codex turn"]);
    // The origin moved on in Claude Code too.
    let mut body = fs::read_to_string(f.claude_path()).unwrap();
    body += &line(json!({"type":"user","cwd":"/work/web","sessionId":CLAUDE_ID,"uuid":"cc-2","parentUuid":"cc-1",
        "timestamp":"2026-10-03T10:00:00.000Z","message":{"role":"user","content":"claude kept going"}}));
    fs::write(f.claude_path(), &body).unwrap();

    f.run(&[]);
    let recs = f.records(&f.claude_path());
    assert_eq!(recs[2]["message"]["content"], "claude kept going");
    assert_eq!(written_texts(&recs), ["continued in Codex on 2026-10-02", "codex turn"]);
    assert_eq!(recs[3]["parentUuid"], "cc-2");
}

#[test]
fn an_open_origin_tool_waits_for_the_next_run() {
    let f = Fake::new();
    let original = f.claude_origin();
    f.codex_continuation(CLAUDE_ID, &["while claude was open"]);
    let r = f.run(&["claude-code"]);
    assert!(r.written.is_empty());
    assert!(r.waiting.iter().any(|w| w.contains("Claude Code is open")), "{:?}", r.waiting);
    assert_eq!(fs::read_to_string(f.claude_path()).unwrap(), original);
    let later = f.run(&[]);
    assert_eq!(later.written.len(), 1);
}

#[test]
fn a_chain_writes_each_continuations_turns_all_the_way_back_once() {
    let f = Fake::new();
    f.claude_origin();
    // Codex continued Claude; OpenCode then continued Codex.
    f.codex_continuation(CLAUDE_ID, &["codex own work"]);
    f.opencode_continuation(CODEX_ID);

    let r = f.run(&[]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let into: Vec<(String, String)> = r.written.iter().map(|w| (w.0.clone(), w.2.clone())).collect();
    assert!(into.contains(&("codex-cli".into(), "opencode".into())), "{into:?}");
    assert!(into.contains(&("claude-code".into(), "opencode".into())), "{into:?}");
    assert!(into.contains(&("claude-code".into(), "codex-cli".into())), "{into:?}");
    assert_eq!(into.len(), 3);

    // Codex got OpenCode's turns in both of its views.
    let codex = f.records(&f.codex_path());
    let ours: Vec<&Value> = codex.iter().filter(|r| r.get("agentbridge").is_some()).collect();
    assert!(ours.iter().any(|r| r["type"] == "event_msg" && r["payload"]["message"] == "add rate limits"), "screen view");
    assert!(ours.iter().any(|r| r["type"] == "response_item" && r["payload"]["content"][0]["text"] == "rate limits added"), "model view");

    // A second run: the OpenCode turns now sitting in Codex are not taken for
    // Codex's own new work, so Claude does not get them twice.
    let again = f.run(&[]);
    assert!(again.written.is_empty(), "{:?}", again.written);
    let claude = written_texts(&f.records(&f.claude_path()));
    assert_eq!(claude.iter().filter(|t| *t == "add rate limits").count(), 1, "{claude:?}");
    assert_eq!(claude.iter().filter(|t| *t == "codex own work").count(), 1, "{claude:?}");
}

#[test]
fn opencode_as_origin_gets_rows_that_sort_after_its_own() {
    let f = Fake::new();
    f.opencode_continuation("nothing-to-read");
    // Codex continued the OpenCode session.
    let mut body = line(json!({"timestamp":"2026-10-06T08:00:00.000Z","type":"session_meta",
        "payload":{"id":CODEX_ID,"timestamp":"2026-10-06T08:00:00.000Z","cwd":"/work/app","source":"cli"}}));
    body += &line(json!({"timestamp":"2026-10-06T08:00:01.000Z","type":"response_item","payload":{"type":"function_call",
        "name":"mcp__agentbridge__read_session","arguments": json!({"id": OC_ID, "tool": "opencode"}).to_string(),"call_id":"c"}}));
    body += &line(json!({"timestamp":"2026-10-06T08:00:02.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"..."}}));
    body += &line(json!({"timestamp":"2026-10-06T08:00:03.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"from codex"}]}}));
    fs::create_dir_all(f.codex_path().parent().unwrap()).unwrap();
    fs::write(f.codex_path(), body).unwrap();

    let r = f.run(&[]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let conn = rusqlite::Connection::open(f.oc_db()).unwrap();
    let ids: Vec<String> = conn.prepare("SELECT id FROM message WHERE session_id = ?1 ORDER BY id").unwrap()
        .query_map([OC_ID], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(ids.len(), 6);
    assert!(ids[4].starts_with("msg_f004_ab"), "new ids extend the last one: {ids:?}");
    assert!(ids[5] > ids[4]);
    assert!(ids[5].starts_with("msg_f004_ab0001"), "each id extends the original last id, not the previous new one: {ids:?}");
    assert!(ids[5].as_str() < "msg_f005", "and still sort before OpenCode's next own id");
    let read = agentbridge::mcp::read_session(&agentbridge::connectors::all(), json!({"id": OC_ID}).as_object().unwrap()).unwrap();
    let texts: Vec<&str> = read["messages"].as_array().unwrap().iter().filter_map(|m| m["text"].as_str()).collect();
    assert_eq!(texts[texts.len() - 2..], ["continued in Codex on 2026-10-06", "from codex"]);

    // unsync deletes exactly those rows.
    agentbridge::sync::unsync_with(false, &|_: &str| false);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM message WHERE session_id = ?1", [OC_ID], |r| r.get(0)).unwrap();
    assert_eq!(n, 4);
    let parts: i64 = conn.query_row("SELECT COUNT(*) FROM part", [], |r| r.get(0)).unwrap();
    assert_eq!(parts, 4);
}

#[test]
fn antigravity_as_origin_gets_steps_appended() {
    let f = Fake::new();
    let home = f.home().join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&home).unwrap();
    let db = home.join(format!("{AGY_ID}.db"));
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0,
        has_subtrajectory numeric NOT NULL DEFAULT false, metadata blob, error_details blob, permissions blob, task_details blob,
        render_info blob, step_payload blob, step_format integer NOT NULL DEFAULT 0, PRIMARY KEY (idx));
        CREATE TABLE trajectory_metadata_blob (id text DEFAULT 'main', data blob, PRIMARY KEY (id));").unwrap();
    // One user step: field 19.2 holds the text, 5.1 the time.
    let user_step = hex_step();
    conn.execute("INSERT INTO steps (idx, step_type, status, step_payload) VALUES (0, 14, 3, ?1)", [user_step]).unwrap();
    drop(conn);
    let before = agentbridge::mcp::read_session(&agentbridge::connectors::all(), json!({"id": AGY_ID}).as_object().unwrap()).unwrap();
    assert_eq!(before["total_messages"], 1, "{before}");

    f.codex_continuation(AGY_ID, &["continued from agy"]);
    let r = f.run(&[]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let after = agentbridge::mcp::read_session(&agentbridge::connectors::all(), json!({"id": AGY_ID}).as_object().unwrap()).unwrap();
    let texts: Vec<&str> = after["messages"].as_array().unwrap().iter().filter_map(|m| m["text"].as_str()).collect();
    assert_eq!(texts, ["hello agy", "continued in Codex on 2026-10-02", "continued from agy"]);

    agentbridge::sync::unsync_with(false, &|_: &str| false);
    let back = agentbridge::mcp::read_session(&agentbridge::connectors::all(), json!({"id": AGY_ID}).as_object().unwrap()).unwrap();
    assert_eq!(back["total_messages"], 1);
}

/// A minimal Antigravity user step saying "hello agy" at 2026-10-01.
fn hex_step() -> Vec<u8> {
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    fn field(n: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(n << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    let mut created = Vec::new();
    varint(1 << 3, &mut created);
    varint(1790000000, &mut created);
    let mut meta = Vec::new();
    field(1, &created, &mut meta);
    let mut body = Vec::new();
    field(2, b"hello agy", &mut body);
    let mut out = Vec::new();
    varint(1 << 3, &mut out);
    varint(14, &mut out);
    field(5, &meta, &mut out);
    field(19, &body, &mut out);
    out
}

#[test]
fn unsync_puts_jsonl_origins_back_byte_for_byte() {
    let f = Fake::new();
    // An origin whose last line has no newline: write back adds one first.
    let original = f.claude_origin();
    let trimmed = original.trim_end_matches('\n').to_string();
    fs::write(f.claude_path(), &trimmed).unwrap();
    // And a Codex origin, continued by OpenCode.
    let codex_original = f.codex_continuation(CLAUDE_ID, &["codex turn"]);
    f.opencode_continuation(CODEX_ID);

    f.run(&[]);
    let grown = fs::read_to_string(f.claude_path()).unwrap();
    assert!(grown.starts_with(&format!("{trimmed}\n")), "a newline before the first new record");
    assert!(fs::read_to_string(f.codex_path()).unwrap().len() > codex_original.len());

    let report = agentbridge::sync::unsync_with(false, &|_: &str| false);
    assert!(report.kept_foreign.is_empty(), "{:?}", report.kept_foreign);
    assert_eq!(fs::read_to_string(f.claude_path()).unwrap(), trimmed, "byte identical, newline taken off again");
    assert_eq!(fs::read_to_string(f.codex_path()).unwrap(), codex_original);
    assert!(!fs::read_to_string(f.home().join(".agentbridge/manifest.jsonl")).unwrap_or_default().contains("writeback"));
}

#[test]
fn unsync_keeps_turns_the_tool_added_after_the_write_back() {
    let f = Fake::new();
    let original = f.claude_origin();
    f.codex_continuation(CLAUDE_ID, &["codex turn"]);
    f.run(&[]);
    // Claude Code was opened and the session continued after the write back.
    let mut body = fs::read_to_string(f.claude_path()).unwrap();
    let own = line(json!({"type":"user","cwd":"/work/web","sessionId":CLAUDE_ID,"uuid":"cc-9","timestamp":"2026-10-04T10:00:00.000Z",
        "message":{"role":"user","content":"typed in claude later"}}));
    body += &own;
    fs::write(f.claude_path(), &body).unwrap();

    agentbridge::sync::unsync_with(false, &|_: &str| false);
    assert_eq!(fs::read_to_string(f.claude_path()).unwrap(), original + &own);
}

#[test]
fn a_server_start_with_nothing_to_write_touches_nothing() {
    let f = Fake::new();
    f.claude_origin();
    let r = f.run(&[]);
    assert!(r.written.is_empty() && r.errors.is_empty());
    assert!(!f.home().join(".agentbridge").exists(), "not even the data dir or the lock");
}

/// Manual live check only: runs write back on the sandbox named by
/// `AGENTBRIDGE_LIVE_SANDBOX` with every tool treated as closed, so a Claude
/// Code origin can be written while Claude Code itself is running the check.
#[test]
#[ignore = "manual live check against a sandbox"]
fn live_write_back_into_sandbox() {
    let Ok(home) = std::env::var("AGENTBRIDGE_LIVE_SANDBOX") else { return };
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let h = Path::new(&home);
    unsafe {
        std::env::set_var("HOME", h);
        std::env::set_var("AGENTBRIDGE_DATA_DIR", h.join(".agentbridge"));
        std::env::set_var("CLAUDE_CONFIG_DIR", h.join(".claude"));
        std::env::set_var("CODEX_HOME", h.join(".codex"));
        std::env::remove_var("ANTIGRAVITY_HOME");
        std::env::set_var("XDG_DATA_HOME", h.join(".local/share"));
        std::env::set_var("XDG_CONFIG_HOME", h.join(".config"));
    }
    let r = writeback::run(&agentbridge::connectors::all(), false, &|_: &str| false);
    eprintln!("LIVE written={:?} waiting={:?} errors={:?}", r.written, r.waiting, r.errors);
}
