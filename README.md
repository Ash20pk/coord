# knoot

Realtime coordination for coding agents. A sequenced event log per repo, with
claims, leases, and conflict briefs — so multiple Claude Code (or any) agent
sessions can work the same repo without stepping on each other.



## Enrolling a team

```sh
knoot init --relay wss://relay.example.com/ws   # once, by one person
git add .knoot.toml .claude/settings.json && git commit
```

Both files are meant to be committed. The hooks call `knoot` **by name**, so
they resolve on every machine that has the binary on `PATH` — set `KNOOT_BIN`
if yours lives somewhere unusual. Each teammate then needs three things: the
binary, `knoot daemon` running, and `knoot login` if the relay requires a
token.

Because knoot fails open, a broken install looks exactly like a quiet one from
inside an agent. `knoot status` is how a human tells the difference:

```
$ knoot status
[ok  ] binary    /usr/local/bin/knoot
[ok  ] repo      /Users/you/project (id: project-4f2a1c)
[ok  ] hooks     6 events, resolved from PATH
[ok  ] daemon    responding
[ok  ] relay     wss://relay.example.com/ws (token: present)

coordination is on.
```

Anything less than that prints what is wrong and the command that fixes it.


## Hosting it for a team

The relay is unauthenticated by default, which is right for `127.0.0.1` and
wrong for anything else. Give it a shared secret and it requires one:

```sh
KNOOT_RELAY_TOKEN=$(openssl rand -hex 24) knoot relay --listen 0.0.0.0:7420
```

Each teammate stores that token once, per relay:

```sh
knoot login --relay wss://relay.example.com/ws --token <token>   # ~/.knoot/credentials.toml, 0600
```

`KNOOT_TOKEN` overrides it, for CI and containers. Tokens deliberately do
**not** live in `.knoot.toml`: that file is committed so a clone is enrolled
with no setup, and a secret must never ride along with it.

Use `wss://` off-machine — a bearer token over plaintext is a token anyone on
the path can take. knoot speaks `wss://` directly; terminate TLS with a proxy
in front of the relay, or at your load balancer.

**A relay that refuses you still fails open.** A rejected token means
coordination is off, not that anyone is blocked: the daemon says so once, on
stderr, and every edit is allowed. An operator's auth mistake cannot become an
outage for the team.


### A hosted relay, end to end

`deploy/` provisions one on a fresh Ubuntu droplet — Caddy terminating TLS in
front of a loopback-bound relay, systemd keeping it up, a token generated on
the box and never in the repo:

```sh
scp -r deploy root@<droplet-ip>:/root/
ssh root@<droplet-ip> 'DOMAIN=relay.example.com APEX=example.com bash /root/deploy/provision.sh'
```

Point `A` records for both names at the droplet first; Caddy gets the
certificates itself on the first request. The script is idempotent — re-run it
to deploy a new revision, and it keeps the existing token rather than rotating
it out from under the team. It refuses to claim success without checking that
the relay rejects an untokened request, accepts a tokened one, validates
registration input, and has a replicable event log.

**It downloads the binary rather than building it.** CI publishes a static
musl build to the `nightly` release on every push to `main`; the provisioner
verifies its checksum and swaps it in only once everything around it is in
place, so a failed download leaves the running version untouched. A 1 vCPU /
1 GB box needs a 2 GB swapfile to link this at all, and would be doing it
while serving the relay it is about to replace. `SOURCE=build` still compiles
on the box if you want that.

Only `/` and `/app` and `/ops` are served without a token, and they are static
shells: the event log, the repo list, the team API, and the websocket all check
it. A browser cannot set a header, so the console takes `?token=` once and
keeps it in `localStorage`.

### Not losing the log

The event log is the product. Two layers, because they fail differently:

- **Nightly snapshots, on the box.** A `sqlite3 .backup` — never `cp`, which
  half-copies a WAL database into one that restores as corrupt — gzipped, 7
  kept. This covers what actually happens: a bad `DELETE`, a corrupted page, a
  mistake.
- **Continuous replication, off the box,** via Litestream, which covers losing
  the droplet: ten seconds of loss rather than a day. It needs object-storage
  credentials, so it turns itself on only once `/etc/knoot/litestream.env`
  exists and says so loudly while it is off — a backup that silently does
  nothing is worse than one you know you do not have.

