#!/usr/bin/env bash
# Four Haiku agents, one goal — the headless counterpart to lab.sh.
#
# lab.sh puts four interactive sessions in front of you. This runs the same
# four roles against the same GOAL.md with `claude -p --model haiku`, in
# parallel and unattended, then reports the coordination metrics straight out
# of the relay's event log. That comparison — same substrate, cheaper model —
# is the "four Haiku agents" section of REPORT.md.
#
#   ./lab/haiku-run.sh          run against the current lab state
#   ./lab/haiku-run.sh reset    wipe repo + event log first (comparable numbers)
#   ./lab/haiku-run.sh report   re-print metrics for the last run
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/lab/metrics.sh"
source "$ROOT/lab/agent.sh"
COORD="$ROOT/target/release/knoot"
LAB="${COORDLAB_DIR:-$HOME/knootlab}"
RELAY_ADDR="${KNOOT_RELAY_ADDR:-127.0.0.1:7420}"
RELAY_URL="ws://${RELAY_ADDR}/ws"
DB="$HOME/.knoot/relay.db"
MODEL="${KNOOT_LAB_MODEL:-haiku}"
# Pushed context lands at the *start of a turn*, so a one-shot `claude -p` can
# never see it: its single UserPromptSubmit fires before any peer has written
# anything. Several turns per agent is the only setup where the mechanism is
# observable at all.
TURNS="${KNOOT_LAB_TURNS:-3}"
OUT="${KNOOT_LAB_OUT:-/tmp/knoot-haiku}"

die() { echo "error: $*" >&2; exit 1; }
[[ -x "$COORD" ]] || die "knoot binary not built — run: cargo build --release"

# The hooks call `knoot` by name, so the agents must be able to resolve it.
# Without this every hook in the lab is a silent no-op and the whole run
# measures four agents working in isolation — which is exactly what happened
# the first time this was run, and `knoot status` was the only thing that said
# so. Exported, so `claude` and its hooks inherit it.
export KNOOT_BIN="$COORD"
export PATH="$(dirname "$COORD"):$PATH"
command -v claude >/dev/null || die "claude CLI not on PATH"
command -v sqlite3 >/dev/null || die "sqlite3 not on PATH"

# Roles mirror GOAL.md's division of labour, one file each.
prompt_for() {
  case "$1" in
  ash)    echo "You own src/auth.js: session expiry and refresh." ;;
  priya)  echo "You own src/billing.js: discount, tax, rounding." ;;
  sam)    echo "You own src/api.js: the POST /invoice endpoint, wiring and status codes." ;;
  ci-bot) echo "You own test.js and the shared types in src/types.js that auth.js and billing.js both need." ;;
  esac
}
AGENTS=(ash priya sam ci-bot)

# Phase 4's exit criterion, planted before the run.
#
# The fact is deliberately one a cheap model gets wrong on its own: money in
# floats is the default it reaches for, and nothing in the repo says otherwise.
# So if `billing` still divides by 100 into a float, the injection did not
# land, or landed and was not read — and either way the format is wrong and
# phase 5 waits. This is the arm the design says the phase turns on, and it
# tests the *delivery*, which is the part a unit test cannot reach.
PLANTED_NAME="money-representation"
PLANTED_TEXT="all money in this repo is integer cents. Never use floats for money: no parseFloat, no division into a fraction of a cent. Round with Math.round on cents."
PLANTED_PATH="src/billing.js"

