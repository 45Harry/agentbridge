# CONNECTORS.md

Per-provider on-disk formats, as reverse-engineered by direct inspection. These
formats are undocumented by the vendors and **will drift** — every section
below carries a "last verified" line; treat anything older than a few months
as suspect and re-verify before relying on it.

This file also tracks empirical cross-tool interoperability testing (§5),
since that directly informs the scope of M4/M5.

---

## 1. Claude Code

**Last verified:** against `2.1.220 (Claude Code)`, on 2026-07-30, macOS.

- **Storage root:** `~/.claude/projects/` (override: `CLAUDE_CONFIG_DIR`, unverified — inferred from spec, not yet tested).
- **Layout:** one directory per project, named by encoding the absolute cwd
  (non-alphanumerics → `-`), e.g. `/Users/harry/Documents` →
  `-Users-harry-Documents`. Inside: one `<session-uuid>.jsonl` file per
  session.
- **Confirmed lossy encoding:** `/Users/harry/Documents` and a hypothetical
  `/Users/harry-Documents` would encode identically — confirms the spec's hard
  constraint that the project path must be read from *inside* the record
  (a `cwd` field on transcript entries), never reconstructed from the
  directory name.
- **Session ID format:** standard UUID v4-shaped string, used directly as the
  filename stem.
- **Resume:** `claude --resume <uuid>` (or `-r`), `--continue`/`-c` for most
  recent in cwd. `--resume` **validates the ID against its own local index
  before doing anything else** — fed it real session IDs belonging to Codex
  and OpenCode and got an immediate, clean rejection in both cases:
  `No conversation found with session ID: <id>` (non-UUID-shaped foreign IDs
  get a slightly different pre-check error, see §5).
- **`--fork-session`, `--session-id <uuid>`** exist alongside resume — worth
  revisiting for M5's copy-based shim (target session may need a fresh ID to
  avoid colliding with the original).

## 2. Codex CLI

**Last verified:** against `codex-cli 0.146.0`, on 2026-07-30, macOS.

- **Storage root:** `~/.codex/` (override: `CODEX_HOME`, unverified — inferred
  from spec).
- **Layout:** date-partitioned: `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO
  timestamp with `:`→`-`>-<uuid>.jsonl`, e.g.
  `sessions/2026/07/29/rollout-2026-07-29T15-04-08-019fad2b-ae9f-7350-b403-176a6ac0f1af.jsonl`.
  Filename embeds both a start timestamp and a UUID — cheap metadata (M1's
  `RawSession`) can be derived from the filename alone without opening the
  file, an optimization worth taking in the connector.
- Other files of note in `~/.codex/`: `history.jsonl` (separate from
  per-session rollouts — appears to be a flat cross-session command/prompt
  log, not yet characterized), `logs_2.sqlite` (14MB+, purpose unconfirmed),
  `goals_1.sqlite`, `auth.json`, `config.toml`. Needs a follow-up pass before
  M1 parses anything beyond the `sessions/` rollouts.
- **Resume, two paths:**
  - `codex resume [SESSION_ID] [PROMPT]` — interactive TUI. In a non-TTY
    environment this refuses immediately with `Error: stdin is not a
    terminal`, *before* any ID validation is observable — could not determine
    validation order for the TUI path.
  - `codex exec resume [SESSION_ID] [PROMPT]` — the headless/scriptable path.
    **Does not appear to validate the session ID up front.** Given a
    deliberately-invalid all-zero UUID with no prompt, it proceeded straight
    to `Reading prompt from stdin... No prompt provided via stdin.` rather
    than rejecting the ID. Whether an invalid ID is caught later (once a real
    prompt is supplied and the session is actually loaded) is **untested** —
    testing further requires sending a real prompt, which risks a live model
    call; deferred until M1 needs the answer, and then only test against a
    local/oss provider (`--oss`/`--local-provider`) to avoid API cost.
- `--all` on `resume`/`exec resume` disables cwd filtering — Codex already
  supports the cross-directory resume Claude Code lacks (per spec §1.3).

### ⛔ The rollout files are NOT the index (discovered 2026-07-31)

