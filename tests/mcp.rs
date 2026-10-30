//! `agentbridge mcp` end to end (spec 0001): spawn the binary in a fake
//! `HOME`, talk MCP to it over stdio, and check what the AI would see.
//!
//! Every session here is synthetic and built by the test in each tool's real
//! on-disk shape.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const CLAUDE_ID: &str = "8f1d2c3b-4a5e-4f60-9b7a-0c1d2e3f4a5b";
const CODEX_ID: &str = "019fc209-1111-7222-8333-944455566677";
/// Messages in the Claude fixture: page 1 is 57..=96, page 3 is 0..=16.
const CLAUDE_MESSAGES: usize = 97;

struct Fake {
    tmp: tempfile::TempDir,
}

impl Fake {
    fn home(&self) -> &Path {
        self.tmp.path()
    }
    /// The folder the Claude session started in.
    fn web(&self) -> PathBuf {
        self.home().join("work/web")
    }
    /// The folder the Codex session started in, and where the server runs.
    fn app(&self) -> PathBuf {
        self.home().join("work/app")
    }
    fn claude_file(&self) -> PathBuf {
        self.home().join(".claude/projects/-work-web").join(format!("{}.jsonl", CLAUDE_ID))
    }
}

fn line(v: Value) -> String {
    serde_json::to_string(&v).unwrap() + "\n"
}

