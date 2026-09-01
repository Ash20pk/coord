# coord

Realtime coordination for coding agents. A sequenced event log per repo, with
claims, leases, and conflict briefs — so multiple Claude Code (or any) agent
sessions can work the same repo without stepping on each other.

## How it works

```
agents ──hooks──► coord hook ──unix socket──► coordd ──websocket──► coord relay
 (any terminal)     (shim)                    (local mirror)        (sequencer + arbitration)
```

- Every agent turn's writes auto-claim the touched files (10-minute leases,
  renewed on activity, expired on crash — nothing can wedge the repo).
- Before any edit, a hook checks the local claim mirror (microseconds) and
  acquires through the relay (single-digit ms). A conflict returns a
  **conflict brief** — who holds the file, and their stated intent — straight
  into the model's context so it re-plans instead of colliding.
- New sessions are told who else is active and where, at startup.
- Fail-open everywhere: relay down, daemon dead, network gone → sessions work
  solo, exactly like today. Code never leaves your machine; only paths and
  intent strings cross the wire.

## Quick start

```sh
cargo build --release            # → target/release/coord

coord relay --listen 0.0.0.0:7420   # one shared relay (any box, or localhost)
coord daemon                        # one per machine
cd your-repo && coord init          # writes .coord.toml + installs Claude Code hooks
```

Restart your Claude Code sessions in that repo. Then:

```sh
coord who      # who's active, what they're doing, what they hold
```

## The lab — see it in front of you

```sh
cargo build --release
./lab/lab.sh              # start (or re-attach)
./lab/lab.sh reset        # wipe repo state + event log, then start
./lab/lab.sh kill         # tear it all down
```

One tmux window: four Claude Code sessions in a 2x2 grid — `ash`, `priya`,
`sam`, `ci-bot` — over a live dashboard.

```
┌── agent: ash ───────────────┬── agent: sam ───────────────┐
│                             │                             │
├── agent: priya ─────────────┼── agent: ci-bot ────────────┤
│                             │                             │
├── coord watch ──────────────┴─────────────────────────────┤
│ coord ●  coordlab   4 session(s)  3 claim(s)  blocked 2   │
│ USER      SESSION   INTENT                    HOLDS       │
│ ash       a1b2c3d4  Refactor src/auth.js…     src/auth.js │
│ priya     c9d0e1f2  Add refreshSession…       —           │
│ ─────────────────────────────────────────────────────────  │
│ 21:02:56  ash       claim   src/auth.js                    │
│ 21:02:56  priya     BLOCKED src/auth.js (held by ash)      │
└────────────────────────────────────────────────────────────┘
```

`TASKS.md` in the lab repo has four tasks to paste in — the first two collide on
`src/auth.js` on purpose. Wants a terminal at least 150x40; `ctrl-b z` zooms a
pane, `ctrl-b arrow` moves between them.

`coord watch` works in any repo, not just the lab.

## Testing it with two sessions on one machine

You don't need two OS users. Sessions are distinguished by Claude Code's own
session id; `COORD_USER` just gives them readable names in conflict briefs.

```sh
# terminal 1
COORD_USER=ash claude

# terminal 2 (same repo)
COORD_USER=priya claude
```

Give both an overlapping task ("refactor the auth module"). The second session
to touch a file gets denied with the first session's identity and intent, and
re-plans. Note `$USER` itself must not be overridden — Claude Code resolves its
credentials through it.

Inspect what happened afterwards:

```sh
sqlite3 ~/.coord/relay.db \
  "SELECT seq, datetime(ts/1000,'unixepoch','localtime'), json FROM events ORDER BY seq;"
```

## Status

v1 — shared-tree claims mode. Planned: fleet mode (worktree isolation +
agent-driven merge queue), semantic claims (symbol-level via tree-sitter),
dashboard/audit surface over the event log.

## Tests

```sh
cargo test          # 44 tests, ~2s
```

Four layers:

| Layer | File | What it protects |
|---|---|---|
| Unit | `src/proto.rs` | path-overlap boundaries (`src/auth` vs `src/auth2`), lease expiry, log-replay determinism |
| Arbitration | `tests/arbitration.rs` | 400+ concurrent races → exactly one winner; conflict briefs carry holder + intent; repo isolation |
| Failure | `tests/failure.rs` | fail-open on dead daemon, dead relay, unresponsive relay, malformed input; crash recovery via lease expiry |
| Contract | `tests/e2e.rs` | real Claude Code hook payloads through the binary; exact deny/context JSON; latency ceiling |

One test is kept **red on purpose**: `bash_write_to_a_claimed_file_is_blocked`
(`#[ignore]`d) specifies behaviour v1 does not have. See Known gaps.

## Known gaps (found by running real sessions, not by the suite)

- **Bash writes bypass claims.** Only Write/Edit/MultiEdit/NotebookEdit are
  gated. A session that edits via `sed`, `tee`, or heredocs walks straight past
  coord — and Claude Code's auto mode prefers Bash for edits. Observed live in
  the lab: a session modified a claimed file with zero recorded events. In
  `acceptEdits` mode, where the model uses the Edit tool, gating works.
- **Presence goes stale.** Peer context is injected once at SessionStart and
  never refreshed, so a session can reason about peers that left minutes ago.
- **Fail-open is ambiguous by design.** An allowed edit and an unreachable
  daemon look identical from the agent's side. Tests use a positive control
  (the relay must hold the claim) to tell them apart; humans should check
  `coord watch` shows a green dot.

Not testable by the suite at all: whether the conflict brief persuades a model
to re-plan. One live run says yes (session prepared its patch and waited, no
bypass attempt); N=1.