Litestream reads the write-ahead log, so the relay sets `journal_mode=WAL`
(with `synchronous=NORMAL`, which keeps a disk flush off the claim path). That
is asserted by a test: against a rollback-journal database, replication copies
nothing and reports success.


## knoot.dev

The hosted relay has a front end: [knoot.dev](https://knoot.dev) tells the
story, and `/app` is the team console — register, manage tokens, and watch the
live event log for your repos.

```sh
# no email, no password. The token is the account.
open https://knoot.dev/#start
knoot init  --relay wss://knoot.dev/ws
knoot login --relay wss://knoot.dev/ws --token <token>
```

Registration is open, which changes what a valid token is worth, so the relay
is built accordingly:

- **Tokens are stored as SHA-256 hashes.** A database dump hands over nothing
  that works, and an existing token can never be shown to you again — only
  replaced. Mint one per machine, so revoking one costs you nothing else.
- **A team cannot address another team's log.** Every repo key is namespaced by
  team id at the two places a repo is named, so two teams can both have a repo
  called `api` and neither can read the other. `tests/teams_api.rs` asserts
  that through the HTTP surface, not of a helper.
- **A team is not an operator.** The lab's terminals are real shells on the
  host, so they require the relay's own configured secret — not merely a valid
  token.
- **You cannot revoke your way out.** The last live token is refused, because
  there is no recovery path and nobody to ask.

The pages are embedded in the binary, so your own relay serves them too — `/`
the site, `/app` the console, `/ops` the original single-team operator view.


## How it works

```
agents ──hooks──► knoot hook ──unix socket──► knootd ──websocket──► knoot relay
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
cargo build --release            # → target/release/knoot

knoot relay --listen 0.0.0.0:7420   # one shared relay (any box, or localhost)
knoot daemon                        # one per machine
cd your-repo && knoot init          # writes .knoot.toml + installs Claude Code hooks
```

Restart your Claude Code sessions in that repo. Then:

```sh
knoot who      # who's active, what they're doing, what they hold
```

## The browser lab

Two live Claude Code sessions in the browser, with the knoot event log beside
them — no tmux required.

```sh
./lab/lab.sh reset     # seed the repo once
./lab/lab.sh web       # opens http://127.0.0.1:7420/lab
```

The relay hosts each agent as a real pty running `claude` with its own
`KNOOT_USER`, bridged to xterm.js over a WebSocket. Output is buffered, so a
page reload replays the session rather than showing a blank screen. Each
terminal's header shows what that agent currently holds, and turns red the
moment it is blocked. The log on the right is the same event stream the CLI
dashboard reads.

Give both agents the same file to see a block:

- **ash** — a long refactor of `src/auth.js` (holds the claim for minutes)
- **priya** — ~30s later, anything touching `src/auth.js`

Plain `knoot relay` spawns no processes; terminals exist only when it is
started with `--lab-dir`:

```sh
knoot relay --lab-dir /path/to/repo --agents ash,priya
```

## The tmux lab

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
├── knoot watch ──────────────┴─────────────────────────────┤
│ knoot ●  knootlab   4 session(s)  3 claim(s)  blocked 2   │
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

`knoot watch` works in any repo, not just the lab.

## Testing it with two sessions on one machine

You don't need two OS users. Sessions are distinguished by Claude Code's own
session id; `KNOOT_USER` just gives them readable names in conflict briefs.

```sh
# terminal 1
KNOOT_USER=ash claude

# terminal 2 (same repo)
KNOOT_USER=priya claude
```

Give both an overlapping task ("refactor the auth module"). The second session
to touch a file gets denied with the first session's identity and intent, and
re-plans. Note `$USER` itself must not be overridden — Claude Code resolves its
credentials through it.

Inspect what happened afterwards:

```sh
sqlite3 ~/.knoot/relay.db \
  "SELECT seq, datetime(ts/1000,'unixepoch','localtime'), json FROM events ORDER BY seq;"
```

## Report

[REPORT.md](REPORT.md) — current state, live-run results, every bug found by
running real sessions, and known gaps.

## Status

v1 — shared-tree claims mode. Planned: fleet mode (worktree isolation +
agent-driven merge queue), semantic claims (symbol-level via tree-sitter),
dashboard/audit surface over the event log.

## Tests

```sh
cargo test          # 140 tests, ~4s
```

Four layers:

| Layer | File | What it protects |
|---|---|---|
| Unit | `src/proto.rs` | path-overlap boundaries (`src/auth` vs `src/auth2`), lease expiry, log-replay determinism |
| Arbitration | `tests/arbitration.rs` | 400+ concurrent races → exactly one winner; conflict briefs carry holder + intent; repo isolation |
| Failure | `tests/failure.rs` | fail-open on dead daemon, dead relay, unresponsive relay, malformed input; crash recovery via lease expiry |
| Contract | `tests/e2e.rs` | real Claude Code hook payloads through the binary; exact deny/context JSON; latency ceiling |
| Multi-tenancy | `tests/teams_api.rs` | registration, token minting/revocation, and that one team cannot read, list, or revoke another's anything |
| Durability | `tests/failure.rs` | claims and sequence numbers survive a relay restart; the log stays replicable (WAL) |

One test is kept **red on purpose**: `bash_write_to_a_claimed_file_is_blocked`
(`#[ignore]`d) specifies behaviour v1 does not have. See Known gaps.

## Sessions talk to each other

Blocking alone is not multiplayer. A blocked session used to wait on a lease it
could not observe, and nobody told it when the work finished.

**Release notifications.** Being denied registers interest in that path. When
the holder releases it — explicitly, by ending, or by its lease expiring — every
waiter is told, with what the holder was doing. Delivery uses the `Stop` hook:
the moment an agent tries to end its turn, pending news sends it back to work,
so notice arrives in real time rather than whenever the human next types. A
per-user cap means a chatty peer can never keep a session spinning.

**Direct messages.** Agents coordinate in their own words:

```sh
knoot msg priya "auth.js is yours, exports are stable"
knoot msg all "goal is green, stop editing"
knoot inbox                    # read and clear pending notes
```

Identity comes from `KNOOT_USER` (else `$USER`), not from a session id — Claude
Code exposes no session id to the commands it runs, and assuming otherwise
attributed every message to the OS user.

## Shell writes

Bash is gated too, or the scheme would be optional: agents reach for `sed` and
heredocs as readily as the Edit tool, and auto mode prefers Bash outright.

`PreToolUse` parses the command for write targets — redirects, heredoc
targets, `tee`, `sed -i`, `cp`/`mv`, `rm`, `dd of=` — and gates each one.
Quoting is respected, and heredoc *bodies* are skipped: a body containing
`(sum, i) => sum + i` would otherwise read `=>` as a redirect.

What the parser cannot read — interpreters, build tools, anything unknown — is
allowed but *audited*: the working tree is fingerprinted before and after, and
any change landing on a peer's claim is recorded as `UngatedWrite`. That is
detection, not prevention, and the dashboard labels it that way.

## What four agents on one goal actually did

`lab/GOAL.md` gives the lab a shared objective — ship an invoice endpoint, four
owners, one codebase — because tasks with no common goal collide only by
accident. Two runs, unprompted behaviour:

- 18–28 messages per run: agents announced ownership, published export
  contracts, corrected each other when messages crossed, and declared done.
- A session asked to edit a file another held did not attempt it. Presence told
  it who held the file, so it sent the holder a patch and offered to wait. The
  collision was avoided *before* the block, which is the better outcome and the
  reason that run recorded no denials at all.
- The tree ended green: `node test.js`, 57–58 passing.

## Known gaps

- **Attribution under concurrent writes is inferred.** The working tree is
  shared, so a peer's edit lands inside our audit window too. Authorship comes
  from their `FileWritten` event; if that has not arrived yet, an ungated write
  can be attributed to the wrong session. Observed live before the fix.
- **Interpreters are only detected, never blocked.** `python3 -c "open(...)"`
  writes first and is recorded second.
- **Same-name sessions share a mailbox.** Mail is keyed by user, so two
  sessions running as the same `KNOOT_USER` both receive its notes.
- **Fail-open is ambiguous by design.** An allowed edit and an unreachable
  daemon look identical from the agent's side. Tests use a positive control
  (the relay must hold the claim) to tell them apart; humans should check
  `knoot watch` shows a green dot.

Not testable by the suite at all: whether the conflict brief persuades a model
to re-plan. Live runs say yes so far — one session prepared its patch and
waited rather than writing; another, seeing a peer mid-rename, wrote its
function and flagged that the peer's rename would need to cover it. Neither
attempted a shell bypass. Small N.
