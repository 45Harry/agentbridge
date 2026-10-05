# 0001. Rationale: read sessions through an MCP server instead of copying them

## Context

> ⚠️ Premise note: you asked to reach any session from any folder "without making a single copy". A tool's own resume list can only ever show sessions stored in that tool's own format and place, so without copies there is no native resume. What can be done without copies is letting the AI read the old session in and continue from it. This spec delivers that, and keeps `agentbridge resume` for the rare case where a true native resume matters.

Each tool lists only the sessions it saved itself, and only for the folder it was opened in. Today agentbridge closes that gap by converting every session and writing a copy into every tool's store, folder by folder (`sync`), kept fresh by a shell hook and reconciled by `pull`. On this machine that means 264 tracked copies across 7 Claude Code folders, a 5.1 GB OpenCode database, and sessions that are visible only in folders where a terminal happened to run `sync`. You reported that sessions from other tools were not reachable from where you were working.

Forces:
- You want every session reachable from every folder, with nothing copied.
- The project rules already say agentbridge keeps no copy and never changes a tool's own session files; writing into live databases is the riskiest code in the repo (backups, tags, refusing while a tool is open).
- All four tools (Claude Code, Codex, OpenCode, Antigravity) support MCP servers started over stdio, configured in a settings file per tool.
- The crate is synchronous today; `DECISIONS.md` reserves `tokio` for the MCP transport and watch mode only.
- Some sessions are very large (one has 5,670 messages), so a session cannot be returned in one piece.
- No network calls and no telemetry.

## Options considered

### Option 1: MCP server written in house, settings edited directly (chosen)

A loop over stdin and stdout with `serde_json` handles the five messages needed (`initialize`, the initialized notification, `ping`, `tools/list`, `tools/call`). `mcp install` edits each tool's settings file with a format preserving editor.

**Pros**:
- No async runtime, no SDK; the whole protocol surface is about 200 lines that are easy to test with a spawned binary.
- Works offline and in a fake `HOME`, so tests never touch real settings.
- Reuses the safety gates the repo already has (backup, refuse while a tool is open, manifest, `--dry-run`).

**Cons**:
- agentbridge owns protocol upkeep; a future MCP version change is ours to follow.
- Four settings formats to edit and keep correct as tools update.

### Option 2: rmcp, the official Rust MCP SDK

**Pros**:
- Protocol details and future versions are handled upstream.
- Typed tool definitions and schemas.

**Cons**:
- Pulls in `tokio` and an async call graph into a crate that has none, for a surface of two tools.
- More compile time and binary size for every PyPI wheel.

### Option 3: register through each tool's own CLI (`claude mcp add`, `codex mcp add`, ...)

**Pros**:
- Each tool writes its own format, so a format change on their side is not our bug.

**Cons**:
- Not every tool has such a command (Antigravity has none), so file editing is needed anyway.
- Runs real tool binaries, which the project rules forbid in tests (`CODEX_HOME` does not fully isolate Codex), so the install path could not be tested safely.

### Option 4: keep copy based sync, but sync into every known folder

**Pros**:
- Native resume lists in every folder.

**Cons**:
- Multiplies copies (hundreds of sessions times every folder), the opposite of what you asked for.
- Keeps all the risk of writing into live databases.

## Rationale

You ruled out copies, which leaves reading sessions where they are, and MCP is the one mechanism all four tools share for calling an outside helper. Writing the protocol in house fits a synchronous crate and a two tool surface: `rmcp` would bring `tokio` into every code path for no feature we need now, which runs against the scoped use of async recorded in `DECISIONS.md`. Editing settings files directly is the only path that covers Antigravity and can be tested in a fake `HOME` without running real tool binaries.

You chose no redaction and full tool output with no character limit. Both are workable on your own machine, and both are recorded as follow ups, because `SPEC.md` requires redaction and a single huge tool log can exceed an AI's context window.

Choices made in this spec without a separate question, each with the runner up:
- Current folder comes from the server's working directory, with a `project` input to override. Runner up: the MCP `roots` request, which needs the server to send requests back to the client; kept as a follow up.
- Pages are numbered from the newest (`page` 1 = newest 40). Runner up: an offset from the start, which makes the common case (continue from the end) need two calls.
- Copies are recognized by the manifest and by the title label, both cheap. Runner up: `label::is_copy` on a loaded session, which needs a full load of every entry.
- MCP entries are rows in the existing manifest with a new `kind` field, so the rule "everything agentbridge creates goes in the manifest" holds and `unsync` stays a full undo. Runner up: a separate `mcp.json` state file, simpler but breaks that rule.
- The `list_sessions` index is rebuilt on every call. Runner up: an in memory cache invalidated by the existing fingerprint, deferred until a measured slowness.
- `since` is an ISO date filter rather than a date match inside `query`, so it can compare instead of matching text.
