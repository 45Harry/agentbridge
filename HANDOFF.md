# HANDOFF.md

Session continuity note. If you're picking this project up on a different
machine (new Claude Code / Codex / etc. session with no memory of the prior
conversation), read this file first, then `SPEC.md` (the original build
brief), then `DECISIONS.md` (choices already locked in), then `CONNECTORS.md`
(reverse-engineered provider formats).

**Last updated:** 2026-07-30.

## Where things stand

**M1 complete + cross-tool resume working.** The connectors detect real
sessions on this machine and can convert sessions between Claude Code and
Codex CLI formats bidirectionally.

### What exists

- **Connectors (2):** `src/connectors/claude_code.rs` and
  `src/connectors/codex_cli.rs` — both handle real on-disk formats
  (Claude Code's `permission-mode`/`user`/`assistant`/`tool_use`/`tool_result`
  records, Codex CLI's `session_meta`/`event_msg`/`response_item` records)
  as well as the synthetic test fixtures. Handle truncated files,
  non-UTF-8 bytes, empty files, legacy epoch timestamps, paths with
  hyphens/spaces.

- **Cross-tool session conversion:** `src/convert.rs` — converts normalized
  `Session` into any provider's format. `ClaudeCodeConverter` and
  `CodexCliConverter` work (tested on real sessions). Placeholder
  `OpenCodeConverter` for SQLite.

- **CLI commands:**
  - `agentbridge info` — show detected connectors and their roots
  - `agentbridge ls` — list sessions from all providers
  - `agentbridge index` — load all sessions (with message counts)
  - `agentbridge resume <session-id> <target-provider>` — copy session
    to another provider's format on disk
  - `agentbridge start <provider>` — inject cross-tool brief (dry-run works)
  - `agentbridge inject <provider> <session-ids>` — inject specific sessions

- **Test fixtures:** `tests/fixtures/claude-code/` (10 fixtures),
  `tests/fixtures/codex-cli/` (10 fixtures in date-partitioned layout).

- **Data model:** `src/model.rs` — `Project`, `RawSession`, `Session`,
  `Message`, `Artifact`, `Fact`, `Provenance`.

- **Connector trait:** `src/connector.rs` — `Connector`, `Registry`,
  `ConnectorError`, `InjectTarget`, `SessionStream`.

- **19 tests pass,** `cargo clippy` clean (style nits only), builds in
  debug and release.

### Verified on this machine (real data)

```
$ cargo run -- ls
[Claude Code]
  7adbc643-e0bd-4c49-8432-6ef37c9001fd | /home/harry/Documents
  fc6ddb7b-02b2-47c3-9dce-dc873ede46db | /home/harry/Documents/Mantra/apf-digital-border-ai
[Codex CLI]
  019fb0ce-8d89-7e82-9d2a-8639d3a57afa | /home/harry

$ cargo run -- resume 019fb0ce-8d89-7e82-9d2a-8639d3a57afa claude-code
✓ Session copied → /home/harry/.claude/projects/-home-harry/019fb0ce-...

$ cargo run -- resume 7adbc643-e0bd-4c49-8432-6ef37c9001fd codex-cli
✓ Session copied → /home/harrow/.codex/sessions/2026/07/30/rollout-...
```

### What's missing / pending

1. **Redaction pass** (`src/redact.rs`) — must exist and run before anything
   touches permanent storage. Ship default rules (AWS keys, bearer tokens,
   `sk-` keys, connection strings). Fail closed.

2. **OpenCode connector** — SQLite format detected in `~/.local/share/opencode/`,
   but connector not yet implemented. `OpenCodeConverter` returns "not yet
   implemented" for now.

3. **Injection target resolution** — `connector.inject()` for Claude Code
   returns an error because it can't determine the active project directory
   yet. Codex CLI `inject()` also not implemented.

4. **SQLite storage** — no `schema_version` table, no `migrations/`,
   no persistent index. `agentbridge index` currently just loads and
   prints.

5. **M2 connectors:** OpenCode + agy (agy storage location still unknown —
   needs research spike per `DECISIONS.md`).

### Key findings (full detail in CONNECTORS.md)

- **Claude Code**: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`. Directory
  encoding is confirmed lossy — always read `cwd` from inside records. Real
  format uses nested `message` objects with `role`/`content` fields.
- **Codex CLI**: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`,
  date-partitioned. Real format uses `session_meta` first record with
  nested `payload`, then `event_msg`/`response_item` records.
- **OpenCode**: SQLite at `~/.local/share/opencode/opencode.db` — not JSONL.
  `session` table stores plain (unencoded) cwd in `directory` column.
- **Cross-tool resume works via format conversion** — `agentbridge resume`
  converts sessions between provider formats. Native `--resume` still only
  works within same tool due to ID validation, but the converted file is
  placed where the target tool reads it, so `claude --resume <id>` works
  after conversion.
