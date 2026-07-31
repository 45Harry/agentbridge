<p align="center">
  <img src="assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

One session layer for every AI coding agent on your machine. Start a
conversation in Claude Code, keep going in Codex — from any directory, using
each tool's own session picker. No new UI to learn.

**Status: not working end to end yet.** Sessions are indexed, converted and
placed correctly, and are resumable *by id* across tools — but they do **not
yet show up in the tools' session pickers**, which is the point. Codex lists
from a SQLite index (`state_5.sqlite` → `threads`) rather than from the
rollout files, so dropped files are invisible to it. See
[Current state](#current-state).

## The problem

Every agent tool scopes its session list to the directory you launched it in,
and none of them can read another tool's sessions. Measured on one real
machine:

| `opencode session list` run from | sessions shown |
| --- | --- |
| `~/Documents/bankNotes-OCR` | 9 |
| `~/Users/harry` | 11 |
| **actually in OpenCode's database** | **148** |

Claude Code does the same thing via `~/.claude/projects/<encoded-cwd>/`, and
Codex filters rollouts by cwd. So your history is real, it's on your disk, and
almost all of it is invisible from wherever you happen to be standing.

## What agentbridge does

The intent: after `sync`, opening any tool in that directory shows every
session in its own native picker, because as far as that tool can tell they
are its own sessions. **That last step does not work yet** — see
[Current state](#current-state).

## Commands

| Command | What it does | Writes? |
| --- | --- | --- |
| `agentbridge init` | Find every agent session on this machine and index it. Zero config — no tool to register, no directory to point at. | no |
| `agentbridge status` | Per synced file: how many turns agentbridge wrote vs how many are on disk now, so you can see what has new work. | no |
| `agentbridge sync` | Surface every session in the current directory for every detected tool. Pulls new turns first, then republishes. | yes |
| `agentbridge pull` | Recover turns you added to a synced session in some other tool, into agentbridge's overlay. | yes |
| `agentbridge unsync` | Remove exactly what `sync` created — files verified by inode, OpenCode rows by marker — so it never deletes anything else. | yes |
| `agentbridge auto install` | Add a shell hook so every new terminal syncs on its own. Run once; stop thinking about syncing. | yes |
| `agentbridge auto watch` | Foreground loop that re-syncs whenever your sessions change. | yes |
| `agentbridge ls` | List sessions across all providers. | no |
| `agentbridge info` | Which connectors are detected, and where they store sessions. | no |
| `agentbridge resume <id> <tool>` | Materialize one specific session into one tool. | yes |

Every writing command (`sync`, `pull`, `unsync`, `resume`) takes `--dry-run`
to show the plan without touching anything. `sync` and `resume` take
`--project <dir>` to target a directory other than the current one; `unsync`
reverses everything recorded in the manifest regardless of directory.

### Typical use

```bash
# once, at install
agentbridge init          # see what you have (read-only)
agentbridge auto install  # new terminals sync themselves from now on

# ...that's it. Or drive it manually:
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

**The blocking gap:** Codex's `/resume` picker lists from
`~/.codex/state_5.sqlite` (table `threads`), not by scanning
`~/.codex/sessions/`. After a sync that wrote 164 rollouts, `threads` still
held only the 11 real sessions and the picker showed 1. Making sessions
*appear* requires inserting `threads` rows, the same way OpenCode requires
INSERTs. Claude Code's picker has not been checked yet.

Verified against the real `claude` and `codex` binaries — resume *by id*,
which is a different code path from listing:

- A Codex session opening and resuming inside Claude Code, and vice versa.
- The same session resumable from multiple directories, one physical copy.
- `sync` → continue a session in Claude Code → `sync` → **the new turn appears
  in the Codex copy** → `unsync` leaves the original untouched.
- Idempotency: repeated syncs create nothing new.
- OpenCode: 148 real sessions read; writes refuse while OpenCode is running
  (confirmed on a live machine — the database was left untouched).

### OpenCode is handled carefully

It is the only tool whose sessions are rows in a live database rather than
files, so it is the only place agentbridge writes into real data. Every write
backs the database up first, tags its rows in the `metadata` column (unused by
OpenCode itself), refuses to run while OpenCode is open, and is removable by
that tag alone — `unsync` cannot touch a session OpenCode authored.

Not done yet:

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
