# DESIGN.md — the unified session layer

**Status:** architecture agreed 2026-07-31, core write-path verified against
real binaries. Sync loop not yet implemented.

## 1. The goal, stated precisely

Every agent session that exists anywhere on this machine should be visible and
resumable from **any** agent tool, in **any** directory — using each tool's own
native UI. Open OpenCode, type `/session`, and a thread you started in Claude
Code is simply in the list. No new CLI to learn; agentbridge is plumbing.

Corollary the operator was explicit about: **do not store the same data twice.**
agentbridge indexes and links the session data already scattered across the
machine; it does not become a second copy of it.

## 2. The obstacle, measured

Every tool scopes its session picker to the current working directory. This is
not a UI default — it is how each one looks sessions up. Measured 2026-07-31:

| Tool | Scoping mechanism | Evidence |
|---|---|---|
| Claude Code | `~/.claude/projects/<encoded-cwd>/` — dir *is* the index | resume of a `/Users/harry` session from another dir → `No conversation found` |
| Codex CLI | filters rollouts by cwd (`--all` disables) | documented flag + `CONNECTORS.md` §2 |
| OpenCode | `session.directory` column filter | `session list` → 9 rows in `bankNotes-OCR`, 11 in `/Users/harry`, **147 in the DB** |

So no tool shows you the machine's sessions; each shows a slice of its own.
That slicing is the entire problem agentbridge exists to remove.

## 3. Why the naive fix is unacceptable

"Mirror everything into every tool's store for every directory" is
O(sessions × tools × directories) full copies. At today's real numbers
(147 OpenCode sessions alone, one 278 MB DB, sessions up to ~100 MB) that is
tens of GB of duplicated transcript. It also creates N copies to keep
consistent, so every later edit becomes a fan-out write.

## 4. The design: index in place, derive once, link many

Three rules, in priority order.

### Rule 1 — Never copy a session body into agentbridge

The source files **are** the store. agentbridge keeps only an index:
`(session id, provider, source path, byte offsets, timestamps, title, content
hash)`. Bodies are streamed from the original file on demand and never
duplicated into an agentbridge database. This is what "utilize the existing
session data scattered across the PC" means concretely.

Consequence: the index is small and cheap to rebuild; losing it costs nothing.
It is a cache, not a system of record.

### Rule 2 — One derived artifact per (session, target format)

A Codex session cannot appear in Claude Code without being *rewritten* into
Claude Code's schema — the formats genuinely differ (`CONNECTORS.md` §1–3), so
some derived bytes are unavoidable. But exactly one copy is:

```
~/.agentbridge/cache/<content-hash>-<target>.jsonl
```

Content-addressed, so re-running sync is idempotent and an unchanged session is
never rewritten. Generated **lazily** — a session is only converted for a target
format when that target actually needs to show it.

No conversion is needed for a session's *own* tool: it is already in the right
format, in the right place. agentbridge leaves it strictly alone.

### Rule 3 — Directory presence via hardlinks, not copies

To appear in directory D's picker, a file must exist at that tool's path for D.
That path is a **hardlink** to the single cached artifact — same inode, zero
additional bytes.

Verified 2026-07-31 with the real `claude` binary: one 19,330-byte artifact,
hardlinked into two project directories, resumed successfully from **both**
(`links=3 inode=38821113` on all three names). The same directory had returned
`No conversation found` before the link existed.

Cost model, replacing O(sessions × tools × dirs) copies:

| | bytes on disk |
|---|---|
| Naive mirror | sessions × tools × dirs × size |
| **This design** | sessions × (tools−1) × size, **once**, + one inode entry per directory |

Hardlinks require same-filesystem, which holds here (`~/.agentbridge`,
`~/.claude`, `~/.codex` are all in `$HOME`). Fallback to symlink, then copy,
when it does not.

## 5. OpenCode is the exception — it has no files to link

