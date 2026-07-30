# Build Prompt: `agentbridge` — cross-tool session & memory bridge for AI coding agents

The original build prompt this project is implemented from, preserved verbatim
for reference. Milestone status and any deviations from this spec are tracked
in `DECISIONS.md`.

---

You are building a new open-source tool called **`agentbridge`**. Work incrementally, commit at each milestone, and do not move to the next milestone until the current one's tests pass. Ask me before making any irreversible decision about scope.

## 1. Problem statement

I use several AI coding agents on the same machine and the same repos: Claude Code, Codex CLI, OpenCode, and agy. Each writes its own session transcripts and memory artifacts into its own private directory format. Three concrete pains:

1. **Context amnesia across tools.** Codex spends a session deriving the architecture of my repo and writes summaries into `~/.codex/`. I then open Claude Code in the same repo and it starts from zero, burning tokens and wall-clock time rediscovering the same facts.
2. **Context amnesia across sessions.** When a context window fills up, I restart. The prior session's conclusions exist on disk as JSONL but are not reachable as startup context.
3. **Directory-scoped resume.** Claude Code stores sessions at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` where `<encoded-cwd>` is the absolute path with non-alphanumerics replaced by `-`. Resume is hard-filtered to the current working directory, so a session started in one folder is invisible from another. Codex, Copilot CLI, Cursor agent, Goose and Amp all allow cross-directory resume; Claude Code does not.

## 2. What `agentbridge` is (and is not)

**Is:** a local-first CLI + MCP server that (a) indexes the session transcripts and memory files that my existing agents *already write*, (b) distills them into a portable, compact project brief, and (c) injects that brief into whichever agent I start next — plus a cross-directory resume shim.

**Is not:**
- Not another vector-DB memory server. That space is saturated (Memorix, agentmemory, mcp-memory-service, claude-mem, mem0). Do not reimplement them. Do offer an optional export adapter that can push distilled facts into an existing MCP memory server if one is configured.
- Not a rules-file syncer. `rulesync` and `agent-rules-sync` already do that. Detect if they are installed and defer to them.
- Not a cloud service. No network calls except the LLM API used for distillation, and that must be opt-in and swappable.

## 3. Hard constraints

- **Read-only on foreign tool directories.** `agentbridge` may read `~/.codex/`, `~/.claude/`, OpenCode and agy storage. It may NEVER write, move, or delete inside them — with exactly one exception, the resume shim in Milestone 5, which must copy (never move), must be behind an explicit flag, and must be reversible via `agentbridge undo`.
- **Never parse the encoded directory name to recover a project path.** That encoding is lossy: a real path containing a hyphen is indistinguishable from a path separator. Always read the `cwd` / `project` field from inside the transcript records.
- **Crash-safe and concurrent-safe.** Other agents may be writing these files while you read them. Use streaming line-by-line reads. Tolerate truncated final lines. Open SQLite databases belonging to other tools read-only and with an immutable/URI mode so a lock held by a running agent never blocks the scan. Scan each provider in a separate task so one slow or locked provider cannot stall the others.
- **Secret hygiene.** Transcripts contain API keys, tokens, `.env` dumps. Run a redaction pass over every extracted snippet before it is stored in the index or sent to any LLM. Ship a default ruleset (AWS keys, bearer tokens, private key blocks, `sk-` prefixed keys, connection strings with credentials) and let users extend it. Redaction failures must fail closed.
- No telemetry. Ever.

## 4. Language & stack

Choose **one** of Rust or TypeScript/Node and justify the choice in `DECISIONS.md` before writing code. Bias toward whichever gives the cleanest single-binary distribution and the best streaming-JSONL and SQLite story. Requirements either way:
- Single-command install (`cargo install` / `npm i -g`).
- SQLite as the normalized store, with FTS for search.
- Stable, versioned schema with forward migrations.

## 5. Architecture

Build around a **connector interface**. Each agent is one connector. Adding a new agent must require touching exactly one new file and one registration line — no changes to core.

```
Connector:
  id()            -> stable string, e.g. "claude-code"
  detect()        -> is this agent present on this machine?
  roots()         -> directories to scan (honor env overrides: CLAUDE_CONFIG_DIR, CODEX_HOME, etc.)
  scan()          -> stream of RawSession (metadata only; cheap, no full body reads)
  load(id)        -> full normalized Session
  resume_cmd(...) -> the exact argv needed to relaunch this session, or None
  inject(brief)   -> how a startup brief is handed to this agent
