# src

## Overview

The whole crate. The library (`lib.rs` and the modules it lists) does discovery, conversion, sync, and write back with no UI code. The binary (`main.rs`, `tui.rs`, `dashboard.rs`) is the CLI and the terminal screens, and it drives the library through plain public functions.

## Key files

| File | Owns |
|---|---|
| `main.rs` | clap CLI: `ls`, `index`, `init`, `sync`, `pull`, `auto`, `status`, `unsync`, `resume`, `inject`, `start`, `info`, `tui` |
| `index.rs` | Discovery. Metadata plus a pointer to the source file, never a copy of the body |
| `sync.rs` | Cache, hardlink fan out, manifest, `sync_into`, `pull_back`, `status`, `unsync` |
| `convert.rs` | Writers for the Claude Code and Codex file formats, plus the brief builder |
| `label.rs` | The shared title label: provider, name, start time, first 8 chars of the id |
| `opencode_write.rs` | Inserts rows into OpenCode's SQLite database |
| `codex_write.rs` | Writes `threads` rows into Codex's `state_5.sqlite` so its picker lists synced files |
| `antigravity_write.rs` | Writes a conversation database and a summaries row for Antigravity; the only place protobuf is authored |
| `auto.rs` | Change fingerprint, watch loop, shell hook install and removal |
| `inject.rs` | Fenced begin and end markers for injected briefs |
| `model.rs` | Normalized types: `Project`, `Session`, `Message`, `Artifact`, `Fact` |
| `connector.rs` | The `Connector` trait and `Registry` |
| `tui.rs`, `dashboard.rs` | Pull conflict screen and the dashboard. Binary only |

## Conventions

- Keep the library free of UI dependencies. `sync.rs` reaches the conflict screen only through the `ConflictResolver` trait.
- Code outside `connectors/` works on `model.rs` types, not on a tool's native shapes.
- Every module that writes into another tool's live database follows the same gates: back the database up before the first real insert of a run, tag every row agentbridge creates, refuse to write while that tool is running, and render statements under `--dry-run` without running them.
- Read env overrides on every call. Do not cache them in a `LazyLock` or a static; that made `CODEX_HOME` first reader wins.
- Tests that need the built binary (a CLI flag, an env override) live in `tests/*.rs` and run it with every store variable pointed at a temp dir.
- Tests live beside the code in `#[cfg(test)]` modules. Tests that touch env vars go through `Sandbox::new()` in `sync.rs`, which holds a lock and redirects `HOME` and every store variable into a temp dir.
- Assert on content, not on "not empty". A test that cannot tell 1 message from 40 is not testing the decoder.

## Gotchas

- Generated JSONL must end with a newline. Without it, the tool's next record is appended onto the last line and corrupts the session.
- Never `fs::copy` onto a destination that already shares the source's inode. It truncates the file before reading it.
- Files agentbridge wrote must never be picked up as source sessions. The manifest marks them, and it keeps only the last row per destination.
- OpenCode keeps every session in one database, so a manifest row for it has the database as `dest` and the row id in `cache`. Anything that matches rows by `dest` alone (loop prevention, updating a row on a repeat sync) must also use the row id.
- A copy must say it is a copy without the manifest: its title is a label naming another origin, or (Codex, which has no title) its first record carries an `agentbridge` origin. `label::is_copy` reads both. Judge it on the entry's own file, since a copy can share its origin's id.
- `sync` and `pull` take `sync.lock` in the data dir. The shell hook starts many runs at once; without the lock they copy each other's copies.
- Before writing a file target, check what is already there. A file that is not in the manifest and is not a copy belongs to the tool and is left alone.
- agy rebuilds its index when it starts and blanks the title and marker on rows it did not write. Recognise and replace agentbridge's agy rows by their version 5 id (`antigravity_write::is_derived_id`), never by the marker alone.
- A copy written into OpenCode must have an id of OpenCode's own shape (`ses_`, 12 hex, 14 letters or digits; `opencode_write::derive_id`). OpenCode's free models refuse a session with any other id shape. Tell agentbridge's rows by the metadata marker, never by the id.
- A fresh OpenCode database has no `global` project row. `opencode_write::write_session` creates it; any other insert path must too, or the foreign key fails.
- The shell hook runs `sync --changed`: it compares a stored fingerprint and shares only sessions changed since the last run (`sync_into_since`). Leave SQLite's `-shm` file out of that fingerprint; reading a database changes it.
- SQLite WAL writes do not change the `.db` mtime. The fingerprint also stats the `-wal` and `-shm` siblings.
- `proto_varint` returns the end offset, not a length. Use `i = n`, not `i += n`.
- Rank and label sessions by timestamps inside the file, not by file mtime. Tools rewrite files on compaction and title changes.
- Build before you override `HOME` for a sandbox run. Overriding it breaks rustup.
- `ANTIGRAVITY_HOME` redirects only the write target. The read scan still covers the real stores, so redirect `HOME` too for a fully isolated run.
- The redaction pass that `SPEC.md` and `DESIGN.md` require does not exist yet (`src/redact.rs` is planned).

Full history of these lessons: `HANDOFF.md` section 4.

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
