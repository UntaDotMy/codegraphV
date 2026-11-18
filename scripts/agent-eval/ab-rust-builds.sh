#!/usr/bin/env bash
# A/B two codegraphV builds on one question: same agent, same repo, same prompt —
# only the binary differs. The Rust counterpart of ab-new-vs-baseline.sh, which is
# wired to the TypeScript engine ($ENGINE/dist/bin/codegraph.js, `npm run build`,
# src/ diffs) and cannot drive a Rust build.
#
# Why this shape:
#   - Both arms are codegraph-ON. What is under test is whether the extraction fix
#     changes what explore can answer, not whether the agent adopts codegraph.
#   - Each arm runs its OWN binary as the MCP server, so the arm's index and the
#     arm's server are the same build. Mixing them would measure nothing.
#   - Each arm gets its own throwaway copy and its own index. One copy indexed by
#     two builds would let whichever ran second read the first's rows.
#   - A daemon is pre-warmed per arm, because a cold MCP attach loses the first
#     turn: the agent dives into Read/Grep before the server finishes opening the
#     project, and the arm then measures "no codegraph".
#   - The codegraphV CLI is blocked on Bash (no-cli-shim.sh, shared with the other
#     harnesses): an explore issued through the shell is not a tool call, so it
#     silently drops out of every count.
#
# Usage: ab-rust-builds.sh <repo> "<question>" <baseline-bin> <new-bin> [runs-per-arm]
#   <repo>          a Rust source tree (copied per arm; its index is not copied)
#   <baseline-bin>  codegraphV executable for the BEFORE arm
#   <new-bin>       codegraphV executable for the AFTER arm
# Env: AGENT_EVAL_OUT (default /tmp/ab-rust-builds), MODEL, EFFORT, RUNS
#   MODEL/EFFORT default sonnet/high, as every other harness here: sonnet is the
#   deliberate floor (docs/AGENTS.md). Both arms always use the same pair.
set -uo pipefail
REPO="${1:?usage: ab-rust-builds.sh <repo> \"<question>\" <baseline-bin> <new-bin> [runs]}"
Q="${2:?question required}"
BASE_BIN="${3:?baseline binary required}"
NEW_BIN="${4:?new binary required}"
RUNS="${5:-2}"
HARNESS="$(cd "$(dirname "$0")" && pwd)"
OUT="${AGENT_EVAL_OUT:-/tmp/ab-rust-builds}"
PARSE="$HARNESS/parse-run.mjs"

command -v claude >/dev/null || { echo "claude CLI not on PATH"; exit 1; }
[ -x "$BASE_BIN" ] || { echo "baseline binary not executable: $BASE_BIN"; exit 1; }
[ -x "$NEW_BIN" ] || { echo "new binary not executable: $NEW_BIN"; exit 1; }
[ -d "$REPO" ] || { echo "repo not found: $REPO"; exit 1; }

