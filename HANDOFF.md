# HANDOFF.md

Pick this project up cold — new machine, new session, no memory of prior
conversation. Read this, then `DESIGN.md` (architecture and why), then
`CONNECTORS.md` (each tool's on-disk format).

**Last updated:** 2026-08-19.

## 0. Start here

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build && cargo test      # 300 tests pass (+5 ignored: live-verification suites)
cargo run -- init              # read-only: what's on this machine
```

Requires Rust 1.89+ (edition 2024). Binary version is 0.4.0.

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
one tool are pulled into the overlay and republished.

DECISIONS.md (2026-08-01) dropped native *picker listing* as a requirement,
but that was later reversed per tool once each vendor's index was actually
understood: Codex gets `threads` rows (2026-08-01), OpenCode gets `session`
rows, and Antigravity gets a `conversation_summaries` row plus a conversation
body (2026-08-19). So synced sessions **do** appear in each tool's own picker
today — read those three decisions together rather than the first one alone.

## 2. Current state — verified live 2026-08-19

**On the operator's machine, the loop is running and proven:**

- `auto watch` (PID noted in §7) re-scans every 15s; `init` reports 11,219
  sessions across 4 tools (Claude Code 3,714, Codex CLI 5,365, OpenCode 2,115,
  Antigravity 25) in 23 project directories.
- **All four connectors are bidirectional** — every tool is both a discovery
  source and a write target. Antigravity was the last read-only one (§2g).
- Every synced copy carries a cross-tool label in its title —
  `provider · name · started_at · id[..8]` — so one conversation is
  recognizable in all four pickers (§2h).
- A fake Codex session dropped into `~/.codex/sessions/` surfaced as a
  UUIDv5 claude artifact in `~/.claude/projects/<encoded-dir>/` within 35s,
  then cleaned up.
- Antigravity: **read and write, all stores** — see §2g. 25 conversations
  load across `antigravity-cli` (12) and `antigravity-ide` (13); foreign
  sessions materialize into agy's own picker. The IDE store gained one
  conversation between the §2g measurement (24 / 733 messages) and this one,
  which is the multi-home scan working.
- Write-back: `sync` pulls turns appended in any tool into the overlay and
  republishes (WRITE-BACK-OK markers verified in the claude copies).
- OpenCode write path proven against the real database (sessions created by
  `resume` appear in OpenCode's own picker; refusal-to-write-while-running
  confirmed on a live machine).

**Known limits:**

- OpenCode write-back is gated while OpenCode itself is running (`PID` check);
  `pull` still reads it, so recovery works, republish into opencode.db waits.
- Codex write path: sessions now get `threads` rows (`src/codex_write.rs`,
  v0.3.0) so they appear in `codex /resume` — same guard/backup/marker gates
  as OpenCode, verified against the real binary (`codex delete` resolves an
  inserted row; `codex /resume` shows them). Refused while Codex is running,
  same as OpenCode.
- Antigravity: **shipped 2026-08-19** (§2g). Model text is mapped
  (`payload.20.1`), the write connector exists, and both directions are
  verified. Remaining gaps: the encrypted `.pb` bodies in the desktop/backup
  stores cannot be read at all (no key), and merge-back into a *native* agy
  body is deliberately refused — recovered turns stay in the overlay.
- `start`/`inject`/`clean` **work as of 2026-10-05** (§2j): they write a fenced
  block into `CLAUDE.md` / `AGENTS.md` and `clean` restores the file exactly.
  Tested on agentbridge's side only; whether each *agent* reads the file is
  unverified (no agent binaries on the dev machine). The brief itself is still
  a simple digest, not a distilled one.
- Redaction (`SPEC.md` §3) **shipped 2026-10-05** — see §2i and `SECURITY.md`.
  Known misses are documented there (short passwords, unlabelled strings).

## 2a. Cross-tool folder visibility — measured live 2026-08-03

The question this round answered: **does a synced session actually show up in
any folder, in Claude Code and OpenCode?** Measured on the operator's machine
against the real binaries (claude 2.x, opencode 1.17.15). Answer: *it shows up
in the folders sync was pointed at, not everywhere* — and OpenCode's half was
broken outright. What was established:

- **The installed binary was 0.1.0 while the repo was at 0.3.3.** Every
  database write path (OpenCode rows, Codex `threads`) shipped after 0.1.0, so
  none of them had ever run here: `opencode.db` held **0** agentbridge rows and
  `state_5.sqlite` **0** tagged threads, while the manifest claimed 487
  materializations. `cargo install --path .` after code changes is not
  optional — check `agentbridge --version` before believing any live result.
- **OpenCode's picker filters by `project_id`, not by `directory`.** The
  `directory` column is metadata. A folder resolves to the `project` row whose
  `worktree` matches it, else to the catch-all `global`. Consequence: one row
  in `global` is visible from *every* folder that is not a worktree of its own,
  and a git repo with its own project row sees only rows carrying that id.
  Verified: a session written with `directory=…/ab-crosstest` listed from
  `/Users/harry`, `/tmp` and `/Users/harry/Documents`, and **not** from
  `/Users/harry/Documents/agentbridge`.
- **Claude Code is strictly per-directory**, no catch-all. A session is
  listed/resumable exactly in the directories whose encoded folder holds a copy
  (`~/.claude/projects/-Users-harry-Documents-ab-crosstest/…`). Verified: the
  same id loaded from `ab-crosstest` and `$HOME`, and returned
  `No conversation found` from `/Users/harry/Documents` and `/tmp`.
- **Both directions of the cross-tool hop work.** An OpenCode session
  (`ses_04e603643ffe…`) materialized into Claude Code in a folder that had
  never held a session, and `claude --resume` continued it. A Claude Code
  session (`c6f114a0-…`) materialized into OpenCode and appears in OpenCode's
  own `opencode session list`, titled `claude-code session c6f114a0-…`.

Two real bugs fell out, both fixed in this commit. **Sandbox-verified
2026-08-11** against a copy of the real `opencode.db` (project table intact)
with the real `opencode session list` binary — the session lists exactly once
from `/tmp`, `$HOME`, `ab-crosstest` (all `global`) and from the agentbridge
worktree (its own project), and the legacy `ses_ab…` row was reclaimed. The
final live pass with OpenCode closed still stands (see §6.0):

1. `UNIQUE constraint failed: session.id` — `derive_id` hashed only
   (provider, source id), so one session could own exactly one row for the
   whole machine. The second directory's write always failed, and sync
   swallowed it into `report.errors`. OpenCode multi-directory visibility has
   therefore never worked. Ids (and message/part ids) are now per **project**:
   the minimum for visibility everywhere, and the maximum before the picker
   lists the same conversation twice.
2. `append_manifest` deduped by `dest` alone. Every OpenCode row shares one
   dest (the database), so a whole run's rows collapsed into a single manifest
   entry and `status`/`pull` tracked one session out of hundreds. The key is
   now (dest, row id) for OpenCode, unchanged for file targets.

## 2b. Session titles didn't sync, and mtime confused pickers — fixed 2026-08-14

Operator report, live: rename a session in OpenCode (`claude-opencode-funny-
joke-code`), it never shows up back in Claude Code. Root-caused and fixed —
**unit-tested only, not yet re-verified against the real binaries** (§4
applies: that has bitten this project three times already). This is the
single most important next step; see the flagged item at the top of §6.

**Bug 1 — `live_root()` ignored `CLAUDE_CONFIG_DIR`/`CODEX_HOME`.** Found
first, while chasing the title bug. `sync.rs::live_root()` hardcoded
`~/.claude/projects` / `~/.codex`, while the read connectors already honored
the env var overrides. On this operator's machine
(`CLAUDE_CONFIG_DIR=/Users/harry/.claude-mantra`), materialized copies were
silently landing in `~/.claude` — a directory the real, redirected Claude Code
install never reads. Fixed via `write_root()`/`config_home()` helpers shared
with the read side; 1352 stray files this had produced were manually removed
(matched against `manifest.jsonl`, not a blanket `unsync`; the 190 unrelated
pre-existing files at that path were left alone).

**Bug 2 — Claude Code never parsed its own title.** `-n/--name` and in-session
rename write dedicated `{"type":"custom-title","customTitle":"…"}` /
`{"type":"agent-name","agentName":"…"}` records — not a field on a turn — so
`claude_code.rs` had nothing to feed the sync/write-back machinery even though
Codex (`threads.title`) and OpenCode (`session.title`) were already wired
correctly. Fixed:

- `connectors/claude_code.rs`: both `scan_file()` and `load_from_path()` now
  recognize `custom-title`/`agent-name`, last-one-wins (a later rename
  replaces an earlier one).
- `convert.rs::ClaudeCodeConverter`: emits both records (before even the
  `mode`/`permission-mode` control records, matching real files) when
  materializing a session that has a title.
- `sync.rs`: new **title overlay**, symmetric to the existing message
  overlay — `LinkRecord` gained a `title: Option<String>` field so
  `pull_back()` can tell "the tool renamed it" from "we never wrote a title
  here." A rename detected in a materialized copy (title in the file/DB no
  longer matches what agentbridge last wrote) is written to
  `~/.agentbridge/overlay/<session>.title` and reported in `PullReport.renamed`
  (printed by `agentbridge pull`); `fold_overlay()` applies it on top of the
  native title before the next `sync` re-materializes every other copy.

**Known limitation, by design, unchanged from the message case (invariant
2):** a rename recovered from a non-native copy propagates to every *other*
materialized copy, but never back into the session's true origin file — same
rule that already blocks message write-back into the origin file. Same escape
hatch: `agentbridge resume --merge` opts a session into merge-back, which
folds recovered turns (and now titles) into the native file too.

**Narrower limitation:** `agentbridge list`'s title column comes from
`scan()`, which stops reading at the first record carrying `cwd` (RawSession
is meant to be cheap — no full-file read, see `model.rs`). A rename recorded
*after* that point (mid-conversation, not at session start) is invisible to
`list` until the fuller `load()` path runs (which sync/pull always use, so
propagation itself is unaffected — only the CLI's own listing can lag).
Covered by `test_claude_code_title_prefers_last_custom_title_record`
(`src/connectors/mod.rs`), which asserts the split explicitly.

**Bug 3 — "current time" confusion.** Reported alongside the title bug:
synced sessions look freshly active. Materializing a file via `fs::write`
leaves its mtime at "now" (sync time), and Claude Code's own resume picker is
filesystem-scanned with no separate index (`CONNECTORS.md` §1) — so it sorts
by that mtime, putting a months-old conversation at the top. Fixed: both
`ClaudeCodeConverter::convert()` and `CodexCliConverter::convert_multi()` now
call a new `set_mtime_from_session()` (`convert.rs`) right after writing the
file, setting mtime to the session's own `last_event_at` (falling back to
`started_at`) via `File::set_modified()`.

Test coverage added this round (see `cargo test`, now 87 + 2 ignored):
`test_pull_back_recovers_a_rename`, `test_recovered_rename_propagates_to_other_tools`,
`test_claude_code_title_prefers_last_custom_title_record`,
`test_converted_claude_file_mtime_matches_session_last_event`,
`test_codex_convert_multi_mtime_matches_session_last_event`.

## 2c. §2b re-verified live, and a second bug found — 2026-08-14

Ran the §6 "START HERE" checklist against the real `claude` (2.1.232), `opencode`
(1.18.15) and `codex` (0.147.0) binaries, in a sandbox (fake `HOME`, real
binaries, per §4 — never on the operator's own data). Found and fixed one more
bug; everything else confirmed working:

- **`ClaudeCodeConverter` had no real `convert_multi`.** It used the
  `SessionConverter` trait's default (`convert()` once, `dirs` argument
  discarded), while `sync_into`'s per-directory loop does
  `dirs.iter().zip(&artifacts)` — with only one artifact ever produced, `zip`
  silently truncated to the first directory and dropped every other one,
  including the `$HOME` fallback `target_dirs()` computes. In practice: a
  Claude-Code-native session synced from directory B only ever got a copy in
  B, never in `$HOME` too — contrary to what this doc claimed in §6 item 1.
  Fixed by giving `ClaudeCodeConverter` a real `convert_multi` (one session
  variant per directory, `project_id` swapped per copy, each through the
  existing single-directory `convert()`), mirroring how `CodexCliConverter`
  already does it. Regression tests:
  `test_claude_convert_multi_writes_one_file_per_directory` (`convert.rs`),
  `test_sync_materializes_claude_session_into_project_and_home` (`sync.rs`).
  This was very likely compounding the original "renamed in OpenCode, not
  showing in Claude Code" report: even after §2b's title-overlay fix, the
  directory the operator happened to be checking may simply never have had a
  Claude Code copy at all.
- **Title write-back confirmed end-to-end, real binaries.** Built a native
  Claude Code session (accepted by the real `claude --resume`, verified via
  the zero-cost "No deferred tool marker found" signal from §4), synced it
  into OpenCode (row visible in real `opencode session list`), renamed it via
  a direct `UPDATE session SET title=…` — the same mutation OpenCode's own
  rename does — reconfirmed via `opencode session list`, then `agentbridge
  pull` (reported the rename), then `agentbridge sync --project <a directory
  that never held a copy>`. The new Claude Code copy there carried the
  renamed title in a real `custom-title`/`agent-name` record and was accepted
  by `claude --resume` (same zero-cost signal) — a rename made through
  OpenCode's real database reached a directory that had never seen this
  session before, through a real Claude Code file. The session's actual
  native file was never given a `custom-title` record by agentbridge (still
  none there) — invariant 2 held.
- **mtime fix confirmed**, isolated from the run above (which got a stray
  real edit from an unrelated auth-failed `claude -p` probe and briefly
  looked like it hadn't): a clean session with content timestamped
  `2020-01-01` produced a materialized copy with that exact mtime, not the
  sync wall-clock time.
- **Not exercised live**: Codex's `threads.title` upsert. `codex_write.rs`
  only activates once `~/.codex/state_5.sqlite` already exists, which the
  real `codex` binary only creates on first authenticated use — out of scope
  for a sandbox run. Already covered by `codex_write.rs`'s own unit tests
  against the reverse-engineered real schema (`REAL_SCHEMA` in its test
  module); still worth a real pass per §4's doctrine when convenient.

## 2d. Codex never showed a rename either — third bug, fixed 2026-08-14

Operator follow-up: "what about codex?" §2c had explicitly left Codex's
`threads.title` unverified live (state_5.sqlite needs a real authenticated
`codex` run to bootstrap). Two findings from actually chasing that down:

- **`CODEX_HOME` is not fully honored by the real `codex` binary.** Sandboxing
  `codex exec` with `CODEX_HOME=<sandbox>` still touched the operator's real
  `~/.codex/state_5.sqlite` (confirmed by mtime, moments after the sandboxed
  run) — the sessions themselves went into the sandboxed dir correctly, but
  something about opening the state DB reached the default location instead.
  No corruption resulted (row count unchanged, no new/bogus rows — it looks
  like an open/checkpoint touch, not a write of new data), but **do not
  invoke the real `codex` binary against a `CODEX_HOME` override expecting
  full isolation** — it does not give you one, unlike `claude`/`opencode`,
  which respected their equivalent overrides throughout all of §2c's testing.
  Safe alternative used here instead: copy the operator's real
  `state_5.sqlite` (schema + realistic prior rows) into a sandboxed
  `CODEX_HOME`, then let *agentbridge itself* (not the real `codex` binary)
  write into it — that fully respects `CODEX_HOME` since it's our own code.
- **The real bug**: `codex_write.rs::ensure_thread_rows` computed `threads.title`
  as `if first_user.is_empty() { session.title } else { clip(first_user) }` —
  i.e. it used the first-user-message preview whenever one existed
  (virtually always), and only fell back to `session.title` for a session
  with zero user turns. An explicit title — a real Codex rename, or one
  recovered from Claude Code/OpenCode via §2b's title overlay — was silently
  discarded every time. Fixed to prefer `session.title` whenever set,
  falling back to the preview only for an unnamed session (matching Codex's
  own default-before-rename behavior). This predates §2b/§2c entirely — a
  rename made *natively in Codex itself* was just as broken, since
  `session.title` there passed straight through the same code path.
  Regression test: `test_explicit_title_beats_first_message_preview`
  (`codex_write.rs`). Verified against a copy of the operator's real
  `state_5.sqlite` schema (not the original — see the `CODEX_HOME` note
  above): a fresh row picked up the recovered title correctly; existing rows
  from directories not touched by that particular `sync --project` run kept
  their old title, exactly as expected (a sync only refreshes the project
  directory + `$HOME`, not every directory a session was ever materialized
  into — re-sync each directory to refresh it).

## 2e. §2b's fix flooded 705 false "renames" outside the sandbox — fixed 2026-08-14

The operator asked to install and test the title-sync work against real
machine data (not synthetic fixtures) — real risk, since this machine's real
manifest tracks ~19,000 rows across ~4,500 sessions. Found a real, machine-
scale bug immediately:

**`agentbridge pull` reported 705 "renames"** on the very first run against
real data, for sessions nobody had touched. Root cause: `LinkRecord.title`
was recorded as the raw `session.title` — which is `None` for the (very
common) case of an untitled session — while `opencode_write::write_session`
always persists *something* (falling back to `"{provider} session {id}"`
when `session.title` is `None`). Once that fallback round-trips through
`load_from_db` on the next `pull`, it is indistinguishable from a real title:
`rec.title` (`None`) no longer matches what's actually in the row (the
fallback text), so every untitled OpenCode-materialized session looked
"renamed" — not a one-time transition cost as the original `LinkRecord.title`
doc comment assumed, but a permanent, ongoing false positive for any session
without an explicit title. The same class of bug existed for Codex's
`threads.title` (also always falls back to a message preview or "New
conversation") — though in practice it couldn't manifest as a `pull_back`
false positive there, since `load_materialized("codex-cli", …)` reads the
rollout *file*, which never carries title data in the modern format, so a
codex-side mismatch could never be observed through that read path either
way (harmless, but the same principle applies if that ever changes).

**Fixed**: both `opencode_write::RowWritten` and `codex_write::ThreadRowReport`
now return the title actually persisted, and `sync.rs` records *that* — not
`session.title` — as `LinkRecord.title` for OpenCode (Codex's `LinkRecord.title`
deliberately still tracks `session.title` directly, matching what its
file-based read path can ever observe — see the code comment at the
`ensure_codex_row` call sites). Regression tests:
`test_untitled_session_fallback_title_is_not_a_false_rename`, and the two
existing OpenCode pull tests now assert `report.renamed.is_empty()`
explicitly instead of only checking message counts.

**Cleanup performed on this operator's real machine** (no other remediation
needed — nothing had been synced with the bad data yet, since `pull` only
writes to `~/.agentbridge/overlay/` and `manifest.jsonl`, never a materialized
copy directly):
1. Deleted all 212 unique spurious `~/.agentbridge/overlay/*.title` files
   (all timestamped from this session — the title-overlay feature didn't
   exist before today, so there was nothing legitimate to lose).
2. Left the stale-but-self-consistent `rec.title` values already written into
   `manifest.jsonl` alone — they match what's currently in each OpenCode row,
   so they cannot trigger another false positive, and self-heal the next time
   `sync` touches each session (fresh `LinkRecord`s are written unconditionally
   for every OpenCode target).
3. Verified with the fixed binary: `agentbridge pull --dry-run` now reports
   **0** renames against the same real data that produced 705 before.
4. `~/.agentbridge/manifest.jsonl` confirmed structurally intact throughout
   (19,106 lines, all valid JSON) — this was a false-positive bug, not data
   corruption.

This is exactly the class of bug §4's "unit tests mean nothing here" doctrine
exists for: `test_pull_back_recovers_a_rename` and
`test_recovered_rename_propagates_to_other_tools` (added when §2b landed)
both passed the whole time, because they only ever exercised a *titled*
fixture session — the untitled-session path was never touched until this
outside-the-sandbox pass forced it.

## 2f. `agentbridge pull` now asks when two tools both have new work — 2026-08-18

Operator request: when a session is continued in more than one tool between
pulls (write-back from Claude Code *and* Codex both waiting), let the operator
choose what happens instead of always silently merging — with a real
interactive terminal prompt. Full rationale in DECISIONS.md (2026-08-18);
summary here.

- `sync::pull_back` now groups pending write-back by session id before
  applying it. Exactly one contributing tool: unchanged, no prompt, applied
  exactly as `pull_back` always has (regression-tested:
  `test_pull_back_single_tool_new_work_is_not_a_conflict`). Two or more tools:
  a `ConflictResolver` (new trait in `sync.rs`) is asked once per session —
  `AutoMerge` (today's behavior, keep everyone) is the default for anything
  non-interactive; `pull_back_with(dry_run, resolver)` is the entry point for
  a caller that wants to choose.
- `agentbridge pull`, run from a real terminal, shows a **full-screen TUI**
  (new dependency: `ratatui` + `crossterm` — native Rust, no runtime outside
  the single static binary; the operator's first pointer was
  `github.com/ahmadawais/terminui`, evaluated and rejected: it's TypeScript,
  which would break the no-Node language decision of 2026-07-30, so `ratatui`
  is its native-Rust equivalent: double-buffered, full-screen, panel-based).
  The conflict screen (`src/tui.rs`, only ever constructed from `cmd_pull`,
  gated on `IsTerminal` like before) draws one panel per contributing tool
  with the actual new turns/rename it added, a highlighted menu (merge all /
  keep only tool X / skip), and `↑/↓`+`Enter` (or `j/k`, `Esc`/`q` to skip).
  A broken terminal falls back to `Skip` (re-ask next pull), never
  `MergeAll` (that would apply a choice nobody made). `--dry-run`,
  `--auto-merge`, and a non-TTY stdin all skip the TUI and keep the old
  merge-everything behavior — `sync`'s internal pull and `auto watch`'s pull
  are unaffected (still `AutoMerge`, still unattended-safe), just now flagging
  conflicts in their output/log so the operator knows to revisit with
  `agentbridge pull`. The `ConflictResolver` trait now carries the actual
  turns per tool (`ConflictItem`), so both the TUI and the scripted test
  resolver see what each side contributed — not just tool names.
- `KeepOnly(tool)` is permanent for that batch of turns: the discarded tool's
  manifest record still advances past the discarded turns, so re-pulling does
  not re-offer them. `Skip` is the opposite — the manifest is left untouched,
  so the same conflict is asked again next time. Both directions
  regression-tested (`test_pull_back_keep_only_discards_the_other_tool`,
  `test_pull_back_skip_leaves_manifest_untouched_and_reasks`), plus the
  default-merge path (`test_pull_back_two_tools_is_a_conflict_and_auto_merge_keeps_both`).
- **Verified live**, real terminal via `expect`, real binary, fully isolated
  sandbox (`HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
  `AGENTBRIDGE_DATA_DIR` all redirected — see §4's sandbox recipe): a real
  turn appended to a real materialized Claude Code copy and a real turn
  appended to a real materialized Codex rollout copy, `agentbridge pull` from
  a real pty rendered the full-screen ratatui UI (alternate screen, panels,
  highlighted list), arrow-selecting "keep only claude-code" left the Codex
  turn out of the overlay, the kept turn propagated into every re-synced
  copy, and the discarded turn was physically gone from every materialized
  file after the next correct `sync --project` (verified by grep across the
  whole sandbox). Re-pull is quiet — `KeepOnly` permanent. One test-runner
  stumble: a sync run *without* `--project` re-homed variants into the
  shell's CWD instead of the sandbox project — always pass `--project` in
  live runs; the "discarded turn still on disk" scare was that, not a bug.
  Cleaned up with `agentbridge unsync`, nothing left behind.
- **A real near-miss during this verification, worth remembering**: the first
  sandbox attempt overrode only `HOME`, not `CLAUDE_CONFIG_DIR` — this
  operator's shell always has `CLAUDE_CONFIG_DIR=~/.claude-mantra` set for
  real, so `sync` happily materialized ~800 real session hardlinks into two
  new subdirectories under the *real* `~/.claude-mantra/projects/`. No
  existing file was touched (hardlinks only land in *new* directories keyed
  by the sandboxed project path), and `agentbridge unsync` — run with the same
  env the sync used — removed exactly those files, matching §4's doctrine
  exactly. Lesson reinforced: **every** live-root env var
  (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, not just `HOME`) has to
  be overridden together for a sandbox to actually be one — `HOME` alone is
  not enough on a machine where the operator has redirected any tool's config
dir. Also: files materialized via `set_mtime_from_session()` (§2b) carry the
   *session's* timestamp, not "now" — `find -newermt` will not find them; check
   the manifest's `dest` paths directly instead.
- **Same session, same day — bare `agentbridge` opens a full-screen
  dashboard.** Operator ran `agentbridge` with no subcommand, got the static
  help, and asked for an interactive terminal GUI as the default entry
  point (`src/dashboard.rs`, ratatui): one table of every tool's sessions
  (tool / title / project / last-activity, provider-colored, newest first),
  a provider filter (`Tab` cycles all → each detected tool), and one-key
  actions — `s` runs a full sync pass (`pull_back` + `sync_into`, mirroring
  `cmd_sync`'s pipeline), `p` runs pull with `AutoMerge` semantics
  (deliberate: the dashboard is already a TUI and cannot nest the conflict
  screen from `tui.rs`; conflicts are reported in the status line with a
  pointer to run `agentbridge pull` in a real terminal), `↑/↓`/`j/k` move,
  `g`/`G` jump, `q`/`Esc` quit, terminal restored on any exit path. Bare
  invocation on a non-TTY stdin (pipes, cron) still prints the static help —
  a TUI can never be entered unattended. **Operator follow-up (same day):
  `agentbridge tui` (alias `dashboard`) is now an explicit subcommand, the
  header carries an ASCII bridge-between-two-agents brand mark, `u` previews
  `unsync` (dry-run counts in the status line) and `y` confirms it — the
  status panel was also clipped to zero inner rows (Length(2) + borders) and
  now renders. Live pitfalls recorded for next time: ratatui renders nothing
  in a 0×0 pty — set the window size (`TIOCSWINSZ`) in any automated pty
  harness or frames come back empty; `expect`'s log_file misses ratatui
  frames (writes bypass its pty capture) while Python's `pty` module catches
  them. Verified live in the sandbox
  through a real pty: table rendered against real fixture sessions, `s`
  produced a real manifest + links on disk, `q` restored the terminal.
- **Same session, same day — Codex fallback-title mangling fixed.** An
  untitled session synced in from another tool showed a long, mid-word-cut
  prompt fragment as its picker name in `codex /resume`: the untitled
  fallback used the 120-char preview clip, which chops words mid-way and
  looks like a data dump. `codex_write::ensure_thread_rows` now falls back to
  `short_title` (word-boundary-safe, ≤60 chars + ellipsis, whitespace
  collapsed) instead; the `preview` column keeps the long clip, the `title`
  column now reads as a name. Regression-tested
  (`test_untitled_session_gets_a_short_word_safe_title`).

## 2g. Antigravity was reporting success while syncing almost nothing — fixed 2026-08-19

**Symptom the operator asked about:** "verify the agy sessions are synced, test
the write back, fix so all sessions are accessible in agy and can be written as
well, and vice versa."

**What verification actually found.** `ls --provider antigravity` listed 12
sessions and every one loaded with exactly 1–4 messages. `agentbridge status`
showed **zero** antigravity rows, meaning nothing had ever been written into it
and nothing could be pulled back out. The ignored real-data test passed the
whole time: it asserted only `!session.messages.is_empty()`, which one message
satisfies. Measured against the real store: **12 of 24 readable sessions,
15 of 733 messages.**

Four read bugs, all verified on the operator's own databases:

| # | Bug | Consequence |
| --- | --- | --- |
| 1 | Only `~/.gemini/antigravity-cli` scanned | 12 IDE conversations (1,798 steps) invisible |
| 2 | Only the *error* field `.24.3.1` decoded | every model answer discarded |
| 3 | `preview` read as the title | a rename inside agy is invisible → title write-back dead |
| 4 | `last_modified_time` parsed as strict RFC 3339 | every summary timestamp dropped |

Bug 2 is the important one. Real model text is **`payload.20.1`** on step type
15, confirmed on all 12 CLI conversations. The old header comment claimed this
was "not yet mapped" because every recorded step 17 was a quota failure — but
step 17 is the *failure* variant; step 15 is the normal one and was being
skipped entirely. `payload.20.3` is the model's private reasoning and `.20.8`
repeats `.20.1`; only `.20.1` is surfaced, or chain-of-thought would leak into
every brief and diff.

**There are four stores, not one.** `antigravity-cli` (12 `.db`),
`antigravity-ide` (12 `.db` + 103 `.pb`), `antigravity` (101 `.pb`),
`antigravity-backup` (100 `.pb`). Same schema, non-overlapping ids, so all are
scanned and deduped by id. The `.pb` files are **encrypted** — measured byte
entropy 8.000/8.000, no valid leading wire type. The earlier note calling them
"not protobuf (wire type 7)" understated it. There is no key, so they are
skipped **silently**; reporting them would emit 200+ errors per scan.

**Where the project path comes from.** The summaries index only exists for
`antigravity-cli`, and 11 of its 12 bodies have no row in it. So
`trajectory_metadata_blob` (`.1.1` workspace URI, `.2` created) is the primary
source, not the fallback — that is what makes IDE sessions usable (12 of 12
carry a workspace URI). The 11 CLI conversations with no workspace really are
headless (`project_id = default-cli-project`), so `(unknown)` is correct there.

**The write connector** (`src/antigravity_write.rs`) reverses the 2026-08-01
"read-only first" decision. Both blockers named then are gone: model text is
mapped, and exercising the real binary proved unnecessary — the store is plain
SQLite, so a written conversation is verified by reading it back through our own
connector *and* agy's index, with no model call and no quota.

Antigravity is unlike the other three writers in two ways:

1. **Visibility needs two writes**, a conversation body *and* a
   `conversation_summaries` row. Claude Code needs only a file; OpenCode only a
   row.
2. **Payloads are protobuf**, so this is the one place agentbridge *authors*
   protobuf. The encoder writes exactly the five fields the decoder reads
   (`.1`, `.4`, `.5.1`, `.19.2`, `.20.1`/`.20.8`), so the round trip is exact
   for everything agentbridge models and agy tolerates the rest being absent.

Guards mirror `opencode_write`/`codex_write` exactly: backup before the first
*inserting* write of a run, marker column `agent_name = "agentbridge"` (verified
unused by agy — 0 of 102 rows), refuse while `agy`/`antigravity`/`Antigravity`
runs, `--dry-run`. Bodies go to a temp file and are renamed, so a crash cannot
hand agy a half-built database to open. `unsync` removes body **and** index row
together — an orphaned body would still be found by a filesystem scan.

**Merge-back into a native agy body is refused**, a stronger rule than
OpenCode's exclusion. A real body carries fields we do not decode (tool calls,
reasoning, `gen_metadata`); rewriting one with the minimal encoder would destroy
them. Enforced at the call site in `sync_into` and backstopped in
`merge_back_native`.

### Two pre-existing bugs found while wiring this

- **`main.rs` had a live `unreachable!()`** in the `resume` write dispatch. Any
  target outside the three hardcoded arms panicked on a user's machine. Now a
  reported error.
- **`codex_cli::config_home` cached `CODEX_HOME` in a `LazyLock`.** The value
  became whatever the first reader in the process saw; a later change was
  ignored for the rest of the run. Sibling connectors already re-read per call.
  This surfaced as a *flaky* `test_live_root_honors_config_dir_overrides` only
  after new sandboxed tests changed test ordering — the suite had been hiding it
  by accident of ordering, exactly the failure mode §5 warns about. Confirmed
  pre-existing by stashing this branch and re-running.

`Sandbox` now also sets `ANTIGRAVITY_HOME`. Without it a sandboxed test writes
into the operator's real `~/.gemini` store — the same isolation gap already
documented for `CODEX_HOME`.

### Live verification (copy of the real store, all five env vars redirected)

1. `sync` wrote 26 conversations (12 → 38 bodies), took an automatic backup,
   left all 102 of agy's own rows intact.
2. `status` showed antigravity rows with matching counts; appending one step
   showed `+1` drift.
3. `pull` recovered **both** the appended turn and a rename made in agy's index.
4. A following `sync` propagated both into the Claude Code copy — the full
   round trip, agy → overlay → other tools.
5. Three more `sync` passes left the count at 26 — no feedback loop.
6. `unsync` restored exactly 12 bodies / 102 rows, all 12 original bodies
   byte-identical (`cmp`), recovered work preserved in the overlay.
7. The operator's real store was confirmed untouched: 0 marked rows, no backup
   files, 12 bodies, 102 summary rows.

Test count went 97 → 128 (10 connector, 13 write, 6 sync, plus the strengthened
real-data test which now asserts model responses decode, not just "not empty").

## 2h. One session, four picker rows, nothing tying them together — 2026-08-19

Operator asked for "agent name + session name + date and time + session id" in
each tool, with the **exact** session date rather than the sync date, so one
conversation can be tracked across agents. Then corrected the first draft: a
session that already has a name keeps it — only unnamed sessions get one.

Implemented in `src/label.rs`, applied in `sync_into` immediately after
`fold_overlay` and before every write, so all four copies carry one string:

```
claude-code · My Important Session · 2026-08-19 10:00 · aaaaaaaa
provider    · name                 · started_at       · id[..8]
```

**The three things that make this dangerous, and how each is handled.** The
title is not a cosmetic field here — `pull_back` compares the title it wrote
against the title it reads back to detect renames, so anything unstable in the
label reports every session as renamed on every pull (the 705-false-rename
regression, §2e):

- **Timestamp is `started_at`, never `now`,** and always UTC. Sync time would
  change every run; local time would change with the machine's timezone.
- **Labeling is idempotent.** A rename made inside a tool arrives *as a labeled
  title*; `apply` strips the old label first and rebuilds around the user's new
  name rather than nesting.
- **An existing name is verbatim.** Only a name agentbridge derives for an
  unnamed session is clipped (word-safe, deterministic, marked with `…`). The
  metadata fields are never truncated — a cut id or date defeats the point.

`provider` and `id` are the **origin's**, which is what makes one session yield
one identical label in every tool: `sync_into` re-homes `project_id` per target
but leaves those two alone. A test asserts the label set for one session has
size 1.

`parse` demands all four fields be well formed, so a user's own title is never
mistaken for a label and rewritten. Covered: titles containing the separator,
unknown provider, malformed stamp, short id.

**Session ids are not always UUIDs.** Claude Code derives one from the filename
stem, so `renamed-in-claude-code` is a real id, and a UUID's first 8 characters
can themselves contain `-`. The first id check was alphanumeric-only, rejected
those labels, and `pull_back` immediately read agentbridge's own label as a
foreign rename. An existing sync test caught it.

### A duplication bug in the antigravity write path

Found while verifying labels: the antigravity branch **appended** manifest rows
instead of updating them, so re-syncing grew the manifest 52 → 78 → 104 across
three runs. `pull` then read one session as several tools' worth of new work and
reported a conflict against itself:

```
1 session(s) had new work from more than one tool:
  aaaaaaaa-…  antigravity+antigravity+antigravity -> merged
```

The file targets already solved this in their `unchanged` branch (update the
row in place, count it unchanged); the antigravity branch now does the same.
Regression test asserts one row per (session, dest) and a stable count across
re-syncs. Worth noting the *symptom* pointed at the conflict resolver and the
*cause* was in manifest bookkeeping two layers away.

### Verified on a copy of the real store

A named claude session kept `My Important Session` exactly; unnamed agy
conversations got word-safe derived names; every label carried the conversation's
real date (2026-08-18/19), never the sync date; the identical label appeared in
both agy's index and the Claude Code copy; a rename made inside agy was
recovered **bare** into the overlay (not labeled) and republished with the label
rebuilt around it; three sync+pull cycles produced a stable 52-row manifest and
zero renames. Tests 128 -> 143.

## 2i. Phase 1 safety layer — 2026-10-05

Redaction, a run lock, atomic state writes, and a durable marker. Full
reasoning (including rejected options) in DECISIONS.md 2026-10-05; the user-facing
model is `SECURITY.md`. What a cold reader needs:

- **`src/redact.rs`.** `Redactor::load()` (defaults + `~/.agentbridge/redact.rules`)
  is called at the top of `sync_into`, `pull_back_with`, `cmd_resume`, and for
  briefs. `redactor.session(&mut s)` runs right after `source.load()` in
  `sync_into`, *before* `fold_overlay`, so overlay dedup compares redacted text
  with redacted text. A bad rules file returns an error and writes nothing
  (`sync` exits 2). `--no-redact` is a global flag, refused for `auto watch`.
- **Do not redact native files.** `merge_back_native` and same-tool `resume`
  rewrite a tool's own file. They use `ClaudeCodeConverter::unmarked()` /
  `CodexCliConverter::unmarked()` and skip the redactor on the loaded session.
  Redacting or marking there silently damages a real session.
- **`src/lock.rs`.** `lock::acquire_default()` at command level (`main.rs`
  `lock_or_exit`, watch round, dashboard actions). Not inside the library
  functions: it is not re-entrant. Dry runs and read-only commands never lock.
- **`sync::write_atomic`** for the manifest, overlay and title overlay.
- **`src/marker.rs`.** Extra `agentbridge` JSON key on the Claude `mode` record
  and Codex `session_meta` payload. `sync_into` treats marked files as
  never-a-source; `status` lists orphans; `unsync --orphans` removes unmodified
  ones.
- **Re-scrubbing.** Copies are rewritten in place on every sync, so running
  `sync` after upgrading removes secrets from copies written by older versions.
  Verified live: `--no-redact` copies the key, a following normal `sync` removes
  it from the cache and the hardlinked live copies.

**Verified live (2026-10-05, sandbox, real binary, all six store env vars
redirected, the repo's secret fixtures):** every planted secret appears only in
the two seeded originals; every copy carries `[REDACTED:…]`; eight simultaneous
`sync` processes left a valid 8-row manifest with no duplicates and no temp
files; a lock held by another process made `sync` wait 2.6s then succeed while
`--dry-run` returned in 0.1s; a deleted manifest left no copies-of-copies and
`status` listed the 8 orphans; an invalid rules file wrote nothing and exited 2;
a user rule applied. Test count 145 -> 186 (208 after §2j).

**The overlap was a real bug, not a theoretical one.** Eight simultaneous
`sync` processes (what a burst of new terminals with the shell hook does) on the
pre-lock code left a manifest of 78, 100 and 148 lines across three rounds, with
36-50 destinations listed more than once; the correct count for that fixture set
is 40. With the lock: 40 lines and 0 duplicates in all three rounds.

**Not verified:** (1) the marker against a real `claude`/`codex` — neither is
installed on this development machine; use §4's zero-cost signals. (2) Redaction
throughput on a real multi-GB store (measured: ~380 MB/s clean, ~16 MB/s dense
secrets, release build, synthetic text).

**Found, not fixed:** `non-utf8` and `hyphens-and-spaces` fixtures are scanned
but fail `load()` by id, so sessions like them are not synced (pre-existing).

## 2j. Phases 0 and 2 — 2026-10-05

**A data-loss bug, found by a test written to check a hunch.** Sync always also
targets `$HOME`. A Claude Code session *native* to `$HOME` already lives at the
exact path its `$HOME` copy would take, so syncing from any other folder
replaced the user's real transcript with a lossy conversion of itself (and
`unsync` would later have deleted it). Pre-existing, in every release before this
one. Fixed with a general rule: **never write onto a file agentbridge did not
create** (tracked in the manifest or carrying the marker). Regression test
`test_sync_never_overwrites_a_native_session_living_in_home`. Anyone who ran an
earlier version and had sessions started in `$HOME` should check them: such a
file is now marked as agentbridge's, so `unsync --orphans` would treat it as
removable.

**`start` / `inject` / `clean`** (`src/inject.rs`). The `Connector` trait changed
(a core change, but the right one): connectors now only declare
`instruction_file(project)` and `launch_program()`; one shared `write_fenced`
does the work. Block is `<!-- agentbridge:begin v=1 pad=N created=B -->` …
`<!-- agentbridge:end -->`; `pad` and `created` are recorded in the marker so
`clean` restores byte-for-byte with no side record. Verified: `clean` is
byte-identical for empty / no-trailing-newline / unicode files; deletes a file it
created; refreshes in place; follows symlinks (people link `AGENTS.md` to
`CLAUDE.md`); preserves file mode; refuses unbalanced fences and non-UTF-8 files;
neutralises marker text inside the brief. Live-run in a sandbox with a stand-in
`claude` on `PATH`: arguments after `--` pass through, cwd is the project, the
agent's exit code is returned, a missing binary leaves the context in place with a
clear message. The brief is built from the project's recent sessions in the
*other* tools only (not all 11k), skips agentbridge's own copies, is redacted, and
is capped at 8 KB. Files: Claude Code `CLAUDE.md`; Codex, OpenCode, Antigravity
`AGENTS.md`. **None of those four is verified against the real agent**;
Antigravity's is a guess. `find_session` also no longer gives up the whole search
when one session fails to load.

**`scan()` reads the tail** (`connectors::tail_records`). It used to set
`last_event_at` to the session's *start* time, contradicting SPEC §5, so `ls`
order and "most recent" were wrong, and a rename made after the first record was
invisible. Cost measured on a synthetic 6,000-session, 1.1 GB store: 6.2 s vs
5.9 s. (The baseline scan being ~1 ms per file is its own pre-existing cost, worth
a look before the watch loop is trusted on a store that size.)

**`sync --all-known`**: syncs every project directory sessions have used that
still exists. Refuses without `--yes` (or `--dry-run`), because it re-reads the
history once per directory and gives OpenCode a copy per project.

**Phase 0:** `.github/workflows/ci.yml` (build, `clippy -D warnings`, tests plus a
check that nothing is written outside the test sandbox, smoke test, and a Rust
1.89 build), `scripts/sandbox.sh` (all six env vars), `scripts/smoke.sh` (13
end-to-end checks of the built binary). README no longer claims Windows or
overstates `start`. **The CI file has never run** (no GitHub access from the dev
machine); its two shell steps were exercised locally, and MSRV 1.89 is assumed
from the APIs used, not built.

**Deferred, on purpose:** mapping Antigravity tool-call steps (needs real protobuf
data to reverse; Phase 5), a Windows port (README now says unsupported),
`resume` undo log (`unsync` covers it).

## 2k. Phase 3 — test depth, and what it found — 2026-10-05

Writing the spec's missing tests (round-trip, scan/load consistency, read-only,
locks) turned up four real defects. None was visible to the existing suite, which
only ever read the repo's own synthetic fixtures.

1. **The Claude and Codex readers dropped tool calls from real sessions.** The
   writers emit the real format (tool calls as `tool_use` content blocks, results as
   `tool_result` blocks, Codex `function_call` / `function_call_output`), but the
   readers only understood the fixtures' synthetic shape. A real tool call read
   back as an empty turn with no name or input; a tool result as an empty *user*
   message; Codex reasoning and calls as empty assistant turns. Every real session
   synced across tools carried this. Measured on a real machine (23 Claude
   sessions): 0 tool calls decoded before, ~8,100 after. Readers now map the real
   blocks, pair results to calls (by `tool_use_id` / `call_id`), and skip
   thinking-only records (about one assistant record in six on that machine; the
   model's private reasoning, not a turn).
2. **Fixing (1) created a new hazard, handled in the same change.** Once calls are
   real, converters must pair every result to its call or the real tool rejects the
   transcript on resume. They kept one "pending id", which breaks on parallel tool
   calls (common). `convert::pair_tool_turns` now pairs by tool name, else in order,
   and drops true orphans (an interrupted call, a result cut off by compaction).
   Verified on real data: every transcript converted from 23 real sessions pairs up
   under both converters; 2 of 8,135 calls were orphans. **Still unverified against
   the real `claude` / `codex` resuming such a transcript.**
3. **Codex `load()` could not load some sessions `scan()` listed** (rollout filename
   and in-file id disagreed). `load` now falls back to the in-file id, so anything
   listed can be loaded. Test: every scanned fixture loads.
4. **A locked foreign database stalled reads for 5 s each** (rusqlite's default
   busy timeout, and `load` opens a connection per session). Now 250 ms
   (`connectors::open_read_only`), so a held lock becomes an ordinary per-session
   error. Lock tests: 10 s -> 0.5 s.

New tests: seeded randomised round trips for both converters (60 sessions each,
adversarial text), hand-authored real-shape records for both readers (independent
of our writers), a fuzz of tool-call shapes asserting pairing, a read-only test that
snapshots every byte of all four stores around every read-only operation (and again
with the tree chmod'd read-only; mutation-checked), concurrency tests (appending
writer with torn lines; WAL writer mid-transaction; exclusive lock), a generated
100 MB session (scan 0.1 ms, full load 135 ms, release build).

Ignored tests worth running on a real machine: `cargo test --release --lib --
--ignored test_real_sessions test_perf`. The real-data ones assert properties of the
content (tool calls decode, <5% empty turns, every converted transcript pairs), not
"not empty", and print counts only.

**Caveats.** Upgrading changes message counts for real sessions (tool calls now
count; thinking records no longer do). `pull` is unaffected (it compares a copy to
what was written, both read by the same reader) and the next `sync` refreshes
copies with real tool history. The pairing drops orphans rather than inventing
results. Not done from the spec's unit list: path canonicalization (symlinks,
worktrees, case-insensitive filesystems) — the `Project` model that would carry it
is not wired in yet (Phase 4).

## 2l. Phase 4 — index, brief, MCP, optional model — 2026-10-05

The original SPEC §6 M3/M6 on top of the sync tool. Reasoning and rejected options
are in DECISIONS.md; what a cold reader needs:

- **`src/store.rs`** — `~/.agentbridge/index.db` (SQLite, WAL, FTS5/Porter).
  `Store::open` migrates; `refresh(store, registry, redactor, only_provider, progress)`
  is the incremental fill from the tools' stores. Text is redacted and truncated at
  insert (`index_session`). It is a *cache* (DESIGN Rule 1 amended).
- **`src/brief.rs`** — `build` (extract) → `render` (budget by re-measuring) →
  `brief_for_project` (cache). `count_tokens` is a fixed conservative proxy.
  Weights live in `build`; `Item::new` neutralises `[`/`]` in extracted text.
- **`src/mcp.rs`** — `Server::handle` is a pure `Value -> Option<Value>`;
  `serve` is the stdio loop. `agentbridge mcp` runs a background refresher thread
  with its own SQLite connection.
- **`src/llm.rs`** — `LlmProvider` trait, `CommandProvider` (prompt on stdin, no
  shell, 256 KB output cap, timeout), `validate` (shape, citations, budget),
  `summarise` (always falls back to the plain brief).
- **Commands:** `index [--provider] [--rebuild]`, `search`, `brief [--since]
  [--budget] [--no-refresh] [--llm-cmd]`, `fact`, `mcp`; `start` / `inject` use
  the brief; `auto watch` refreshes the index.

**Verified:** index of 24 real Claude sessions (19,867 messages) built in 6.8 s,
8.2 MB; incremental run 0.0 s; search instant; real-data brief reviewed by eye
(and six quality bugs fixed from it, see DECISIONS); the real binary answers a
handshake, `get_brief` and `search_history` over a pipe against real data; 300
tests; smoke script covers index/search/brief/fact/MCP/`--llm-cmd` fallback.

**Not verified:** (0) see §2m for what a real Claude Code did and did not confirm;
(1) the MCP server against any real agent's MCP client (only a
hand-written one); (2) `--llm-cmd` against a real model (only a recording mock, a
`cat` echo and failure/timeout commands); (3) the brief's usefulness beyond one
machine's sessions: it is heuristic, so expect misses and the odd false positive;
(4) first-build time on a very large store (11k sessions): extrapolating the
measured ~0.3 ms/message suggests minutes, untested; (5) the `start` brief reaching
each agent (still the Phase 2 caveat).

**Known gaps:** no optional export to an external memory MCP server; `index` has no
`--since`; brief sections are English-only (cue words); search has no phrase or
prefix syntax by design; path canonicalization (symlinks/worktrees as one project)
is still not done: `sessions_for_project` matches the recorded path.

## 2m. Real-binary verification and Antigravity tool calls — 2026-10-05

- **A real Claude Code is on the dev machine**: the VS Code extension's bundled
  binary (`~/.vscode/extensions/anthropic.claude-code-*/resources/native-binary/claude`).
  Use it in a sandbox with `HOME` + `CLAUDE_CONFIG_DIR` redirected (it has no
  credentials there, so it cannot call the API). Verified against 2.1.289: a
  converted copy with the marker resumes like one without it; junk and the old
  invented schema are rejected (`No conversation found`).
- **Still unverifiable without a login**: the API accepting our tool-call pairing,
  and the model reading the injected `CLAUDE.md`. Still unverified entirely: Codex,
  OpenCode and Antigravity as *consumers* of our copies.
- **Antigravity tool calls** (step type 132) are now decoded; layout and the
  verification in CONNECTORS §7.6. 71 calls / 71 results on the real conversation.
- `scripts/` has no helper for the real-claude check yet; the recipe above is the
  whole of it (sandbox env, `claude --resume <uuid>` from the project directory).

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
  redact.rs     secret redaction applied to every derived copy (fail closed)
  store.rs      persistent SQLite index (FTS5), facts, brief cache
  brief.rs      extractive cited brief within a measured token budget
  mcp.rs        MCP server over stdio (search_history, get_brief, get_session, record_fact)
  llm.rs        optional model pass via a user-supplied command; validated output
  lock.rs       one-writer-at-a-time advisory lock in the data dir
  marker.rs     durable "agentbridge wrote this" key inside generated files
  sync.rs       cache, hardlink fan-out, manifest, pull_back, status, unsync
  convert.rs    native-format writers (Claude Code, Codex) + brief builder
  label.rs      the cross-tool session label written into every target's
                title: provider · name · started_at · id[..8]
  opencode_write.rs  SQLite write path for OpenCode (backup, tags, PID guard)
  codex_write.rs     `threads` index rows so Codex's picker lists them
  antigravity_write.rs  SQLite body + summaries row for agy; the only place
                 agentbridge *authors* protobuf (same gates as the above)
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
- **"Not empty" is not an assertion.** The antigravity real-data test asserted
  `!messages.is_empty()` and passed for weeks while 718 of 733 messages were
  being silently dropped (§2g). A test that cannot distinguish 1 message from
  40 is not testing the decoder. Assert a *property of the content* — that
  model responses decode at all, that a count is plausible.
- **One store is an assumption, not a fact.** Antigravity keeps four separate
  stores under `~/.gemini/`; the connector hardcoded one and silently missed
  half the readable sessions (§2g). Before believing a tool has a single
  session directory, list its config root and check the siblings.
- **Env-var caching turns config into first-reader-wins.** `codex_cli` held
  `CODEX_HOME` in a `LazyLock`, so whichever code path read it first fixed the
  value for the whole process. It hid as a *passing* test until unrelated new
  tests changed ordering. Resolve env overrides per call; a `getenv` is cheap.
- **High entropy means encrypted, not "unknown format".** 200+ antigravity
  `.pb` bodies were logged as an unmapped protobuf variant. Measuring byte
  entropy (8.000/8.000) settled it in one command — no schema work would ever
  have read them. Measure before mapping.

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

**`HOME` alone is not enough — override every store env var.** The connectors
honor `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `ANTIGRAVITY_HOME`, so if the
operator's shell exports any of them, a "sandboxed" run writes into their real
store. The full set:

```bash
env HOME=$SB AGENTBRIDGE_DATA_DIR=$SB/.agentbridge \
    CLAUDE_CONFIG_DIR=$SB/.claude CODEX_HOME=$SB/.codex \
    ANTIGRAVITY_HOME=$SB/agy-store \
    ./target/debug/agentbridge sync --project $SB/work
```

`Sandbox::new()` in `sync.rs`'s tests sets all of these for the same reason.
Note `ANTIGRAVITY_HOME` redirects the *write* target but the read scan still
covers the real homes by design — for a fully isolated write test, redirect
`HOME` too (that is how §2g was verified).

**⛔ Never invoke the real `codex` binary directly for testing, even with
`CODEX_HOME` set — it is not a full sandbox for that tool.** Confirmed
2026-08-14 (§2d): a `CODEX_HOME=<sandbox> codex exec …` run still touched the
operator's real `~/.codex/state_5.sqlite` (mtime moved within seconds of the
run; row count and content were unaffected, but that was luck, not a
guarantee). `claude` and `opencode` *did* fully respect their equivalent
overrides (`CLAUDE_CONFIG_DIR`, `XDG_DATA_HOME`) throughout the same session
— this is specifically a `codex` gap, not a pattern to expect elsewhere.
agentbridge itself never shells out to any of these binaries (`resume_cmd()`
only *prints* a suggested command for the operator to run themselves — see
`src/convert.rs`), so this risk is entirely about how a *session testing this
codebase* behaves, not a bug reachable through any agentbridge code path.
To verify `codex_write.rs` against real data, copy `~/.codex/state_5.sqlite`
into the sandbox and let **agentbridge** write into the copy — never drive
the real `codex` binary against it.

