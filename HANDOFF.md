# HANDOFF.md

Session continuity note. If you're picking this project up on a different
machine (new Claude Code / Codex / etc. session with no memory of the prior
conversation), read this file first, then `SPEC.md` (the original build
brief), then `DECISIONS.md` (choices already locked in), then `CONNECTORS.md`
(reverse-engineered provider formats).

**Last updated:** 2026-07-30.

## Where things stand

Scaffolding stage. **No connector implementation has started yet.** What
exists:

- Rust project (`Cargo.toml`, edition 2024), builds clean (`cargo build`).
- `src/model.rs` — the normalized data model: `Project`, `RawSession`,
  `Session`, `Message`, `Artifact`, `Fact`, `Provenance`. Matches `SPEC.md`
  §5 exactly.
- `src/connector.rs` — the `Connector` trait
  (`id/detect/roots/scan/load/resume_cmd/inject`), `Registry`,
  `ConnectorError`, `InjectTarget`, `SessionStream`. `scan()` is a lazy
  streaming iterator that yields `Err` per-session rather than aborting a
  whole scan on one bad file — this was a deliberate design choice per the
  spec's crash-safety hard constraint.
- `src/connectors/mod.rs` — empty registry (`all()` returns zero connectors).
  This is the single file (+ one new connector file) that changes per
  provider added.
- `DECISIONS.md` — Rust over TypeScript (single-binary distribution,
  `rusqlite` bundled FTS5, sync core / async only in M6-M7), MIT license,
  SQLite forward-only migrations, why `agy` is deferred to a research spike.
- `CONNECTORS.md` — real, empirically-verified findings for all four
  providers (see below), plus a cross-tool resume interoperability test
  (§5 of that file).
- Logo (`assets/logo.svg`, `assets/logo-wordmark.svg`), embedded in
  `README.md`.
- Repo: **public**, `https://github.com/45Harry/agentbridge`, pushed to
  `master`.

## Blocking: awaiting sign-off before M1

Per `SPEC.md` §9 the operator asked to review two things before any parser
gets written. **As of this note, neither has been explicitly confirmed yes —
do not assume approval, ask first if picking this back up:**

1. **The `Connector`/`Session`/`Fact` interface** in `src/model.rs` /
   `src/connector.rs` — presented for review, no explicit "yes, proceed" on
   record yet.
2. **The M1 fixture list** (one of each per provider: normal multi-turn,
   tool-calls-with-large-output, compacted/summarized, embedded-fake-secret,
   truncated-final-line, legacy-timestamp-format, hyphens-and-spaces-path,
   non-UTF-8, empty-file, 100MB-perf) — also presented, not yet confirmed.

If the operator says "go ahead" / "looks good" / equivalent in a fresh
session, that counts as sign-off — proceed to M1 (Claude Code + Codex CLI
connectors, `tests/fixtures/claude-code/` and `tests/fixtures/codex-cli/`).

## Key findings so far (full detail in CONNECTORS.md)

- **Claude Code**: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`. Directory
  encoding is confirmed lossy in practice — never decode it, always read
  `cwd` from inside records.
- **Codex CLI**: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`,
  date-partitioned. `history.jsonl` and two `.sqlite` files in `~/.codex/`
  are still uncharacterized — check before M1 assumes `sessions/` is the only
  relevant data.
- **OpenCode**: SQLite at `~/.local/share/opencode/opencode.db` (not JSONL —
  the odd one out). `session` table stores the **plain, unencoded** cwd in a
  `directory` column — no lossy-encoding problem here. IDs are `ses_...`
  strings, not UUIDs.
- **agy (Antigravity CLI)**: storage location **still unknown** — this
  blocks M2's `agy` connector. It's a separate Go binary from the Antigravity
  IDE app; do not assume they share a session store without verifying.
  Needs a dedicated research spike (trace a real `agy` run, or check
  `antigravity.google/docs/cli/reference`) before writing a parser.
- **Cross-tool resume was tested empirically and does not work** — confirmed
  each tool only resolves IDs against its own local storage; Claude Code and
  OpenCode fail cleanly/immediately on a foreign ID, Codex's headless
  `exec resume` and `agy --conversation` do not visibly validate up front
  (agy in particular hung for 8s on an invalid ID + prompt during testing —
  treat probing it with real prompt content as potentially cost-incurring
  until proven otherwise). This is why M4 (brief injection) and not literal
  session transfer is the right approach for cross-tool continuity.

## Next steps (once M1 gets sign-off)

1. Write `tests/fixtures/claude-code/*.jsonl` and
   `tests/fixtures/codex-cli/*.jsonl` per the fixture list above — synthetic,
   hand-authored, never copied from real session data.
2. Implement `src/connectors/claude_code.rs` and `src/connectors/codex_cli.rs`
   against those fixtures.
3. Wire SQLite storage (`schema_version` table + `migrations/0001_init.sql`)
   and `agentbridge index` / `agentbridge ls --project . --provider all`.
4. Redaction pass (`src/redact.rs`, not started) needs to exist and run
   before anything touches the DB — see `SPEC.md` §3 hard constraint and the
   "Safety tests (must-fail tests)" in §7.
