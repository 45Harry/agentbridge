# 0001. Read sessions through an MCP server instead of copying them

**Date**: 2026-10-05
**Status**: Proposed

## Summary

agentbridge stops copying sessions into every tool. Instead it runs as a small MCP server (MCP is the standard way an AI tool calls an outside helper), set up once in Claude Code, Codex, OpenCode and Antigravity. From any folder, the AI in any of those tools can list every session on the machine and read any of them, page by page, straight from the original files. Nothing is written while it reads; `agentbridge mcp install` sets it up and cleans away the old copies and shell hook.

## Requirements

**User stories**:
- As a developer, I want the AI in any tool, in any folder, to find a session I had in another tool, so I can continue that work without copying anything.
- As a developer, I want to set this up with one command and have the old copies and shell hook cleaned away, so nothing is left behind.
- As a developer, I want to undo the setup exactly, so my tools' settings go back to how they were.

**Acceptance criteria**:
- **AC-1**: `agentbridge mcp` speaks MCP over stdio (messages on standard input and output). `initialize` answers with the server name, version and the tools capability; `tools/list` returns exactly two tools, `list_sessions` and `read_session`, each with a JSON schema; stdout carries only protocol messages, every log line goes to stderr.
- **AC-2**: `list_sessions` with no arguments returns sessions from every detected tool and every folder: sessions of the current folder first, then all others, each group newest first. It pages with `limit` (default 20, max 100) and `offset`, and returns the `total` count.
- **AC-3**: `query` matches, ignoring case, any of: title, first user message, folder path, tool id, session id prefix. `tool`, `project` and `since` narrow the list. A query with no match returns an empty list, not an error.
- **AC-4**: Sessions agentbridge itself wrote (copies) never appear in `list_sessions`.
- **AC-5**: `read_session` returns page 1 as the newest 40 messages, in time order inside the page, with `page`, `total_pages` and `total_messages`. Page 2 holds the 40 before those, and so on. `page_size` (1 to 200) overrides 40.
- **AC-6**: Each message in a page carries its role, time, full text, and for tool calls the tool name, full input and full output, with nothing cut.
- **AC-7**: Turns recovered earlier by `pull` (the overlay) follow the session's own messages, in order, each marked as recovered.
- **AC-8**: An unknown id, a session whose body cannot be read (for example an encrypted Antigravity body), or a store that stays locked returns a tool error (`isError: true`) with a plain message saying what happened and what to try. The server keeps running.
- **AC-9**: Running `agentbridge mcp` and calling both tools creates, changes or deletes no file anywhere (checked by comparing a fake `HOME` before and after).
- **AC-10**: `agentbridge mcp install` adds an `agentbridge` entry to the settings file of every detected tool. The entry runs the absolute path of the binary that ran install, with the argument `mcp`. Every other byte of the file stays as it was, comments and layout included. Each file is backed up before its first change. `--dry-run` writes nothing and prints, per tool, the file and the entry it would add.
- **AC-11**: Running install twice changes nothing the second time (or only updates the binary path in agentbridge's own entry if the binary moved). An `agentbridge` entry that agentbridge did not create is left untouched and reported.
- **AC-12**: If a tool is open, install skips that tool, sets up the others, and names the skipped tool so you can run install again after closing it.
- **AC-13**: Before it adds any entry, install migrates away from copies: it runs `pull` first; if a pull conflict needs your decision it stops before deleting anything and tells you to run `agentbridge pull`; otherwise it removes the shell hook and removes every session copy agentbridge made. Recovered turns in the overlay are kept. `--dry-run` shows all of this without doing it.
- **AC-14**: `agentbridge mcp uninstall` removes exactly the entries agentbridge added, leaving the rest of each file as it was. `agentbridge unsync` also removes them, because every created entry is in the manifest.
- **AC-15**: `sync`, `pull`, `status` and `auto` keep working for this release and print one line on stderr saying they are replaced by `agentbridge mcp install`.
- **AC-16**: In each of the four real tools, opened in a folder where a session did not start, the AI can call `list_sessions`, find a session from another tool, and read it with `read_session`.

## Decision

**Chosen option**: Option 1: a small MCP server written in house (stdio, serde_json, no async runtime), with an install command that edits each tool's settings file directly.

agentbridge reads sessions live from each tool's own store and hands them to whichever AI asks, replacing the copy based sync.

## Rationale

Reasoning and options: see [rationale.md](rationale.md).

## Feature design

**Data model sketch**:

No database. Two things change shape:

| Entity | Field | Type | Required | Notes |
|---|---|---|---|---|
| `IndexEntry` (existing, `src/index.rs`) | `first_prompt` | `Option<String>` | no | New. The text of the first `Role::User` message that is not bookkeeping (`label::is_bookkeeping`), cut to 300 Unicode characters (not bytes). Filled by `scan()` where it is cheap; `None` when the connector cannot get it cheaply, and search then uses the title alone. Mirrors a new `RawSession::first_prompt`. |
| `LinkRecord` (existing manifest row, `src/sync.rs`) | `kind` | `String` | yes, `#[serde(default = "session")]` | New. `"session"` for every existing row (old rows parse unchanged), `"mcp-config"` for an MCP entry. |
| `LinkRecord` | `entry` | `Option<serde_json::Value>` | no, `#[serde(default)]` | New. For `mcp-config` rows, the exact value agentbridge wrote under its key (TOML stored as its JSON equivalent). `None` on session rows. |
| `LinkRecord` | `created_file` | `bool` | no, `#[serde(default)]` | New. `true` when install created the settings file because none existed. |
| `LinkRecord` for `kind = "mcp-config"` | `dest` | path | yes | The settings file that was edited (the resolved target if it is a symlink). |
| | `target_provider` | string | yes | `claude-code`, `codex-cli`, `opencode` or `antigravity`. |
| | `cache` | path | yes | Holds the entry key, `agentbridge`. (Reuses the field the OpenCode rows already use for a row id.) |
| | `session_id`, `source_provider`, `project` | | | Empty strings / the install folder; unused for this kind. |
| | `inode` | u64 | yes | `0`; ownership is judged by comparing the file's current entry with `entry`. |
| MCP response message (new, `src/mcp.rs` only) | `ordinal`, `role`, `time`, `text`, `tool_name`, `tool_input`, `tool_output`, `recovered` | from `Message`, plus `recovered: bool` | | A wrapper built at read time; `model.rs` is not changed. Native messages keep their own ordinals; recovered ones continue the numbering after the last native ordinal. |

Invariants on the manifest:
- Unique per (`kind`, `target_provider`, `dest`, `cache`, `session_id`). `dedup_manifest` and the append path in `sync.rs` both use this key.
- Every reader of the manifest filters by `kind`: `pull`, `status`, `sync`, loop prevention and copy detection read only `session` rows; `mcp install`/`uninstall` read only `mcp-config` rows. `unsync` with no filter removes both kinds, each with its own removal code (a settings file is never deleted as if it were a copy).
- Rows of kind `mcp-config` are written only after the edit succeeds.

**Per tool settings target** (verify the Antigravity row first, see Build plan step 4):

| Tool | File | Entry written |
|---|---|---|
| Claude Code | `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json` | `mcpServers.agentbridge = {"type":"stdio","command":"<abs>","args":["mcp"]}` |
| Codex | `$CODEX_HOME/config.toml`, else `~/.codex/config.toml` | `[mcp_servers.agentbridge]` with `command = "<abs>"`, `args = ["mcp"]` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/opencode.jsonc`, else `opencode.json` there, else create `opencode.json` | `mcp.agentbridge = {"type":"local","command":["<abs>","mcp"],"enabled":true}` |
| Antigravity | `$ANTIGRAVITY_HOME/mcp_config.json`, else `~/.gemini/antigravity/mcp_config.json` (today a symlink to `~/.gemini/config/mcp_config.json`, empty) | `mcpServers.agentbridge = {"command":"<abs>","args":["mcp"]}` |

**State transitions** (install, per tool):

`not installed` → (`mcp install`, tool closed) → `installed` → (`mcp uninstall` or `unsync`) → `not installed`.
`not installed` → (`mcp install`, tool open) → `skipped` (nothing written) → next `mcp install` retries.
`installed` → (`mcp install` from a binary at a new path) → `installed` with the new path.

Migration inside install runs once per run, before any entry, all under one held `sync.lock` so the old shell hook cannot start a sync in the middle: dry pull with a resolver that records conflicts → (any conflict or pull error) stop, nothing deleted → real `pull` → remove hook → unsync rows of kind `session` → add entries.

`mcp uninstall`, per entry: the file's current value under `agentbridge` equals the manifest `entry` → remove it (and delete the file if `created_file` and nothing else is left in it) → drop the row. Value differs (you edited it) → leave it, keep the row, report it. Key gone already → drop the row.

**API surface** (MCP over stdio, JSON-RPC 2.0, one message per line):

| Method / tool | Key inputs | Key outputs | Auth | Key errors |
|---|---|---|---|---|
| `initialize` | `protocolVersion`, `clientInfo` | `protocolVersion` (the client's if supported, else `2025-06-18`), `serverInfo {name: "agentbridge", version}`, `capabilities {tools: {}}` | none (local process of the same user) | -32602 bad params |
| `notifications/initialized` | none | none (no reply) | none | none |
| `ping` | none | `{}` | none | none |
| `tools/list` | none | the two tools below with input schemas | none | none |
| `list_sessions` (via `tools/call`) | `query: string` (opt), `tool: string` (opt), `project: string` (opt; `"all"` disables the current folder boost), `since: string` (opt), `limit: int` (opt, 20, 1 to 100), `offset: int` (opt, 0, at least 0) | see response shape below | none | `isError`: bad `since`, unknown `tool`, out of range numbers |
| `read_session` (via `tools/call`) | `id: string` (req), `tool: string` (opt, picks one when an id exists in two tools), `page: int` (opt, 1 = newest, at least 1), `page_size: int` (opt, 40, 1 to 200) | see response shape below | none | `isError`: unknown id, ambiguous id, body unavailable, store locked, page out of range |
| unknown method | any | JSON-RPC error | none | -32601 |

Input schemas set `additionalProperties: false`; an unknown argument is `isError`, not ignored.

Response shape: every `tools/call` result carries the same JSON object twice, as `structuredContent` and as pretty printed JSON in `content[0].text` (clients that ignore `structuredContent` still get it).

```json
{ "total": 312, "offset": 0, "limit": 20, "current_folder": "/home/harry/Documents/agentbridge",
  "warnings": 2,
  "sessions": [ { "id": "019fc209-...", "tool": "codex-cli", "title": "Fix login bug",
                  "folder": "/home/harry/Documents/app", "started_at": "2026-10-01T09:12:00Z",
                  "last_event_at": "2026-10-01T10:40:00Z", "first_prompt": "the login form...",
                  "in_current_folder": false } ] }
```

```json
{ "id": "019fc209-...", "tool": "codex-cli", "title": "Fix login bug", "folder": "/home/harry/Documents/app",
  "started_at": "2026-10-01T09:12:00Z", "page": 1, "page_size": 40, "total_pages": 3, "total_messages": 97,
  "recovered_from_overlay": 2,
  "messages": [ { "ordinal": 57, "role": "assistant", "time": "2026-10-01T10:01:00Z", "text": "...",
                  "tool_name": "shell", "tool_input": { "cmd": "cargo test" }, "tool_output": "...",
                  "recovered": false } ] }
```

Fields that have no value are `null`, never left out. An empty session returns `total_pages: 0`, `messages: []` for page 1; any other page is out of range.

Protocol rules (the supported subset of JSON-RPC 2.0):
- One JSON object per line on stdin and stdout, UTF-8, no embedded newlines (serde_json compact output). Batches (arrays) are rejected with -32600.
- A line that is not JSON: reply -32700 with `id: null`. Wrong `jsonrpc` value or a missing `method`: -32600. The request `id` is echoed back exactly as received (number or string).
- `tools/call` before `initialize`: -32002 "not initialized". Notifications never get a reply, even on error.
- End of stdin: exit 0. A failed write to stdout: exit 1 (the client has gone).

CLI surface:

| Command | Flags | Writes |
|---|---|---|
| `agentbridge mcp` | none | nothing |
| `agentbridge mcp install` | `--dry-run` | each tool's settings file, the manifest, backups; plus the migration (hook, copies) |
| `agentbridge mcp uninstall` | `--dry-run` | each tool's settings file, the manifest |

**Value sourcing**:

| Action | Value produced / displayed | Source |
|---|---|---|
| `initialize` | server version | `env!("CARGO_PKG_VERSION")` |
| `initialize` | protocol version | client's `protocolVersion` if in the supported set {`2025-06-18`, `2025-03-26`, `2024-11-05`}, else `2025-06-18` |
| `list_sessions` | the session list | `index::discover(&connectors::all())`, run on every call (no cache) |
| `list_sessions` | "current folder" | the `project` input if given, else the server process's working directory (each tool starts the server in the folder it was opened in; verified per tool in step 7) |
| `list_sessions` | which entries are copies | a `session` manifest row with the same `target_provider` as the entry's tool and, for OpenCode (one database for all sessions), the same row id in `cache` as the entry's id; for the other tools the same `dest` as the entry's `source_path`. Also any entry whose title parses with `label::parse`. Never the database path alone |
| `list_sessions` | first user message | new `IndexEntry::first_prompt`, from each connector's `scan()` |
| `list_sessions` | how `query` matches | split on whitespace; every word must appear (ignoring case, Unicode lowercase) in at least one of title, `first_prompt`, folder path, tool id; or the whole query is a prefix of the session id |
| `list_sessions` | how `project` matches | the given path and each entry's folder are made absolute and have trailing slashes removed (no symlink resolution), then compared exactly; `"all"` means no folder filter and no boost |
| `list_sessions` | how `since` compares | accepts `YYYY-MM-DD` (read as 00:00 UTC) or a full RFC 3339 time; keeps entries whose `last_event_at`, else `started_at`, is at or after it; entries with neither time are dropped when `since` is given |
| `list_sessions` | how filters combine | all given filters must hold (AND) |
| `list_sessions` | newest first order | `last_event_at`, else `started_at`; entries with neither go last; ties broken by (tool id, session id, source path) so the order is stable between calls |
| `list_sessions` | `total` | count after filters, before `limit`/`offset` |
| `list_sessions` | `warnings` | `Index::errors.len()` from the same discovery pass; the sessions that failed to parse are not in `total` |
| `read_session` | the session to load | the `IndexEntry` from a fresh discovery matching (`tool` if given, `id`); `Registry::by_id(entry.provider).load(id)`; the loaded `Session::source_path` must equal the entry's `source_path`, otherwise `isError` (another store has the same id) |
| `read_session` | which entry when the id matches several | the `tool` input; then drop copies; one left wins; still several → `isError` listing each tool and source path so the AI can call again with `tool` |
| `read_session` | recovered turns | `sync::overlay_messages(id)`, appended after native messages in file order, with `recovered: true`. The overlay is keyed by session id only, so it is merged only when exactly one non copy entry has that id; otherwise it is skipped and `recovered_from_overlay` is `0` |
| `read_session` | message order | native messages in `load()` order (their ordinals), then recovered ones; pages are cut from the end of that combined list |
| `read_session` | `total_pages` | `ceil((native + recovered) / page_size)` |
| `read_session` | message time | `Message::timestamp`; `null` when `None` |
| `read_session` | tool input and output | `Message::tool_input` and `tool_result` as the connector fills them. Antigravity's connector fills neither today, so its sessions show text only; noted in the README |
| any SQLite read | locked store | each read only SQLite open sets `busy_timeout` to 2 seconds; only `SQLITE_BUSY` / `SQLITE_LOCKED` after that becomes "<tool> is busy writing, try again in a moment"; other errors pass through as they are |
| `mcp install` | binary path written | `std::env::current_exe()`, canonicalized |
| `mcp install` | is the tool open | the existing `ensure_safe_to_write()` checks in `codex_write`, `opencode_write`, `antigravity_write`; a new equivalent for Claude Code (a `claude` process is running) |
| `mcp install` | did agentbridge create an entry | a manifest row of kind `mcp-config` for that file and key |
| `mcp install` | backup location | new `backup_settings(path)`: copies the file next to itself as `<name>.agentbridge-backup-<UTC stamp>.<ext>`, the same naming the database writers use; no backup when the file does not exist yet (`created_file` is set instead); backups are never deleted by agentbridge |
| `mcp install` | pull conflicts | a dry `pull_back_with` using a new resolver that records each conflict and answers `Skip`; any recorded conflict or pull error stops the install before anything is deleted |

**Key invariants**:
- `agentbridge mcp` never opens a file for writing and never opens SQLite for writing (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`, never `immutable=1`).
- Nothing on stdout except protocol messages.
- A settings file keeps every byte outside agentbridge's own entry, comments, spacing and key order included, in all three JSON style files and in TOML.
- Settings edits are atomic and checked: hold an install lock (`mcp.lock` in the data dir), read the file, compute the edit, read the file again just before writing and stop for that tool if it changed, then write a temp file in the same folder as the target and rename it over the target.
- A symlinked settings file stays a symlink: resolve the full chain first, write and rename onto the final target only, then check the link still points there. A dangling link (target missing) is skipped with a message, never replaced with a file.
- The migration holds `sync.lock` from the dry pull until the last session copy is removed.
- No entry is added for a tool whose store is not detected.
- Install never deletes a copy before `pull` has finished without an unresolved conflict.
- Every entry install adds has a manifest row; nothing is removed that has no row.