**Codex lists sessions from a SQLite index, not by scanning `sessions/`.**
This invalidates the assumption the whole file-copy approach rested on for
Codex, and it was only caught by the operator actually opening `/resume`.

- Index: `~/.codex/state_5.sqlite`, table **`threads`** (sqlx-migrated;
  siblings `logs_2.sqlite`, `memories_1.sqlite`, `goals_1.sqlite`).
- Columns of note: `id`, **`rollout_path`** (pointer to the `.jsonl`), `cwd`,
  `title`, `first_user_message`, `preview`, `created_at_ms`, `updated_at_ms`,
  `recency_at_ms`, `archived`, `is_pinned`, `git_branch`, `cli_version`.
- Measured on a real machine after syncing: `threads` held **11** rows — the
  operator's genuine sessions — while `~/.codex/sessions/` held **175**
  `.jsonl` files. `/resume` listed exactly one session for that cwd.

So writing a well-formed rollout into `sessions/` makes the session
*resumable by id* (`codex resume <id>` and `codex delete <id>` both resolve
it — verified) but **invisible in the picker**, because nothing inserted a
`threads` row.

To make a session appear in Codex's list, agentbridge must additionally
`INSERT` into `threads` with `rollout_path` pointing at the generated file —
i.e. Codex needs the same treatment as OpenCode (write into a live database,
with backup / marker / not-running guard), not a plain file drop.

**Open question before implementing:** whether Codex rebuilds or prunes
`threads` from disk at startup (there is a `backfill_state` table, which
hints at exactly that). If it backfills, a simpler and safer route may be to
let Codex discover the rollouts itself rather than inserting rows.

## 3. OpenCode

**Last verified:** against `1.17.15`, on 2026-07-30, macOS.

- **Storage root:** `~/.local/share/opencode/` (data) and `~/.config/opencode/`
  (config) — XDG-style split, unlike the other three which keep everything
  under one dir.
- **Format: SQLite**, not JSONL — `opencode.db` (278MB observed on a real
  machine after months of use; WAL mode, `opencode.db-wal`/`-shm` present).
  This is the one provider where the connector reads a real relational schema
  instead of parsing a transcript format by hand.
- **Relevant tables** (from `.tables`, read-only immutable connection):
  `session`, `project`, `project_directory`, `message`, `part`, `todo`,
  `permission`, `account`, `workspace`, `event`, `session_message`,
  `session_input`, `session_share`, `session_context_epoch`, plus a
  `__drizzle_migrations` table confirming the app uses the Drizzle ORM's own
  forward-migration system — useful reference point for our own migration
  approach (`DECISIONS.md`).
- **`session` schema** (columns of note): `id` (text PK, format `ses_<...>` —
  **not a UUID**, opaque alphanumeric), `project_id` (FK → `project`),
  `parent_id` (session forking/threading — maps to our `Session`/`Fact`
  parent-link concept), `directory` (**plain absolute path, stored verbatim,
  not encoded** — e.g. `/Users/harry/Documents/bankNotes-OCR` — no lossy
  encoding problem here, unlike Claude Code), `title`, `time_created`/
  `time_updated`/`time_compacting`/`time_archived` (epoch millis — confirms
  spec's "true in-file/in-db event time, not mtime" ordering rule maps
  directly to `time_updated`), `agent`, `model`, `cost`, `tokens_input`/
  `tokens_output`/`tokens_reasoning`/`tokens_cache_read`/`tokens_cache_write`
  (a full token/cost breakdown available for free — richer than what Claude
  Code or Codex expose in-band).
- **Resume:** `opencode run --session <id> "<prompt>"` (also
  `-s`/`--session`/`--continue` flags documented on the base command).
  **Validates immediately** — fed it real Claude Code and Codex session IDs,
  got a clean `Error: Session not found` both times, no side effects.
- `opencode session list` / `opencode session delete <id>` exist as first-class
  subcommands — no need to hit the SQLite file directly for the connector's
  `scan()`, though `load()` may still want direct DB access for full
  `message`/`part` bodies if the CLI's own output is lossy.

## 4. Antigravity CLI (`agy`)

