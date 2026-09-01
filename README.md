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

Not covered by the suite, and deliberately so: whether the conflict brief actually
persuades a model to re-plan. That needs adversarial dogfooding with two live
sessions on overlapping tasks.