OpenCode stores sessions as rows in a live 278 MB SQLite DB, so Rules 2–3 do
not apply: presence means `INSERT`, and rows cannot be hardlinked. This is the
only place agentbridge writes into another tool's real data, so it is the only
place that can destroy real data. Non-negotiable safety rules:

1. Back up `opencode.db` before any write.
2. Tag every inserted row so `unsync` removes exactly what agentbridge added.
3. Refuse to write while OpenCode is running.
4. `--dry-run` prints the exact SQL first.

Deferred until the file-based loop (Claude Code + Codex) is proven end to end.

## 6. Write-back — what makes it a bridge and not an export

One-shot copying is not continuity. After any tool appends turns to a
materialized session, those turns must flow back so every other tool sees them.

Since bodies live in source files (Rule 1), "flowing back" is re-indexing, not
re-copying: sync re-reads the changed file, notes the new tail offset, and marks
dependent cached artifacts stale by content hash. Loop prevention comes from
provenance — every message records the provider and native session it came
from, so a message agentbridge materialized into tool B is never re-ingested
from B as if it originated there.

## 7. Invariants any implementation must hold

1. A session body is never stored twice by agentbridge.
2. A tool's own sessions are never modified — read-only, always.
3. Materialization is idempotent; running sync twice changes nothing.
4. Every artifact agentbridge creates is removable by `unsync`, exactly.
5. Every derived record carries provenance back to its origin.
6. Redaction (`SPEC.md` §3) runs before anything is written anywhere.

## 8. First run must require zero configuration

Installing agentbridge is the only setup step. On first run it discovers every
agent session already on the machine and maps them into one index — the
operator never registers a tool, points at a directory, or imports anything.

```
$ agentbridge init
scanning…
  Claude Code   5 sessions   ~/.claude/projects
  Codex CLI    11 sessions   ~/.codex/sessions
  OpenCode    147 sessions   ~/.local/share/opencode/opencode.db
  agy           — not detected
indexed 163 sessions across 3 tools, 24 project directories
```

Discovery proceeds in two passes:

1. **Known roots** — each connector's `detect()`/`roots()` (the `Connector`
   trait already provides this), plus the documented env overrides
   (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`). Fast, covers the
   normal install of every supported tool.
2. **Sweep for strays** — sessions scattered outside the default roots (a tool
   installed under a different `$HOME`, a relocated data dir, a copied project)
   are found by scanning for each provider's signature layout. Bounded: skips
   `node_modules`/`.git`/system dirs, respects a depth limit, and is
   incremental afterward. This is the "scattered across the entire PC" case.

Both passes are **read-only** and produce only the index (Rule 1) — first run
writes no session data anywhere. Materialization happens later and on demand,
so `init` is safe to run at any time and cheap to re-run.

"Maps them together" means the index links sessions across tools by project
path and time, so one project's history is a single ordered timeline no matter
which tool produced each part. How far that correlation goes — same project vs
same topic/thread — is the open question in §10.

## 9. Current state

Done and verified against real binaries:

- Reading Claude Code + Codex sessions (`ls`, `index`).
- Writing **correct native schema** for both, such that each tool genuinely
  loads the result. See `CONNECTORS.md` §6 — the previous implementation
  produced an invented format that neither tool accepted.
- Hardlink fan-out across directories (§4 Rule 3).
- `--project` re-homing a session into any working directory.

Not built yet: the index, content-addressed cache, lazy materialization, sync
and write-back loop, `unsync`, OpenCode/agy/Kilo connectors, redaction.

## 10. Open question — how far does "same thread" go?

§8 maps sessions together by **project path + time**, which is unambiguous and
needs no guessing. The operator's example goes further: a "python programming"
thread started in Claude, continued in Codex, possibly from a different
directory — that is a *topic* identity, not a path identity.

Path-based mapping is being built first because it is always correct. Topic
threading (naming a thread explicitly, or inferring it) sits on top of the same
index and does not change Rules 1–3. Decide it once the sync loop is proven.
