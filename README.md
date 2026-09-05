# knoot

**If knoot breaks, your agents do not know.** Every failure path ends in an
allowed write: relay unreachable, token refused, daemon dead, key missing,
memory unreadable — the agent is told nothing and carries on. That is the
distinction from every alternative, it is enforced by tests that fail the build
if it stops being true, and it is why this can be installed on a repo people
are actually paid to work in.

What it does when it *is* working: realtime coordination for coding agents. A
sequenced event log per repo, with claims, leases, and conflict briefs — so
several Claude Code sessions, a teammate in another editor, and whatever your
colleague is running can work one repo without stepping on each other. Nothing
an agent needs to coordinate sits behind a command it has to think of.


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

The hosted relay has a front end: [knoot.dev](https://knoot.dev) is the site,
`/docs` the documentation, `/status` a live health check, and `/app` the team
console — sign in, manage agent tokens, and watch the live event log.

```sh
open https://knoot.dev/app/#signup      # email and password, for a person
knoot init --relay wss://knoot.dev/ws
knoot join <key> --relay wss://knoot.dev/ws   # a device key, for a machine
```

`join` stores the key and then asks the relay who it is for, printing the team,
the member, the rooms and the areas it opens. `login` still exists and still
works; it just believes the key without asking, which was honest when a key
named only a team.

**People and machines authenticate differently, on purpose.** A person signs in
with email and password; that is a Supabase account, and it is what the console
checks. A machine presents a device key minted in the console: resolved
against local SQLite with no network call, because the hot path has to keep
working when everything else is down. The CLI is unchanged and existing keys
keep working.

- **A key names a person, not just a team.** One row per machine per person, so
  the relay can say who wrote something without taking the agent's word for it
  — authorship on every event comes from the key, and `KNOOT_USER` can no
  longer write an event as somebody else. Keys minted before this land in the
  team's `general` room as "unassigned", still working, until an admin attaches
  them to a person.
- **A room is an access group over areas.** People, plus the `(repo, area)`
  pairs they work in; a member's key grants the union of their rooms. Every
  team gets one room called `general` over every repository, so a small team
  never meets the word. `MULTIPLAYER.md` is the design this comes from.
- **Device keys are stored as SHA-256 hashes.** A database dump hands over nothing
  that works, and an existing key can never be shown to you again — only
  replaced. Mint one per machine, so revoking one costs you nothing else.
- **A team cannot address another team's log.** Every repo key is namespaced by
  team id at the two places a repo is named, so two teams can both have a repo
  called `api` and neither can read the other. `tests/teams_api.rs` asserts
  that through the HTTP surface, not of a helper.
- **A team is not an operator.** The lab's terminals are real shells on the
  host, so they require the relay's own configured secret — not merely a valid
  token.
- **You cannot revoke your way out.** The last live device key is refused, and
  so is removing the last person holding one, because there is no recovery path
  and nobody to ask.

The front end is a Vite app in `web/`, built to `web/dist` and embedded into
the binary with `include_dir!`, so your own relay serves all of it with no
second deployment and no CORS: `/` the site, `/docs`, `/status`, `/app` the
console, `/ops` the original single-team operator view.

```sh
npm --prefix web ci
npm --prefix web run build     # required before cargo build
cargo build --release
```

`web/dist` is committed for exactly one reason: `cargo install --git` has no
way to run npm, and a relay that cannot serve its own console is not one
binary. CI rebuilds it on every push, so a stale `dist` cannot ship.

Sign-in needs a Supabase project. Without one the relay still runs and agent
tokens still work; the console simply says sign-in is not configured.

```sh
# the browser bundle, at build time. The publishable key is public by design.
VITE_SUPABASE_URL=… VITE_SUPABASE_PUBLISHABLE_KEY=sb_publishable_… \
  npm --prefix web run build

# the relay, at run time — it verifies a signed-in person's access token.
# The secret key never goes near the browser.
SUPABASE_URL=… SUPABASE_PUBLISHABLE_KEY=sb_publishable_… \
  SUPABASE_SECRET_KEY=sb_secret_… knoot relay
```

These are Supabase's current API keys. The legacy `anon` and `service_role`
JWTs still work and are still read under their old names
(`SUPABASE_ANON_KEY`, `SUPABASE_SERVICE_ROLE_KEY`, `VITE_SUPABASE_ANON_KEY`),
which matters because Supabase retires them at the end of 2026. The two formats
are not interchangeable on the wire: a `sb_secret_…` key is not a JWT, so
sending it as a bearer token is rejected as an invalid JWT. It goes in the
`apikey` header alone, and `src/cloud.rs` has a test for each format.

Apply the migrations in `supabase/migrations` in order. `0001_teams.sql`
creates `teams` and `team_members` behind row-level security, so a browser
holding the publishable key can read only its own team. `0002_invites.sql`
adds `invites` and `accept_invite`, which is how a second person gets into an
existing team — `create_team` refuses a second team per user, so without it a
team is permanently a team of one. Invitations are stored as hashes too, are
good for seven days, and only work for the address they were sent to.

Three places hold configuration, and the split is the security boundary:

| Where | What goes there | Why |
|---|---|---|
| `web/.env` (gitignored) | `VITE_SUPABASE_URL`, `VITE_SUPABASE_PUBLISHABLE_KEY` | Local development. Vite reads it at build time. |
| GitHub Actions secrets | `SUPABASE_URL`, `SUPABASE_PUBLISHABLE_KEY` | CI bakes them into the released binary's front end. |
| `/etc/knoot/supabase.env` on the relay host, 0600 | all three, including `SUPABASE_SECRET_KEY` | The relay resolves team membership at run time. |

The secret key appears in exactly one of those. Anything named `VITE_*` is
compiled into JavaScript that anyone can read, so a secret key must never be
set there.

The `dist/` committed to this repository is built by `npm run build:oss`, which
points Vite at an empty env directory. That keeps one project's keys out of
what `cargo install --git` serves; a self-hosted console simply reports that
sign-in is not configured, and agent tokens work as usual.


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
- **A write is also checked against what the agent read.** A file it read and
  reasoned about, that somebody else has since changed, is reported before the
  next write — even when the file being written is nobody's. That is the half
  of a conflict a lock cannot see, and it is advisory: the agent re-reads
  instead of being stopped.
- **Creations, deletions and duplicate tasks are reported too.** Two agents
  creating one new file, a file deleted under someone who had read it, a peer
  who declared the same task a minute ago — the three collisions a claim on an
  existing path is blind to, and between them a large share of what actually
  goes wrong when agents share a repo.
- **Widely-shared files are queued, not owned.** A path three sessions claim
  inside half an hour — or one named in `hubs` in `.knoot.toml`, such as
  `package.json` or a shared type file — gets a two-minute lease instead of
  ten, renewed by writing rather than by being active, and a denial says how
  many sessions are ahead of you. A hub held for a whole turn is every other
  agent's critical path.
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

On a repo big enough that "everyone" is the wrong audience, divide it into
areas — the unit of who can collide with whom. A room grants `(repo, area)`
pairs, and a session is only told about work in the areas its key was granted.

```sh
knoot areas                                 # what this repo's subtrees are
knoot areas --import-codeowners --write     # take them from CODEOWNERS
```

Declaring none is the normal case and means one area, `/`, holding the whole
repo — exactly how every repo behaved before areas existed.

## Working with people, not only agents

Claude Code sessions announce themselves through hooks. Anyone else — a
teammate in VS Code, an agent under another tool — is invisible unless they say
so:

```sh
knoot present --doing "rewriting the tax rounding by hand"
```

You appear in `knoot who` as a **person**, files you touch are held while you
are in them, and anything addressed to you prints as it arrives. Agents are
told something different about you than about each other: a person cannot be
asked to release a file, so their brief says to pick different work rather than
to wait.

## Why is this file like this?

```sh
knoot why src/response.js
```

```
src/response.js
    2m ago  sam@example.com set out to: normalise the error shape in response.js
    2m ago  sam@example.com took it — "normalise the error shape in response.js"
    2m ago  sam@example.com wrote it
    1m ago  priya@example.com was blocked; sam@example.com held it
    1m ago  sam@example.com said: "taking response.js, about 10 min"

what the team knows about it:
  [facts] error-shape
    errors are {code, message}; never a bare string
```

Every event has always been on the log; this reads it back as one file's story
— the claims, the denials, what people said to each other, and what the team
has since decided about it.

## Shared memory

What one agent worked out, the next one is told — on the turn it opens the same
code, without running a command for it.

```sh
knoot remember --name money --path src/billing.js "all money is integer cents; never floats"
knoot plan --path src/billing.js --decided "cents, not floats" "rewriting the tax rounding"
knoot cache --name "how tests run" --path test.js "node test.js"
knoot recall                                       # what this repo knows
```

Three kinds, all scoped to an area and all reaching an agent unasked:

| | what it is | lives |
|---|---|---|
| **facts** | a durable statement written on purpose — a convention, a decision, a gotcha | 90 days, superseded chains kept |
| **repo_cache** | something derived: where a symbol lives, how the tests run | 14 days, **dropped** the moment its files change |
| **session_context** | what a session is doing now, and what it has settled | the session |

Nobody has to run anything for the third row. Every turn, the daemon publishes
what a session appears to be doing, composed from the intent it declared and the
files it holds — a lab run found that agents told outright to publish a plan
simply did not, so this cannot be a command. It is marked as composed, and reads
that way to a peer: *appears to be working on*, not a plan they wrote.

`knoot plan` is what a capable agent adds on top, and it is the one worth
knowing about. An intent is one line scraped from a prompt; a plan says what the
approach is and what has already been settled, which is what stops a peer
designing against work in progress. Once a session declares one, the daemon
stops composing for it — a scrape must never overwrite a plan. Either way it
appears at the top of every same-area session's next turn.

A fact names the paths it is about, and that is what makes it more than a note:
a write to one of those files marks the fact *possibly stale* and names who
changed it, and opening one puts the fact on the agent's brief. Writing the same
`--name` again supersedes the earlier statement rather than standing beside it,
so two agents contradicting each other produce one current answer and a record
of what changed. Every fact carries the person who wrote it, taken from their
device key rather than from what their client says about itself.

Publishing is **refused**, and the attempt logged, when the text or its source
file looks like a credential — anything `.gitignore`d, `.env*`, `*.pem`,
`*.key`, `id_*`, or a token this project recognises. Nothing is ever published
that an agent did not write on purpose.

Facts are sealed on the machine that writes them, through a key provider. The
relay chooses, because sealing is a property of the deployment:

- **`plaintext`** (default) stores shards readable. Right for a relay inside
  your own network, where the box is the trust boundary and "can the vendor
  read this" was answered by where the box is. An integrity tag still catches
  a store that swaps or loses rows.
- **`mls`** (`KNOOT_KEY_PROVIDER=mls` on the relay) makes each room an MLS
  group (RFC 9420, via OpenMLS). Each machine is a leaf; the key for an area's
  memory is exported from the group and sent nowhere. The relay is the
  Delivery Service — it holds key packages and orders handshake messages, and
  can read neither those nor a single shard. Adding someone is an Add
  proposal from a member's machine; removing them moves the room to an epoch
  their laptop cannot derive.

`knoot status` says which provider is in use, and tells you when this machine
is still waiting for a room's key.

## Adding people

A team starts with one person, whoever registered it. On a relay attached to
Supabase, the console invites the rest by email. On a self-hosted relay there is
no sign-in to invite anybody to, so you create the person and hand them a key:

```sh
knoot member add priya@example.com --label "priya laptop"   # prints the key once
knoot member ls                                             # who is here, and their machines
knoot member rm priya@example.com                           # their keys stop; nobody else's change
```

The key is shown once and is not recoverable — send it over something private,
because it speaks as them until revoked. The console's Members tab does the
same thing when the relay has no Supabase behind it.

This matters more than it looks. A key names a *person*, and that is what
authorship on the log, membership of a room, and the provenance on every
memory shard are resolved from. A team where every key named the same human
had rooms and areas that could not mean anything.

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
cargo test          # 146 tests, ~4s
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
