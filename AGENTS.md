# agentbridge

One session layer for the AI coding agents on a machine (Claude Code, Codex CLI, OpenCode, Antigravity). It indexes each tool's sessions in place, converts them once, and makes them show up in every other tool.

## Stack

- **Language / Runtime**: Rust, edition 2024, one crate (library in `src/lib.rs`, binary in `src/main.rs`)
- **Framework**: `clap` 4 (derive) for the CLI, `ratatui` + `crossterm` for the terminal screens
- **Key dependencies**: `rusqlite` (bundled SQLite), `serde` / `serde_json`, `chrono`, `uuid` (v5), `anyhow` / `thiserror`
- **Package manager**: cargo

## Build approach

<TBD, set by /scope>

## Commands

```bash
# Build
cargo build

# Run (bare `cargo run` opens the dashboard; `init` is read only discovery)
cargo run -- init

# Test
cargo test

# Real data checks (ignored by default, run after any connector change)
cargo test -- --ignored

# Install the binary from this checkout
cargo install --path .
```

There is no CI, lint, or format config in the repo yet.

## Specs

Stored in `docs/specs/`. Format: `docs/specs/NNNN-title.md`. Older design records live at the root: `DESIGN.md` (architecture and invariants), `CONNECTORS.md` (each tool's format on disk), `DECISIONS.md` (dated choices), `HANDOFF.md` (current state and lessons), `SPEC.md` (the original build prompt).

## Rules

- A tool's own session files are never changed. New turns are recovered into an overlay that agentbridge owns (`~/.agentbridge/overlay`).
- A session body is never stored twice. Convert once into `~/.agentbridge/cache`, then hardlink into each directory.
- Sync must be idempotent and deterministic. Ids are UUID v5 of the source id and paths come from the session's own start time. No random ids and no `Utc::now()` in names.
- Everything agentbridge creates goes in the manifest, so `unsync` removes exactly that. Never `rm -rf ~/.agentbridge`; run `agentbridge unsync`.
- Every command that writes supports `--dry-run`.
- Never test against real session stores. Use a fake `HOME` and also set `AGENTBRIDGE_DATA_DIR`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and `ANTIGRAVITY_HOME`. Never run the real `codex` binary in a test; `CODEX_HOME` does not fully isolate it.
- Fixtures in `tests/fixtures/` are synthetic and written by hand. Never commit real session data.
- Every bug fix gets a regression test. Green unit tests are not proof for a format change; check it against the real tool or a copy of its real database.
- Work lands on `develop`; `master` is the release branch and is merged by PR. Commit subjects use `feat:`, `fix:`, `docs:`.
- No network calls and no telemetry.

## Context files

- [src/AGENTS.md](src/AGENTS.md): module map, the gates on writing into another tool's database, and lessons from real bugs
- [src/connectors/AGENTS.md](src/connectors/AGENTS.md): the `Connector` contract and how to add or change a tool reader

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
