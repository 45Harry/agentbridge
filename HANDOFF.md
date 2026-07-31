# HANDOFF.md

Pick this project up cold — new machine, new session, no memory of prior
conversation. Read this, then `DESIGN.md` (architecture and why), then
`CONNECTORS.md` (each tool's on-disk format).

**Last updated:** 2026-07-31.

## 0. Start here

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build && cargo test      # 57 tests, all pass
cargo run -- init              # read-only: what's on this machine
```

Requires Rust edition 2024.

## 1. What this is

One session layer for every agent tool on a machine. Every tool scopes its
session picker to the current directory and can't read other tools' sessions,
so most of your history is invisible from wherever you're standing.
agentbridge makes all of it visible in every tool's *own* picker.

The goal in the operator's words: start a "python programming" session in
Claude, open Codex anywhere on the box, continue where Claude left off.

## 2. ⛔ FAILED TEST — read this first (2026-07-31)

**The picker does not list synced sessions in Codex.** The operator ran the
real verification and it failed. Everything below about "working" is scoped
by this.

What was run: `agentbridge sync` in `~`, then `codex` → `/resume`.
What happened: the picker showed **1** session, not 164.

Root cause, since found and confirmed:

**Codex lists sessions from a SQLite index, not from the rollout files.**
`~/.codex/state_5.sqlite` → table `threads` (columns `id`, `rollout_path`,
`cwd`, `title`, `first_user_message`, …). Measured right after a sync:
`threads` had **11** rows (the operator's genuine sessions) while
`~/.codex/sessions/` had **175** `.jsonl` files. Nothing inserts a `threads`
row, so a dropped rollout is invisible to the picker.

Note what *does* work, and why the failure went unnoticed for so long: a
generated rollout **is** resolvable by id — `codex resume <id>` and
`codex delete <id>` both find it (verified against the real binary). That is
what earlier testing checked, and it passed. Listing and resolving are
different code paths in Codex, and only listing was ever the operator's
requirement.

**The fix**: Codex needs the same treatment as OpenCode — insert a `threads`
row with `rollout_path` pointing at the generated file, behind the same
backup / marker / not-running guard. Before building that, check the
`backfill_state` table in the same database: Codex may rebuild `threads`
from disk on start, in which case letting it discover the files is safer
than inserting rows.

**Still unverified, do not assume either way:**

- Claude Code's `/resume` picker — never checked by anyone. Claude Code's
  project *directory* is its index, so it may well work, but it may also
  keep a separate index the way Codex does. **Check this first**; it is one
  command and it decides whether the file-based approach survives at all.
- OpenCode's `/session` — writes were refused during the whole test because
  OpenCode was running, so nothing was ever inserted. The write path has unit
  tests but has never run against the real database.

**Operator's machine was left synced** (~164 generated rollouts in
`~/.codex/sessions`, ~161 files in `~/.claude/projects/-Users-harry/`,
~164 MB in `~/.agentbridge`). `agentbridge unsync` reverses all of it; the
genuine 11 Codex rollouts and the real Claude sessions are untouched.

## 3. Where things stand

**Verified against the real binaries** (Claude Code ↔ Codex CLI) — note this
is resume-*by-id*, not picker listing (see §2):

- Discovery, sync, write-back, unsync, status.
- A Codex session resuming inside Claude Code and vice versa.
- One session resumable from multiple directories, one physical copy.
- Continue a synced session in Claude Code → `sync` → the turn appears in the
  Codex copy → `unsync` leaves the original untouched.

**Not done:** picker listing for any tool (§2 — the blocking issue), agy /
Kilo connectors, redaction, and a real-database run of the OpenCode write
path.

## 4. Architecture in one page

Full detail in `DESIGN.md`; the three rules that matter:

1. **Never copy a session body.** The source files are the store; agentbridge
   keeps an index pointing at them.
2. **One derived artifact per (session, target format, directory)**, content
   in `~/.agentbridge/cache`. Formats genuinely differ, so some derived bytes
   are unavoidable — but exactly one copy is.
3. **Directory presence via hardlink**, not copy. Same inode, zero extra
   bytes. Refreshing the cache artifact updates every directory at once.

Write-back: a tool's own files are never modified, so turns appended to a
materialized session are recovered into an append-only **overlay** that
agentbridge owns (`~/.agentbridge/overlay/<session>.jsonl`) and folded into
other tools' copies on the next sync.

```
src/
  index.rs      discovery — metadata only, bodies stay in source files
  sync.rs       cache, hardlink fan-out, manifest, pull_back, status, unsync
  convert.rs    native-format writers (Claude Code, Codex) + brief builder
  connectors/   per-tool readers; mod.rs is the single registration point
  model.rs      Project / Session / Message / Artifact / Fact
  connector.rs  the Connector trait every provider implements
```

## 5. Hard-won lessons — read before changing anything

**Unit tests passing means nothing here.** Three separate times a feature was
"verified" by green tests and was completely broken against the real binaries.
Every format change must be checked by running the actual tool.

- The very first converter emitted an invented JSONL schema that neither tool
  accepted. Its tests passed because they asserted the converter's *own*
  output. Tests now assert the real on-disk contract (`CONNECTORS.md` §6).
- **Missing trailing newline**: generated JSONL didn't end with `\n`, so a
  tool appending its first record concatenated onto our last line and
  corrupted the session.
- **Self-truncation**: re-linking a destination already sharing the source's
  inode fell through to `fs::copy`, which truncates the destination *before*
  reading the source. A 240-record session became one line.
- **Non-determinism breaks everything**: random v4 ids and `Utc::now()` in
  filenames meant every sync minted new paths and duplicated sessions. Ids are
  now UUID v5 of the source id; Codex rollout paths derive from the session's
  own start time.
- **Sync used to feed on its own output** — files it wrote were rediscovered
  as new sessions and re-materialized, multiplying every run. The manifest now
  marks generated files as never-a-source.

**Never `rm -rf ~/.agentbridge` — always `agentbridge unsync`.** Deleting the
manifest orphans generated files, and without it agentbridge cannot tell its
own output from a real session. This actually happened and polluted a real
`~/.codex` with 16 stray rollouts, which then produced duplicate index
entries. A durable marker inside generated files would remove the footgun —
still to do, and probably the next thing worth building.

**Test in a sandbox, not on real data.** Use a fake `HOME` with *copies*:

```bash
SB=/tmp/ab-sandbox
mkdir -p $SB/.claude/projects/-work-proj $SB/.codex/sessions/2026/07/29 $SB/work
cp <a real claude session>.jsonl $SB/.claude/projects/-work-proj/
cp <a real codex rollout>.jsonl $SB/.codex/sessions/2026/07/29/
cargo build     # build FIRST — HOME override breaks rustup
HOME=$SB AGENTBRIDGE_DATA_DIR=$SB/.agentbridge ./target/debug/agentbridge sync --project $SB/work
```

**Zero-cost signals for checking a tool accepted a session** (no model call,
no cost):

- Claude Code: `claude --resume <id>` → `No conversation found` means
  rejected; `No deferred tool marker found…` means it loaded the session.
  Run it **from the session's own directory** — resume is cwd-scoped, and
  testing from the wrong directory makes a genuine session look rejected.
- Codex: `codex delete <id> --force` → `Deleted session` means recognized;
  `Error: failed to delete session` means not. (Destructive — synthetic files
  only.)
- Wrap CLI probes in a timeout; macOS has no GNU `timeout`, and `agy` once
  hung for 8s on an invalid id.

## 6. Next steps, in order

0. **Fix picker listing — this is the whole product.** Check Claude Code's
   picker first (one command), then implement Codex `threads` insertion per
   §2, then actually run the OpenCode write path with OpenCode quit. Until a
   picker lists a foreign session, nothing else matters.

1. **Durable marker in generated files** so orphans can never be mistaken for
   real sessions (see the footgun above).
2. **OpenCode connector** — storage fully mapped in `CONNECTORS.md` §3
   (SQLite, `session` table, plain `directory` column, `ses_…` ids). It is the
   only tool where materializing means `INSERT`ing into real data, so it needs
   backup-before-write, tagged rows, refusal while OpenCode is running, and
   `--dry-run` printing the SQL. Deferred deliberately.
3. **Redaction** (`src/redact.rs`, doesn't exist) — `SPEC.md` §3 requires it
   before anything is written or sent. Fail closed.
4. **agy connector** — storage location still unknown; needs a tracing spike
   (`CONNECTORS.md` §4). **Kilo Code** — not investigated at all.
5. **Verify the interactive pickers** actually list synced sessions.
6. Topic threading — grouping sessions across tools by subject rather than
   project path (`DESIGN.md` §10).

## 7. Repo hygiene

- Public: `https://github.com/45Harry/agentbridge`, branch `master`.
- Keep tests green; add a regression test for every bug, and verify format
  changes against the real binary before believing them.
- Never commit session data.
