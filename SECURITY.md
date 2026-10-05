# Security

agentbridge copies whole session transcripts from one tool into every other
tool's store. Transcripts routinely contain API keys, tokens, passwords and
`.env` dumps that you pasted or a tool printed. This document says what
agentbridge does about that, and where it stops.

It makes no network calls and sends no telemetry. Everything stays on the
machine.

## The redaction model

**Originals are never modified.** Redaction applies to what agentbridge
*writes*, not to what it reads. Your Claude Code, Codex, OpenCode and
Antigravity sessions stay exactly as those tools wrote them.

**Every derived copy is redacted before it is written.** One choke point in
`sync` covers all four writers, so no target can be forgotten:

| Where text is written | Redacted |
| --- | --- |
| `~/.agentbridge/cache` (converted copies) | yes |
| Files and rows written into Claude Code, Codex, OpenCode, Antigravity | yes |
| `~/.agentbridge/overlay` (turns recovered from copies) | yes, on write and again on read |
| The search index (`~/.agentbridge/index.db`) | yes, before it is stored |
| Briefs printed, injected, or returned over MCP | yes |
| Text sent to an `--llm-cmd` command | yes (the prompt is redacted again) |
| Facts recorded with `fact` / `record_fact` | yes |
| `resume` into a *different* tool | yes |

Two writes are deliberately **not** redacted, because they rewrite a tool's own
file and redacting would replace text you actually wrote:

- `resume` into the session's own tool.
- Opt-in merge-back (`resume --merge`) into the original session file. Only the
  turns folded in from other tools' copies (which are redacted) are added.

**What is redacted.** Matching text is replaced with `[REDACTED:<rule>]`, keeping
the surrounding context (`API_KEY=[REDACTED:env-secret]`, `Bearer
[REDACTED:bearer-token]`) so a redacted transcript still reads sensibly.
Applied to message text, titles, tool-call inputs and results (including every
string nested inside their JSON), and recorded commands.

Default rules:

- Private key blocks (PEM, PGP), including an unterminated block in a truncated
  transcript, which is redacted to the end of the text.
- AWS access key ids and secret access keys.
- GitHub, GitLab, Slack, Stripe, npm, Hugging Face, SendGrid and Google API
  keys; OpenAI/Anthropic-style `sk-…` keys.
- JWTs, `Bearer` tokens and `Authorization` headers.
- The password in `scheme://user:password@host` connection strings.
- Upper-case environment assignments whose name contains `SECRET`, `TOKEN`,
  `PASSWORD`, `API_KEY`, `PRIVATE_KEY`, `ACCESS_KEY` and similar.
- `password`, `secret`, `api_key`, `access_token` and similar assignments in
  config, YAML and JSON text, including prefixed keys such as `jwt_secret` and
  `db_password`.
- In structured tool-call arguments, any string under a key such as `password`,
  `authorization`, `cookie`, or `*_token`, whatever the value looks like.

**It fails closed.** If your rules file cannot be read or parsed, `sync`,
`pull`, `resume` and `start` write nothing and exit with status 2. They never
fall back to the defaults or carry on unredacted.

**It is idempotent and deterministic.** Redacting redacted text changes nothing.
Write-back compares turns by their text, so this is what stops a redacted copy
from looking like new work on every pull.

**It cannot be turned off by accident.** The only opt-out is the explicit
`--no-redact` flag on an interactive command, which prints a warning. `auto
watch` and the shell hook never accept it. A later normal `sync` re-scrubs any
copy that an opted-out run wrote, because the copies are rewritten in place.

## Adding your own rules

Create `~/.agentbridge/redact.rules` (or `$AGENTBRIDGE_DATA_DIR/redact.rules`),
one rule per line:

```
# name = regex          (lowercase letters, digits and `-` in the name)
internal-id = ACME-[0-9]{6}
badge = badge=(?P<secret>[0-9]+)
```

A named group `(?P<secret>…)` limits the replacement to that part; without one
the whole match is replaced. Your rules run in addition to the defaults. A
malformed line is an error, not a skipped rule: silently ignoring a rule you
wrote would leave the thing you wanted hidden in plain sight.

