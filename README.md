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
- **Set and forget.** `auto install` hooks your shell so every new terminal
  syncs itself. `auto watch` adds a live re-sync daemon.

## Install

```bash
# if you don't have Rust yet: https://rustup.rs
cargo install --git https://github.com/45Harry/agentbridge
agentbridge --version
```

Works on Linux and macOS. **Windows is not supported yet**: sync relies on Unix
hardlinks and process checks. Needs Rust 1.89 or newer. Building from a local
checkout: `cargo install --path .` (then re-run to update it again later).

## Get started

```bash
agentbridge init          # look around: what sessions are on this machine?
agentbridge auto install  # new terminals sync from now on
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
| `index` | Build or refresh the search index (incremental). `--rebuild` starts over. | its own index only |
| `search <words>` | Full-text search across every tool's history. | no |
| `brief` | A cited summary of a project's history across all tools. | its own index only |
| `fact <text>` | Record a durable fact about a project; it appears in its brief. | its own index only |
| `mcp` | Serve search, briefs and facts to an agent over MCP. | its own index only |
| `start <tool>` | Inject the project brief into the agent's startup file, then launch it. | yes |
| `inject <tool> <ids...>` | Same, for sessions you pick. | yes |
| `clean` | Remove what `start`/`inject` added, restoring the file exactly. | yes |
| `unsync` | Remove exactly what `sync` created. `--orphans` removes marked files the manifest lost. | yes |
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
  gets one derived from its opening message.
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

## Searching and summarising your history

```bash
agentbridge index                          # first build is the slow one; later runs read only what changed
agentbridge search retry backoff --since 2w
agentbridge brief                          # this project, all tools, ~2000 tokens
agentbridge brief --since 7d --budget 800
agentbridge fact "Releases are cut from release/* branches only"
```

`brief` is built **without any model**: it reads the index and extracts, per
project, the files most edited, the test and build commands, decisions with their
reasons, known problems and dead ends, conventions, open threads, and any facts
you recorded. **Every line cites its source** as `[tool:session#turn]`, so you can
check it; an aggregate such as "most edited files" cites its most recent
occurrence. The budget is enforced on the final text with a fixed, conservative
counter (a deterministic proxy, not any model's tokenizer). Results are cached on
exactly what went in, so re-running with nothing new is free.

The brief is a heuristic digest: it finds sentences with decision, problem and
convention wording, so it will sometimes miss things and sometimes include
something that only sounds like a decision. The citations are there so you can
tell.

To have a model condense it, name a command that reads a prompt on stdin and
writes the answer on stdout:

```bash
agentbridge brief --llm-cmd "claude -p"
```

This is opt-in and agentbridge opens no connection itself; the redacted brief goes
to that command, and the CLI says so first. The answer is accepted only if every
bullet still ends with a citation that exists in the original brief and it fits
the budget; otherwise you get the plain brief and the reason.

### Letting an agent query it (MCP)

`agentbridge mcp` is a Model Context Protocol server over stdio with four tools:
`search_history`, `get_brief`, `get_session` and `record_fact`. Add it to an agent
that supports MCP servers, for example:

```json
{ "mcpServers": { "agentbridge": { "command": "agentbridge", "args": ["mcp"] } } }
```

Results are redacted, size-bounded, and marked as quoted history rather than
instructions. Tested here against a hand-written client over a pipe; not yet
against each agent's own MCP client.

## Starting an agent with context

`agentbridge start claude-code` writes the project brief into the file that agent
reads at startup, and launches it. Arguments after `--` go to the agent:

```bash
agentbridge start claude-code -- --model opus
agentbridge start codex-cli --dry-run      # show the file and text, change nothing
agentbridge clean                          # take it out again
```

| Agent | File it reads | Status |
| --- | --- | --- |
| Claude Code | `CLAUDE.md` in the project | file handling verified against `claude 2.1.289`; whether the model reads it not observable without a login |
| Codex CLI | `AGENTS.md` | expected; not yet run against a real install |
| OpenCode | `AGENTS.md` | expected; not yet run against a real install |
| Antigravity | `AGENTS.md` | least certain; not yet run against a real install |

What is tested is agentbridge's side: the file is written, fenced, and
restored exactly. Whether each agent picks the file up is the part to confirm on
your machine.

The text goes between `<!-- agentbridge:begin -->` / `<!-- agentbridge:end -->`
markers, after anything already in the file. Your own text is never changed, a
second run refreshes the block in place, `clean` restores the file
byte-for-byte (and deletes it if agentbridge created it), and the summary is
redacted and within the token budget (`--budget`, default 1500).

## Secrets

Session transcripts often contain API keys, tokens and passwords. Every copy
agentbridge writes into another tool is **redacted first** (`[REDACTED:…]`);
your original sessions are never touched. A rules file you can extend, a
`--no-redact` opt-out for interactive commands, and the known limits are in
[`SECURITY.md`](SECURITY.md). If your rules file is broken, agentbridge writes
nothing rather than writing unredacted.

## How it compares

The neighbouring tools each solve a different part of "my agents don't share what
they know". This table is built from what each project says about itself on its
GitHub page, checked on 2026-10-05. It is a snapshot from descriptions, not a
hands-on review, and these projects move fast, so read their pages for what they do
today. Several share a name with other repositories; the links are the ones that came
up as the main project.

| | What it is | Where agentbridge differs |
| --- | --- | --- |
| [cass](https://github.com/Dicklesworthstone/coding_agent_session_search) | A Rust TUI and CLI that indexes and searches local session history across many coding agents (it describes 22), with lexical, semantic and hybrid search and an agent-facing mode. | cass is the stronger **search** tool: more tools covered, and semantic search, which agentbridge does not have (its search is full-text with stemming). agentbridge's difference is that it also **writes** sessions into the other tools so you can resume them there. |
| [claude-code-history-viewer](https://github.com/jhlee0409/claude-code-history-viewer) | A desktop app (also a headless server) for browsing, searching and analysing conversations from Claude Code, Codex, Gemini CLI, Antigravity, OpenCode and others. | A viewer is read-only by design and has a real UI and analytics; agentbridge has a terminal dashboard only and no analytics. |
| [Memorix](https://github.com/AVIDS2/memorix) | A cross-agent memory layer served over MCP: a shared, curated project memory (SQLite) that survives new chats and tool switches. | Memorix keeps **curated memory** as the product. agentbridge derives its brief from the real history and cites every line, and its `fact` store is much smaller and unreviewed. |
| [agentmemory](https://github.com/rohitg00/agentmemory) | Persistent memory for AI coding agents. (A different project of the same name, [jayzeng/agentmemory](https://github.com/jayzeng/agentmemory), keeps memory as local Markdown files.) | Same distinction as Memorix: a memory store you write to, versus a view derived from sessions that already exist. |
| [rulesync](https://github.com/dyoshikawa/rulesync) | Generates each tool's config files (rules, commands, MCP, ignore files, subagents, skills) from one set of unified source files. | A different problem: rulesync syncs **instructions and configuration**; agentbridge syncs **conversations**. They compose, and agentbridge deliberately does not sync rule files. |

**What agentbridge does that I could not find stated elsewhere:** it converts a
session into each other tool's native format and installs it in that tool's own
session list, so a conversation started in one agent can be continued in another.
Absence of a claim is not proof, so treat that as "not found", not "unique".

**What agentbridge does not do**, plainly:

- No semantic or vector search; no cloud sync; no GUI beyond a terminal dashboard.
- Four tools only (Claude Code, Codex CLI, OpenCode, Antigravity), against the dozens
  some of the tools above cover.
- Linux and macOS only.
- No curated, reviewed memory store; no rule-file syncing; no analytics or cost
  tracking.
- Much of it is verified only against synthetic data, one real Claude Code, and one
  real Antigravity conversation (see `HANDOFF.md`, section 2m, for what has and has
  not been checked against a real tool).

## How it works, briefly

1. **Index in place.** Sessions are never copied — the index points at the
   files already on disk.
2. **Convert once, link many.** Each session is converted once into
   `~/.agentbridge/cache`; every directory gets a hardlink to that one file.
3. **Never touch a tool's own sessions.** Recovered work lives in an
   agentbridge-owned overlay; `unsync` removes only what agentbridge created.

## Docs

- `SECURITY.md` — what is redacted, what is not, and why.
- `DESIGN.md` — architecture and the bugs real testing found.
- `CONNECTORS.md` — each tool's on-disk format, reverse-engineered.
- `HANDOFF.md` — pick the project up on a new machine.
- `DECISIONS.md` — dated record of every design choice.
- `SPEC.md` — the original build spec.

## License

MIT — see `LICENSE`.