**Last verified:** against `agy 1.1.8`, on 2026-07-30, macOS. **Storage
location now confirmed on Linux 2026-08-01 — see §7 for the full on-disk
format; the connector built from it is read-only and verified against real
databases.**

- `agy` is a separate Go binary (`~/.local/bin/agy`, ships its own bubbletea
  TUI) from the Antigravity IDE app (`~/.antigravity`,
  `~/Library/Application Support/Antigravity` — a full VS Code–style Electron
  app with its own `workspaceStorage`/`state.vscdb` layout). They are
  related but **not confirmed to share a session store** — do not assume the
  IDE's `workspaceStorage` is where `agy` CLI conversations live without
  verifying first.
- Flag surface is nearly a 1:1 match with Claude Code's:
  `--dangerously-skip-permissions`, `--effort <low|medium|high>`,
  `--mode <accept-edits|plan>`, `--output-format <text|json|stream-json>`,
  `-p/--print`, `-c/--continue`. Strongly suggests both are built on a common
  underlying agent-CLI framework/SDK, not independent implementations —
  worth keeping in mind if a shared parser can cover both, though the actual
  transcript format is still unverified for `agy`.
- Binary strings reference `antigravity.google/docs/cli/reference`, an
  internal `agentapi` CLI, and conversation-migration keys
  (`no_conversations_to_migrate`, `persist_destination_project`,
  `root_assigned_to_standalone`) — implies standalone CLI conversations can
  be adopted into an IDE-tracked project, which is a different mental model
  than the other three providers' flat local session stores. **M2 needs a
  dedicated research spike** (per `DECISIONS.md`) before writing a parser:
  either inspect a real conversation's storage location directly (`fs_usage`/
  `dtruss`-style tracing while a real `agy -p` run is active, or ask Google's
  published docs at the URL above) rather than guessing further.
- **Resume flag:** `--conversation <id>`. Observed behavior, not yet fully
  characterized:
  - No prompt supplied → does not appear to validate the ID before attempting
    to open its interactive TUI (failed only on `bubbletea: could not open
    TTY` in this headless environment, regardless of whether the ID was a
    deliberately-invalid all-zero UUID).
  - Prompt supplied (`-p "hi"`) with the same invalid ID → **hung for the
    entire 8s test timeout and had to be force-killed**, rather than failing
    fast the way Claude Code and OpenCode do. No corresponding log activity
    was found afterward in the Antigravity app's log directory, so it likely
    did not complete a real model call, but this was **not** confirmed with
    certainty. **Do not probe `agy --conversation` with real prompt content
    during connector development without a local/offline model configured
    (if one exists) — treat it as potentially cost-incurring until proven
    otherwise.**

## 5. Cross-tool resume interoperability (tested 2026-07-30)

> **2026-08-01 amendment — superseded by delivery.** The conclusion below —
> "none of the tools can resume a session started by a different tool" — was
> true for *unconverted* foreign IDs, and it is exactly what agentbridge now
> fixes: `resume` converts a session into the target's real on-disk schema
> (threading `sessionId`/`payload.session_id` through every record per §6),
> so a materialized session *is* native by the time the target looks it up.
> Verified live in both directions (Claude Code ↔ Codex CLI) and against
> OpenCode's real database. Read §6's original finding for the schema
> facts; ignore its "non-functional" verdict, which predates the rewritten
> writers.

Direct question: can tool B resume a session created by tool A? Tested with
real session IDs pulled from each provider's own storage on one machine with
all four installed.

| Feed → into ↓ | Claude Code | Codex CLI | OpenCode |
| --- | --- | --- | --- |
| **Claude Code's real ID** | (self) | not tested (Codex resume needs TTY / risk-gated) | `Error: Session not found` — clean, immediate |
| **Codex's real ID** | `No conversation found with session ID: ...` — clean, immediate | (self) | `Error: Session not found` — clean, immediate |
| **OpenCode's real ID** (`ses_...`, non-UUID) | `Error: --resume requires a valid session ID or session title ... is not a UUID and does not match any session title.` — clean, immediate, different message because the format itself is rejected pre-lookup | not tested | (self) |