**Zero-cost signals for checking a tool accepted a session** (no model call,
no cost):

- Claude Code: `claude --resume <id>` → `No conversation found` means
  rejected; `No deferred tool marker found…` means it loaded the session.
  Run it **from the session's own directory** — resume is cwd-scoped.
- Codex: `codex delete <id> --force` → `Deleted session` means recognized;
  `Error: failed to delete session` means not. (Destructive — synthetic files
  only.)
- OpenCode: `opencode session list` **from the folder under test** prints the
  picker's own view — non-interactive, no model call, and the only honest check
  that a row is visible where you think it is. `grep -c <id>` on it also catches
  the double-listing a second row in the same project would cause.
- Wrap CLI probes in a timeout; macOS has no GNU `timeout` (`scripts`-free
  stand-in: run the command in the background and `kill -9` it from a
  `sleep` subshell).

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

**§2b/§2c/§2d done — re-verified live 2026-08-14** (sandbox, real `claude`/
`opencode`/`codex` binaries, plus a copy of the operator's real
`state_5.sqlite` schema for the Codex title upsert): title write-back, mtime,
invariant 2, `ClaudeCodeConverter::convert_multi`, and the Codex
`threads.title` fix all confirmed. **Still open, next in line:**

1. **A real `codex resume` picker pass**, once convenient — §2d verified the
   `threads.title` column directly via SQL against a copy of the schema (safe,
   given the `CODEX_HOME` isolation gap §2d documents); nobody has yet
   confirmed the real picker UI actually renders that column as the
   displayed title rather than `preview`/`first_user_message`. Needs a real
   authenticated `codex` session (state_5.sqlite only fully initializes on
   one) — do this on the operator's own machine, not a fresh sandbox.
