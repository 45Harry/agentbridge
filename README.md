<p align="center">
  <img src="assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

One session layer for every AI coding agent on your machine. Start a
conversation in Claude Code, continue it in Codex or OpenCode — from any
directory. Sessions you create in one tool automatically appear in every
other tool, and the work you add anywhere is pulled back and shared everywhere.

Works today, verified live: **Claude Code, Codex CLI, OpenCode, Antigravity**.
All four are bidirectional — sessions flow in and out of every one of them.

## The problem

Every agent tool scopes its session list to the directory you launched it in,
and none of them can read another tool's sessions. Your history is real, it's
on your disk, and almost all of it is invisible from wherever you happen to be
standing.

## What agentbridge does

Indexes every agent session on the machine (`init`), then a sync loop keeps
each directory's view fresh in every tool's own format:

- **New session anywhere → visible everywhere.** Drop a session in any tool,
  agentbridge converts it once and hardlinks the result into every other
  tool's store — one physical artifact, zero extra bytes per directory.
- **Continue across tools.** `agentbridge resume <id> <tool>` opens a session
  started elsewhere in the tool of your choice.
- **Write-back.** Turns you append in one tool are recovered into an
  append-only overlay (`pull`) and folded into the other tools' copies on the
  next sync. Your original files are never modified. If a session was
  continued in *more than one* tool between pulls, `agentbridge pull` asks —
  merge everyone's new turns, keep only one tool's, or decide later
  (`--auto-merge` skips the prompt for scripts and non-interactive shells).
- **Automatic.** `agentbridge auto install` hooks your shell; `auto watch`
  re-syncs within seconds of any session change.

## Install (Linux)

Requires Rust (edition 2024). One-time toolchain, then the binary:

```bash
# 1. Install Rust (one-time, if you don't have it)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# then restart your shell, or run: source "$HOME/.cargo/env"

# 2. Install agentbridge
cargo install --git https://github.com/45Harry/agentbridge

# 3. Verify
agentbridge --version
```

Other platforms: `cargo install --git https://github.com/45Harry/agentbridge`
works wherever Rust does (macOS, Windows). For local development:
`cargo install --path .`.

## Get started

```bash
# once, at install
agentbridge init          # read-only: see what's on this machine
agentbridge auto install  # new terminals sync themselves from now on
agentbridge auto watch    # (optional) live re-sync daemon — or rely on the hook

# that's it. Sessions now flow both ways between your tools.
```

Manual driving:

```bash
cd ~/code/my-project
agentbridge sync                        # surface all sessions here, for all tools
claude --resume <id>                    # continue any session in Claude Code
codex resume <id>                       # ...or in Codex
agentbridge resume <id> opencode        # ...or in OpenCode
agentbridge status                      # what has new work since the last sync
```

### Try it safely first

`sync` writes into your tools' session stores. Every writing command takes
`--dry-run` to show the plan without touching anything:

```bash
agentbridge sync --dry-run
```

`agentbridge unsync` removes exactly what `sync` created (files verified by
inode, OpenCode rows by marker) and never deletes recovered work.

## Commands

| Command | What it does | Writes? |
| --- | --- | --- |
| `agentbridge init` | Find every agent session on this machine and index it. Zero config. | no |
| `agentbridge ls [--project P] [--provider T]` | List sessions across all providers. | no |
| `agentbridge index [--provider T]` | Index sessions from all (or one) detected provider. | no |
| `agentbridge info` | Which connectors are detected, and where they store sessions. | no |
| `agentbridge status` | Per synced file: turns agentbridge wrote vs on disk now — who has new work. | no |
| `agentbridge sync [--project DIR]` | Pull new turns first, then republish every session into every tool for that directory. | yes |
| `agentbridge pull` | Recover turns you added to a synced session in some other tool into agentbridge's overlay. | yes |
| `agentbridge resume <id> <tool>` | Materialize one specific session into one tool. | yes |
| `agentbridge inject <tool> <ids...>` | Inject session context (cross-tool brief) into a tool's startup. | yes |
| `agentbridge start <tool> [args...]` | Launch an agent with cross-tool context injected. | yes |
| `agentbridge unsync` | Remove exactly what `sync` created — never anything else. | yes |
| `agentbridge auto install` | Add a shell hook so every new terminal syncs on its own. Run once. | yes |
| `agentbridge auto uninstall` | Remove the shell hook. | yes |
| `agentbridge auto watch [--interval SECS]` | Loop that re-syncs whenever your sessions change. | yes |

## Supported tools

| Connector | Sessions live in | Read | Write |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` | yes | yes |
| Codex CLI | `~/.codex/sessions/<date>/rollout-*.jsonl` + `state_5.sqlite` | yes | yes, incl. picker rows* |
| OpenCode | `~/.local/share/opencode/opencode.db` (SQLite) | yes | yes, guarded† |
| Antigravity | `~/.gemini/antigravity-{cli,ide}/conversations/*.db` (SQLite) | yes, all stores‡ | yes, guarded† |

\* Codex's `/resume` lists from the `threads` table in `state_5.sqlite`, never
from the rollout files (its disk backfill is a one-time migration — verified).
agentbridge inserts a `threads` row per synced session so they appear in the
picker, with the same guards as OpenCode.

† OpenCode and Antigravity keep sessions in live databases. Every write backs
the database up first, tags its rows (removable by that tag alone), and refuses
to run while the tool is open. Antigravity needs both a conversation body and
an index row to be visible, so agentbridge writes both — it is the one place
agentbridge authors protobuf rather than JSON.

‡ Antigravity ships several surfaces (CLI, IDE) that each keep a separate store
under `~/.gemini/`; all are scanned. Some conversations are stored as encrypted
`.pb` files with no available key — those are skipped rather than reported as
corrupt. See `CONNECTORS.md` §7.

## How it works

1. **Index in place.** Session bodies are never copied; the index points at
   the files already on your disk.
2. **Derive once, link many.** Each session is converted once per target
   format into `~/.agentbridge/cache`, and directories get **hardlinks** to
   it — same inode, zero extra bytes. Refreshing one artifact updates every
   directory at once.
3. **Never touch a tool's own sessions.** New turns are recovered into an
   append-only overlay agentbridge owns and folded into the other tools'
   copies. `unsync` deletes only what it created.

## Tracking one session across tools

Every synced copy is titled with the same label, so the four picker rows for
one conversation are recognizably the same conversation:

```
claude-code · My Important Session · 2026-08-19 10:00 · aaaaaaaa
└ origin tool  └ session name        └ session start    └ session id
```

- **The date is the session's own start time**, read from inside the
  transcript — not when the sync ran. It never changes, so the same
  conversation shows the same date in every tool.
- **A name the tool already has is kept verbatim.** `claude -n`, an in-session
  rename, agy's `title` column: that name is yours and is never truncated or
  reworded. Only a session with *no* name gets one derived from its opening
  message (marked with `…` when shortened).
- **Renaming a copy in any tool** is picked up by `pull` and republished, with
  the label rebuilt around your new name and the id and date preserved.
- Timestamps are UTC and the label is a pure function of the session, so
  re-syncing never reports a session as renamed just for having been synced.

## Docs

- `DESIGN.md` — architecture, the cost model, and the bugs real testing found.
- `HANDOFF.md` — pick the project up fresh on another machine.
- `CONNECTORS.md` — each tool's on-disk format, reverse-engineered, with
  "last verified" dates.
- `DECISIONS.md` — dated record of every significant choice.
- `SPEC.md` — the original build spec, verbatim.

## License

MIT — see `LICENSE`.
