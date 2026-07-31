<p align="center">
  <img src="assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

One session layer for every AI coding agent on your machine. Start a
conversation in Claude Code, keep going in Codex — from any directory, using
each tool's own session picker. No new UI to learn.

**Status:** working for Claude Code ↔ Codex CLI, verified end to end against
the real binaries. OpenCode, agy and Kilo Code are not connected yet. See
[Current state](#current-state) for exactly what is and isn't proven.

## The problem

Every agent tool scopes its session list to the directory you launched it in,
and none of them can read another tool's sessions. Measured on one real
machine:

| `opencode session list` run from | sessions shown |
| --- | --- |
| `~/Documents/bankNotes-OCR` | 9 |
| `~/Users/harry` | 11 |
| **actually in OpenCode's database** | **147** |

Claude Code does the same thing via `~/.claude/projects/<encoded-cwd>/`, and
Codex filters rollouts by cwd. So your history is real, it's on your disk, and
almost all of it is invisible from wherever you happen to be standing.

## What agentbridge does

After `sync`, opening any tool in that directory shows every session in its
own native picker — because as far as that tool can tell, they are its own
sessions.

## Commands

| Command | What it does | Writes? |
| --- | --- | --- |
| `agentbridge init` | Find every agent session on this machine and index it. Zero config — no tool to register, no directory to point at. | no |
| `agentbridge status` | Per synced file: how many turns agentbridge wrote vs how many are on disk now, so you can see what has new work. | no |
| `agentbridge sync` | Surface every session in the current directory for every detected tool. Pulls new turns first, then republishes. | yes |
| `agentbridge pull` | Recover turns you added to a synced session in some other tool, into agentbridge's overlay. | yes |
| `agentbridge unsync` | Remove exactly the files `sync` created — verified by inode, so it never deletes anything else. | yes |
| `agentbridge ls` | List sessions across all providers. | no |
| `agentbridge info` | Which connectors are detected, and where they store sessions. | no |
| `agentbridge resume <id> <tool>` | Materialize one specific session into one tool. | yes |

Every writing command (`sync`, `pull`, `unsync`, `resume`) takes `--dry-run`
to show the plan without touching anything. `sync` and `resume` take
`--project <dir>` to target a directory other than the current one; `unsync`
reverses everything recorded in the manifest regardless of directory.

### Typical use

```bash
# once, to see what you have — read-only
agentbridge init

# in whatever project you're working in
cd ~/code/my-project
agentbridge sync

# now open any tool here; its own picker lists every session on the machine
claude          # /resume shows them
codex resume    # picker shows them

# after working in one tool, publish those turns to the others
agentbridge sync

# put the directory back exactly as it was
agentbridge unsync
```

### Try it safely first

`sync` writes into your tools' session directories. To see exactly what it
would do without touching anything:

```bash
agentbridge sync --dry-run
```

And `agentbridge unsync` reverses a real run completely.

### It doesn't duplicate your data

Session bodies are never copied into agentbridge; the index points at the
files already on your disk. Each session is converted once per target format
into a cache, and directories get **hardlinks** to it — same inode, zero extra
bytes. 32 sessions surfaced across two tools cost ~1.7 MB, not 32 transcripts.

A useful side effect: because destinations are hardlinks to one artifact,
refreshing that artifact updates every directory at once.

### Your sessions are never modified

agentbridge only ever reads a tool's own sessions. When you continue a synced
session, the new turns are recovered into an append-only overlay that
agentbridge owns and folded into the other tools' copies — the original file
is left alone. `unsync` removes exactly the files it created (verified by
inode) and never deletes recovered work.

## Install

```bash
cargo install --path .
```

Requires Rust (edition 2024). See `DECISIONS.md` for why Rust over TypeScript.

## Current state

Verified end to end against the real `claude` and `codex` binaries:

- A Codex session opening and resuming inside Claude Code, and vice versa.
- The same session resumable from multiple directories, one physical copy.
- `sync` → continue a session in Claude Code → `sync` → **the new turn appears
  in the Codex copy** → `unsync` leaves the original untouched.
- Idempotency: repeated syncs create nothing new.

Not done yet:

- **OpenCode** (the 147-session one) — it stores sessions as rows in a live
  SQLite database, so it needs `INSERT`s with backup, tagged rows and a
  dry-run. Deferred deliberately: it is the only place agentbridge would write
  into real data rather than a copy.
- **agy / Kilo Code** — agy's storage location is still unknown
  (`CONNECTORS.md` §4); Kilo has not been investigated.
- **Redaction** — `SPEC.md` §3 requires secret redaction before anything is
  written. Not implemented.
- **Interactive picker listing** — resume *by id* is proven; that the TTY
  picker lists synced sessions has not been automated.

## Documentation

- `DESIGN.md` — the architecture, the cost model, and the bugs real testing
  found. Read this to understand *why* it works the way it does.
- `HANDOFF.md` — pick the project up fresh on another machine.
- `CONNECTORS.md` — each tool's on-disk format, reverse-engineered, with
  "last verified" dates.
- `SPEC.md` — the original build spec, verbatim.
- `DECISIONS.md` — dated record of every significant choice.

## License

MIT — see `LICENSE`.
