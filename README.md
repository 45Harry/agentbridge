<p align="center">
  <img src="assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

One session layer for every AI coding agent on your machine. Start a
conversation in Claude Code, continue it in Codex or OpenCode — from any
directory. Sessions you create in one tool automatically appear in every
other tool, and the work you add anywhere is pulled back and shared everywhere.

Works today, verified live: **Claude Code, Codex CLI, OpenCode, Antigravity CLI**.

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
  next sync. Your original files are never modified.
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
| Antigravity CLI | `~/.gemini/antigravity-cli/conversations/*.db` (SQLite) | yes | pending‡ |

\* Codex's `/resume` lists from the `threads` table in `state_5.sqlite`, never
from the rollout files (its disk backfill is a one-time migration — verified).
agentbridge inserts a `threads` row per synced session so they appear in the
picker, with the same guards as OpenCode.

† OpenCode is the only tool whose sessions live in a live database. Every
write backs the database up first, tags its rows (removable by that tag alone),
and refuses to run while OpenCode is open.

‡ Antigravity is read-only: its sessions are surfaced into every other tool,
but foreign sessions are not yet materialized into it (write path deferred
until the CLI's model quota resets and the binary can be exercised live).

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

## Docs

- `DESIGN.md` — architecture, the cost model, and the bugs real testing found.
- `HANDOFF.md` — pick the project up fresh on another machine.
- `CONNECTORS.md` — each tool's on-disk format, reverse-engineered, with
  "last verified" dates.
- `DECISIONS.md` — dated record of every significant choice.
- `SPEC.md` — the original build spec, verbatim.

## License

MIT — see `LICENSE`.
