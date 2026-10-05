# Changelog

## 0.4.0 — 2026-10-05

A large release. Read **Upgrading** first.

### Fixed (data loss and correctness)

- **Syncing could overwrite your own session.** Sync always also targets `$HOME`;
  a Claude Code session native to `$HOME` already sat at the path its copy would
  take, and was replaced by a lossy conversion of itself. agentbridge now never
  writes onto a file it did not create.
- **Real tool calls were dropped.** The Claude Code and Codex readers only
  understood the test fixtures' shape, so real tool calls read back as empty turns
  and tool results as blank user messages. Both readers now decode the real formats,
  and skip the model's private reasoning records.
- **Converted transcripts could contain unpaired tool calls**, which real tools
  reject on resume. Calls and results are now paired (by tool name, then order) and
  true orphans dropped.
- Concurrent runs (the shell hook, `auto watch`, a manual `sync`) could corrupt the
  manifest: eight simultaneous syncs left 78–148 manifest lines where 40 was right.
  A run lock and atomic writes fix it.
- `ls` reported a session's start time as its last event and missed later renames.
- Codex sessions could be listed but not loaded; a locked database stalled reads for
  5 s per session (now 250 ms); `find_session` gave up at the first bad session.

### Added

- **Secret redaction** on every copy agentbridge writes (fail closed; extendable
  with `redact.rules`; `--no-redact` for interactive commands). See `SECURITY.md`.
- **`index`, `search`, `brief`, `fact`, `mcp`**: a persistent full-text index, a
  cited project brief built with no model, and an MCP server (`search_history`,
  `get_brief`, `get_session`, `record_fact`). Optional `brief --llm-cmd` condenses
  the brief with a command you name; the answer is validated or discarded.
- **`start`, `inject`, `clean`** now work: the brief goes into `CLAUDE.md` /
  `AGENTS.md` between markers, and `clean` restores the file byte-for-byte.
- `sync --all-known [--yes]`; `unsync --orphans`; a durable marker in generated
  Claude Code / Codex files so they are recognised even if the manifest is lost.
- Antigravity tool calls (step type 132) are decoded.
- CI, `scripts/sandbox.sh` and `scripts/smoke.sh`.

### Changed

- The index is a derived, redacted, truncated copy of message text (an amendment to
  the "never copy a session body" rule; see `DESIGN.md`).
- `scan()` also reads the last 32 KiB of a session file.
- Requires Rust 1.89 or newer. Windows is not supported (the README now says so).

### Upgrading

- Run `agentbridge sync` once: copies written by older versions are rewritten with
  secrets redacted and real tool history.
- If you used an earlier version and started sessions in `$HOME`, check them; see
  the first fix above.
- Message counts for real sessions change (tool calls count, reasoning records do
  not). `pull` is unaffected.
- `start --sessions` is replaced by `--budget`.

### Known limits

See `HANDOFF.md` §2m and §2l. In short: not yet verified against a real Codex,
OpenCode or Antigravity consuming our copies, the MCP server against a real agent's
client, or `--llm-cmd` against a real model; the brief is heuristic.
