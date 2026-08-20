# DECISIONS.md

Record of significant, hard-to-reverse choices for `agentbridge`. Append new entries;
never silently rewrite history here — if a decision changes, add a new dated entry
that supersedes the old one and say so explicitly.

---

## 2026-07-30 — Language: Rust

**Choice:** Rust, single binary, `cargo install agentbridge`.

**Alternatives considered:** TypeScript/Node (`npm i -g`).

**Rationale:**

- **Distribution.** The spec's explicit bias is "cleanest single-binary distribution."
  A Rust binary has zero runtime dependency — no Node version to match, no
  `node_modules` to go stale. `cargo install` (and later prebuilt release binaries)
  gives a true single artifact. A Node CLI still requires a compatible Node on `PATH`
  and either a global npm install or a bundler (pkg/nexe) with its own edge cases.
- **Concurrent-safe reads of foreign SQLite/JSONL.** Hard constraint #3 requires
  opening other tools' SQLite databases read-only/immutable while they may be
  actively written by a running agent, and streaming very large (100MB+) JSONL files
  line-by-line without loading them fully into memory. Rust's ownership model plus
  `rusqlite`'s explicit `OpenFlags` (read-only, immutable, no mutex) makes the
  "never block, never corrupt" guarantee easier to state and test than in Node,
  where `better-sqlite3` is synchronous-only and WASM SQLite builds complicate
  immutable/URI mode.
- **FTS story.** SQLite's built-in FTS5 extension is available from Rust via
  `rusqlite` with the `bundled` feature (statically links its own libsqlite3, so we
  are never at the mercy of a system SQLite lacking FTS5). No second search engine
  (e.g. Tantivy) is needed at this project's scale — deferred, see below.
- **Streaming JSONL.** `serde_json::Deserializer::from_reader(...).into_iter()` gives
  a natural streaming row-by-row parse over a `BufReader`, which is what "never load
  a 100MB transcript fully into memory" and "tolerate a truncated final line" need.

**Costs accepted:**

- Slower iteration than TypeScript for a first-time contributor; more upfront types.
- Async story (needed for M7 watch mode and possibly the MCP server transport) adds
  a runtime dependency (`tokio`) — scoped to only the crates/binaries that need it.
- Fewer existing "provider format" examples to crib from in Rust vs. the JS agent
  ecosystem; connectors are written from scratch against reverse-engineered formats
  either way (see `CONNECTORS.md`), so this cost is mostly about developer ergonomics,
  not correctness risk.

**Not chosen now, may revisit:** Tantivy (full-text engine) — SQLite FTS5 covers the
search surface (`search_history` MCP tool, `agentbridge ls`/`grep`-style queries) at
the scale of "one person's local agent transcripts." Revisit only if FTS5 query
latency becomes a measured problem.

---

## 2026-07-30 — License: MIT

Simplest permissive license, matches the stated goal of a small, inspectable local
CLI others can freely embed or fork. No patent-heavy dependency surface that would
make Apache-2.0's explicit patent grant meaningfully safer.

---

## 2026-07-30 — SQLite schema/migration approach

- Single SQLite file in the tool's own data dir (`XDG_DATA_HOME`-style, override via
  `AGENTBRIDGE_DATA_DIR`), **never** inside a foreign tool's directory (hard
  constraint: read-only on foreign dirs, one narrow exception in M5 covered
  separately).
- A `schema_version` table plus an ordered list of embedded `.sql` migration files
  (`migrations/0001_init.sql`, `0002_...sql`, ...), applied inside a transaction at
  startup, forward-only. No down-migrations — matches "stable, versioned schema with
  forward migrations" in the spec.
- Content-derived FTS5 virtual table (`messages_fts`) kept in sync via SQLite
  triggers on the `messages` table, not app-level dual-writes, so it can never drift
  out of sync with a partial write.

---

## 2026-07-30 — Connector interface: sync + streaming iterators, not async, at the core

