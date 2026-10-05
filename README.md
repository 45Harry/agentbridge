<p align="center">
  <img src="assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

**One session layer for every AI coding agent on your machine.** Start a
conversation in Claude Code, continue it in Codex, OpenCode or Antigravity —
from any directory. Sessions you create in one tool show up in all the others,
and the work you add anywhere is shared everywhere.

Verified live: **Claude Code, Codex CLI, OpenCode, Antigravity** — all four
bidirectional.

## The problem in one line

Every agent tool only shows the sessions it started itself, in the directory
you launched it from. Most of your history is on disk but invisible.

## What agentbridge does

- **One session, everywhere.** Sync makes every session appear in every tool's
  own list, from any directory.
- **Continue anywhere.** `agentbridge resume <id> <tool>` opens a session
  started in another tool, in the tool you choose.
- **Work follows you.** Turns you add in one tool are pulled back (`pull`) and
  folded into the other tools' copies. Your original files are never modified.
- **Set and forget.** `auto install` hooks your shell. A new terminal then
  shares only the sessions created or changed since the last run, and does
  nothing at all when there are none. `auto watch` adds a live re-sync daemon.

## Install

```bash
# if you don't have Rust yet: https://rustup.rs
cargo install --git https://github.com/45Harry/agentbridge
agentbridge --version
```

Works on Linux, macOS and Windows. Building from a local checkout:
`cargo install --path .` (then re-run to update it again later).

## Get started

```bash
agentbridge init          # look around: what sessions are on this machine?
agentbridge auto install  # new and changed sessions are shared from now on
agentbridge sync          # surface sessions in this directory, for all tools
```

That's it. Sessions now flow both ways between your tools, automatically.

See what `sync` would do before it does it — it accepts `--dry-run` everywhere:

```bash
agentbridge sync --dry-run
```

`agentbridge unsync` removes only what agentbridge created — never anything you
recovered, and never a tool's own files.

## Everyday commands

```bash
cd ~/code/my-project
agentbridge sync                        # surface all sessions here, for all tools
claude --resume <id>                    # continue any session in Claude Code
codex resume <id>                       # ...in Codex
agentbridge resume <id> opencode        # ...in OpenCode
agentbridge resume <id> antigravity     # ...in Antigravity
agentbridge status                      # who has new work since the last sync?
agentbridge pull                        # recover that new work
```

| Command | What it does | Writes? |
| --- | --- | --- |
| `init` | Discover every session on the machine. | no |
| `ls` | List sessions across all tools. | no |
| `info` | Which tools are detected, and where sessions live. | no |
| `status` | Per synced copy: what agentbridge wrote vs what's on disk now. | no |
| `sync` | Republish every session into every tool for this directory. | yes |
| `pull` | Recover turns/renames made in any tool. | yes |
| `resume <id> <tool>` | Copy one session into one tool. | yes |
| `inject <tool> <ids...>` | Feed session context into a tool's startup. | yes |
| `start <tool>` | Launch an agent with cross-tool context injected. | yes |
| `sync --changed` | Share only what is new since the last run (what the hook runs). | yes |
| `unsync` | Remove exactly what `sync` created. `--project <dir>` or `--session <id>` limits it. | yes |
| `auto install` / `uninstall` | Add / remove the shell hook. | yes |
| `auto watch` | Re-sync whenever sessions change. | yes |

## Tracking one session across tools

Synced and resumed copies carry the same label in their title, so the four
picker rows for one conversation are recognizable as one conversation:

```
claude-code · My Important Session · 2026-08-19 10:00 · aaaaaaaa
└ origin tool  └ session name        └ session start    └ session id
```

- The date is the session's **own start time**, never the sync date — so the
  same conversation shows the same date in every tool.
- A name the tool already has is kept **verbatim**. Only a session with no name
  gets one derived from the first thing you wrote in it. A tool's own
  placeholder (OpenCode's `New session - <time>`) counts as no name.
- The id is the part that tells sessions apart: the first 8 characters, or the
  last 8 for OpenCode's `ses_…` ids, which all start alike.
- Renaming a copy in any tool is picked up by `pull` and republished, keeping
  your new name and the original id and date.

## Supported tools

| Connector | Sessions live in | Read | Write |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/projects/.../*.jsonl` | yes | yes |
| Codex CLI | `~/.codex/sessions/.../rollout-*.jsonl` | yes | yes, incl. `/resume` rows |
| OpenCode | `~/.local/share/opencode/opencode.db` | yes | yes, guarded* |
| Antigravity | `~/.gemini/antigravity-*/conversations/*.db` | yes, all stores | yes, guarded* |

\* Live databases: every write backs the database up first, tags its own rows,
and refuses to run while the tool is open. Details per tool in `CONNECTORS.md`.

## How it works, briefly

1. **Index in place.** Sessions are never copied — the index points at the
   files already on disk.
2. **No second copy.** A tool cannot read another tool's format, so each
   session is converted once and written into that tool's own store.
   agentbridge keeps no copy of it in `~/.agentbridge`.
3. **Never touch a tool's own sessions.** Recovered work lives in an
   agentbridge-owned overlay; `unsync` removes only what agentbridge created.

## Docs

- `DESIGN.md` — architecture and the bugs real testing found.
- `CONNECTORS.md` — each tool's on-disk format, reverse-engineered.
- `HANDOFF.md` — pick the project up on a new machine.
- `DECISIONS.md` — dated record of every design choice.
- `SPEC.md` — the original build spec.
- `test.py` — live check across the real tools: `python3 test.py` (makes real
  model calls; `--quick` for fewer). It removes what it made unless you pass
  `--keep`.

## License

MIT — see `LICENSE`.