```

Normalized data model (provider-agnostic):

- `Project` — canonical absolute path, git remote, git root, list of aliases (worktrees, symlinks, renamed dirs, case variants).
- `Session` — id, provider, project_id, started_at, last_event_at, model, title, token totals, source file path, plus the raw provider payload retained verbatim.
- `Message` — session_id, ordinal, role, timestamp, text, tool_name, tool_input, tool_result, and the parent link where the provider exposes one.
- `Artifact` — files touched, commands run, git branch/SHA observed.
- `Fact` — a distilled, durable claim about the project (see Milestone 3).

Ordering rule: rank sessions by the true last event timestamp found *inside* the file, not by file mtime. Agents rewrite transcripts on compaction and title-writes, so mtime lies about "when did I last work on this."

## 6. Milestones

Each milestone ends with: passing tests, a `README` section, and a commit.

**M1 — Connectors: Claude Code + Codex CLI.**
Discover, scan, normalize, and store into SQLite. `agentbridge index` and `agentbridge ls --project . --provider all`. Handle Claude's per-project JSONL layout and Codex's rollout transcripts. Cull-tolerant: if a body is missing but sidecar metadata survives, index the metadata and mark the body unavailable rather than dropping the session.

**M2 — Connectors: OpenCode + agy.** Prove the connector abstraction holds by adding two more without touching core. If core needs changes, that is a design bug — fix the abstraction, then add them.

**M3 — Distillation.** `agentbridge brief --project . [--since 7d] [--budget 2000]` produces a compact Markdown brief from all indexed sessions for that project regardless of which tool produced them. Sections: architecture & entry points, key decisions and their rationale, known-broken things and dead ends already tried, conventions observed, open threads. Requirements:
- Deterministic extractive pass first (git branches touched, files most edited, commands most run, test invocations). This must work with **zero LLM calls**.
- Optional abstractive pass over the extractive output, behind `--llm`. Model-agnostic via a provider interface. Never send un-redacted text.
- Hard token budget, enforced by measurement not estimation.
- Every fact carries a provenance pointer back to `(session_id, message_ordinal)`. A brief with unattributable claims is a bug.
- Cache briefs, keyed on the set of session ids + their last event timestamps, so re-running is free when nothing changed.

**M4 — Injection.** `agentbridge start claude|codex|opencode|agy [-- <passthrough args>]` writes the brief where that agent will read it and launches the agent. Prefer each tool's native mechanism. Injected content must be clearly fenced with begin/end markers, must never clobber hand-written user content, and `agentbridge clean` must remove it exactly.

**M5 — Cross-directory resume shim.** `agentbridge resume [--project <path>] [--all]` lists sessions across every project and provider, then relaunches. For providers with native cross-dir resume, shell out to them. For Claude Code, implement the copy-based shim: compute the target encoded path, copy (not move) the transcript and its sidecar directory, launch with `--resume`, and record the operation in an undo log. Warn the user that this touches Claude's internal storage layout and may break if that layout changes. Gate it behind a config opt-in.

**M6 — MCP server.** `agentbridge mcp` exposes a small tool surface: `search_history(query, project?, provider?, since?)`, `get_brief(project)`, `get_session(id)`, `record_fact(text, tags)`. Keep the default tool set minimal — a bloated tool list is itself a context tax. Optional export adapter to push facts into an already-configured external memory MCP server.

**M7 — Watch mode.** Optional background daemon that incrementally re-indexes on file change with debounce. Must be strictly optional; the CLI is fully functional without it.

## 7. Testing — this is not optional

I want real end-to-end coverage, not just unit tests on happy paths.

**Fixtures.** Build `tests/fixtures/<provider>/` containing realistic, *synthetic* transcripts — hand-authored, never copied from my real history. Cover for each provider: a normal multi-turn session; a session with tool calls and large tool outputs; a compacted/summarized session; a session with an embedded fake secret; a truncated final line; a legacy timestamp format (integer epoch vs ISO-8601); a path containing hyphens and spaces; a non-UTF-8 byte sequence; an empty file; a 100MB session for performance.

**Unit.** Every connector's parser against every fixture. Redaction rules with positive and negative cases. Path canonicalization including symlinks, worktrees, trailing slashes, and case-insensitive filesystems. Token budget enforcement.

**Integration.** Point the indexer at a synthetic `$HOME` built in a temp dir with all four providers' layouts populated. Assert: correct session count, correct project grouping across providers, correct chronological ordering by in-file timestamp, and that a session whose file mtime disagrees with its content sorts by content.

**End-to-end.** Script the full loop in a container against a throwaway git repo:
1. Seed provider A's storage with a fixture session that contains a distinctive, checkable fact.
2. Run `agentbridge index`.
3. Run `agentbridge brief` and assert the fact appears with correct provenance.
4. Run `agentbridge start <provider B> --dry-run` and assert the brief lands at the exact path provider B reads, correctly fenced.
5. Run `agentbridge clean` and assert byte-identical restoration of the target file.
6. Run the resume shim with `--dry-run` and assert the computed target path and argv are correct; run `undo` and assert full reversal.

**Property-based.** Round-trip: for any normalized Session, serialize → store → load must be lossless on all fields the connector claimed to support.

**Safety tests (must-fail tests).** Assert with a read-only-mounted fixture home that a full index run performs zero writes outside `agentbridge`'s own data dir. Assert that a fixture containing a fake API key never appears in the SQLite index, in any brief, or in any payload handed to the LLM provider interface (use a recording mock provider and scan its captured input).

**Concurrency.** Index while a writer process appends to a transcript and while another holds a SQLite lock. No panics, no partial-row corruption, no hangs.

**CI.** GitHub Actions matrix on Linux + macOS. Run the full suite plus a lint and a `--dry-run` smoke test on every PR. Fail the build on any test that writes outside its temp dir.

## 8. Deliverables

- Working CLI with all milestones.
- `README.md` with a 60-second quickstart and an honest comparison table against `cass`, `claude-code-history-viewer`, `Memorix`, `agentmemory`, and `rulesync`, stating plainly what `agentbridge` does *not* do.
- `DECISIONS.md` recording the language choice and each significant tradeoff.
- `CONNECTORS.md` documenting each provider's on-disk format as reverse-engineered, with a "last verified against version X on date Y" line — these formats are undocumented and will drift.
- `SECURITY.md` covering the redaction model and its known limits.
- MIT or Apache-2.0 license.

## 9. How to proceed

Start by asking me any clarifying questions. Then write `DECISIONS.md` and the connector interface and get my sign-off before implementing M1. Show me the fixture list before you write parsers — I want to review the edge cases first.