`scan()` returns a lazy iterator of cheap `RawSession` metadata (no full-body read);
`load(id)` does the expensive full parse. Both are synchronous blocking calls
executed on worker threads (`std::thread` / a small scoped-thread pool), one thread
per provider, per hard constraint "scan each provider in a separate task so one slow
or locked provider cannot stall the others." Async (`tokio`) is reserved for M6 (MCP
server transport) and M7 (watch mode / file-change debounce), where it earns its
complexity; the core indexing path stays synchronous and easy to reason about /
easy to unit test without a runtime.

---

## 2026-07-30 — `agy` / "antigravity cli" connector deferred to a research spike

User confirmed the fourth M2 connector target is **Antigravity CLI** (Google's
agentic CLI/IDE tool, also referenced as an OAuth provider style in unrelated
projects). I do not have a verified, current description of its on-disk session
storage format. Per the spec's own reverse-engineering methodology (`CONNECTORS.md`,
"last verified against version X on date Y"), this connector will be built in M2 by:

1. Installing/inspecting a real Antigravity CLI session on a test machine (or asking
   the user to share a **redacted** sample directory listing + one sample file).
2. Documenting the discovered format in `CONNECTORS.md` before writing the parser.
3. Building synthetic fixtures from that documented format — never shipping fixtures
   copied from real user data.

This does not block M1 (Claude Code + Codex CLI), which proceeds first.

---

## 2026-08-01 — Sync loop is the product; picker-listing is dropped as a requirement

Supersedes the "everything must appear in each tool's native session picker"
goal (see DESIGN.md §1, and HANDOFF's now-archived failed-picker section). The
real verification of 2026-07-31 showed Codex's `/resume` picker lists from a
SQLite index (`state_5.sqlite` → `threads`) that no file drop can reach, and
Claude Code's picker was never automated either. Chasing each vendor's private
index is unbounded work with no durable payoff.

**What replaced it:** a write-back sync loop, verified live end to end:

- `agentbridge init` once (read-only discovery), `agentbridge auto install`
  (shell hook), then the `auto watch` loop re-syncs any new session created
  in any tool into every other tool.
- Sessions continue across tools *by id* (`resume`), which the real binaries
  accept — proven for Claude Code ↔ Codex CLI ↔ OpenCode.
- Turns appended in one tool are pulled back into an append-only overlay and
  folded into the other tools' copies (`pull` + `sync`).
- Delivery is files (hardlinked cache artifacts), not foreign index INSERTs —
  except OpenCode, where the SQLite write path carries backup-before-write,
  tagged rows, and a refuse-while-running guard.

**Accepted:** a foreign session is reachable by id in another tool's picker
only when that tool indexes by directory scan (Claude Code does; Codex CLI
does not unless a future `backfill_state` check says otherwise).

## 2026-08-01 — Antigravity CLI connector: read-only first

The research spike from 2026-07-30 completed. Antigravity CLI stores sessions
as SQLite databases under `~/.gemini/antigravity-cli/conversations/` (tables:
`trajectory_meta`, `steps`, `gen_metadata`, `executor_metadata`,
`parent_references`, `trajectory_metadata_blob`, `battle_mode_infos`), with a
`conversation_summaries.db` alongside for previews/workspace URIs. Step
payloads are Cortex protobufs decoded with a minimal hand-rolled reader (no
deps, raw-slice descent — documented in `CONNECTORS.md` §6).

**Decision:** ship the **read connector** now — antigravity sessions are
discovered as sources and materialized into every other tool, verified
against the operator's real databases. The **write connector** (materializing
foreign sessions into antigravity, and mapping successful model-response text
in type-17 steps, currently only error text at `.24.3.1` is known) waits for
the CLI's model quota to reset so the binary can be exercised live. This is
safe by construction: antigravity has no `live_root`/converter in sync.rs, so
sync never writes into it.

---

## 2026-08-01 — Codex picker listing: build the `threads` writer

