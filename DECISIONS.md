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