# Memory needs a *verified member*, and an open relay with no team resolves to
# an identity with no person behind it — which may not publish, because a
# shard whose provenance is a display string is worse than no shard. So the lab
# registers a team and joins it. Without this the plant fails and the run
# silently measures nothing about memory.
enrol() {
  # `knoot status` is the only thing that knows whether this machine holds a
  # key the relay accepts. Two ways to need enrolling, and `reset` produces the
  # second every time: it wipes the relay's database, which leaves a stored
  # credential that no longer names anybody.
  local st
  st=$( (cd "$LAB" && "$COORD" status 2>&1) || true )
  case "$st" in
    *"names no verified person"*|*"rejected this token"*|*401*) ;;
    *) return 0 ;;
  esac
  rm -f "$HOME/.knoot/credentials.toml"

  local http="http://${RELAY_ADDR}" tok=""
  tok=$(curl -s -X POST "$http/api/register" -H 'content-type: application/json' \
        -d '{"team":"knootlab","email":"lab@knoot.local"}' 2>/dev/null \
        | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("token",""))
except Exception: print("")' 2>/dev/null) || tok=""
  if [[ -z "$tok" ]]; then
    echo "note: could not register a lab team — the memory arm will measure nothing"
    return 0
  fi
  "$COORD" join "$tok" --relay "$RELAY_URL" >/dev/null 2>&1 || true
  # The daemon reads its credential when it dials, so it has to come back.
  pkill -f "knoot daemon" 2>/dev/null || true
  sleep 0.5
  "$COORD" daemon >/tmp/knoot-daemon.log 2>&1 &
  sleep 1.5
}

plant_fact() {
  local out
  out=$(cd "$LAB" && "$COORD" remember \
    --name "$PLANTED_NAME" --path "$PLANTED_PATH" "$PLANTED_TEXT" 2>&1)
  if [[ "$out" != *remembered* ]]; then
    # Loud, not silent. A planted fact that was never planted turns the
    # phase-4 arm into a test of nothing, and reports a pass as a result.
    echo "WARNING: the planted fact was NOT published — the memory arm of this"
    echo "         run measures nothing. knoot said: $out"
  fi
}

report() {
  knoot_metrics "$DB" "$(sqlite3 "$DB" "select repo from events group by repo order by max(ts) desc limit 1")"
  echo "transcripts: $OUT/<agent>.log"
  # Phase 2 of the multiplayer design is about awareness, and awareness that
  # nothing delivered is worth nothing — so the check is what reached an
  # agent's transcript, not only what reached the log.
  echo
  echo "--- awareness that reached a transcript ---"
  for pat in "already exists" "you read" "has been deleted" "very like this" "is a hub"; do
    printf '%-18s %s transcript(s)\n' "$pat" \
      "$(grep -l "$pat" "$OUT"/*.log 2>/dev/null | wc -l | tr -d ' ')"
  done
  # Phase 4: shared memory. The question is not whether the fact was stored —
  # a test answers that — but whether a cheap model, told it unasked, acted on
  # it instead of re-deriving the wrong thing.
  echo
  echo "--- the planted fact ---"
  printf '%-18s %s transcript(s)\n' "reached an agent" \
    "$(grep -l "integer cents" "$OUT"/*.log 2>/dev/null | wc -l | tr -d ' ')"
  if [[ -f "$LAB/src/billing.js" ]]; then
    if grep -qE 'parseFloat|/ *100(\.0)?[^0-9]' "$LAB/src/billing.js"; then
      echo "billing.js:       FLOATS — the fact did not change what the model wrote"
    else
      echo "billing.js:       integer cents — the fact held"
    fi
  fi
  # Phase 6: the other two kinds. A plan is only worth publishing if peers are
  # actually told, so the check is again what reached a transcript.
  printf '%-18s %s transcript(s)\n' "peers' plans seen" \
    "$(grep -l "what your peers are doing" "$OUT"/*.log 2>/dev/null | wc -l | tr -d ' ')"
  printf '%-18s %s\n' "plans published" \
    "$("$COORD" recall 2>/dev/null | grep -c "^\[session_context\]" || echo 0)"
  printf '%-18s %s\n' "refusals logged" \
    "$(sqlite3 "$DB" "select count(*) from events where json like '%memory_refused%'" 2>/dev/null || echo 0)"

  if [[ -f "$LAB/test.js" ]]; then
    echo
    echo "--- node test.js ---"
    (cd "$LAB" && node test.js 2>&1 | tail -15) || true
  fi
}

