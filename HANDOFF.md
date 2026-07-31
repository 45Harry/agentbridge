# HANDOFF.md

Session continuity note. If you're picking this project up on a different
machine (new Claude Code / Codex / etc. session with no memory of the prior
conversation), read this file first, then `SPEC.md` (the original build
brief), then `DECISIONS.md` (choices already locked in), then `CONNECTORS.md`
(reverse-engineered provider formats).

**Last updated:** 2026-07-30.

---

## 0. Getting started on a new machine

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build              # should build clean
cargo test               # 19 tests, all pass
cargo clippy             # style nits only, no errors
cargo run -- info        # shows detected connectors on this machine
```

Requires Rust edition 2024 toolchain (Rust 1.85+; repo was built with
1.97.0).

## 1. Where things stand

**M1 complete + cross-tool resume working.** The connectors detect real
sessions and can convert sessions between Claude Code and Codex CLI formats
bidirectionally.

## 2. Architecture map

```
src/
  model.rs               Data model: Project, RawSession, Session, Message,
                         Artifact, Fact, Provenance
  connector.rs           Connector trait (id/detect/roots/scan/load/
                         resume_cmd/inject), Registry, ConnectorError,
                         InjectTarget, SessionStream
  connectors/
    mod.rs               Registration + test helpers (all_for_testing)
    claude_code.rs       Claude Code connector (real + fixture formats)
    codex_cli.rs         Codex CLI connector (real + fixture formats)
  convert.rs             Cross-tool session converters + brief builder
  inject.rs              Brief injection helpers (start/inject commands)
  main.rs                CLI: ls, index, resume, start, inject, info
tests/
  fixtures/
    claude-code/         10 synthetic fixtures (realistic but hand-authored)
    codex-cli/           10 synthetic fixtures in date-partitioned layout
```

### Connector formats handled

- **Claude Code** (`~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`):
  skips `permission-mode`/`file-history-snapshot` records; parses `user`,
  `assistant`, `tool_use`, `tool_result` event types with nested
  `message.role`/`message.content` (string or array-of-blocks). Skips
  `isMeta` records. Handles RFC3339 + epoch timestamps.
- **Codex CLI** (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`):
  `session_meta` first record (metadata in `payload`), then `event_msg`
  (user messages in `payload.message`), `response_item` (assistant content
  in `payload.content`), `tool_use`/`tool_result` (in `payload`). Skips
  `world_state`/`turn_context`. Also parses the older flat fixture format
  for backward compat with tests.

Both connectors tolerate: truncated final line, non-UTF-8 bytes, empty
files, paths with hyphens/spaces, integer epoch timestamps.

## 3. CLI usage

| Command | What it does |
|---|---|
| `agentbridge info` | Show connectors + detected roots |
| `agentbridge ls [--project <p>] [--provider <p>]` | List sessions from all providers |
| `agentbridge index [--provider <p>]` | Load all sessions, print message counts |
| `agentbridge resume <session-id> <target-provider> [--dry-run]` | **Cross-tool session copy** — converts session and writes it into the target tool's storage dir |
| `agentbridge start <provider> [--dry-run]` | Inject cross-tool brief into a provider |
| `agentbridge inject <provider> <session-ids...> [--dry-run]` | Inject specific sessions |

### Verified working (2026-07-30)

```
$ cargo run -- ls
[Claude Code]
  7adbc643-e0bd-4c49-8432-6ef37c9001fd | /home/harry/Documents
  fc6ddb7b-02b2-47c3-9dce-dc873ede46db | /home/harry/Documents/Mantra/apf-digital-border-ai
[Codex CLI]
  019fb0ce-8d89-7e82-9d2a-8639d3a57afa | /home/harry

$ cargo run -- resume 019fb0ce-8d89-7e82-9d2a-8639d3a57afa claude-code
✓ Session copied → /home/harry/.claude/projects/-home-harry/019fb0ce-....jsonl
# now `claude --resume 019fb0ce-...` works from Claude Code

$ cargo run -- resume 7adbc643-e0bd-4c49-8432-6ef37c9001fd codex-cli
✓ Session copied → /home/harry/.codex/sessions/2026/07/30/rollout-...jsonl
# now `codex resume 7adbc643-...` works from Codex CLI
```

## 4. What's pending (next session's work)

In priority order:

1. **Redaction pass** — `src/redact.rs` does not exist yet. `SPEC.md` §3
   hard constraint: must run over every extracted snippet before it is
   stored or sent anywhere. Default rules: AWS keys, bearer tokens, private
   key blocks, `sk-` keys, connection strings with creds. **Fail closed.**
   Needs positive/negative unit tests + must-fail safety tests (§7).

2. **OpenCode connector (M2)** — storage verified in `CONNECTORS.md` §3:
   SQLite at `~/.local/share/opencode/opencode.db`, `session` table with
   plain `directory` column, IDs `ses_...`. Connector not yet implemented;
   `OpenCodeConverter::convert` returns "not yet implemented".

3. **Injection target resolution** — `ClaudeCodeConnector::inject()` and
   `CodexCliConnector::inject()` return errors (cannot determine the target
   file yet). Needs: resolve active project dir → `CLAUDE.md` (or Codex's
   equivalent) → write fenced block with begin/end markers → `clean`
   subcommand to remove exactly the fence. Dry-run paths exist in the CLI.

4. **Persistent SQLite storage** — no `schema_version` table, no
   `migrations/`, no DB writes. `index` currently only loads and prints.
   Decision locked in `DECISIONS.md`: forward-only migrations, FTS5 via
   triggers, own data dir via `AGENTBRIDGE_DATA_DIR`.

5. **agy connector (M2)** — storage location still unknown; needs research
   spike (see `DECISIONS.md` last entry + `CONNECTORS.md` §4).

6. **M3 distillation** — `brief` command with extractive pass (zero LLM),
   optional `--llm` abstractive pass, hard token budget, provenance on
   every fact.

## 5. Key findings (full detail in CONNECTORS.md)

- **Claude Code**: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`. Directory
  encoding is lossy — always read `cwd` from inside records.
- **Codex CLI**: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`,
  date-partitioned. `history.jsonl` + two `.sqlite` files in `~/.codex/` are
  still uncharacterized.
- **OpenCode**: SQLite at `~/.local/share/opencode/opencode.db` — the odd
  one out (not JSONL). Plain unencoded cwd in `directory` column.
- **agy**: storage location unknown — blocks M2.
- **Native cross-tool resume does NOT work** — each tool validates session
  IDs against its own storage only. That's why `agentbridge resume` does
  format conversion + file placement instead of shelling out to foreign
  `--resume` flags. Do not attempt literal session transfer; M4 brief
  injection + M5 copy-shim are the spec'd approaches.

## 6. Repo hygiene

- Repo: **public**, `https://github.com/45Harry/agentbridge`, branch `master`.
- `.gitignore` excludes `/target` only — don't commit local session data.
- Commit at each milestone per `SPEC.md` §6.
- 19 tests currently pass; keep them green before committing.