2. ~~**`agentbridge list`'s title lag.**~~ **Fixed 2026-10-05** (§2j): `scan()`
   now also reads the last 32 KiB, so a recent rename and the true last-event
   time show in `list`. A rename more than 32 KiB from the end of a very long
   session is still only seen by `load()`.
3. ~~Claude Code per-directory guard.~~ **It was needed, and worse than
   suspected** (§2j): a Claude session native to `$HOME` was overwritten by a
   converted copy of itself. Fixed with a general never-overwrite-foreign-file
   guard.

0. **Re-verify the 2026-08-03 OpenCode fix live.** The
   per-project id + manifest key changes pass 78 unit tests and nothing else;
   §4's first line applies. (The 2026-08-11 sandbox pass above proved the
   write path and picker visibility against real DB bytes and the real binary;
   what remains is the same run against the live database with OpenCode
   closed.) The exact sequence that failed before:

   ```bash
   cargo install --path . && agentbridge --version     # must print 0.3.4+
   agentbridge resume c6f114a0-3e7b-40c4-9d55-64df6b468426 opencode \
     --project /Users/harry/Documents/ab-crosstest       # → global project
   agentbridge resume c6f114a0-3e7b-40c4-9d55-64df6b468426 opencode \
     --project /Users/harry/Documents/agentbridge        # → own worktree; this
                                                         #   died on UNIQUE
   cd /Users/harry/Documents/agentbridge && opencode session list  # expect 1 hit
   cd /tmp && opencode session list                                # expect 1 hit
   ```

   Then check the same session is listed **once**, not twice, from a `global`
   folder, and that the pre-0.3.4 row (`ses_ab…` keyed on the session alone)
   was reclaimed rather than left beside the new ones. `/Users/harry/Documents/
   ab-crosstest` is the throwaway folder used for the 08-03 run; it and the
   probe rows come out with `agentbridge unsync` (never `rm -rf`).