case "${1:-run}" in
report) report; exit 0 ;;
reset)
  pkill -f "knoot relay" 2>/dev/null || true
  pkill -f "knoot daemon" 2>/dev/null || true
  sleep 0.4
  rm -rf "$LAB" "$DB"
  # `lab.sh web` reseeds $LAB and brings the relay back up with the browser
  # terminals attached; `lab.sh reset` would fall through into tmux mode and
  # start a relay with no --lab-dir, silently dropping /lab.
  "$ROOT/lab/lab.sh" web </dev/null >/dev/null 2>&1 || true
  ;;
run) ;;
*) die "usage: haiku-run.sh [run|reset|report]" ;;
esac

[[ -d "$LAB/.git" ]] || die "lab repo not seeded — run: ./lab/lab.sh reset"
pgrep -f "knoot relay" >/dev/null || { "$COORD" relay --listen "$RELAY_ADDR" >/tmp/knoot-relay.log 2>&1 & sleep 0.6; }
pgrep -f "knoot daemon" >/dev/null || { "$COORD" daemon >/tmp/knoot-daemon.log 2>&1 & sleep 0.6; }
# Both halves, checked separately: `.knoot.toml` can survive a reseed while
# `.claude/settings.json` does not, and hooks that are not installed make every
# agent in the run invisible to every other.
if [[ ! -f "$LAB/.knoot.toml" ]]; then
  (cd "$LAB" && "$COORD" init --relay "$RELAY_URL" >/dev/null)
  printf 'hubs = ["src/types.js", "test.js"]\n' >> "$LAB/.knoot.toml"
elif [[ ! -f "$LAB/.claude/settings.json" ]]; then
  (cd "$LAB" && "$COORD" init --relay "$RELAY_URL" >/dev/null)
fi

enrol
plant_fact
# Prove the agents can actually reach knoot before spending anything on them.
# Nothing is spent until the substrate is proven. Each of these has silently
# turned a run into four agents working in isolation at least once.
lab_status=$( (cd "$LAB" && "$COORD" status 2>&1) || true )
case "$lab_status" in
  *"[ok  ] hooks"*) ;;
  *) die "hooks are not installed in $LAB. status said:"$'\n'"$lab_status" ;;
esac
case "$lab_status" in
  *"[ok  ] relay"*) ;;
  *) die "the relay is not usable from $LAB. status said:"$'\n'"$lab_status" ;;
esac
command -v knoot >/dev/null || die "knoot is not on PATH; every hook would be a no-op"
mkdir -p "$OUT"
echo "running ${#AGENTS[@]} $MODEL agents headless in $LAB, $TURNS turns each ..."
pids=()
for name in "${AGENTS[@]}"; do
  read -r -d '' task <<EOF || true
Read GOAL.md — it is the shared objective and you are one of four agents working
this repo at the same time. You are user "$name". $(prompt_for "$name")

The other three are working in parallel right now and you depend on their files.
Before you write, run \`knoot who\` to see who holds what, and use
\`knoot msg <user|all> "text"\` to agree on interfaces and to say when you have
finished something someone else is waiting on. When you have decided how to do
your part, say so once with
\`knoot plan --path <your file> --decided "<an interface you have fixed>" "<your approach>"\`
so the other three are told before they design against it. Stay inside the file
you own.
Work until your part of the definition of done in GOAL.md holds.
EOF
  ( cd "$LAB" && run_agent "$name" "$task" >"$OUT/$name.log" 2>&1 ) &
  pids+=($!)
  echo "  started $name (pid ${pids[${#pids[@]}-1]}) -> $OUT/$name.log"
done

fail=0
for pid in "${pids[@]}"; do wait "$pid" || fail=1; done
(( fail )) && echo "note: at least one agent exited non-zero — check $OUT/*.log"
echo "all agents done."
report