The operator hit the picker gap in person (`codex /resume` showed exactly one
session). The open question in CONNECTORS.md §2 was then answered on the real
machine: `backfill_state` = 'complete' means backfill is a **one-time
migration** (it ran once, indexed nothing, never re-runs), so file drops can
never reach the picker and the "let Codex discover the files" route does not
exist.

**Decision:** implement the planned `threads` INSERT after all — the original
requirement, now with the facts to justify it. `src/codex_write.rs` (v0.3.0)
mirrors the OpenCode treatment exactly: backup before first write, rows tagged
`thread_source = 'agentbridge'`, `INSERT OR IGNORE` keyed on the deterministic
UUID v5 (genuine rows never touched), refuse-while-running guard, `unsync`
removes tagged rows only, `--dry-run` untouched. Verified against the real
binary: `codex delete <id> --force` → `Deleted session` for an inserted row.

---

## 2026-08-03 — OpenCode: one row per *project*, not per directory

Measuring cross-tool visibility from arbitrary folders (HANDOFF §2a) turned up
the fact the OpenCode write path was built on the wrong key: its picker filters
sessions by `project_id` — the `project` row whose `worktree` matches the launch
directory, or the catch-all `global` — and `session.directory` is only metadata.
agentbridge derived one id per (provider, source id) for the whole machine, so
the second directory's write hit `UNIQUE constraint failed: session.id` and was
swallowed into `report.errors`. The `$HOME` fan-out added in 0.3.x had therefore
never landed a row.

**Decision:** the unit of OpenCode materialization is the **project**, not the
directory. `derive_id` (and the message/part id namespace) hashes the project id
too; `write_sessions` resolves each requested directory to its project and
writes one row per distinct project — the minimum for visibility everywhere, and
the maximum before the same conversation is listed twice in one picker. Rows
under the pre-0.3.4 directory-independent id are reclaimed on write (they carry
the marker, so they are ours to replace).

Two consequences worth keeping in mind:

- Syncing `$HOME` covers **every** folder that is not a worktree of its own,
  because they all resolve to `global`. A git repo with its own project row is
  reached only by syncing that repo.
- Rows are not free the way Claude Code hardlinks are: each one duplicates every
  message and part row. Fanning out to all known projects is a real decision
  about database size (278 MB for 148 native sessions today), not a cheap win —
  see HANDOFF §6.1, still open.

---

## 2026-08-18 — Pull conflicts: ask the operator, native `dialoguer` prompt, no Node

Operator report: when a session is continued in more than one tool between
pulls (e.g. a turn added in Claude Code *and* a turn added in Codex before the
next `agentbridge pull`), write-back silently merged both into the overlay
with no way to say "keep only one." Two decisions:

**Prompt library: `dialoguer`, not `terminui`.** The operator's first pointer
was `github.com/ahmadawais/terminui` — checked (`gh repo view`) and it is a
TypeScript library. Pulling it in would mean shelling out to a Node runtime
for a single Select prompt, which directly contradicts the 2026-07-30 language
decision above (zero runtime dependency, true single binary). `dialoguer` is a
native Rust crate with the same `Select`/`Confirm` primitives and costs one
`[dependencies]` line. Confirmed with the operator before building (AskUserQuestion)
rather than assuming.

**Where the prompt lives: `agentbridge pull` only, not `sync`.** `sync` already
calls `pull_back` internally (to refresh materialized copies with the latest
write-back) and runs unattended from the shell hook and `auto watch` — making
it interactive would block every new terminal and the watch loop. The prompt
is scoped to the explicit, human-run `pull` command; `sync`'s internal pull and
`auto watch`'s pull keep the pre-existing `AutoMerge` behavior (merge every
tool's new work), with the conflict still surfaced in the report/log so the
operator knows to revisit it. Also confirmed via AskUserQuestion.

