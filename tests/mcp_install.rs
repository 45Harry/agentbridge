//! `mcp install` / `mcp uninstall` and the migration off copies (spec 0001,
//! AC-10 to AC-14), run against a fake `HOME` through the library so the
//! "is this tool open" check is supplied by the test, never read from the
//! machine running it.
//!
//! Every settings file and session here is synthetic.

#![cfg(unix)]

use agentbridge::mcp_install::{self, Action};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// HOME and the store variables are process wide.
static ENV: Mutex<()> = Mutex::new(());

const EXE: &str = "/opt/agentbridge/bin/agentbridge";

const CLAUDE_JSON: &str = "{\n  \"numStartups\": 12,\n  \"projects\": {\n    \"/work/web\": { \"allowedTools\": [] }\n  }\n}\n";
const CODEX_TOML: &str = "# my Codex settings\nmodel = \"gpt-5\" # pinned\n\n[profiles.fast]\nmodel = \"mini\"\n";
const OPENCODE_JSONC: &str = "{\n  // theme first\n  \"theme\": \"opencode\",\n  \"mcp\": {\n    \"other\": { \"type\": \"local\", \"command\": [\"other\"] }, // keep\n  },\n}\n";

struct Fake {
    tmp: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Fake {
    fn new() -> Self {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        unsafe {
            std::env::set_var("HOME", h);
            std::env::set_var("SHELL", "/bin/bash");
            std::env::set_var("AGENTBRIDGE_DATA_DIR", h.join(".agentbridge"));
            std::env::set_var("CLAUDE_CONFIG_DIR", h.join(".claude"));
            std::env::set_var("CODEX_HOME", h.join(".codex"));
            std::env::set_var("ANTIGRAVITY_HOME", h.join(".gemini/antigravity-cli"));
            std::env::set_var("XDG_DATA_HOME", h.join(".local/share"));
            std::env::set_var("XDG_CONFIG_HOME", h.join(".config"));
        }
        // Every tool's store exists, so every tool is detected.
        fs::create_dir_all(h.join(".claude/projects")).unwrap();
        fs::create_dir_all(h.join(".codex/sessions")).unwrap();
        fs::create_dir_all(h.join(".gemini/antigravity-cli/conversations")).unwrap();
        let db = h.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
            )
            .unwrap();

        // Settings as the tools left them.
        fs::write(h.join(".claude/.claude.json"), CLAUDE_JSON).unwrap();
        fs::write(h.join(".codex/config.toml"), CODEX_TOML).unwrap();
        fs::create_dir_all(h.join(".config/opencode")).unwrap();
        fs::write(h.join(".config/opencode/opencode.jsonc"), OPENCODE_JSONC).unwrap();
        // Antigravity reads `~/.gemini/config/mcp_config.json`, here a
        // symlink to an empty file elsewhere (as the IDE's file is on a
        // default install).
        fs::create_dir_all(h.join(".gemini/config")).unwrap();
        fs::create_dir_all(h.join(".gemini/shared")).unwrap();
        fs::write(h.join(".gemini/shared/mcp_config.json"), "").unwrap();
        std::os::unix::fs::symlink(
            h.join(".gemini/shared/mcp_config.json"),
            h.join(".gemini/config/mcp_config.json"),
        )
        .unwrap();
        Fake { tmp, _guard: guard }
    }

    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn settings(&self) -> [PathBuf; 4] {
        let h = self.home();
        [
            h.join(".claude/.claude.json"),
            h.join(".codex/config.toml"),
            h.join(".config/opencode/opencode.jsonc"),
            h.join(".gemini/shared/mcp_config.json"),
        ]
    }

    fn install(&self, exe: &str, open: &[&str]) -> mcp_install::InstallReport {
        let open: Vec<String> = open.iter().map(|s| s.to_string()).collect();
        mcp_install::install(&agentbridge::connectors::all(), Path::new(exe), false, &move |t: &str| open.iter().any(|o| o == t))
    }

