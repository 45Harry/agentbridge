# Connectors

## Overview

One reader per supported tool. Each file implements the `Connector` trait from `src/connector.rs` and turns that tool's native session format into the normalized types in `src/model.rs`. Connectors only read; writing into a tool lives in `src/convert.rs` and the `src/*_write.rs` modules.

## Key files

| File | Owns |
|---|---|
| `mod.rs` | `all()`, the single registration point, plus `all_for_testing()` for fixture roots |
| `claude_code.rs` | JSONL transcripts under `~/.claude/projects` (honors `CLAUDE_CONFIG_DIR`) |
| `codex_cli.rs` | Rollout JSONL under `~/.codex/sessions` (honors `CODEX_HOME`) |
| `opencode.rs` | Rows in `opencode.db` under the XDG data dir (honors `XDG_DATA_HOME`) |
| `antigravity.rs` | SQLite conversation stores under `~/.gemini/antigravity*`, with a hand written protobuf reader |

## Conventions

- Adding a tool means one new file here plus one line in `mod.rs::all()`. If a core module has to change, treat that as a flaw in the abstraction and fix the trait first.
- `id()` is a stable string (`claude-code`, `codex-cli`, and so on). It is stored on every session and used as a CLI value, so it never changes once shipped.
- `detect()` is existence checks only. `roots()` honors the tool's own env override before the default path.
- `scan()` is lazy and metadata only. One unreadable session becomes an `Err` item; it never aborts the scan.
- If a body is missing but metadata survives, yield a `RawSession` with `body_available: false` instead of dropping the session.
- For Claude Code, sessions are only the files under `projects/` in the config folder. Other `.jsonl` files beside it (`history.jsonl`) are not sessions.
- When a format has no title, `load` hands the first record through in `raw_payload` (Codex does), so `label::is_copy` can see an origin record agentbridge wrote.
- Read the project path from the `cwd` or project field inside the records. Never decode it from the directory name; that encoding is lossy.
- Open another tool's SQLite database with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`, never for writing and never with `immutable=1`.
- Tolerate a truncated last line, bytes that are not UTF-8, and empty files without a panic.
- Verify a tool's format against real data first, then record it in `CONNECTORS.md` before or with the code change.

## Gotchas

- Do not assume a tool has one store. Antigravity keeps four under `~/.gemini/`; list the config root and check the siblings.
- Some Antigravity `.pb` bodies are encrypted (byte entropy is 8.0). Measure before trying to map a schema.
- `scan()` for Claude Code stops at the first record that carries `cwd`, so a later rename shows in `list` only after `load()` runs.
- The fixture based test connectors (`TestClaudeCode`, `TestCodexCli`) cover two tools only. OpenCode and Antigravity tests build their own temp databases.
- `test_load_real_antigravity_conversation` is ignored by default and needs real local data. Run `cargo test -- --ignored` after any change here.

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