Patterns use Rust's `regex` crate, which matches in linear time, so a crafted
transcript cannot stall a sync with a pathological pattern.

## Known limits

Redaction is pattern matching. It will miss secrets that do not look like
secrets.

- **Unlabelled, high-entropy strings** with no recognisable prefix or nearby
  keyword (a bare random password pasted on its own line).
- **Short passwords in free text.** The `password: …` rule needs six or more
  characters so ordinary prose such as `secret: true` is left alone.
- **Encoded or split secrets:** base64-wrapped values, or a key split across two
  messages or two tool calls.
- **Binary and image content.** Only text is inspected.
- **Personal data that is not a credential:** names, emails, internal hostnames
  and file paths are not redacted. Add rules for what matters to you.
- **False positives.** A long `sk-…` identifier or an uppercase variable that
  happens to contain `TOKEN` is redacted even if harmless. This is deliberate:
  a leaked key costs more than a redacted word.

Things redaction does not protect, because agentbridge does not own them:

- The original session files, and the tools' own databases. If a secret is in
  your Claude Code history, it is still there.
- **Backups** agentbridge takes of OpenCode, Codex and Antigravity databases
  before writing. Those are copies of the tool's own data, unredacted.
- agentbridge's own data directory uses your default file permissions. Treat
  `~/.agentbridge` like the session stores it mirrors.

If a secret was already copied before you upgraded, run `agentbridge sync`
again: copies are rewritten from the redacted version. Rotate any credential
that has been in a transcript regardless; redaction reduces the spread, it does
not un-leak what a tool already stored.

## The index, MCP and the optional model

**The index is a derived copy of your conversations.** `index.db` holds message
text so search and briefs do not re-read every transcript. It is redacted before
it is stored, truncated per message (4,000 characters of text, 500 of a tool
result), and never leaves the data directory. It is a cache: delete it and
`agentbridge index` rebuilds it. Treat it like the session stores it mirrors;
it uses your default file permissions.

**`agentbridge mcp` returns history to an agent.** Everything it returns is
redacted again on the way out, bounded to 16 KB per call, and prefixed with a
notice that it is quoted history, not instructions. Past transcripts contain
whatever anyone or any tool ever wrote, including text crafted to steer an
agent, and an agent that reads it can be steered by it. agentbridge never acts on
what it finds (reading history cannot record a fact or run anything), but the
agent on the other end is not under agentbridge's control. The brief also
neutralises square brackets in extracted text, so a message cannot forge a
citation that looks like agentbridge's own.

**`--llm-cmd` sends text to whatever you name.** agentbridge runs the command
without a shell and gives it only the redacted brief. It makes no network
connection of its own, so where the text goes next depends entirely on that
command (`claude -p` talks to a hosted model; `ollama run` stays local). The
model's answer is validated before use and redacted again.

## Other safety properties

- **One writer at a time.** An advisory lock in the data dir serialises `sync`,
  `pull`, `resume`, `unsync` and the watch loop, so the shell hook, the daemon
  and a manual run cannot overwrite each other's manifest updates. The OS drops
  the lock if a process dies, so it cannot go stale. Read-only commands and
  `--dry-run` never take it.
- **Atomic state files.** The manifest, overlay and title overlay are replaced
  by write-then-rename, so a crash mid-write cannot leave a truncated manifest.
- **Generated files are self-identifying.** Files agentbridge writes into Claude
  Code and Codex carry a small `agentbridge` marker, so they are still
  recognised as agentbridge's (and never re-ingested as real sessions) if the
  manifest is lost. `agentbridge status` lists such orphans and
  `unsync --orphans` removes the ones nobody has added work to.
- **Database writers** (OpenCode, Codex, Antigravity) back up before the first
  insert, tag their own rows, and refuse to run while the tool is open.

## Reporting a problem

Open an issue at <https://github.com/45Harry/agentbridge/issues>. Please do not
include real credentials in the report; a redacted excerpt is enough.
