# HANDOFF.md

Pick this project up cold — new machine, new session, no memory of prior
conversation. Read this, then `DESIGN.md` (architecture and why), then
`CONNECTORS.md` (each tool's on-disk format).

**Last updated:** 2026-07-31.

## 0. Start here

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build && cargo test      # 42 tests, all pass
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

## 2. Where things stand

**Working and verified end to end against the real binaries** (Claude Code ↔
Codex CLI):

- Discovery, sync, write-back, unsync, status.
- A Codex session resuming inside Claude Code and vice versa.
- One session resumable from multiple directories, one physical copy.
- Continue a synced session in Claude Code → `sync` → the turn appears in the
  Codex copy → `unsync` leaves the original untouched.

**Not done:** OpenCode / agy / Kilo connectors, redaction, and proof that the
tools' *interactive* pickers list synced sessions (resume-by-id is proven;
driving a TTY picker isn't automated).

## 3. Architecture in one page

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

## 4. Hard-won lessons — read before changing anything

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

## 5. Next steps, in order

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

## 6. Repo hygiene

- Public: `https://github.com/45Harry/agentbridge`, branch `master`.
- Keep tests green; add a regression test for every bug, and verify format
  changes against the real binary before believing them.
- Never commit session data.
