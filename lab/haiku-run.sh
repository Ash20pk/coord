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
COORD="$ROOT/target/release/coord"
LAB="${COORDLAB_DIR:-$HOME/coordlab}"
RELAY_ADDR="${COORD_RELAY_ADDR:-127.0.0.1:7420}"
RELAY_URL="ws://${RELAY_ADDR}/ws"
DB="$HOME/.coord/relay.db"
MODEL="${COORD_LAB_MODEL:-haiku}"
# Pushed context lands at the *start of a turn*, so a one-shot `claude -p` can
# never see it: its single UserPromptSubmit fires before any peer has written
# anything. Several turns per agent is the only setup where the mechanism is
# observable at all.
TURNS="${COORD_LAB_TURNS:-3}"
OUT="${COORD_LAB_OUT:-/tmp/coord-haiku}"

die() { echo "error: $*" >&2; exit 1; }
[[ -x "$COORD" ]] || die "coord binary not built — run: cargo build --release"
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

# One agent, several turns. Turn 1 opens the session and reports its id; the
# rest resume it, so each later turn begins with a UserPromptSubmit — which is
# where coord hands the agent its peers, what moved under it, and its mail.
# The continuation prompt says nothing about coordinating: if an agent reacts
# to a peer, the pushed context is the only place it could have learned.
run_agent() {
  local name="$1" task="$2" first sid t
  first=$(COORD_USER="$name" claude -p "$task" \
            --model "$MODEL" --permission-mode acceptEdits \
            --output-format json 2>/dev/null || true)
  sid=$(printf '%s' "$first" | python3 -c \
        'import json,sys;print(json.load(sys.stdin).get("session_id",""))' 2>/dev/null || true)
  printf '%s' "$first" | python3 -c \
        'import json,sys;print(json.load(sys.stdin).get("result",""))' 2>/dev/null || true
  [[ -n "$sid" ]] || { echo "[$name] no session id; single turn only"; return 0; }
  for (( t = 2; t <= TURNS; t++ )); do
    echo "--- turn $t ---"
    COORD_USER="$name" claude -p --resume "$sid" \
      "Continue. When your part of the definition of done holds, verify it and stop." \
      --model "$MODEL" --permission-mode acceptEdits 2>/dev/null || true
  done
}

report() {
  coord_metrics "$DB" "$(sqlite3 "$DB" "select repo from events group by repo order by max(ts) desc limit 1")"
  echo "transcripts: $OUT/<agent>.log"
  if [[ -f "$LAB/test.js" ]]; then
    echo
    echo "--- node test.js ---"
    (cd "$LAB" && node test.js 2>&1 | tail -15) || true
  fi
}

case "${1:-run}" in
report) report; exit 0 ;;
reset)
  pkill -f "coord relay" 2>/dev/null || true
  pkill -f "coord daemon" 2>/dev/null || true
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
pgrep -f "coord relay" >/dev/null || { "$COORD" relay --listen "$RELAY_ADDR" >/tmp/coord-relay.log 2>&1 & sleep 0.6; }
pgrep -f "coord daemon" >/dev/null || { "$COORD" daemon >/tmp/coord-daemon.log 2>&1 & sleep 0.6; }
[[ -f "$LAB/.coord.toml" ]] || (cd "$LAB" && "$COORD" init --relay "$RELAY_URL" >/dev/null)

mkdir -p "$OUT"
echo "running ${#AGENTS[@]} $MODEL agents headless in $LAB, $TURNS turns each ..."
pids=()
for name in "${AGENTS[@]}"; do
  read -r -d '' task <<EOF || true
Read GOAL.md — it is the shared objective and you are one of four agents working
this repo at the same time. You are user "$name". $(prompt_for "$name")

The other three are working in parallel right now and you depend on their files.
Before you write, run \`coord who\` to see who holds what, and use
\`coord msg <user|all> "text"\` to agree on interfaces and to say when you have
finished something someone else is waiting on. Stay inside the file you own.
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