**Security model**:
- The server runs as a child process of the tool, as the same user, over stdio. It opens no port and makes no network call, so it needs no authentication.
- It can read every session of every tool for that user, in every folder. That is the purpose; it is stated in the README.
- No redaction: text is returned as stored (your choice, see Consequences). The tool's own AI provider receives whatever the AI reads.
- Read only: no tool on the server changes anything.

**Configuration required**: no new environment variables. Existing overrides are honored for both reading and the settings paths: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `ANTIGRAVITY_HOME`, `AGENTBRIDGE_DATA_DIR`. New crate dependencies: `toml_edit` (TOML edits that keep comments and layout) and `jsonc-parser` with its `cst` feature (edits that keep comments and layout, used for all three JSON style files: `.claude.json`, `opencode.json[c]`, `mcp_config.json`).

**Critical test scenarios**:
- Happy path: spawn `agentbridge mcp` in a fake `HOME` with fixture sessions of two tools in two folders; send `initialize`, `tools/list`, `list_sessions`, `read_session`; assert the current folder comes first and the page holds the right 40 messages, verifies **AC-1**, **AC-2**, **AC-5**, **AC-6**.
- Read only: hash every file under the fake `HOME` before and after a full session of calls; identical, verifies **AC-9**.
- Failure case: `read_session` with an unknown id, then a valid call on the same process; first is `isError`, second succeeds, verifies **AC-8**.
- Settings edit: a JSONC file with comments and a TOML file with comments get the entry; diff shows only the added entry; second run is a no op, verifies **AC-10**, **AC-11**.
- Foreign entry: a settings file already has an `agentbridge` entry with no manifest row; install leaves it and reports it, verifies **AC-11**.
- Migration safety: a copy with an unpulled turn and a forced conflict; install stops and no copy is deleted, verifies **AC-13**.
- Undo: install then uninstall; each settings file is byte identical to before, verifies **AC-14**.
- Edited entry: install, change agentbridge's entry by hand, uninstall; the entry stays and is reported, verifies **AC-14**.
- Manifest kinds: a manifest with session rows and `mcp-config` rows; `pull`, `status` and `sync` never touch the settings rows, and `unsync` removes both correctly, verifies **AC-13**, **AC-14**.
- OpenCode copy filter: one OpenCode database holding a native session and an agentbridge copy; `list_sessions` shows the native one only, verifies **AC-4**.
- Protocol errors: a non JSON line, a batch, a call before `initialize`; each gets the right error code and the process keeps running, verifies **AC-1**, **AC-8**.
- Symlink: the Antigravity settings file is a symlink; after install it is still a symlink and its target holds the entry, verifies **AC-10**.

