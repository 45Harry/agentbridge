#!/usr/bin/env bash
# End-to-end smoke test of the built binary in a sandbox seeded from
# tests/fixtures. Exits non-zero on the first failed expectation.
#
#   cargo build && scripts/smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SB="$(mktemp -d)"
trap 'rm -rf "$SB"' EXIT
ab() { "$ROOT/scripts/sandbox.sh" "$SB" "$@"; }
fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }
expect() { # expect <description> <pattern> <text>
  printf '%s' "$3" | grep -Eq -- "$2" || fail "$1 (wanted /$2/ in: $3)"
  echo "ok   $1"
}

# Seed: the fixture sessions, re-homed under the sandbox project directory.
mkdir -p "$SB/work" "$SB/.claude/projects/-w" "$SB/.codex/sessions/2026/07/02"
cp "$ROOT/tests/fixtures/claude-code/embedded-secret.jsonl" \
   "$SB/.claude/projects/-w/sec-0001-0000-0000-000000000001.jsonl"
CODEX="rollout-2026-07-02T14-00-00-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl"
sed "s#/home/user/project#$SB/work#g" \
  "$ROOT/tests/fixtures/codex-cli/sessions/2026/07/02/$CODEX" \
  > "$SB/.codex/sessions/2026/07/02/$CODEX"
SECRET='sk-abc123def456ghi789jkl012'

expect "version"         '^agentbridge [0-9]' "$(ab --version)"
expect "info"            'Claude Code'        "$(ab info)"
expect "init discovers"  'Codex CLI'          "$(ab init)"

DRY="$(ab sync --project "$SB/work" --dry-run)"
expect "sync --dry-run plans copies" 'would materialize [1-9]' "$DRY"
[ ! -e "$SB/.agentbridge/manifest.jsonl" ] || fail "dry run wrote a manifest"

OUT="$(ab sync --project "$SB/work")"
expect "sync creates copies"  'created +[1-9]'        "$OUT"
expect "sync redacts"         'redacted +secrets'     "$OUT"
[ -s "$SB/.agentbridge/manifest.jsonl" ] || fail "no manifest after sync"

# The headline guarantee: the planted key exists only in the seeded original.
LEAKS="$(grep -rlF "$SECRET" "$SB" | grep -v '/.claude/projects/-w/' || true)"
[ -z "$LEAKS" ] || fail "secret copied into: $LEAKS"
echo "ok   secret appears only in the original"

expect "second sync is idempotent" 'created +0' "$(ab sync --project "$SB/work")"
expect "status"          'tracked'              "$(ab status)"
expect "pull (dry)"      'No new turns'         "$(ab pull --dry-run)"

# Index, search, brief and facts.
expect "index builds"      'Index: [1-9][0-9]* session'            "$(ab index)"
expect "index is incremental" '0 indexed now'                      "$(ab index)"
expect "search finds text" '\[codex-cli:[0-9a-f]+#[0-9]+\]'       "$(ab search deployment configuration)"
expect "search is redacted" 'No matches'                           "$(ab search abc123def456ghi789jkl012)"
expect "fact is recorded"  'Recorded fact'                         "$(ab fact --project "$SB/work" 'Deploys need the staging VPN')"
BRIEF="$(ab brief --project "$SB/work" --budget 400 2>/dev/null)"
expect "brief has citations" '\[codex-cli:[0-9a-f]+#[0-9]+\]'     "$BRIEF"
expect "brief has the fact"  'staging VPN \[fact:1\]'             "$BRIEF"
[ "$(printf '%s' "$BRIEF" | grep -c "$SECRET")" -eq 0 ] || fail "secret in brief"
expect "brief is cached on the second run" 'cached'               "$(ab brief --project "$SB/work" --budget 400 --no-refresh 2>&1 >/dev/null)"
# A "model" that just echoes its prompt is not a valid summary: fall back to the plain brief.
LLM_ERR="$(ab brief --project "$SB/work" --budget 400 --no-refresh --llm-cmd cat 2>&1 >/dev/null)"
expect "llm-cmd says where the text goes" 'Sending the redacted brief' "$LLM_ERR"
expect "llm-cmd output is validated"      'model output not used'      "$LLM_ERR"
set +e; ab brief --project "$SB/work" --budget 5 >/dev/null 2>&1; RC=$?; set -e
[ "$RC" -eq 2 ] || fail "a too-small budget should exit 2, got $RC"
echo "ok   brief rejects a budget smaller than its header"

