//! `agentbridge resume <id> opencode` must use the same database `sync` does.
//!
//! Found live 2026-10-02: with `XDG_DATA_HOME` set, `resume` looked for
//! `$XDG_DATA_HOME/opencode.db` instead of `$XDG_DATA_HOME/opencode/opencode.db`,
//! created an empty database (and an empty "backup") at the wrong path, and
//! then failed with `no such table: project`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run the built binary with every store redirected into `home`.
fn agentbridge(home: &Path, xdg: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentbridge"))
        .args(args)
        .env("HOME", home)
        .env("AGENTBRIDGE_DATA_DIR", home.join(".agentbridge"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("ANTIGRAVITY_HOME", home.join(".gemini/antigravity-cli"))
        .env("XDG_DATA_HOME", xdg)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("binary runs")
}

/// A sandbox home holding one synthetic Claude Code session.
fn home_with_claude_session() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".claude/projects/-home-user-project");
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code/normal-multi-turn.jsonl");
    std::fs::copy(fixture, dir.join("normal-multi-turn.jsonl")).unwrap();
    (tmp, "normal-multi-turn".to_string())
}

fn opencode_db(xdg: &Path) -> PathBuf {
    xdg.join("opencode").join("opencode.db")
}

fn create_opencode_schema(db: &Path) {
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            sandboxes TEXT NOT NULL);
        CREATE TABLE session (
            id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
            parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
            title TEXT NOT NULL, version TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            metadata TEXT,
            FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            data TEXT NOT NULL,
            FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
            session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL,
            FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE);
        INSERT INTO project VALUES ('global','/',0,0,'[]');
        "#,
    )
    .unwrap();
}

/// The write is refused while a real OpenCode is open on this machine; that
/// is the guard working, not the behavior under test.
fn opencode_is_open(out: &Output) -> bool {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.to_lowercase().contains("running")
}

#[test]
fn resume_into_opencode_writes_to_the_database_under_xdg_data_home() {
    let (home, id) = home_with_claude_session();
    let xdg = home.path().join("xdg");
    let db = opencode_db(&xdg);
    create_opencode_schema(&db);

    let out = agentbridge(home.path(), &xdg, &["resume", &id, "opencode", "--copy"]);
    if opencode_is_open(&out) {
        return;
    }

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (rows, title): (i64, String) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(title), '') FROM session WHERE metadata LIKE '%agentbridge%'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        rows,
        1,
        "the session must land in the real database: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        title.starts_with("claude-code · Add authentication feature · 2026-07-01 12:00 · "),
        "and carry the cross tool label: {title}"
    );
    assert!(
        !xdg.join("opencode.db").exists(),
        "nothing may be created at the wrong path"
    );
}

#[test]
fn resume_into_opencode_never_creates_a_database_that_does_not_exist() {
    let (home, id) = home_with_claude_session();
    let xdg = home.path().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();

    let out = agentbridge(home.path(), &xdg, &["resume", &id, "opencode", "--copy"]);
    if opencode_is_open(&out) {
        return;
    }

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("OpenCode database not found"),
        "a missing database is reported plainly: {text}"
    );
    let stray: Vec<_> = walk(&xdg)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "db"))
        .collect();
    assert!(stray.is_empty(), "no database file may be created: {stray:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