1. ~~**Decide the folder-coverage story**~~ — **decided 2026-10-05**: keep the
   shell hook as the default, add opt-in `sync --all-known [--yes]` (§2j). The
   original discussion, kept for the reasoning:  the open product question behind
   §2a. Today a session reaches a folder only when sync was pointed at it
   (`sync --project X`, or the shell hook running `agentbridge sync` in each
   new shell), plus `$HOME` — which for OpenCode means every non-worktree
   folder for free, and for Claude Code means only `$HOME` itself. Options:
   fan out to every known project directory (cheap for Claude Code — hardlinks,
   same inode; expensive for OpenCode — each row duplicates every message and
   part row, and the database is already 278 MB for 148 sessions), or keep the
   shell hook as the answer and document it. Not decided.

2. ~~**Durable marker in generated files**~~ — **done 2026-10-05** (§2i), but
   **not yet verified against real `claude`/`codex` binaries** — do that first.
3. ~~**Antigravity write path + model-text mapping**~~ — **done 2026-08-19**
   (§2g). Model text is `payload.20.1` on step type 15; the write connector is
   `src/antigravity_write.rs`. Remaining antigravity work, if wanted:
   decrypting the `.pb` bodies (201 conversations in the desktop/backup stores
   are unreadable without a key — the embedded Cortex protos in
   `Antigravity/resources/bin/language_server` may help, but the blocker is the
   *cipher*, not the schema), and mapping tool-call steps (type 21, 258
   occurrences) into `Message::tool_name`/`tool_input`, which currently decode
   as no turn at all.
4. ~~**Redaction**~~ — **done 2026-10-05** (§2i). `src/redact.rs`, fail closed.
5. **Kilo Code / other connectors** — as requested, on their own
   `CONNECTORS.md` sections with verified formats first.
6. **Live verification of `start`/`inject`** against the *real* agents: confirm
   each actually reads the file (Antigravity's is the least certain). The launch
   path itself was verified with a stand-in program (§2j).
7. Topic threading — grouping sessions across tools by subject rather than
   project path (`DESIGN.md` §10).

## 7. Repo hygiene

- Public: `https://github.com/45Harry/agentbridge`. Work lands on `develop`;
  `master` is the release branch (merged via PR).
- Keep tests green (300 + 5 ignored); add a regression test for every bug, and
  verify format changes against the real binary before believing them.
- Never commit session data. `~/.agentbridge` is never the source of fixtures.
- Ignored tests are the real-data checks: run them explicitly after any
  connector change (`cargo test -- --ignored`) — they are cheap and catch
  exactly the class of bug §4 warns about.
- Watcher on the operator's machine: `ps aux | rg "agentbridge auto"`.