# MCP server: handshake, tool list, and a real search over stdio. stdout must
# carry nothing but JSON-RPC, one message per line.
MCP_OUT="$( (
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  echo "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"search_history\",\"arguments\":{\"query\":\"deployment configuration\"}}}"
  echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"record_fact","arguments":{"text":"MCP smoke fact","project":"'"$SB/work"'"}}}'
  sleep 1
) | ab mcp 2>/dev/null)"
[ "$(printf '%s\n' "$MCP_OUT" | wc -l)" -eq 4 ] || fail "mcp should answer 4 requests with 4 lines, got: $MCP_OUT"
printf '%s\n' "$MCP_OUT" | while IFS= read -r line; do
  printf '%s' "$line" | python3 -c 'import json,sys; json.loads(sys.stdin.read())' || { echo "SMOKE FAIL: non-JSON on mcp stdout: $line" >&2; exit 1; }
done
expect "mcp initialize"   '"name":"agentbridge"'            "$MCP_OUT"
expect "mcp lists tools"  'search_history.*get_brief.*get_session.*record_fact' "$MCP_OUT"
expect "mcp search cites" 'codex-cli:[0-9a-f]+#'             "$MCP_OUT"
expect "mcp records fact" 'Recorded fact'                    "$MCP_OUT"

# start / clean round trip leaves a hand-written file byte-identical.
printf '# Rules\nuse tabs\n' > "$SB/work/CLAUDE.md"
cp "$SB/work/CLAUDE.md" "$SB/orig.md"
expect "start --dry-run" 'Would write to'       "$(ab start claude-code --project "$SB/work" --dry-run)"
cmp -s "$SB/work/CLAUDE.md" "$SB/orig.md" || fail "dry run changed CLAUDE.md"
ab start claude-code --project "$SB/work" --no-launch >/dev/null
grep -q 'agentbridge:begin' "$SB/work/CLAUDE.md" || fail "nothing injected"
grep -qF "$SECRET" "$SB/work/CLAUDE.md" && fail "secret injected into CLAUDE.md"
ab clean --project "$SB/work" >/dev/null
cmp -s "$SB/work/CLAUDE.md" "$SB/orig.md" || fail "clean did not restore CLAUDE.md"
echo "ok   start/clean restores the file byte-for-byte"

# --all-known refuses to run unconfirmed, previews with --dry-run, and works
# with --yes (only directories that still exist are synced).
ab unsync >/dev/null
set +e; ab sync --all-known >/dev/null 2>&1; RC=$?; set -e
[ "$RC" -eq 2 ] || fail "--all-known without --yes should exit 2, got $RC"
expect "--all-known --dry-run previews" 'known directories' "$(ab sync --all-known --dry-run)"
expect "--all-known --yes syncs"        'created +[1-9]'    "$(ab sync --all-known --yes)"

# A broken rules file must stop everything and write nothing.
ab unsync >/dev/null
printf 'broken = (unclosed\n' > "$SB/.agentbridge/redact.rules"
set +e; ab sync --project "$SB/work" >/dev/null 2>&1; RC=$?; set -e
[ "$RC" -eq 2 ] || fail "invalid rules should exit 2, got $RC"
rm "$SB/.agentbridge/redact.rules"
echo "ok   invalid redaction rules fail closed"

ab unsync >/dev/null
[ ! -s "$SB/.agentbridge/manifest.jsonl" ] || fail "unsync left manifest rows"
echo "SMOKE PASS"