**Conclusion: none of the four tools can resume a session started by a
different tool.** This is structural, not a missing feature — each tool's
resume is a lookup into its own local storage with its own ID format/schema
(see §1–4); there is no shared namespace to look something up in. Claude Code
and OpenCode both fail this cleanly and immediately with no side effects.
Codex's headless `exec resume` and `agy`'s `--conversation` do not visibly
validate the ID up front, which is a liveness/safety concern for M2/M5
(probing them with a foreign ID is not provably side-effect-free) but does not
change the structural conclusion.

This confirms the project's premise (`SPEC.md` §1.3, §6 M4/M5): cross-tool
continuity has to go through a distilled brief (M4), not literal session
hand-off. M5's cross-directory *resume shim* stays scoped to Claude Code
resuming its own sessions across directories — never to resuming a foreign
tool's session.

## 6. `agentbridge resume` file-copy conversion — retested and does NOT work (2026-07-31)

> **2026-08-01 amendment — superseded.** The writers were rewritten against
> the real schemas recorded below (`sessionId` threaded through every Claude
> record, `payload.session_id` + `session_meta` for Codex) and the real
> binaries then accepted the output: cross-tool resume is verified working
> and is the product's core loop (see §5's amendment). The schema facts in
> this section remain the authoritative on-disk contract for the writers —
> keep the regression tests pinned to them. Codex list-picker visibility is
> the one true remnant of this section's problem space (see §2).

Commit `cb75d98` (pulled from a different machine/session) added `src/convert.rs`
and wired `agentbridge resume <id> <target-provider>` to convert a session and
**write it directly into the target tool's own storage directory**
(`~/.claude/projects/...`, `~/.codex/sessions/...`), on the theory that the
target tool's native resume would then pick it up. That commit's `HANDOFF.md`
claimed this was "Verified working" with a transcript showing successful
`claude --resume`/`codex resume` output.

**Retested end-to-end on this machine against real binaries and it fails in
both directions**, cleanly reproducing the same rejection as a deliberately
invalid UUID:

- **Codex → Claude Code**: converted a real 16-line Codex session
  (`019efcc7-...`) into `~/.claude/projects/-Users-harry/019efcc7-....jsonl`.
  File landed in the correct directory with the correct filename. `claude
  --resume 019efcc7-...` still returned `No conversation found with session
  ID: ...` — identical to the error for an all-zero fake UUID tested
  immediately before it.
