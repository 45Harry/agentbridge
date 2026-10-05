#!/usr/bin/env bash
# Run agentbridge against a throwaway world, never your real sessions.
#
#   scripts/sandbox.sh <dir> [agentbridge args...]
#
# Every variable that decides where a tool keeps its sessions is redirected into
# <dir>. HOME alone is not enough: Claude Code, Codex, Antigravity and OpenCode
# each honour their own override, and missing one writes into the real store
# (HANDOFF.md §4). Seed <dir>/.claude, <dir>/.codex etc. yourself, or use
# scripts/smoke.sh, which seeds them from tests/fixtures.
#
# Set AGENTBRIDGE_BIN to use a different binary (default: target/debug).
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <sandbox-dir> [agentbridge args...]" >&2
  exit 2
fi
SB="$(mkdir -p "$1" && cd "$1" && pwd)"
shift
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${AGENTBRIDGE_BIN:-$ROOT/target/debug/agentbridge}"
[ -x "$BIN" ] || { echo "no binary at $BIN (run: cargo build)" >&2; exit 2; }

mkdir -p "$SB/work" "$SB/.claude/projects" "$SB/.codex/sessions" "$SB/.local/share" "$SB/.gemini"
exec env \
  HOME="$SB" \
  AGENTBRIDGE_DATA_DIR="$SB/.agentbridge" \
  CLAUDE_CONFIG_DIR="$SB/.claude" \
  CODEX_HOME="$SB/.codex" \
  ANTIGRAVITY_HOME="$SB/agy-store" \
  XDG_DATA_HOME="$SB/.local/share" \
  "$BIN" "$@"
