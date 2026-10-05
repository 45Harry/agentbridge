//! `agentbridge sync --changed` is what the shell hook runs. It must do nothing
//! when no session was created or changed since the last run.
//!
//! Found on a real machine 2026-10-02: the hook ran a full `sync` in every new
//! shell, which meant 68,000 copies and eight minutes of work per terminal.

use std::path::Path;
use std::process::Command;

fn agentbridge(home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentbridge"))
        .args(args)
        .current_dir(home)
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

fn add_claude_session(home: &Path, name: &str) {
    let project = home.join(".claude/projects/-home-user-project");
    std::fs::create_dir_all(&project).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code/normal-multi-turn.jsonl");
    // Written, not copied: a copy keeps the fixture's old modification time,
    // and the point here is a session that was just created.
    std::fs::write(project.join(format!("{name}.jsonl")), std::fs::read(fixture).unwrap()).unwrap();
}

fn rollouts(home: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(&home.join(".codex/sessions"), &mut n);
    n
}

#[test]
fn changed_sync_does_nothing_until_a_session_is_new() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    // Codex is present as a second tool, so there is somewhere to copy to.
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    add_claude_session(home, "first-session");

    let first = agentbridge(home, &["sync", "--changed"]);
    assert!(first.contains("Sharing sessions changed since"), "the first run shares: {first}");
    let after_first = rollouts(home);
    assert!(after_first > 0, "the session reached Codex: {first}");

    let second = agentbridge(home, &["sync", "--changed"]);
    assert!(
        second.contains("Nothing new since the last sync."),
        "with nothing new, the run stops at once: {second}"
    );
    assert_eq!(rollouts(home), after_first);

    // A new session appears: only then is there work, and only for it.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    add_claude_session(home, "second-session");
    let third = agentbridge(home, &["sync", "--changed"]);
    assert!(third.contains("Sharing sessions changed since"), "a new session wakes it: {third}");
    assert!(rollouts(home) > after_first, "and that session reaches Codex: {third}");

    let fourth = agentbridge(home, &["sync", "--changed"]);
    assert!(fourth.contains("Nothing new since the last sync."), "then it is quiet again: {fourth}");
}