    fn manifest(&self) -> Vec<Value> {
        fs::read_to_string(self.home().join(".agentbridge/manifest.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Files whose name says they are a backup agentbridge took.
    fn backups(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![self.home().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() && !p.is_symlink() {
                    stack.push(p);
                } else if p.to_string_lossy().contains(".agentbridge-backup-") {
                    n += 1;
                }
            }
        }
        n
    }
}

fn actions(r: &mcp_install::InstallReport) -> Vec<(String, Action)> {
    r.tools.iter().map(|t| (t.tool.clone(), t.action.clone())).collect()
}

fn json_of(path: &Path) -> Value {
    jsonc_value(&fs::read_to_string(path).unwrap())
}

/// Parse JSONC (comments, trailing commas) to a value, for assertions.
fn jsonc_value(text: &str) -> Value {
    jsonc_parser::cst::CstRootNode::parse(text, &Default::default())
        .unwrap()
        .value()
        .and_then(|v| v.to_serde_value())
        .unwrap()
}

#[test]
fn install_adds_one_entry_per_tool_keeps_everything_else_and_undoes_exactly() {
    let f = Fake::new();
    let [claude, codex, opencode, agy] = f.settings();

    let r = f.install(EXE, &[]);
    assert!(r.stopped.is_none(), "{:?}", r.stopped);
    assert_eq!(
        actions(&r),
        [
            ("claude-code".to_string(), Action::Added),
            ("codex-cli".to_string(), Action::Added),
            ("opencode".to_string(), Action::Added),
            ("antigravity".to_string(), Action::Added),
        ]
    );

    let c = json_of(&claude);
    assert_eq!(c["mcpServers"]["agentbridge"], mcp_install::entry_for("claude-code", Path::new(EXE)));
    assert_eq!(c["mcpServers"]["agentbridge"]["command"], EXE);
    // The store locations travel with the entry: a tool may start the server
    // with a trimmed environment (Codex does).
    assert_eq!(
        c["mcpServers"]["agentbridge"]["env"]["CLAUDE_CONFIG_DIR"],
        f.home().join(".claude").to_string_lossy().as_ref()
    );
    assert_eq!(c["numStartups"], 12, "the rest of the file is kept");
    let toml = fs::read_to_string(&codex).unwrap();
    assert!(toml.starts_with(CODEX_TOML.trim_end()), "Codex settings kept byte for byte:\n{toml}");
    assert!(toml.contains(&format!("[mcp_servers.agentbridge]\ncommand = \"{EXE}\"\nargs = [\"mcp\"]\nenv = {{")), "{toml}");
    assert!(toml.contains("CODEX_HOME = "), "{toml}");
    let oc_text = fs::read_to_string(&opencode).unwrap();
    assert!(oc_text.contains("// theme first") && oc_text.contains("// keep"), "comments kept:\n{oc_text}");
    let oc = jsonc_value(&oc_text);
    assert_eq!(oc["mcp"]["agentbridge"], mcp_install::entry_for("opencode", Path::new(EXE)));
    assert_eq!(oc["mcp"]["agentbridge"]["command"], json!([EXE, "mcp"]));
    assert!(oc["mcp"]["agentbridge"]["environment"]["XDG_DATA_HOME"].is_string(), "OpenCode's key is `environment`");
    assert_eq!(oc["mcp"]["other"]["command"], json!(["other"]));
    assert!(fs::symlink_metadata(f.home().join(".gemini/config/mcp_config.json")).unwrap().file_type().is_symlink(), "still a symlink");
    assert_eq!(json_of(&agy)["mcpServers"]["agentbridge"], mcp_install::entry_for("antigravity", Path::new(EXE)));

    // Every entry is in the manifest; each existing file was backed up once.
    let rows = f.manifest();
    assert_eq!(rows.iter().filter(|r| r["kind"] == "mcp-config").count(), 4);
    assert_eq!(f.backups(), 4);

    // A second run changes nothing.
    let before: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();
    let again = f.install(EXE, &[]);
    assert!(again.tools.iter().all(|t| t.action == Action::Unchanged), "{:?}", actions(&again));
    let after: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();
    assert_eq!(before, after);
    assert_eq!(f.backups(), 4, "nothing changed, so nothing was backed up");

    // Uninstall puts every file back byte for byte.
    let u = mcp_install::uninstall(false, &|_: &str| false);
    assert_eq!(u.removed.len(), 4, "{:?}", u.kept);
    assert_eq!(fs::read_to_string(&claude).unwrap(), CLAUDE_JSON);
    assert_eq!(fs::read_to_string(&codex).unwrap(), CODEX_TOML);
    assert_eq!(fs::read_to_string(&opencode).unwrap(), OPENCODE_JSONC);
    assert_eq!(fs::read_to_string(&agy).unwrap(), "", "the empty file is empty again, not {{}}");
    assert!(f.manifest().iter().all(|r| r["kind"] != "mcp-config"));
}

#[test]
fn a_moved_binary_updates_only_agentbridges_own_entry() {
    let f = Fake::new();
    f.install(EXE, &[]);
    let r = f.install("/usr/local/bin/agentbridge", &[]);
    assert!(r.tools.iter().all(|t| t.action == Action::Updated), "{:?}", actions(&r));
    let c = json_of(&f.settings()[0]);
    assert_eq!(c["mcpServers"]["agentbridge"]["command"], "/usr/local/bin/agentbridge");
    // Undo still lands on the file as it was before the first install.
    mcp_install::uninstall(false, &|_: &str| false);
    assert_eq!(fs::read_to_string(&f.settings()[0]).unwrap(), CLAUDE_JSON);
    assert_eq!(fs::read_to_string(&f.settings()[3]).unwrap(), "");
}

#[test]
fn an_open_tool_is_skipped_and_named_and_the_others_are_set_up() {
    let f = Fake::new();
    let r = f.install(EXE, &["claude-code"]);
    let claude = r.tools.iter().find(|t| t.tool == "claude-code").unwrap();
    assert_eq!(claude.action, Action::SkippedOpen);
    assert!(claude.detail.contains("Claude Code"), "{}", claude.detail);
    assert_eq!(fs::read_to_string(&f.settings()[0]).unwrap(), CLAUDE_JSON, "nothing written while open");
    assert_eq!(r.tools.iter().filter(|t| t.action == Action::Added).count(), 3);
    // Closed now: the next run picks it up.
    let again = f.install(EXE, &[]);
    assert_eq!(again.tools.iter().find(|t| t.tool == "claude-code").unwrap().action, Action::Added);
}

#[test]
fn an_entry_agentbridge_did_not_add_is_left_alone() {
    let f = Fake::new();
    let mine = "{\n  \"mcpServers\": { \"agentbridge\": { \"command\": \"/home/me/dev/agentbridge\", \"args\": [\"mcp\"] } }\n}\n";
    fs::write(&f.settings()[0], mine).unwrap();
    let r = f.install(EXE, &[]);
    assert_eq!(r.tools.iter().find(|t| t.tool == "claude-code").unwrap().action, Action::SkippedForeign);
    assert_eq!(fs::read_to_string(&f.settings()[0]).unwrap(), mine);
    assert!(!f.manifest().iter().any(|r| r["kind"] == "mcp-config" && r["target_provider"] == "claude-code"));
    // Uninstall never touches it: it has no row.
    mcp_install::uninstall(false, &|_: &str| false);
    assert_eq!(fs::read_to_string(&f.settings()[0]).unwrap(), mine);
}

#[test]
fn an_entry_edited_by_hand_survives_uninstall() {
    let f = Fake::new();
    f.install(EXE, &[]);
    let path = &f.settings()[0];
    let edited = fs::read_to_string(path).unwrap().replace("\"mcp\"", "\"mcp\", \"--verbose\"");
    fs::write(path, &edited).unwrap();
    let u = mcp_install::uninstall(false, &|_: &str| false);
    let kept: Vec<&str> = u.kept.iter().map(|t| t.tool.as_str()).collect();
    assert_eq!(kept, ["claude-code"]);
    assert_eq!(u.kept[0].action, Action::SkippedEdited);
    assert_eq!(fs::read_to_string(path).unwrap(), edited);
    assert!(f.manifest().iter().any(|r| r["kind"] == "mcp-config" && r["target_provider"] == "claude-code"), "the row stays");
}

#[test]
fn a_missing_settings_file_is_created_and_removed_again() {
    let f = Fake::new();
    let [_, codex, opencode, _] = f.settings();
    fs::remove_file(&codex).unwrap();
    fs::remove_file(&opencode).unwrap();
    f.install(EXE, &[]);
    let created = f.home().join(".config/opencode/opencode.json");
    assert!(codex.exists() && created.exists(), "OpenCode gets opencode.json when it has neither file");
    mcp_install::uninstall(false, &|_: &str| false);
    assert!(!codex.exists() && !created.exists(), "files install created are gone again");
}

#[test]
fn dry_run_writes_nothing_and_says_what_it_would_add() {
    let f = Fake::new();
    let before: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();
    let r = mcp_install::install(&agentbridge::connectors::all(), Path::new(EXE), true, &|_: &str| false);
    assert!(r.tools.iter().all(|t| t.action == Action::Added));
    assert!(r.tools.iter().all(|t| t.detail.contains(EXE)), "each line shows the entry");
    let after: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();
    assert_eq!(before, after);
    assert!(!f.home().join(".agentbridge/manifest.jsonl").exists());
    assert_eq!(f.backups(), 0);
}

// ---------------------------------------------------------------------------
// Migration and manifest kinds

/// A Claude Code copy agentbridge made, with `extra` turns the tool appended
/// after it, and its manifest row.
fn claude_copy(f: &Fake, session: &str, extra: usize) -> Value {
    let dir = f.home().join(".claude/projects/-work-web");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.jsonl", session));
    let mut body = String::new();
    for i in 0..(2 + extra) {
        body += &(json!({"type": if i % 2 == 0 {"user"} else {"assistant"}, "cwd": "/work/web", "uuid": format!("{session}-{i}"),
            "timestamp": format!("2026-10-01T09:00:{:02}Z", i), "message": {"role": "user", "content": format!("turn {i} of {session}")}})
            .to_string() + "\n");
    }
    fs::write(&path, body).unwrap();
    use std::os::unix::fs::MetadataExt;
    json!({"dest": path, "cache": "", "session_id": session, "source_provider": "codex-cli", "target_provider": "claude-code",
        "project": "/work/web", "inode": fs::metadata(&path).unwrap().ino(), "message_count": 2})
}

/// A Codex copy of the same session with `extra` new turns.
fn codex_copy(f: &Fake, session: &str, extra: usize) -> Value {
    let dir = f.home().join(".codex/sessions/2026/10/01");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-10-01T09-00-00-{}.jsonl", session));
    let mut body = json!({"timestamp":"2026-10-01T09:00:00Z","type":"session_meta","payload":{"id":session,"cwd":"/work/web"}}).to_string() + "\n";
    for i in 0..(2 + extra) {
        body += &(json!({"timestamp": format!("2026-10-01T09:00:{:02}Z", i), "type":"response_item",
            "payload":{"type":"message","role":"user","content":[{"type":"input_text","text": format!("codex turn {i}")}]}}).to_string() + "\n");
    }
    fs::write(&path, body).unwrap();
    use std::os::unix::fs::MetadataExt;
    json!({"dest": path, "cache": "", "session_id": session, "source_provider": "claude-code", "target_provider": "codex-cli",
        "project": "/work/web", "inode": fs::metadata(&path).unwrap().ino(), "message_count": 2})
}

fn write_manifest(f: &Fake, rows: &[Value]) {
    let p = f.home().join(".agentbridge/manifest.jsonl");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, rows.iter().map(|r| r.to_string() + "\n").collect::<String>()).unwrap();
}

#[test]
fn install_pulls_then_removes_the_hook_and_every_copy_but_keeps_recovered_turns() {
    let f = Fake::new();
    const S: &str = "0a0a0a0a-1111-4222-8333-444444444444";
    let row = claude_copy(&f, S, 1);
    let copy = PathBuf::from(row["dest"].as_str().unwrap());
    write_manifest(&f, &[row]);
    agentbridge::auto::install_hook(false).unwrap();
    let rc = f.home().join(".bashrc");
    assert!(fs::read_to_string(&rc).unwrap().contains("agentbridge"));

    let r = f.install(EXE, &[]);
    assert!(r.stopped.is_none(), "{:?}", r.stopped);
    assert_eq!(r.migration.pulled, 1, "the turn added in the copy is recovered first");
    assert_eq!(r.migration.hook_removed.as_deref(), Some(rc.as_path()));
    assert_eq!(r.migration.copies_removed, 1);
    assert!(!copy.exists(), "the copy is gone");
    assert!(!fs::read_to_string(&rc).unwrap().contains(">>> agentbridge"), "the hook is gone");
    let overlay = fs::read_to_string(f.home().join(format!(".agentbridge/overlay/{S}.jsonl"))).unwrap();
    assert!(overlay.contains(&format!("turn 2 of {S}")), "recovered turn kept: {overlay}");
    let rows = f.manifest();
    assert!(rows.iter().all(|r| r["kind"] == "mcp-config"), "only settings rows are left: {rows:?}");
}

#[test]
fn a_pull_conflict_stops_install_before_anything_is_deleted() {
    let f = Fake::new();
    const S: &str = "0b0b0b0b-1111-4222-8333-444444444444";
    // The same session gained turns in two tools: that needs a decision.
    let a = claude_copy(&f, S, 1);
    let b = codex_copy(&f, S, 1);
    let paths = [PathBuf::from(a["dest"].as_str().unwrap()), PathBuf::from(b["dest"].as_str().unwrap())];
    write_manifest(&f, &[a, b]);
    let settings_before: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();

    let r = f.install(EXE, &[]);
    let why = r.stopped.expect("install stops");
    assert!(why.contains("agentbridge pull"), "{why}");
    assert!(paths.iter().all(|p| p.exists()), "no copy deleted");
    assert_eq!(f.manifest().len(), 2, "manifest untouched");
    assert!(r.tools.is_empty(), "no entry added");
    let settings_after: Vec<String> = f.settings().iter().map(|p| fs::read_to_string(p).unwrap()).collect();
    assert_eq!(settings_before, settings_after);
}

#[test]
fn a_tool_holding_copies_in_its_database_stops_the_migration_while_open() {
    let f = Fake::new();
    const S: &str = "0c0c0c0c-1111-4222-8333-444444444444";
    let row = codex_copy(&f, S, 0);
    write_manifest(&f, &[row]);
    let r = f.install(EXE, &["codex-cli"]);
    assert!(r.stopped.as_deref().is_some_and(|w| w.contains("Codex")), "{:?}", r.stopped);
    assert_eq!(f.manifest().len(), 1);
}

#[test]
fn session_commands_never_touch_settings_rows_and_unsync_removes_both() {
    let f = Fake::new();
    f.install(EXE, &[]);
    const S: &str = "0d0d0d0d-1111-4222-8333-444444444444";
    // A copy made after install (for example by an old shell hook).
    let mut rows = f.manifest();
    let copy = claude_copy(&f, S, 0);
    let copy_path = PathBuf::from(copy["dest"].as_str().unwrap());
    rows.push(copy);
    write_manifest(&f, &rows);

    // status and pull see the copy only.
    let status = agentbridge::sync::status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].session_id, S);
    agentbridge::sync::pull_back(false);
    assert_eq!(agentbridge::sync::read_manifest().len(), 1);
    assert_eq!(f.manifest().iter().filter(|r| r["kind"] == "mcp-config").count(), 4, "pull kept the settings rows");

    // A partial unsync of that folder leaves the settings alone.
    let filter = agentbridge::sync::UnsyncFilter { project: Some(PathBuf::from("/work/web")), sessions: vec![] };
    agentbridge::sync::unsync_matching(false, &filter);
    assert!(!copy_path.exists());
    assert_eq!(f.manifest().len(), 4);
    assert!(json_of(&f.settings()[0])["mcpServers"]["agentbridge"].is_object());

    // A full unsync removes the entries too, each file back as it was.
    agentbridge::sync::unsync_with(false, &|_: &str| false);
    assert!(f.manifest().is_empty());
    assert_eq!(fs::read_to_string(&f.settings()[0]).unwrap(), CLAUDE_JSON);
    assert_eq!(fs::read_to_string(&f.settings()[1]).unwrap(), CODEX_TOML);
}

/// Regression (the operator's real `opencode.jsonc`, 2026-10-30): a trailing
/// comma and blank lines before `}` came back without the blank lines.
#[test]
fn a_file_with_odd_whitespace_comes_back_byte_for_byte() {
    let f = Fake::new();
    let odd = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n \n\n}\n";
    fs::write(&f.settings()[2], odd).unwrap();
    f.install(EXE, &[]);
    assert!(fs::read_to_string(&f.settings()[2]).unwrap().contains("agentbridge"));
    // Moving the binary re-measures the edit from the original bytes.
    f.install("/usr/local/bin/agentbridge", &[]);
    mcp_install::uninstall(false, &|_: &str| false);
    assert_eq!(fs::read_to_string(&f.settings()[2]).unwrap(), odd);
}
