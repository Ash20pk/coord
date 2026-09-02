#!/usr/bin/env bash
# Driving one agent across several turns.
#
# This exists because of a measurement bug: a one-shot `claude -p` has exactly
# one turn, so its single UserPromptSubmit fires before any peer has written
# anything, and coord's turn-start context is invisible to it by construction.
# Anything measuring pushed coordination has to run more than one turn.
#
# Expects MODEL, TURNS and OUT to be set by the caller.

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

