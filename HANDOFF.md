# HANDOFF.md

Pick this project up cold — new machine, new session, no memory of prior
conversation. Read this, then `DESIGN.md` (architecture and why), then
`CONNECTORS.md` (each tool's on-disk format).

**Last updated:** 2026-08-18.

## 0. Start here

```bash
git clone git@github.com:45Harry/agentbridge.git
cd agentbridge
cargo build && cargo test      # 97 tests pass (+2 ignored: live-verification suites)
cargo run -- init              # read-only: what's on this machine
```

Requires Rust edition 2024. Binary version is 0.3.4.

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
one tool are pulled into the overlay and republished. See DECISIONS.md
(2026-08-01) for why native *picker listing* was dropped as a requirement.

## 2. Current state — verified live 2026-08-01

**On the operator's machine, the loop is running and proven:**

- `auto watch` (PID noted in §7) re-scans every 15s; `init` reports 50
  sessions across 4 tools (Claude Code, Codex CLI, OpenCode, Antigravity CLI).
- A fake Codex session dropped into `~/.codex/sessions/` surfaced as a
  UUIDv5 claude artifact in `~/.claude/projects/<encoded-dir>/` within 35s,
  then cleaned up.
- Antigravity: read connector verified against the two real CLI conversation
  databases — both load (user text at `.19.2`, model errors at `.24.3.1`).
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
- Antigravity write connector + successful model-response text mapping (only
  error text at `.24.3.1` is mapped) — deferred until the CLI's model quota
  resets. Read-only is safe by construction: no `live_root`/converter for
  antigravity in sync.rs, so sync never writes into it.
- `start`/`inject` (cross-tool brief injection) exist; the brief builder is
  unit-tested, live-agent launch verification is light.
- Redaction (`SPEC.md` §3) not implemented.

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
- **Same session, same day — Codex fallback-title mangling fixed.** An
  untitled session synced in from another tool showed a long, mid-word-cut
  prompt fragment as its picker name in `codex /resume`: the untitled
  fallback used the 120-char preview clip, which chops words mid-way and
  looks like a data dump. `codex_write::ensure_thread_rows` now falls back to
  `short_title` (word-boundary-safe, ≤60 chars + ellipsis, whitespace
  collapsed) instead; the `preview` column keeps the long clip, the `title`
  column now reads as a name. Regression-tested
  (`test_untitled_session_gets_a_short_word_safe_title`).

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
  sync.rs       cache, hardlink fan-out, manifest, pull_back, status, unsync
  convert.rs    native-format writers (Claude Code, Codex) + brief builder
  opencode_write.rs  SQLite write path for OpenCode (backup, tags, PID guard)
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
2. **`agentbridge list`'s title lag.** Documented in §2b as a narrower,
   known gap: `scan()` stops at the first `cwd`-bearing record, so a
   mid-conversation rename won't show in `list` until `load()` runs (sync/pull
   are unaffected). Worth deciding whether that's acceptable long-term or
   `list` needs its own fix.
3. Now that `ClaudeCodeConverter` has a real `convert_multi`, check whether
   Claude Code needs the same per-directory "already natively visible, don't
   duplicate" guard Codex has (the `is_codex`-specific block in
   `sync_into`) — not proven necessary yet (Claude Code sessions don't
   multi-file the way Codex rollouts can), but worth a second look now that
   more directories actually get copies.

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

1. **Decide the folder-coverage story** — the open product question behind
   §2a. Today a session reaches a folder only when sync was pointed at it
   (`sync --project X`, or the shell hook running `agentbridge sync` in each
   new shell), plus `$HOME` — which for OpenCode means every non-worktree
   folder for free, and for Claude Code means only `$HOME` itself. Options:
   fan out to every known project directory (cheap for Claude Code — hardlinks,
   same inode; expensive for OpenCode — each row duplicates every message and
   part row, and the database is already 278 MB for 148 sessions), or keep the
   shell hook as the answer and document it. Not decided.

2. **Durable marker in generated files** so orphans can never be mistaken for
   real sessions (the §4 footgun).
3. **Antigravity write path + model-text mapping** — post-quota: map
   successful type-17 response text (error text is `.24.3.1`; success location
   unknown), then materialize foreign sessions into
   `~/.gemini/antigravity-cli/conversations/`. Re-read `CONNECTORS.md` §7
   before starting; the embedded Cortex protos in
   `/opt/Antigravity/resources/bin/language_server` may map payloads faster
   than black-box probing.
4. **Redaction** (`src/redact.rs`, doesn't exist) — `SPEC.md` §3 requires it
   before anything is written or sent. Fail closed.
5. **Kilo Code / other connectors** — as requested, on their own
   `CONNECTORS.md` sections with verified formats first.
6. **Live verification of `start`/`inject`** against a real agent launch.
7. Topic threading — grouping sessions across tools by subject rather than
   project path (`DESIGN.md` §10).

## 7. Repo hygiene

- Public: `https://github.com/45Harry/agentbridge`, branch `master`.
- Keep tests green (95 + 2 ignored); add a regression test for every bug, and
  verify format changes against the real binary before believing them.
- Never commit session data. `~/.agentbridge` is never the source of fixtures.
- Ignored tests are the real-data checks: run them explicitly after any
  connector change (`cargo test -- --ignored`) — they are cheap and catch
  exactly the class of bug §4 warns about.
- Watcher on the operator's machine: `ps aux | rg "agentbridge auto"`.
