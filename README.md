<p align="center">
  <img src="https://raw.githubusercontent.com/45Harry/agentbridge/master/assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

**One session layer for every AI coding agent on your machine.** Start a
conversation in Claude Code and continue it in Codex, OpenCode or Antigravity,
from any directory.

Each tool only lists the sessions it started itself, in the directory you
launched it from. agentbridge makes every session show up in every tool, and
shares new work back across all of them. Your tools' own session files are
never modified.

## Install

```bash
pipx install agentbridge-rs     # or: uv tool install agentbridge-rs
```

Or with Cargo: `cargo install --git https://github.com/45Harry/agentbridge`.
The command is `agentbridge` either way. Linux, macOS and Windows.

## Quick start

```bash
agentbridge init          # see which sessions are on this machine (read only)
agentbridge auto install  # share new and changed sessions in every new terminal
agentbridge sync          # make this directory's sessions visible in all tools
```

Then resume any session in the tool you like:

```bash
claude --resume <id>
codex resume <id>
agentbridge resume <id> opencode
agentbridge resume <id> antigravity
```

Every command that writes accepts `--dry-run`. `agentbridge unsync` removes
exactly what agentbridge created and nothing else.

## Commands

| Command | What it does |
| --- | --- |
| `init`, `ls`, `info` | Discover and list sessions and detected tools (read only) |
| `status` | Show which synced copies have new work |
| `sync` | Make every session in this directory visible in every tool |
| `pull` | Bring back turns and renames made in any tool |
| `resume <id> <tool>` | Copy one session into one tool |
| `inject <tool> <ids...>`, `start <tool>` | Start a tool with context from other sessions |
| `auto install` / `uninstall` / `watch` | Shell hook, or a daemon that re-syncs on change |
| `unsync` | Remove what agentbridge created (`--project`, `--session` to narrow) |

## Supported tools

| Tool | Sessions | Read | Write |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/projects/` | yes | yes |
| Codex CLI | `~/.codex/sessions/` | yes | yes |
| OpenCode | `~/.local/share/opencode/opencode.db` | yes | yes\* |
| Antigravity | `~/.gemini/antigravity-*/` | yes | yes\* |

\* Shared databases: agentbridge backs them up first, tags its own rows, and
won't write while the tool is open.

Copies are titled `origin tool · session name · start time · id`, so one
conversation is easy to spot in every tool's list.

## More

[Architecture](https://github.com/45Harry/agentbridge/blob/master/ARCHITECTURE.md) ·
[Design](https://github.com/45Harry/agentbridge/blob/master/DESIGN.md) ·
[Tool formats](https://github.com/45Harry/agentbridge/blob/master/CONNECTORS.md)

MIT license.