- **Claude Code → Codex CLI**: converted a real Claude Code session
  (`7a65dbea-...`) into `~/.codex/sessions/2026/07/31/rollout-...jsonl`.
  `codex delete 7a65dbea-... --force` (chosen because, unlike `resume`, it
  validates a session ID against Codex's index without any model call)
  returned `Error: failed to delete session` — identical to the error for an
  all-zero fake UUID tested immediately before it.

**Root cause**: correct file path + correct filename is not sufficient;
each tool's resume path validates the *internal record schema*, and the
converter's output doesn't match either tool's real schema:

- Real Claude Code records are shaped like
  `{"type":"mode","mode":"normal","sessionId":"<uuid>"}` /
  `{"type":"permission-mode",...,"sessionId":"<uuid>"}` as the leading
  records, with every subsequent record also carrying `sessionId`. The
  converter emits an invented `{"type":"conversation_start","uuid":...}`
  shape with no `sessionId` field anywhere.
- Real Codex CLI records lead with
  `{"type":"session_meta","payload":{"session_id":"<uuid>","cwd":...,...}}`.
  The converter emits a flat `{"id":...,"type":"conversation","cwd":...}`
  shape with no `payload` wrapper and no `session_meta` record.

Both connectors' own *readers* (`claude_code.rs`, `codex_cli.rs`) already
know the real schema — they parse it correctly for `ls`/`index`. The bug is
isolated to `convert.rs`'s *writers*, which were never checked against the
real format, only against each other (the existing tests in `convert.rs`
assert structural properties of the converter's own invented format, so they
pass regardless of whether real Claude Code/Codex would accept the output).

Test files were deleted after the check — both `~/.claude/projects/` and
`~/.codex/sessions/` are back to their pre-test state.

**Implication**: the file-copy approach is fixable in principle (mirror the
exact real schema, including `sessionId`/`payload.session_id` threaded
through every record, not just the first), but as shipped in `cb75d98` it did
not work, and the previous "Verified working" claim in `HANDOFF.md` was not
accurate. Until the writer is rewritten against the real schema and retested
against real binaries, treat `agentbridge resume` as **non-functional** —
`--dry-run` still gives useful information (source/target/message count) but
the real write should not be relied on. This does not change the project's
core conclusion in §5: native cross-tool resume doesn't exist, and even a
correctly-implemented copy-shim is inherently fragile (must track two
undocumented, drifting vendor formats exactly). It strengthens the case for
prioritizing M4 (distilled brief injection, format-agnostic) over polishing
the M5 copy-shim.

## 7. Antigravity CLI session store (confirmed Linux, 2026-08-01)

**Last verified:** against the operator's real databases on 2026-07-30/08-01,
Linux. Read connector implemented in `src/connectors/antigravity.rs`
(read-only; databases open with `SQLITE_OPEN_READ_ONLY` so a live Antigravity
process is never blocked).

- **Storage root:** `~/.gemini/antigravity-cli/` (Linux). macOS path for the
  CLI is unconfirmed — the `agy` binary and the Antigravity IDE app are
  separate installs (see §4); do not assume the IDE's `workspaceStorage`
  holds CLI conversations.
- **One SQLite database per conversation:** `conversations/<uuid>.db`.
  Tables observed: `trajectory_meta` (id, cascade_id, type=4, source=17,
  created/updated timestamps), `steps` (idx, type, status, payload),
  `gen_metadata`, `executor_metadata`, `parent_references`,
  `trajectory_metadata_blob` (single `main` row, protobuf),
  `battle_mode_infos`.
- **Metadata index:** `conversation_summaries.db` — columns
  `conversation_id`, `title`, `preview`, `step_count`, `last_modified_time`,
  `workspace_uris`, `status`, `source`, `agent_name`, `parent_conversation_id`,
  `nesting_depth`, `battle_id`, `winning_conversation_id`, `not_fully_idle`,
  `killed`, `last_user_input_time`, `last_user_input_step_index`,
  `app_data_dir`. Contains IDE sessions; the two standalone CLI conversations
  observed had **no row here** (project scoping `""`, title `NULL`).
- **Step records:** `steps` rows ordered by `idx`. `status` 3 = DONE.
  `type` 14 = user input with text, 15 = follow-up user input with **no**
  text, 17 = model turn, 98 = context/state row (skipped by the connector).
- **Payloads are Google cortex protobuf blobs**, decoded with a minimal
  hand-rolled reader in `antigravity.rs` (no dependencies; raw-slice descent,
  no string-vs-message guessing). Known offsets, verified against real DBs:

  | Path | Contents |
  | --- | --- |
  | `payload.1` | varint step type (mirrors the `steps.type` column) |
  | `payload.4` | varint status (3 = DONE) |
  | `payload.5` | StepMetadata: `.5.1.1` / `.5.1.2` = created seconds + nanos (RFC 3339 output) |
  | `payload.19.2` | user text (step type 14) |
  | `payload.19.3.1` | fallback user text (input without `2` subfield) |
  | `payload.24.3.1` | model error message (step type 17) |
  | `payload.18` (trajectory_metadata_blob `main`) | project id string (`default-cli-project`) |

- **Not yet mapped:** successful model-response text for type 17. Every real
  step 17 on record was a quota failure, so the text field's location is
  unknown — decode a successful response once the CLI's model quota resets.
  The embedded Cortex protos inside
  `/opt/Antigravity/resources/bin/language_server` may map the remaining
  payload faster than black-box probing.
- **Write connector:** pending (read-only by design). Foreign sessions are
  surfaced *into* other tools; nothing materializes into
  `conversations/<uuid>.db` yet. The real `.pb` files under the IDE's
  `~/.gemini/antigravity/conversations/` were checked and are **not**
  protobuf (wire type 7) — SQLite is the only viable surface.