cleanup() { taskkill //F //FI "WINDOWTITLE eq *ab-rust-builds*" >/dev/null 2>&1; }
trap cleanup EXIT

mkdir -p "$OUT"
# The shared shim needs `jq` for its PreToolUse filter; this machine has none, and
# installing one would change the owner's environment. `jq-shim.py` implements the
# single expression that hook uses and refuses anything else.
if ! command -v jq >/dev/null 2>&1; then
  mkdir -p "$OUT/bin"
  printf '#!/usr/bin/env bash\nexec python -I "%s" "$@"\n' "$HARNESS/jq-shim.py" > "$OUT/bin/jq"
  chmod +x "$OUT/bin/jq"
  PATH="$OUT/bin:$PATH"
  echo "jq: not installed; using the two-expression shim in $OUT/bin"
fi
# The hook is spawned as a fresh `bash no-cli-hook.sh`, so it inherits no PATH of
# ours: put the shim's directory on the ambient PATH so the hook finds `jq` too.
export PATH="$OUT/bin:$PATH"
. "$HARNESS/no-cli-shim.sh"
cg_no_cli_setup "$OUT" || exit 1

echo "###### repo=$REPO"
echo "###### Q=$Q"
echo "###### baseline=$BASE_BIN"
echo "###### new=$NEW_BIN"
echo "###### runs/arm=$RUNS model=${MODEL:-sonnet} effort=${EFFORT:-high}"
echo

# Build the arm's copy and index: the index must come from the arm's own binary.
prepare_arm() { # label, bin
  local label="$1" bin="$2" tgt="$OUT/t-$1"
  # A daemon left by an earlier invocation keeps the directory busy on Windows, and
  # an armed trap is no help here: the whole directory goes, handle and all.
  [ -d "$tgt" ] && ( cd "$tgt" && "$bin" daemon --path "$tgt" --stop >/dev/null 2>&1 )
  for _ in 1 2 3 4 5; do rm -rf "$tgt" 2>/dev/null; [ ! -e "$tgt" ] && break; sleep 1; done
  [ ! -e "$tgt" ] || { echo "  $label: cannot clear $tgt (a daemon still holds it)"; return 1; }
  cp -r "$REPO" "$tgt"
  rm -rf "$tgt/.codegraphV" "$tgt/.git" "$tgt/target" "$tgt/node_modules"
  ( cd "$tgt" && "$bin" init . >/dev/null 2>&1 ) || { echo "  $label: index failed"; return 1; }
  local nodes
  nodes=$( cd "$tgt" && "$bin" status --json 2>/dev/null | tr -d '\n' | sed -E 's/.*"nodeCount"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/' )
  echo "  $label: indexed (nodeCount=${nodes:-?})"
}

# Spawn this arm's own daemon and wait for its endpoint file. `serve --mcp` alone
# does NOT start one: the proxy only spawns the daemon once a client connects, so a
# pre-warm has to name the hidden `__daemon` subcommand the proxy itself uses.
prewarm() { # bin, target
  local bin="$1" tgt="$2" endpoint="$2/.codegraphV/native-daemon.json"
  rm -f "$endpoint"
  ( cd "$tgt" && "$bin" __daemon --path "$tgt" --idle-timeout 1800 </dev/null >/dev/null 2>&1 & )
  local i=0
  while [ ! -f "$endpoint" ] && [ "$i" -lt 150 ]; do sleep 0.2; i=$((i+1)); done
  [ -f "$endpoint" ] && echo "  daemon warm: $tgt" || echo "  WARN: no daemon endpoint for $tgt"
}

run_arm() { # label, bin
  local label="$1" bin="$2" tgt="$OUT/t-$1" cfg="$OUT/mcp-$1.json"
  # Windows paths carry backslashes, which are invalid JSON escapes; `json.dumps`
  # is the only reliable escaper here, and forward slashes work for both the arg
  # parser and the project lookup.
  python -I - "$bin" "$tgt" > "$cfg" <<'PY'
import json, sys
binary, target = sys.argv[1], sys.argv[2]
json.dump({'mcpServers': {'codegraphV': {'command': binary, 'args': ['serve', '--mcp', '--path', target]}}}, sys.stdout)
PY
  echo "############## ARM [$label] ##############"
  local i
  for i in $(seq 1 "$RUNS"); do
    prewarm "$bin" "$tgt"
    ( cd "$tgt" && PATH="$ARM_PATH" CODEGRAPH_NO_PROMPT_HOOK=1 claude -p "$Q" \
        --output-format stream-json --verbose --permission-mode bypassPermissions \
        --model "${MODEL:-sonnet}" --effort "${EFFORT:-high}" --max-budget-usd 4 \
        --strict-mcp-config --mcp-config "$cfg" --settings "$ARM_SETTINGS" \
        </dev/null > "$OUT/run-$label-$i.jsonl" 2>"$OUT/run-$label-$i.err" )
    echo "-- $label run $i --"
    node "$PARSE" --brief "$OUT/run-$label-$i.jsonl" 2>&1 || echo "  (parse failed — see $OUT/run-$label-$i.jsonl)"
    ( cd "$tgt" && "$bin" daemon --path "$tgt" --stop >/dev/null 2>&1 )
  done
  echo
}

echo "== BASELINE arm =="; prepare_arm base "$BASE_BIN" && run_arm base "$BASE_BIN"
echo "== NEW arm ==";      prepare_arm new "$NEW_BIN"   && run_arm new "$NEW_BIN"
node "$HARNESS/compare-arms.mjs" "$OUT" base new 2>&1 || true
echo "###### DONE. Full logs in: $OUT"
