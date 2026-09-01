#!/usr/bin/env bash
# coordlab — a visible multiplayer test rig.
#
# Opens one tmux window: four Claude Code sessions in a 2x2 grid, each with its
# own coord identity, plus a live dashboard across the bottom showing who holds
# what and every collision as it happens.
#
#   ./lab/lab.sh            start (or re-attach)
#   ./lab/lab.sh reset      wipe repo state + event log, then start
#   ./lab/lab.sh kill       tear everything down
set -euo pipefail

SESSION=coordlab
COORD="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/coord"
LAB="${COORDLAB_DIR:-$HOME/coordlab}"
RELAY_ADDR="${COORD_RELAY_ADDR:-127.0.0.1:7420}"
RELAY_URL="ws://${RELAY_ADDR}/ws"
AGENTS=(ash priya sam ci-bot)

die() { echo "error: $*" >&2; exit 1; }
[[ -x "$COORD" ]] || die "coord binary not built — run: cargo build --release"

case "${1:-start}" in
kill)
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  pkill -f "coord relay" 2>/dev/null || true
  pkill -f "coord daemon" 2>/dev/null || true
  echo "lab torn down."
  exit 0
  ;;
reset)
  # Also stop relay/daemon: a long-running instance from an older build would
  # otherwise keep serving, silently missing anything added since.
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  pkill -f "coord relay" 2>/dev/null || true
  pkill -f "coord daemon" 2>/dev/null || true
  sleep 0.4
  rm -rf "$LAB" ~/.coord/relay.db
  echo "state wiped."
  ;;
start) ;;
*) die "usage: lab.sh [start|reset|kill]" ;;
esac

# ---------------------------------------------------------------- seed repo
if [[ ! -d "$LAB/.git" ]]; then
  echo "seeding $LAB ..."
  mkdir -p "$LAB/src"
  cd "$LAB"
  git init -q
  cat > src/auth.js <<'EOF'
// Authentication and session handling.
const SESSION_TTL_MS = 30 * 60 * 1000;
const sessions = new Map();

function createSession(userId) {
  const token = Math.random().toString(36).slice(2);
  sessions.set(token, { userId, createdAt: Date.now() });
  return token;
}

function validateSession(token) {
  const s = sessions.get(token);
  if (!s) return null;
  if (Date.now() - s.createdAt > SESSION_TTL_MS) {
    sessions.delete(token);
    return null;
  }
  return s;
}

function destroySession(token) {
  sessions.delete(token);
}

module.exports = { createSession, validateSession, destroySession };
EOF
  cat > src/billing.js <<'EOF'
// Invoice calculation.
function lineTotal(item) {
  return item.qty * item.unitPrice;
}

function invoiceTotal(items, taxRate) {
  const subtotal = items.reduce((sum, i) => sum + lineTotal(i), 0);
  return subtotal + subtotal * taxRate;
}

module.exports = { lineTotal, invoiceTotal };
EOF
  cat > src/api.js <<'EOF'
// HTTP surface.
const { validateSession } = require('./auth');
const { invoiceTotal } = require('./billing');

function handler(req, res) {
  const session = validateSession(req.headers['x-token']);
  if (!session) return res.status(401).end();
  if (req.path === '/invoice') {
    return res.json({ total: invoiceTotal(req.body.items, 0.2) });
  }
  res.status(404).end();
}

module.exports = { handler };
EOF
  cat > TASKS.md <<'EOF'
# Suggested overlapping tasks

Paste one into each pane. The first two collide on `src/auth.js` on purpose.

1. ash    — Refactor src/auth.js: extract an isExpired(session) helper, add JSDoc
            to every function, and rename the sessions Map to sessionStore.
            Make each change as a separate edit.
2. priya  — Add refreshSession(token) to src/auth.js that resets createdAt to now.
3. sam    — Add a discount(items, pct) function to src/billing.js and use it in
            invoiceTotal.
4. ci-bot — Add input validation to the handler in src/api.js.
EOF
  git add -A && git commit -qm "seed"
fi

# ----------------------------------------------------------- relay + daemon
pgrep -f "coord relay" >/dev/null || {
  echo "starting relay on $RELAY_ADDR ..."
  "$COORD" relay --listen "$RELAY_ADDR" >/tmp/coord-relay.log 2>&1 &
  sleep 0.6
}
pgrep -f "coord daemon" >/dev/null || {
  echo "starting daemon ..."
  "$COORD" daemon >/tmp/coord-daemon.log 2>&1 &
  sleep 0.6
}
[[ -f "$LAB/.coord.toml" ]] || (cd "$LAB" && "$COORD" init --relay "$RELAY_URL" >/dev/null)

# ------------------------------------------------------------------- layout
# The grid needs vertical room: 4 agent panes stacked two-deep plus the strip.
TERM_ROWS=$(tput lines 2>/dev/null || echo 24)
TERM_COLS=$(tput cols 2>/dev/null || echo 80)
DASH_ROWS=$(( TERM_ROWS / 3 )); (( DASH_ROWS < 10 )) && DASH_ROWS=10; (( DASH_ROWS > 18 )) && DASH_ROWS=18
if (( TERM_ROWS < 40 || TERM_COLS < 150 )); then
  echo "note: terminal is ${TERM_COLS}x${TERM_ROWS}; the 2x2 grid wants at least 150x40."
  echo "      go fullscreen for the full view, or use ctrl-b z to zoom one pane."
fi
if tmux has-session -t "$SESSION" 2>/dev/null; then
  exec tmux attach -t "$SESSION"
fi

p0=$(tmux new-session -d -s "$SESSION" -c "$LAB" -x "$(tput cols)" -y "$(tput lines)" \
       -P -F '#{pane_id}')
# dashboard strip across the bottom
dash=$(tmux split-window -v -l "$DASH_ROWS" -c "$LAB" -t "$p0" -P -F '#{pane_id}')
# 2x2 grid of agents in the top region
p1=$(tmux split-window -h -c "$LAB" -t "$p0"  -P -F '#{pane_id}')
p2=$(tmux split-window -v -c "$LAB" -t "$p0"  -P -F '#{pane_id}')
p3=$(tmux split-window -v -c "$LAB" -t "$p1"  -P -F '#{pane_id}')

tmux set -t "$SESSION" -g pane-border-status top
tmux set -t "$SESSION" -g pane-border-format ' #{pane_title} '
tmux set -t "$SESSION" -g mouse on

panes=("$p0" "$p2" "$p1" "$p3")   # visual order: TL, BL, TR, BR
for i in "${!AGENTS[@]}"; do
  name="${AGENTS[$i]}"; pane="${panes[$i]}"
  tmux select-pane -t "$pane" -T "agent: $name"
  tmux send-keys -t "$pane" "clear; COORD_USER=$name claude" C-m
done

tmux select-pane -t "$dash" -T 'coord watch — live claims & collisions'
tmux send-keys -t "$dash" "clear; '$COORD' watch" C-m

tmux select-pane -t "$p0"
echo
echo "lab ready.  repo: $LAB"
echo "paste tasks from $LAB/TASKS.md into the panes (ctrl-b arrow to move, ctrl-b z to zoom)"
exec tmux attach -t "$SESSION"