/// A Claude Code session of `CLAUDE_MESSAGES` records in its real shape:
/// alternating user and assistant turns, with one tool call and its large
/// output in the middle.
fn write_claude(f: &Fake) {
    let mut body = String::new();
    let cwd = f.web().to_string_lossy().to_string();
    for i in 0..CLAUDE_MESSAGES {
        let ts = format!("2026-10-01T09:{:02}:{:02}.000Z", i / 60, i % 60);
        let base = json!({ "cwd": cwd, "sessionId": CLAUDE_ID, "uuid": format!("u{}", i), "timestamp": ts });
        let mut rec = base.as_object().unwrap().clone();
        match i {
            0 => {
                rec.insert("type".into(), json!("user"));
                rec.insert("message".into(), json!({"role":"user","content":"Make the checkout page load faster"}));
            }
            50 => {
                rec.insert("type".into(), json!("assistant"));
                rec.insert("message".into(), json!({"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"npm run bench"}}]}));
            }
            51 => {
                rec.insert("type".into(), json!("user"));
                rec.insert("message".into(), json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content": big_output()}]}));
            }
            _ if i % 2 == 0 => {
                rec.insert("type".into(), json!("user"));
                rec.insert("message".into(), json!({"role":"user","content": format!("user turn {}", i)}));
            }
            _ => {
                rec.insert("type".into(), json!("assistant"));
                rec.insert("message".into(), json!({"role":"assistant","content":[{"type":"text","text": format!("assistant turn {}", i)}]}));
            }
        }
        body.push_str(&line(Value::Object(rec)));
    }
    std::fs::create_dir_all(f.claude_file().parent().unwrap()).unwrap();
    std::fs::write(f.claude_file(), body).unwrap();
}

/// Tool output long enough that any cutting would show.
fn big_output() -> String {
    (0..2000).map(|i| format!("bench line {}\n", i)).collect()
}

/// A Codex rollout in its real shape: `session_meta`, then both views.
fn write_codex(f: &Fake) {
    let cwd = f.app().to_string_lossy().to_string();
    let dir = f.home().join(".codex/sessions/2026/10/02");
    std::fs::create_dir_all(&dir).unwrap();
    let mut body = line(json!({"timestamp":"2026-10-02T08:00:00.000Z","type":"session_meta",
        "payload":{"id":CODEX_ID,"session_id":CODEX_ID,"timestamp":"2026-10-02T08:00:00.000Z","cwd":cwd,"source":"cli"}}));
    body += &line(json!({"timestamp":"2026-10-02T08:00:01.000Z","type":"response_item",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for the repo"}]}}));
    body += &line(json!({"timestamp":"2026-10-02T08:00:02.000Z","type":"event_msg",
        "payload":{"type":"user_message","message":"Fix the login bug on Safari"}}));
    body += &line(json!({"timestamp":"2026-10-02T08:00:02.000Z","type":"response_item",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the login bug on Safari"}]}}));
    body += &line(json!({"timestamp":"2026-10-02T08:05:00.000Z","type":"response_item",
        "payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_9"}}));
    body += &line(json!({"timestamp":"2026-10-02T08:05:30.000Z","type":"response_item",
        "payload":{"type":"function_call_output","call_id":"call_9","output":"test result: ok"}}));
    std::fs::write(dir.join(format!("rollout-2026-10-02T08-00-00-{}.jsonl", CODEX_ID)), body).unwrap();
}

/// An OpenCode database holding one native session and two agentbridge
/// copies: one known only from the manifest row that names its row id, one
/// that OpenCode retitled and that only its metadata marker gives away.
fn write_opencode(f: &Fake) -> PathBuf {
    let db = f.home().join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
            directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER, metadata TEXT);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        INSERT INTO session VALUES ('ses_native','p','', '/srv/api', 'Native work', 1791200000000, 1791200100000, NULL);
        INSERT INTO session VALUES ('ses_copy','p','', '/srv/api', 'Copied work', 1791200000000, 1791200200000, NULL);
        INSERT INTO session VALUES ('ses_renamed','p','', '/srv/api', 'Greeting And Conversation Start', 1791200000000, 1791200300000,
            '{"agentbridge":true,"source_provider":"claude-code","source_id":"x"}');
        INSERT INTO message VALUES ('msg_1','ses_native',1790000000000,1790000000000,'{"role":"user"}');
        INSERT INTO part VALUES ('prt_1','msg_1','ses_native',0,0,'{"type":"text","text":"rate limit the api"}');
        "#,
    )
    .unwrap();
    db
}

fn fake() -> Fake {
    let f = Fake { tmp: tempfile::tempdir().unwrap() };
    std::fs::create_dir_all(f.web()).unwrap();
    std::fs::create_dir_all(f.app()).unwrap();
    write_claude(&f);
    write_codex(&f);
    let db = write_opencode(&f);
    let manifest = f.home().join(".agentbridge/manifest.jsonl");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest,
        line(json!({"dest": db, "cache": "ses_copy", "session_id": CODEX_ID, "source_provider": "codex-cli",
            "target_provider": "opencode", "project": "/srv/api", "inode": 0})),
    )
    .unwrap();
    f
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(f: &Fake) -> Self {
        let home = f.home();
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentbridge"))
            .arg("mcp")
            .current_dir(f.app())
            .env("HOME", home)
            .env("AGENTBRIDGE_DATA_DIR", home.join(".agentbridge"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("ANTIGRAVITY_HOME", home.join(".gemini/antigravity-cli"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("binary runs");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, stdin, stdout, next_id: 1 }
    }

    fn send_raw(&mut self, text: &str) -> Value {
        writeln!(self.stdin, "{}", text).unwrap();
        self.stdin.flush().unwrap();
        let mut reply = String::new();
        self.stdout.read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("stdout is protocol only, got {:?}: {}", reply, e))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let r = self.send_raw(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string());
        assert_eq!(r["id"], id);
        r
    }

    fn initialize(&mut self) {
        let r = self.request("initialize", json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"test","version":"0"}}));
        assert_eq!(r["result"]["serverInfo"]["name"], "agentbridge");
        writeln!(self.stdin, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
    }

    /// A tool call's structured result, asserting it is not a tool error.
    fn call(&mut self, name: &str, args: Value) -> Value {
        let r = self.call_raw(name, args);
        assert_eq!(r["isError"], false, "tool error: {}", r);
        let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, r["structuredContent"], "the text copy matches structuredContent");
        r["structuredContent"].clone()
    }

    fn call_raw(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))["result"].clone()
    }

    fn close(mut self) -> i32 {
        drop(self.stdin);
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

/// Every file under `root` with its bytes, to prove nothing changed.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

#[test]
fn lists_and_reads_sessions_across_tools_without_writing() {
    let f = fake();
    let before = snapshot(f.home());
    let mut mcp = Mcp::start(&f);
    mcp.initialize();

    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_sessions", "read_session"]);

    // The server runs in the Codex session's folder: that session comes
    // first, then the rest newest first (OpenCode, 2026-10-05, then Claude,
    // 2026-10-01). The OpenCode copy is left out.
    let list = mcp.call("list_sessions", json!({}));
    let ids: Vec<&str> = list["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [CODEX_ID, "ses_native", CLAUDE_ID], "{}", list);
    assert_eq!(list["total"], 3);
    assert_eq!(list["current_folder"], f.app().to_string_lossy().as_ref());
    assert_eq!(list["sessions"][0]["in_current_folder"], true);
    assert_eq!(list["sessions"][0]["first_prompt"], "Fix the login bug on Safari", "injected AGENTS.md text is not the prompt");
    assert_eq!(list["sessions"][2]["first_prompt"], "Make the checkout page load faster");
    assert_eq!(list["sessions"][2]["last_event_at"], "2026-10-01T09:01:36Z", "the last event, not the start");

    // Search and filters.
    let found = mcp.call("list_sessions", json!({"query": "CHECKOUT faster"}));
    assert_eq!(found["total"], 1);
    assert_eq!(found["sessions"][0]["id"], CLAUDE_ID);
    let found = mcp.call("list_sessions", json!({"query": "8f1d2c"}));
    assert_eq!(found["sessions"][0]["id"], CLAUDE_ID, "id prefix");
    let none = mcp.call("list_sessions", json!({"query": "kubernetes"}));
    assert_eq!(none["total"], 0);
    assert_eq!(none["sessions"], json!([]));
    let codex = mcp.call("list_sessions", json!({"tool": "codex-cli"}));
    assert_eq!(codex["total"], 1);
    let web = mcp.call("list_sessions", json!({"project": format!("{}/", f.web().display())}));
    assert_eq!(web["total"], 1, "trailing slash ignored");
    let recent = mcp.call("list_sessions", json!({"since": "2026-10-02"}));
    let recent_ids: Vec<&str> = recent["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(recent_ids, [CODEX_ID, "ses_native"], "{}", recent);
    let all = mcp.call("list_sessions", json!({"project": "all", "limit": 1}));
    assert_eq!(all["total"], 3);
    assert_eq!(all["current_folder"], Value::Null);
    assert_eq!(all["sessions"][0]["id"], "ses_native", "with \"all\" the current folder is not boosted");
    let second = mcp.call("list_sessions", json!({"project": "all", "limit": 1, "offset": 1}));
    assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(second["sessions"][0]["id"], CODEX_ID);

    // Page 1 is the newest 40, in time order inside the page.
    let page1 = mcp.call("read_session", json!({"id": CLAUDE_ID}));
    assert_eq!(page1["total_messages"], CLAUDE_MESSAGES);
    assert_eq!(page1["total_pages"], 3);
    let ords: Vec<u64> = page1["messages"].as_array().unwrap().iter().map(|m| m["ordinal"].as_u64().unwrap()).collect();
    assert_eq!(ords, (57..97).collect::<Vec<u64>>());
    let page2 = mcp.call("read_session", json!({"id": CLAUDE_ID, "page": 2}));
    assert_eq!(page2["messages"][0]["ordinal"], 17);
    assert_eq!(page2["messages"][39]["ordinal"], 56);
    let call = &page2["messages"][50 - 17];
    assert_eq!(call["role"], "assistant");
    assert_eq!(call["tool_name"], "Bash");
    assert_eq!(call["tool_input"], json!({"command": "npm run bench"}));
    let output = &page2["messages"][51 - 17];
    assert_eq!(output["role"], "tool");
    assert_eq!(output["tool_name"], "Bash");
    assert_eq!(output["tool_output"], big_output(), "full output, nothing cut");
    let page3 = mcp.call("read_session", json!({"id": CLAUDE_ID, "page": 3}));
    assert_eq!(page3["messages"].as_array().unwrap().len(), 17);
    let small = mcp.call("read_session", json!({"id": CLAUDE_ID, "page_size": 5}));
    assert_eq!(small["total_pages"], 20);
    assert_eq!(small["messages"][4]["text"], "user turn 96");

    // Codex tool calls come through with their parsed arguments.
    let cx = mcp.call("read_session", json!({"id": CODEX_ID}));
    let tool_msgs: Vec<&Value> = cx["messages"].as_array().unwrap().iter().filter(|m| !m["tool_name"].is_null()).collect();
    assert_eq!(tool_msgs.len(), 2, "{}", cx);
    assert_eq!(tool_msgs[0]["tool_input"], json!({"cmd": "cargo test"}));
    assert_eq!(tool_msgs[1]["role"], "tool");
    assert_eq!(tool_msgs[1]["tool_name"], "shell");
    assert_eq!(tool_msgs[1]["tool_output"], "test result: ok");

    // A bad call is a tool error, and the server keeps answering.
    let missing = mcp.call_raw("read_session", json!({"id": "no-such-session"}));
    assert_eq!(missing["isError"], true);
    assert!(missing["content"][0]["text"].as_str().unwrap().contains("list_sessions"), "{}", missing);
    let out_of_range = mcp.call_raw("read_session", json!({"id": CLAUDE_ID, "page": 4}));
    assert_eq!(out_of_range["isError"], true);
    let again = mcp.call("read_session", json!({"id": "ses_native", "tool": "opencode"}));
    assert_eq!(again["messages"][0]["text"], "rate limit the api");

    assert_eq!(mcp.close(), 0, "end of input exits 0");
    assert!(before == snapshot(f.home()), "list_sessions and read_session write nothing");
}

#[test]
fn recovered_turns_follow_the_session_and_are_marked() {
    let f = fake();
    let overlay = f.home().join(".agentbridge/overlay");
    std::fs::create_dir_all(&overlay).unwrap();
    let turn = json!({"session_id": CODEX_ID, "ordinal": 0, "role": "assistant", "timestamp": "2026-10-03T10:00:00Z",
        "text": "continued in OpenCode", "tool_name": null, "tool_input": null, "tool_result": null, "parent_ordinal": null});
    std::fs::write(overlay.join(format!("{}.jsonl", CODEX_ID)), line(turn)).unwrap();

    let mut mcp = Mcp::start(&f);
    mcp.initialize();
    let r = mcp.call("read_session", json!({"id": CODEX_ID}));
    assert_eq!(r["recovered_from_overlay"], 1);
    let msgs = r["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last["text"], "continued in OpenCode");
    assert_eq!(last["recovered"], true);
    assert_eq!(last["ordinal"], msgs[msgs.len() - 2]["ordinal"].as_u64().unwrap() + 1);
    assert!(msgs[..msgs.len() - 1].iter().all(|m| m["recovered"] == false));
    mcp.close();
}

#[test]
fn protocol_errors_keep_the_server_running() {
    let f = fake();
    let mut mcp = Mcp::start(&f);
    let r = mcp.send_raw("this is not json");
    assert_eq!(r["error"]["code"], -32700);
    assert_eq!(r["id"], Value::Null);
    let r = mcp.send_raw("[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]");
    assert_eq!(r["error"]["code"], -32600);
    let r = mcp.request("tools/call", json!({"name": "list_sessions"}));
    assert_eq!(r["error"]["code"], -32002, "a call before initialize");
    mcp.initialize();
    let r = mcp.call("list_sessions", json!({"limit": 1}));
    assert_eq!(r["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(mcp.close(), 0);
}