**Mechanism:** `sync::pull_back` groups new work by session id first; a session
with new work from exactly one tool is applied exactly as before (no prompt,
no behavior change — regression-tested explicitly). Two or more tools showing
up for the same session is a conflict, resolved through a `ConflictResolver`
trait so the merge/keep-only/skip logic in `sync.rs` has zero UI dependency —
`main.rs` supplies the real `dialoguer`-backed resolver, tests supply a
scripted one. `KeepOnly(tool)` is a permanent decision for that batch of turns
(the discarded tool's manifest record still advances, so the same turns are
never re-offered); `Skip` leaves the manifest untouched so the same conflict
returns on the next pull. Verified live end-to-end (`expect`-driven real TTY,
sandboxed `HOME`/`CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`XDG_DATA_HOME`, real
`agentbridge` binary): arrow-key-selected "keep only claude-code" against real
appended turns in both a real Claude Code JSONL copy and a real Codex rollout
copy, confirmed the discarded turn never reached the overlay and the kept turn
propagated into a freshly re-synced Codex copy afterward.

---

## 2026-08-18 (later) — Conflict screen upgraded: `ratatui`, not `dialoguer`

The operator's follow-up: "no ui i asked you to use terminui and create an
interactive terminal gui" — the `dialoguer::Select` above works but is a
plain inline list, not the terminal GUI the operator wanted. `terminui`
itself was already rejected (TypeScript; would reintroduce a Node runtime).
**`ratatui`** is terminui's native-Rust equivalent — a double-buffered
full-screen terminal UI toolkit (`crossterm` for events/raw mode), the same
class of tool. Swapped `dialoguer` to `ratatui + crossterm` (same single
`[dependencies]` footprint, no runtime outside the binary).

**Decisions carried over unchanged:** TUI only ever constructed from
`agentbridge pull` behind the same `IsTerminal` gate; `sync`/`auto watch`
stay `AutoMerge`; `Skip` is the failure fallback (never `MergeAll`); TUI code
lives in the binary (`src/tui.rs`), the library keeps only the
`ConflictResolver` trait. One relaxation: the trait's conflict payload grew
from tool names to the actual new turns per tool (`ConflictItem`), so the
TUI can show each side's content in its own panel and the operator decides
with the real bytes in front of them.

Also fixed during this pass (found live, operator report): untitled sessions
synced into Codex showed a long mid-word-cut prompt fragment as their picker
name — the untitled fallback in `codex_write::ensure_thread_rows` now uses a
short, word-boundary-safe, ellipsized name instead of the long preview clip.

---

## 2026-08-18 (same pass, later) — Bare `agentbridge` = interactive dashboard

Operator ran `agentbridge` with no subcommand and asked (AskUserQuestion):
"what should happen" — the answer was an interactive terminal GUI as the
default entry point. Bare invocation was previously an error ("missing
subcommand"); it now opens a full-screen ratatui dashboard
(`src/dashboard.rs`) whenever stdin is a terminal, and falls back to the
static help exactly when a TUI cannot render (piped/cron stdin) — a TUI is
never entered unattended.

**Scope deliberately kept shallow:** view (sessions table), `s` sync, `p`
pull, `Tab` filter. No session-level actions (resume/inject) yet — those
need sub-pickers and are the next natural layer on top, but gated on the
operator asking for them. `p` in the dashboard is `AutoMerge`-semantics on
purpose: the dashboard is already a TUI, so `tui.rs`'s conflict screen
cannot open inside it; conflicts surface in the status line with a pointer
to a real `agentbridge pull`.

**Alternatives rejected:** making missing-subcommand an implicit
`ls`/`init` (no interactivity, hides the mapping), and wiring
`terminui`-style third-party panels (already settled: native ratatui).

---

## 2026-08-19 — Antigravity write connector, and four hidden read bugs

Operator asked to verify antigravity sessions really were synced, test
write-back, and make every session reachable in both directions. Verification
against the real store first showed the connector was reporting success while
delivering almost nothing: **12 of 24 readable sessions visible, 15 of 733
messages decoded**, and zero antigravity rows in `agentbridge status`. The
existing "real data" test passed throughout, because it only asserted the
message list was non-empty — one message satisfied it.

Four read bugs, each verified on the operator's own databases:

1. **One store scanned out of four.** `ANTIGRAVITY_HOME` was a single hardcoded
   `~/.gemini/antigravity-cli`. The sibling `antigravity-ide` store holds 12
   more conversations (1,798 steps, ids non-overlapping) in the *same* SQLite
   format. All homes are now scanned, deduped by id.
2. **Model responses were never decoded.** Only the *error* field
   (`payload.24.3.1`) was read, so every session loaded as a lone user message.
   Real model text is `payload.20.1` (step type 15). `.20.3` is private
   reasoning and is deliberately *not* surfaced — presenting it as the response
   would leak chain-of-thought into every brief.
3. **`preview` was read as the title.** The schema has a real `title` column;
   reading the preview instead makes a rename invisible, which silently breaks
   title write-back.
4. **Summary timestamps were all dropped.** agy writes
   `2026-07-29 08:54:35+00:00` — a space where RFC 3339 requires `T`, so
   `parse_from_rfc3339` rejected every value.

**Decision:** reverse the 2026-08-01 "read-only first" deferral and ship the
write connector (`src/antigravity_write.rs`). The two blockers named then are
gone: model-response text is mapped (bug 2), and exercising the real binary
turned out to be unnecessary — the store is plain SQLite, so a written
conversation can be verified by reading it back through our own connector plus
agy's own index, without a model call. The `.pb` bodies in the desktop/backup
stores are **encrypted** (measured entropy 8.000/8.000), not an unmapped
format; there is no key, so they are skipped silently rather than reported as
corrupt on every scan.

**What makes antigravity different from the other three writers:** visibility
requires *both* a conversation body and a `conversation_summaries` row, and the
body's step payloads are protobuf. This is the only place agentbridge authors
protobuf rather than JSON. The encoder writes exactly the five fields the
decoder reads, so the round trip is exact for everything agentbridge models,
and agy tolerates the absence of the rest. Guards mirror
`opencode_write`/`codex_write` exactly: backup-before-first-insert, marker
column (`agent_name`, verified unused by agy in 0 of 102 rows), refuse while
agy is running, `--dry-run`. Bodies are written to a temp file and renamed so a
crash cannot hand agy a half-built database.

**Merge-back is refused for antigravity natives, unlike Claude/Codex.** A real
agy body carries protobuf fields we do not decode (tool calls, reasoning,
`gen_metadata` blobs). Rewriting one with a minimal encoder would destroy
everything outside the fields we understand, so recovered turns stay in the
overlay. This is a stronger reason than OpenCode's exclusion and is enforced at
the call site *and* backstopped in `merge_back_native`.

**Two bugs found in existing code while wiring this up:**

- `main.rs` had a live `unreachable!()` in the `resume` write dispatch. Any
  target outside the three hardcoded arms panicked on a user's machine; it now
  returns a reported error.
- `codex_cli::config_home` cached `CODEX_HOME` in a `LazyLock`, so the value
  depended on whichever code path read it first in the process and a later
  change was ignored for the rest of the run. The sibling connectors already
  re-read their env var per call; this one now does too. Surfaced as a flaky
  `test_live_root_honors_config_dir_overrides` once new sandboxed tests changed
  test ordering — the existing suite hid it by accident of ordering, which is
  exactly the failure mode HANDOFF §5 warns about.

`Sandbox` now also sets `ANTIGRAVITY_HOME`. Without it a sandboxed test would
write conversations into the operator's real `~/.gemini` store — the same
isolation gap already documented for `CODEX_HOME`.

**Verified end-to-end on a copy of the real store** (all five env vars
redirected): sync wrote 26 conversations and took an automatic backup; `status`
showed matching counts and then `+1` drift after a step was appended; `pull`
recovered both the appended turn and a rename made in agy's index; a following
`sync` propagated both into the Claude Code copy; three further passes left the
count unchanged (no feedback loop); `unsync` restored exactly 12 bodies / 102
rows with all 12 original bodies byte-identical and recovered work preserved.
The real store was confirmed untouched throughout (0 marked rows, no backups).

---

## 2026-08-19 (same pass, later) — Cross-tool session labels

Operator: "I also want the date, exact date not the new sync date, alongside
session ids, so it will be agent name + session name + date and time + session
id to track the session across multiple agents." Then, on seeing the first
draft: "don't change the session name — it already has one. Some tools' sessions
might have a session name, so keep it as it was; only to no-names add names."

The problem is real: a session materialized into four tools shows up as four
picker rows with nothing tying them together, and each tool's date column
reflects when the *copy* was written, not when the conversation happened.

**Decision:** stamp a label into the one field every tool displays — the title
— built in `src/label.rs` and applied in `sync_into` right after
`fold_overlay`:

```
claude-code · My Important Session · 2026-08-19 10:00 · aaaaaaaa
provider    · name                 · started_at       · id[..8]
```

Four constraints, three of them safety:

1. **`started_at`, never `now`.** `pull_back` compares the title it wrote
   against the title it reads back, so a label containing sync time would
   report every session as renamed on every pull — the 705-false-rename
   regression, re-armed. Verified: three sync+pull cycles, zero renames.
2. **UTC.** Local time would make the label depend on the machine's timezone,
   so syncing from a laptop that changed zones would look like a mass rename.
3. **Idempotent.** `apply` strips any label already present before building a
   new one, so a label that leaks back into a session's title is rebuilt rather
   than nested. A rename made inside a tool arrives *as a labeled title*, and
   the new label is built around the user's new name, not around the old label.
4. **An existing name is kept verbatim** (the operator's correction). Only a
   name agentbridge *derives* for an unnamed session is clipped, and that
   clipping is deterministic and word-safe. Clipping a real name would both
   lose information and, since the written title is what `pull_back` compares
   against, read as a rename. The label's metadata fields are never truncated;
   an id or date cut short defeats the whole point.

**Why `provider` and `id` are the origin's, not the target's:** `sync_into`
re-homes `project_id` per target but deliberately leaves `provider`/`id` alone,
which is exactly what makes one session produce one identical label in all four
tools. Locked by a test asserting the label set for one session has size 1.

`parse` requires all four fields to be well formed — a known provider id, a
structurally valid stamp, an id of the right length — so a user's own title is
never mistaken for a label and mangled. Verified against titles that merely
contain the separator, an unknown provider, a malformed stamp, and a short id.

**Id validation had to accept more than hex.** Session ids are not always
UUIDs: Claude Code derives one from the filename stem, so a real id can be
`renamed-in-claude-code`, and even a UUID's first 8 characters can contain `-`.
The initial alphanumeric-only check rejected those labels, and `pull_back` then
saw agentbridge's own label as a foreign rename. Caught by an existing sync
test, now covered directly.

**Bug found while wiring this up (in the antigravity write path shipped
earlier the same day):** the antigravity branch always *appended* its manifest
rows instead of updating them, so every re-sync added another row per
conversation — 52 → 78 → 104 across three runs. `pull` then read one session as
several tools' worth of new work and reported a conflict against itself
("antigravity+antigravity+antigravity -> merged"). The file targets already
handled this in their `unchanged` branch; the antigravity branch now does the
same. Regression test asserts one manifest row per (session, dest) and a stable
count across re-syncs.

Verified on a copy of the real store: a named session kept "My Important
Session" exactly; unnamed agy conversations got word-safe derived names; every
label carried the session's real date (2026-08-18/19), never the sync date; the
same label appeared in both the agy index and the Claude Code copy; a rename
made inside agy was recovered bare (not labeled) into the overlay and
republished with the label rebuilt around it. Tests 128 → 143.
