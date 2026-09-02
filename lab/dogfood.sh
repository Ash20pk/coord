#!/usr/bin/env bash
# coord on a real repo.
#
# The lab's GOAL.md run measures coordination on a seeded toy: four agents,
# four files, one each, and it produced zero collisions every time. This runs
# the same substrate over a real codebase with real history, and — the point —
# gives three of the four agents work in the *same file*, so the block-and-
# notify path is finally exercised instead of being negotiated away.
#
#   ./lab/dogfood.sh                 clone (if needed) and run
#   ./lab/dogfood.sh clean           reset the checkout, keep the event log
#   ./lab/dogfood.sh report          re-print metrics for the last run
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/lab/metrics.sh"
COORD="$ROOT/target/release/coord"
REPO_URL="${COORD_DOGFOOD_URL:-https://github.com/expressjs/express.git}"
WORK="${COORD_DOGFOOD_DIR:-$HOME/coord-dogfood}"
REPO="$WORK/$(basename "$REPO_URL" .git)"
RELAY_ADDR="${COORD_RELAY_ADDR:-127.0.0.1:7420}"
DB="$HOME/.coord/relay.db"
MODEL="${COORD_LAB_MODEL:-haiku}"
OUT="${COORD_LAB_OUT:-/tmp/coord-dogfood}"

die() { echo "error: $*" >&2; exit 1; }
[[ -x "$COORD" ]] || die "coord binary not built — run: cargo build --release"
command -v claude >/dev/null || die "claude CLI not on PATH"

# Ordinary maintenance work, overlapping on purpose. ash, priya and ci-bot all
# have business in lib/response.js; sam is in lib/utils.js but has to read it.
AGENTS=(ash priya sam ci-bot)
task_for() {
  case "$1" in
  ash)   echo 'In lib/response.js: add a JSDoc block to res.sendStatus and res.redirect describing params, return value and thrown errors. Documentation only — do not change behaviour.' ;;
  priya) echo 'In lib/response.js: res.send repeats the "set Content-Type unless already set" dance in several branches. Extract it into one local helper and call it from each branch. Behaviour must not change.' ;;
  sam)   echo 'In lib/utils.js: add a small pure helper isAbsoluteUrl(value) that returns true for values starting with a scheme or "//", with a JSDoc block. Export it. Do not edit other files; read lib/response.js if you need to see how redirects use it.' ;;
  ci-bot) echo 'In lib/response.js: find every place that builds a header value by string concatenation and note whether it handles an undefined input. Fix only the ones that would produce the literal string "undefined". Small, surgical edits.' ;;
  esac
}

case "${1:-run}" in
report) coord_metrics "$DB" "$(cat "$OUT/.repo" 2>/dev/null || true)"; exit 0 ;;
clean)
  [[ -d "$REPO/.git" ]] || die "nothing to clean at $REPO"
  git -C "$REPO" checkout -- . && git -C "$REPO" clean -fd -e node_modules >/dev/null
  echo "checkout reset."
  exit 0
  ;;
run) ;;
*) die "usage: dogfood.sh [run|clean|report]" ;;
esac

mkdir -p "$WORK" "$OUT"
[[ -d "$REPO/.git" ]] || { echo "cloning $REPO_URL ..."; git clone -q --depth 50 "$REPO_URL" "$REPO"; }

pgrep -f "coord relay" >/dev/null || { "$COORD" relay --listen "$RELAY_ADDR" >/tmp/coord-relay.log 2>&1 & sleep 0.6; }
pgrep -f "coord daemon" >/dev/null || { "$COORD" daemon >/tmp/coord-daemon.log 2>&1 & sleep 0.6; }
[[ -f "$REPO/.coord.toml" ]] || (cd "$REPO" && "$COORD" init --relay "ws://${RELAY_ADDR}/ws" >/dev/null)
# `coord init` writes hook config into the checkout; keep it out of the diff.
grep -qx '.coord.toml' "$REPO/.git/info/exclude" 2>/dev/null || \
  printf '.coord.toml\n.claude/\n' >> "$REPO/.git/info/exclude"

echo "running ${#AGENTS[@]} $MODEL agents on $REPO ..."
git -C "$REPO" rev-parse --short HEAD | sed 's/^/  at commit /'
pids=()
for name in "${AGENTS[@]}"; do
  read -r -d '' prompt <<EOF || true
You are user "$name", one of four agents working this repository at the same
time. This is a real codebase — express — so keep changes minimal, idiomatic and
scoped to exactly what is asked. Do not run the test suite; do not install
anything; do not commit.

Your task: $(task_for "$name")

Others are editing right now, and some of them are in the same file as you.
Run \`coord who\` before you edit to see who holds what. If a write is refused,
you will be told who holds the file and why — wait, pick a different part of
your task, or use \`coord msg <user|all> "text"\` to negotiate. Say when you are
done with a file so whoever is waiting can move.
EOF
  ( cd "$REPO" && COORD_USER="$name" claude -p "$prompt" \
        --model "$MODEL" --permission-mode acceptEdits \
        >"$OUT/$name.log" 2>&1 ) &
  pids+=($!)
  echo "  started $name (pid ${pids[${#pids[@]}-1]}) -> $OUT/$name.log"
done

fail=0
for pid in "${pids[@]}"; do wait "$pid" || fail=1; done
(( fail )) && echo "note: an agent exited non-zero — see $OUT/*.log"
echo "all agents done."

sqlite3 "$DB" "select repo from events group by repo order by max(ts) desc limit 1" > "$OUT/.repo"
coord_metrics "$DB" "$(cat "$OUT/.repo")"

echo
echo "--- did the repo survive? ---"
git -C "$REPO" --no-pager diff --stat | tail -8
if [[ -d "$REPO/node_modules" ]]; then
  (cd "$REPO" && npm test 2>&1 | tail -4)
else
  echo "(node_modules absent — run npm install in $REPO to check the suite)"
fi
