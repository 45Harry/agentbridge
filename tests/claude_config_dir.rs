//! `CLAUDE_CONFIG_DIR` names Claude Code's config folder; sessions live in
//! `projects/` under it.
//!
//! Found on the operator's real machine 2026-10-02 (`CLAUDE_CONFIG_DIR` set):
//! the reader walked the whole config folder, so `history.jsonl` and every
//! other `.jsonl` Claude Code keeps there was listed as a session. Those
//! showed up in every tool as `claude-code · (untitled) · 0000-00-00 00:00`
//! and `claude-code · history · +58579-08-17 12:37`.

use std::path::Path;
use std::process::Command;

fn agentbridge(home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentbridge"))
        .args(args)
        .env("HOME", home)
        .env("AGENTBRIDGE_DATA_DIR", home.join(".agentbridge"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("ANTIGRAVITY_HOME", home.join(".gemini/antigravity-cli"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("binary runs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn only_files_under_projects_are_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join(".claude");
    let project = config.join("projects/-home-user-project");
    std::fs::create_dir_all(&project).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code/normal-multi-turn.jsonl");
    std::fs::copy(fixture, project.join("normal-multi-turn.jsonl")).unwrap();

    // What Claude Code itself keeps beside `projects/`.
    std::fs::write(
        config.join("history.jsonl"),
        "{\"display\":\"hi\",\"timestamp\":1790927716000,\"project\":\"/home/user/project\"}\n",
    )
    .unwrap();
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions/0add4c81.jsonl"), "{\"pid\":1}\n").unwrap();

    let listed = agentbridge(tmp.path(), &["ls"]);
    assert!(listed.contains("normal-multi-turn"), "the real session is listed: {listed}");
    assert!(!listed.contains("history"), "history.jsonl is not a session: {listed}");
    assert!(!listed.contains("0add4c81"), "nor is anything else outside projects/: {listed}");

    let found = agentbridge(tmp.path(), &["init"]);
    assert!(found.contains("1 sessions"), "exactly one session exists: {found}");
}
