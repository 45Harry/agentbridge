<p align="center">
  <img src="https://raw.githubusercontent.com/45Harry/agentbridge/master/assets/logo-wordmark.svg" width="480" alt="agentbridge">
</p>

**One session layer for every AI coding agent on your machine.** Start a
conversation in Claude Code and continue it in Codex, OpenCode or Antigravity,
from any directory.

Each tool only lists the sessions it started itself, in the directory you
launched it from. agentbridge runs as a small MCP server inside every tool, so
the AI in any tool, in any folder, can find a session from any other tool and
read it straight from the original files. Nothing is copied.

## Install

```bash
pipx install agentbridge-rs     # or: uv tool install agentbridge-rs
```

Or with Cargo: `cargo install --git https://github.com/45Harry/agentbridge`.
The command is `agentbridge` either way. Linux, macOS and Windows.

## Set up

Close your AI tools, then:

```bash
agentbridge mcp install --dry-run   # see what would change
agentbridge mcp install
```

This adds one `agentbridge` entry to each detected tool's MCP settings
(`~/.claude.json`, `~/.codex/config.toml`, OpenCode's `opencode.json[c]`,
Antigravity's `mcp_config.json`), keeping every other line, comment and
setting as it was, with a backup next to each file. A tool that is open is
skipped and named; run the command again after closing it.

If you used an older agentbridge, install first recovers any new turns from
its copies (`pull`), then removes the shell hook and every copy it made.

The entry holds the binary's full path, so **run `agentbridge mcp install`
again after upgrading or moving agentbridge**. Undo with
`agentbridge mcp uninstall` (or `agentbridge unsync`, which removes everything
agentbridge created).

## Use it

Open any tool, in any folder, and ask in plain words:

> Find my Claude session about the checkout page and continue it.

The AI calls `list_sessions` (sessions of the current folder first, then every
other folder; searchable by title, first message, folder, tool and id) and
`read_session` (page 1 is the newest 40 messages; it can page back further).

When you continue a session in another tool, agentbridge adds those new turns
to the end of the original session, in its own tool, under a note
"continued in <tool> on <date>". This write back runs on its own when a tool
starts or closes agentbridge's server, and only while the origin's tool is
closed (with any Claude Code window open, Claude sessions wait). Run it by hand
with `agentbridge mcp writeback --dry-run`.

**What the server can see:** every session of every tool for your user, in
every folder, returned as stored (no redaction). Whatever the AI reads goes to
that tool's AI provider. Antigravity tool calls come back without their
output.

## Commands

| Command | What it does |
| --- | --- |
| `mcp install` / `uninstall` | Set up (or remove) the MCP server in every detected tool |
| `mcp` | The MCP server itself (the tools start it; you don't need to) |
| `mcp writeback` | Add continued work to the end of the origin session |
| `init`, `ls`, `info` | Discover and list sessions and detected tools (read only) |
| `resume <id> <tool>` | Copy one session into one tool, for a true native resume |
| `inject <tool> <ids...>`, `start <tool>` | Start a tool with context from other sessions |
| `unsync` | Remove everything agentbridge created |
| `sync`, `pull`, `status`, `auto` | The old copy based sync. Replaced by `mcp install`; removed in the next release |

Every command that writes accepts `--dry-run`.

## Supported tools

| Tool | Sessions | Read | Write |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/projects/` | yes | yes |
| Codex CLI | `~/.codex/sessions/` | yes | yes |
| OpenCode | `~/.local/share/opencode/opencode.db` | yes | yes\* |
| Antigravity | `~/.gemini/antigravity-*/` | yes | yes\* |

\* Write back into a shared database: agentbridge backs it up first, records
every row it adds, and won't write while the tool is open.

## More

[Architecture](https://github.com/45Harry/agentbridge/blob/master/ARCHITECTURE.md) ·
[Design](https://github.com/45Harry/agentbridge/blob/master/DESIGN.md) ·
[Tool formats](https://github.com/45Harry/agentbridge/blob/master/CONNECTORS.md)

MIT license.