## Build plan

No build approach is recorded in `AGENTS.md`, so this assumes thin end to end slices (Tracer Bullet): one working path through every layer first, then widen.

1. Thin thread: add `src/mcp.rs` with the stdio loop (`initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`, method not found) and both tools in their simplest form (`list_sessions` with no filters, `read_session` page 1 only); add the `mcp` subcommand; integration test in `tests/mcp.rs` that spawns the binary with every store variable in a temp dir, satisfies **AC-1**, **AC-9**.
2. Hand wire it into one real tool (Claude Code, `claude mcp add` by hand in a throwaway config) and call both tools once, to prove the protocol against a real client before building more, satisfies **AC-16** (first tool).
3. Finish `list_sessions`: `first_prompt` in `RawSession`/`IndexEntry` and each connector's `scan()`; filters; current folder first; copy exclusion; paging; record the `first_prompt` source per tool in `CONNECTORS.md`, satisfies **AC-2**, **AC-3**, **AC-4**.
4. Finish `read_session`: paging newest first, full tool input and output, overlay merge, id clash rule, every error as `isError`, busy timeout on locked SQLite, satisfies **AC-5**, **AC-6**, **AC-7**, **AC-8**.
5. Check, against the real Antigravity IDE and `agy`, which MCP settings file each one reads, and record it in `CONNECTORS.md` before writing the Antigravity editor, satisfies **AC-10**, **AC-16** (Antigravity).
6. Manifest kinds first: add `kind`, `entry`, `created_file` to `LinkRecord`; the new dedup key in `dedup_manifest` and the append path; a `kind` filter on `UnsyncFilter`; make every existing reader (`pull`, `status`, `sync`, loop prevention, copy detection) skip non `session` rows; regression tests for each, satisfies **AC-13**, **AC-14**.
7. `mcp install` / `mcp uninstall`: `backup_settings`; `mcp.lock`; one editor per tool (`jsonc-parser` CST for the three JSON style files, `toml_edit` for Codex), all with the read again, temp file and rename steps and the symlink rule; tool open check (new one for Claude Code); foreign entry rule; edited entry rule on uninstall; `--dry-run`; `unsync` removal code for `mcp-config` rows, satisfies **AC-10**, **AC-11**, **AC-12**, **AC-14**.
8. Migration inside install, under one held `sync.lock`: dry pull with the conflict recording resolver, stop on any conflict or error, real pull, `auto::uninstall_hook`, unsync rows of kind `session` only, then add entries; `--dry-run` prints every step, satisfies **AC-13**.
9. Deprecation note on `sync`, `pull`, `status`, `auto` (stderr, one line), satisfies **AC-15**.
10. Live check in all four real tools from a folder where the session did not start; confirm each tool starts the server in its own working folder (the current folder boost depends on it), satisfies **AC-16**.
11. Docs: README (setup is `agentbridge mcp install`; how to ask the AI for a session; that the server can read every session; Antigravity sessions show text without tool calls; rerun `agentbridge mcp install` after upgrading or moving agentbridge, because the settings hold the binary's full path), the PyPI description, `DECISIONS.md` entry, satisfies **AC-10**, **AC-11**, **AC-15**.

## Consequences

**Positive**:
- Every session is reachable from every folder in every tool, with no copies to keep in step, no shell hook and no per folder sync.
- Nothing is written into the tools' session stores any more, which removes the riskiest code paths (writing into OpenCode's and Antigravity's live databases) from everyday use.
- The PyPI description ("CLI + MCP server") becomes true.

**Negative / tradeoffs**:
- No native resume: a session never appears in a tool's own resume list. The AI starts a new chat and reads the old one in. `agentbridge resume <id> <tool>` stays for the rare true resume (it makes one copy).
- No redaction: API keys or passwords inside a session go to whichever AI provider reads it. The original design (`SPEC.md`) required redaction; it was declined for now.
- No page size limit in characters: 40 messages with full tool output can be larger than the AI's context window. A smaller `page_size` helps, but a single giant message (one long tool log) can still be too big for the client, even with `page_size: 1`, and that read fails on the client side.
- The settings hold the binary's full path, so upgrading through pipx or moving the binary needs `agentbridge mcp install` again.
- Antigravity sessions come back as text only, without tool calls, until its connector reads them.
- Recovered turns are skipped for a session id that two tools both have natively (the overlay knows the id, not the tool).
- Every `list_sessions` call rescans every store (about 130 ms on this machine today). Fine at hundreds of sessions; may need a look at tens of thousands.
- Search covers titles and the first message only, so a session about a topic mentioned only later in it will not be found by `query`.
- agentbridge now edits four settings files it does not own, each with its own format and each able to change with a tool update.

**Neutral**:
- Two new crate dependencies (`toml_edit`, `jsonc-parser`).
- `LinkRecord` gains `kind`, `entry` and `created_file`; old manifests parse unchanged.
- Code is split so each part is tested alone: protocol loop, the two tools, settings editors, migration.
- `sync`, `pull`, `status`, `auto` and their write modules are removed one release later (a follow up spec decides what of `convert.rs` and the `*_write.rs` modules `resume` still needs).

## Follow-up

- [ ] Next version: let you choose per read between the full session and a summary (a choice shown to you, as you asked), and decide whether pages then get a size limit in characters.
- [ ] Revisit redaction before this is recommended to other people; `SPEC.md` and `DESIGN.md` still require it and `src/redact.rs` does not exist.
- [ ] Teach the Antigravity connector to read tool calls, so its sessions come back complete.
- [ ] Key the overlay by tool as well as id, if same id sessions in two tools turn up in practice.
- [ ] Use the MCP `roots` request (the client tells the server its workspace folders) if any tool turns out not to start the server in the folder it was opened in.
- [ ] Spec for removing `sync`, `pull`, `status`, `auto` in the release after this one.
- [ ] Your OpenCode database is 5.1 GB; check how much of that is rows agentbridge wrote and whether `unsync` during install shrinks it (SQLite needs a `VACUUM` to give the space back).
- [ ] Run `/sync` after this ships to update `AGENTS.md` (the "keeps no copy" rule becomes "writes no copy", and the module map gains `mcp.rs`).
