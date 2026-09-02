#!/usr/bin/env bash
# Coordination metrics out of the relay's event log. Sourced by the run
# scripts so there is exactly one copy of these queries; `claim_denied` is
# contention prevented and `ungated_write` is contention caught too late,
# which are the two numbers the whole design lives or dies by.
#
#   coord_metrics <db> [repo]      # repo defaults to the most recent one
coord_metrics() {
  local db="$1" repo="${2:-}"
  [[ -f "$db" ]] || { echo "no event log at $db" >&2; return 1; }
  [[ -n "$repo" ]] || repo=$(sqlite3 "$db" "select repo from events group by repo order by max(ts) desc limit 1")
  [[ -n "$repo" ]] || { echo "no events in $db" >&2; return 1; }

  local q="json_extract(json,'\$.type')"
  echo
  echo "=== coordination metrics — repo $repo ==================="
  sqlite3 "$db" -column -header \
    "select $q as event, count(*) as n from events where repo='$repo' group by 1 order by n desc;"
  echo
  echo "--- claims by user ---"
  sqlite3 "$db" -column -header \
    "select json_extract(json,'\$.user') as user, json_extract(json,'\$.path') as path, count(*) as n
       from events where repo='$repo' and $q='claim_acquired' group by 1,2 order by 1,2;"
  echo
  echo "--- collisions: denied writes ---"
  sqlite3 "$db" -column -header \
    "select json_extract(json,'\$.user') as blocked, json_extract(json,'\$.path') as path,
            json_extract(json,'\$.holder_user') as holder
       from events where repo='$repo' and $q='claim_denied' order by seq;"
  echo
  echo "--- collisions: ungated writes (caught after the fact) ---"
  sqlite3 "$db" -column -header \
    "select json_extract(json,'\$.user') as writer, json_extract(json,'\$.path') as path,
            json_extract(json,'\$.holder_user') as holder
       from events where repo='$repo' and $q='ungated_write' order by seq;"
  echo
  echo "--- messages between agents ---"
  sqlite3 "$db" -column -header \
    "select json_extract(json,'\$.from_user') as from_user,
            coalesce(json_extract(json,'\$.to'),'all') as to_user,
            substr(json_extract(json,'\$.text'),1,60) as text
       from events where repo='$repo' and $q='message' order by seq;"

  local n
  echo
  for t in claim_acquired file_written claim_denied ungated_write message path_freed; do
    n=$(sqlite3 "$db" "select count(*) from events where repo='$repo' and $q='$t'")
    printf '%-16s %s\n' "$t" "$n"
  done
}
