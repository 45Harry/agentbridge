# HANDOFF.md

Pick this project up cold — new machine, new session, no memory of prior
conversation. Read this, then `DESIGN.md` (architecture and why), then
`CONNECTORS.md` (each tool's on-disk format).

**Last updated:** 2026-08-01.

## 0. Start here

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build && cargo test      # 63 tests pass (+2 ignored: live-verification suites)
cargo run -- init              # read-only: what's on this machine
```

Requires Rust edition 2024. Binary version is 0.2.0.

## 1. What this is

One session layer for every agent tool on a machine. Every tool scopes its
session list to the current directory and can't read other tools' sessions, so
most of your history is invisible from wherever you're standing. agentbridge
makes every session available in every tool: create a session anywhere, it
appears in every other tool's store; work on it anywhere, the new turns flow
back and are folded into the other copies.

The operator's words: start a "python programming" session in Claude, open
Codex anywhere on the box, continue where Claude left off.

Delivered as a **write-back sync loop**, verified live on the operator's
machine: `init` once + `auto install` (shell hook) + `auto watch` (daemon).
New sessions are picked up and propagated within ~15–30s, turns appended in
one tool are pulled into the overlay and republished. See DECISIONS.md
(2026-08-01) for why native *picker listing* was dropped as a requirement.

## 2. Current state — verified live 2026-08-01

**On the operator's machine, the loop is running and proven:**

- `auto watch` (PID noted in §7) re-scans every 15s; `init` reports 50
  sessions across 4 tools (Claude Code, Codex CLI, OpenCode, Antigravity CLI).
- A fake Codex session dropped into `~/.codex/sessions/` surfaced as a
  UUIDv5 claude artifact in `~/.claude/projects/<encoded-dir>/` within 35s,
  then cleaned up.
- Antigravity: read connector verified against the two real CLI conversation
  databases — both load (user text at `.19.2`, model errors at `.24.3.1`).
- Write-back: `sync` pulls turns appended in any tool into the overlay and
  republishes (WRITE-BACK-OK markers verified in the claude copies).
- OpenCode write path proven against the real database (sessions created by
  `resume` appear in OpenCode's own picker; refusal-to-write-while-running
  confirmed on a live machine).

**Known limits:**

- OpenCode write-back is gated while OpenCode itself is running (`PID` check);
  `pull` still reads it, so recovery works, republish into opencode.db waits.
- Antigravity write connector + successful model-response text mapping (only
  error text at `.24.3.1` is mapped) — deferred until the CLI's model quota
  resets. Read-only is safe by construction: no `live_root`/converter for
  antigravity in sync.rs, so sync never writes into it.
- `start`/`inject` (cross-tool brief injection) exist; the brief builder is
  unit-tested, live-agent launch verification is light.
- Redaction (`SPEC.md` §3) not implemented.

## 3. Architecture in one page

Full detail in `DESIGN.md`; the three rules that matter:

1. **Never copy a session body.** The source files are the store; agentbridge
   keeps an index pointing at them.
2. **One derived artifact per (session, target format)**, content in
   `~/.agentbridge/cache`. Formats genuinely differ, so some derived bytes
   are unavoidable — but exactly one copy is.
3. **Directory presence via hardlink**, not copy. Same inode, zero extra
   bytes. Refreshing the cache artifact updates every directory at once.

Write-back: a tool's own files are never modified; turns appended to a
materialized session are recovered into an append-only **overlay**
(`~/.agentbridge/overlay/<session>.jsonl`) and folded into other tools'
copies on the next sync. The manifest (`~/.agentbridge/manifest.jsonl`) maps
source sessions to every destination (id, provider, cache artifact, counts)
and is the single source of truth for `status`/`unsync`.

```
src/
  main.rs       CLI (clap): ls, index, init, sync, pull, auto, status, unsync,
                resume, inject, start, info
  lib.rs        module root
  index.rs      discovery — metadata only, bodies stay in source files
  sync.rs       cache, hardlink fan-out, manifest, pull_back, status, unsync
  convert.rs    native-format writers (Claude Code, Codex) + brief builder
  opencode_write.rs  SQLite write path for OpenCode (backup, tags, PID guard)
  auto.rs       fingerprint + watch loop (WAL-aware), shell-hook install
  connectors/   per-tool readers; mod.rs is the single registration point
    claude_code.rs  codex_cli.rs  opencode.rs  antigravity.rs
  model.rs      Project / Session / Message / Artifact / Fact
  connector.rs  the Connector trait every provider implements
```

## 4. Hard-won lessons — read before changing anything

**Unit tests passing means nothing here.** Three separate times a feature was
"verified" by green tests and was completely broken against the real binaries.
Every format change must be checked by running the actual tool — and since
2026-07-31, against the real databases on this machine (the ignored
`test_load_real_*` tests exist for exactly this).

- The very first converter emitted an invented JSONL schema that neither tool
  accepted. Tests now assert the real on-disk contract (`CONNECTORS.md` §6).
- **Missing trailing newline**: generated JSONL didn't end with `\n`, so a
  tool appending its first record concatenated onto our last line and
  corrupted the session.
- **Self-truncation**: re-linking a destination already sharing the source's
  inode fell through to `fs::copy`, which truncates the destination *before*
  reading the source. A 240-record session became one line.
- **Non-determinism breaks everything**: random v4 ids and `Utc::now()` in
  filenames meant every sync minted new paths and duplicated sessions. Ids are
  UUID v5 of the source id; Codex rollout paths derive from the session's own
  start time.
- **Sync fed on its own output** — files it wrote were rediscovered as new
  sessions and re-materialized, multiplying every run. The manifest now marks
  generated files as never-a-source.
- **WAL writes don't touch the .db mtime** — a fingerprint that only stats
  the db missed live OpenCode writes. Fingerprint now stats `<db>-wal` and
  `<db>-shm` siblings too.
- **Manifest must not duplicate dests** — two source sessions with the same
  id (a genuine Codex rollout plus its claude copy) materialize to one dest;
  append_manifest keeps only the last row per dest, and resyncs update
  `message_count` on hardlink-refreshed copies.
- **Hand-rolled protobuf readers**: `proto_varint` returns the *end* offset,
  not a length — using `i += n` instead of `i = n` silently desynced the
  antigravity step walker (caught by the synthetic fixture asserting exact
  field values, then verified against real DBs).

**Never `rm -rf ~/.agentbridge` — always `agentbridge unsync`.** Deleting the
manifest orphans generated files, and without it agentbridge cannot tell its
own output from a real session. This actually happened and polluted a real
`~/.codex`. A durable marker inside generated files would remove the footgun —
still to do.

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
  Run it **from the session's own directory** — resume is cwd-scoped.
- Codex: `codex delete <id> --force` → `Deleted session` means recognized;
  `Error: failed to delete session` means not. (Destructive — synthetic files
  only.)
- Wrap CLI probes in a timeout; macOS has no GNU `timeout`.

## 5. Running the loop on a real machine

```bash
agentbridge init                     # read-only discovery
agentbridge auto install             # shell hook in ~/.bashrc
# daemon (survives logout):
setsid nohup agentbridge auto watch --project /home/harry/Documents/agentbridge \
  --interval 15 > ~/.agentbridge/watch.log 2>&1 < /dev/null &
```

After code changes: `cargo install --path .` (replaces the binary), kill the
old watch, restart. First pass after restart re-syncs and re-pulls.

`agentbridge status` shows per-session drift: `wrote/on-disk` counts. `1 with
new turns to pull` is normal while OpenCode runs (it keeps appending; pull
reads it fine).

## 6. Next steps, in order

1. **Durable marker in generated files** so orphans can never be mistaken for
   real sessions (the §4 footgun).
2. **Antigravity write path + model-text mapping** — post-quota: map
   successful type-17 response text (error text is `.24.3.1`; success location
   unknown), then materialize foreign sessions into
   `~/.gemini/antigravity-cli/conversations/`. Re-read `CONNECTORS.md` §7
   before starting; the embedded Cortex protos in
   `/opt/Antigravity/resources/bin/language_server` may map payloads faster
   than black-box probing.
3. **Redaction** (`src/redact.rs`, doesn't exist) — `SPEC.md` §3 requires it
   before anything is written or sent. Fail closed.
4. **Kilo Code / other connectors** — as requested, on their own
   `CONNECTORS.md` sections with verified formats first.
5. **Live verification of `start`/`inject`** against a real agent launch.
6. Topic threading — grouping sessions across tools by subject rather than
   project path (`DESIGN.md` §10).

## 7. Repo hygiene

- Public: `https://github.com/45Harry/agentbridge`, branch `master`.
- Keep tests green (63 + 2 ignored); add a regression test for every bug, and
  verify format changes against the real binary before believing them.
- Never commit session data. `~/.agentbridge` is never the source of fixtures.
- Ignored tests are the real-data checks: run them explicitly after any
  connector change (`cargo test -- --ignored`) — they are cheap and catch
  exactly the class of bug §4 warns about.
- Watcher on the operator's machine: `ps aux | rg "agentbridge auto"`.
