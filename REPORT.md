# coord — state of the project

_As of 2 September 2026. Ten commits, `2b4dd0e`..`346ab0f`._

Realtime coordination for coding agents. Multiple Claude Code sessions work one
repo without overwriting each other, and — the part that makes it multiplayer
rather than just a lock — they tell each other what they are doing.

Roughly 3,400 lines of Rust plus two web pages. 84 tests, all passing, ~2.5s.

---

## What works

| Capability | Mechanism | Verified |
|---|---|---|
| Claims with leases | Per-repo sequenced log, single arbiter | 400+ concurrent races, exactly one winner each |
| Conflict briefs | `PreToolUse` deny carrying holder + intent | Live on express: a blocked agent read the holder off the brief and waited |
| Shell-write gating | Bash command parsed for write targets | `sed -i`, heredocs, `tee`, `cp`, `rm`, redirects |
| After-the-fact detection | Working-tree diff around opaque commands | Live: `python3 -c` write recorded as ungated |
| Presence | Peer intents injected every prompt | Live: a session deferred without ever being blocked |
| Release notification | `Stop` hook wakes a blocked session | Live against the running daemon |
| Direct messages | `coord msg <user\|all>`, delivered via hooks | Live: 16–28 messages per four-agent run |
| Audit trail | Every event in SQLite | Every run in this report came out of it |
| Terminal dashboard | `coord watch` | — |
| Browser dashboard | Relay serves `/` | — |
| Browser lab | Relay hosts PTYs, xterm.js at `/lab` | Drove four real sessions through it |

**Fail-open everywhere.** Relay down, daemon dead, relay hung, malformed input,
non-coord repo → the edit is allowed. coord can never be the reason an agent
cannot work. Eight tests hold this line.

---

## Architecture

```
agents ──hooks──► coord hook ──unix socket──► coordd ──websocket──► coord relay
(any terminal)      (shim)                  (local mirror)      (sequencer + arbiter)
                                                                       │
                                                          SQLite log ──┴── dashboards
```

The design decision that carries everything: **agent turns are transactions,
not commits.** Agents can be re-run cheaply, so a collision aborts and re-plans
rather than merging. That rules out both git's manual-merge model and a CRDT —
mutual exclusion cannot be merged, only arbitrated, so there is one sequencer
per repo and no consensus protocol.

Everything else is a consumer of the ordered event log: claims are a policy over
it, messages travel through it, the audit trail *is* it.

Hot path stays local: `PreToolUse` consults the daemon's mirror over a unix
socket. **4.1 ms end to end**, including process spawn and a relay round-trip.

---

## Test coverage

| Layer | Tests | What it protects |
|---|---|---|
| Unit — protocol | 41 | path-overlap boundaries, lease expiry, log-replay determinism, staleness, write attribution |
| Unit — bash parser | 20 | quoting, heredoc bodies, arrows, read-only proofs |
| Arbitration | 9 | 400+ races → one winner; brief contents; repo isolation |
| Failure injection | 8 | dead/hung relay, dead daemon, malformed input, crash recovery |
| Hook contract | 26 | real Claude Code payloads through the binary; exact JSON; latency ceiling |

---

## Live results: four agents, one goal

`lab/GOAL.md` sets a shared objective — ship an invoice endpoint, four owners,
one codebase — because unrelated tasks collide only by accident. Three runs.

**Final run: 65/65 tests passing.** The agents produced a working endpoint with
auth, discounts, tax rounding, and 401/400/404/405 handling. Unprompted, they:

- **negotiated contracts before writing** — one broadcast its assumed interface
  and asked whether `discount()` returned a number or an array
- **corrected a convention clash** — percentage vs fraction for discount,
  caught and broadcast as an explicit correction
- **settled a territory dispute by citing the goal** — *"my brief says I own
  src/api.js… if you think my validation is missing a case, msg me and I'll make
  the change in my file"*
- **found a cross-file bug** — two different half-up rounding implementations,
  *"they agree on today's tests but diverge on 1.005 and 8.835"* — flagged by
  the agent that did not own either file, and deleted by the one that did
- **hit a real coordination failure and engineered around it** — the empty-basket
  status code flipped three times as each deferred to the other; the resolution
  was to read the predicate off the peer's file so the endpoint tracks it either
  way, then ask them to settle it

Claims were attributed correctly: `ash → src/auth.js`, `priya → src/billing.js`,
`sam → src/api.js`, `ci-bot → src/types.js` + `test.js`.

**Zero collisions in the goal runs**, twice, and that is the honest headline:
the agents announced ownership up front and respected it, so presence and
messaging prevented contention *before* any block. Good product outcome; it also
means the block-and-notify path only gets exercised when overlap is forced. This
test measures coordination, not contention.

---

## Same goal, cheaper model: four Haiku agents

The three runs above were Opus. The same `GOAL.md` and the same four roles were
then given to four Haiku sessions, run headless (`claude -p --model haiku`) in
parallel against the live relay.

**The substrate held; the coordination did not.**

| | Opus runs | Haiku run |
|---|---|---|
| Result | 65/65 tests | 9/9 (ci-bot's own suite) |
| Messages between agents | 16–28 | **0** |
| Claims / writes | — | 18 / 18 |
| Denials, ungated writes | 0 | 0 |
| Attribution | correct | correct |

Claims were attributed correctly again — `ash → src/auth.js`, `priya →
src/billing.js`, `sam → src/api.js`, `ci-bot → src/types.js` + `test.js` — and
the endpoint works. Every mechanical claim about coord survived the model swap.

What did not survive is the behaviour the mechanism exists to reward. No agent
ran `coord who`. No agent sent a message, though `coord msg` was in every prompt.
Each stayed inside its own file and stopped. Zero collisions for the third time,
but for the opposite reason: Opus avoided them by negotiating ownership, Haiku
avoided them by never looking outside its lane.

The cost shows up in the sign-offs. `ash` closed with *"Priya can now add
`refreshSession()`"* — priya owns billing, not auth; `ci-bot` likewise recorded
priya as the auth.js owner. Both had the ownership map wrong, and `coord who`
would have corrected either one. Nothing in the Opus runs — the interface
negotiation, the percentage-vs-fraction correction, the cross-file rounding bug
found by the agent that owned neither file — has any analogue here. The 9-vs-65
test gap is the same fact: ci-bot wrote and graded its own suite with no input
from anyone.

So presence and messaging are *offered* affordances, and a cheap model does not
reach for them. If mixed-capability fleets matter, coordination has to be pushed
at the agent — the way a conflict brief is — rather than left as a command it
may or may not think to run.

**Reproduced from a clean seed**, scripted this time as `lab/haiku-run.sh`, with
a prompt that pushed harder than `GOAL.md` does — it told each agent the other
three were running in parallel and to run `coord who` before writing. Same
outcome: 16/16 on ci-bot's own suite, correct attribution, **zero messages and
zero `coord who` invocations** across all four transcripts. Being told to look
outside your lane is not enough; the mechanism has to arrive unasked.

---

## First real repo: express, and the first recorded collision

Every run above is a seeded toy — four files, one owner each — and every one of
them produced zero collisions. So coord's central path had never fired. The next
run used a real codebase: **expressjs/express** at `023767f`, four Haiku agents,
ordinary maintenance work, and three of the four given business in the *same
file* so contention was forced rather than left to chance (`lab/dogfood.sh`).

| | Lab goal runs (x4) | express |
|---|---|---|
| `claim_denied` | **0** | **1** |
| `ungated_write` | 0 | 0 |
| `path_freed` | 0 | 1 |
| Claims / writes | 14-18 | 4 / 4 |
| Messages | 0-28 | 0 |
| Repo suite after | — | 1260/1260 passing |

`priya` was denied `lib/response.js` while `ash` held it, and the brief did the
work it exists to do — she read the holder and the remaining lease straight off
it, completed her analysis without writing, and stopped with *"currently locked
by ash with about 9 minutes remaining... I'll proceed once the file is
released."* `path_freed` fired when ash finished. The diff is 34 lines across
`lib/response.js` and `lib/utils.js`, and express's own 1260 tests still pass.

Two honest limits. **N is one**: a single denial is evidence the path works, not
a measurement of how often it is needed — that still wants a week of ordinary
work. And priya *waited* rather than being woken and resuming: a headless run
ends at its turn boundary, so the release notification had nothing left to wake.
The interactive lab is where that half gets exercised.

---

## Bugs found by running real sessions — none by the test suite

This is the most useful thing to know about the project.

1. **Bash writes bypassed claims entirely.** Only Write/Edit were gated, and
   auto mode prefers `sed`/heredocs. A session modified a claimed file with zero
   recorded events. In practice coord gated almost nothing.
2. **A blind spot from a fix.** Skipping any path a peer wrote during the audit
   window meant a holder editing every few seconds masked *every* intruding
   write. A session added a function to a held file and coord recorded nothing —
   no claim, no denial, no ungated write.
3. **Idle sessions were pruned and could never re-register.** A 41-minute gap
   between startup and the first prompt destroyed identity for a whole run:
   two agents' claims were both logged as the OS user.
4. **Every message read "from ash".** Claude Code exposes no session id to the
   commands it runs, so the daemon fell back to `$USER`. Identity and mailboxes
   are now keyed by user.
5. **Heredoc bodies were lexed as shell.** An arrow function in a heredoc read
   `=>` as a redirect and claimed a file named `sum`; a TTL comparison claimed
   `SESSION_TTL_MS`.
6. **The audit blamed the wrong session** for a peer's concurrent write, because
   shell writes emitted no authorship event.
7. **A cold daemon's first edit bypassed arbitration** — the mirror was still
   empty and it answered from nothing.
8. **The shim's timeout equalled the daemon's worst case**, so under a hung
   relay it timed out blind instead of receiving the explicit fail-open verdict.
9. **`wss://` panicked on the first handshake, and `coord status` said `[ok]`.**
   Found by deploying a relay to a real domain. rustls 0.23 will not pick a
   crypto provider unless the build names one, and its refusal is a panic — in
   the daemon's relay task, which then died. Failing open did the rest: every
   edit allowed, `coord who` answering out of the purely local mirror, and the
   hosted relay's event log empty at zero rows. Every hosted deployment before
   this was decorative.
10. **`coord watch` dialled the relay with no token,** so the one surface a
    human checks to confirm coordination is on would have shown a red dot
    against exactly the relays that need one.
11. **`coord status` inferred the relay from the daemon.** It printed `[ok]`
    whenever the daemon answered a local request and a token was on disk,
    neither of which is evidence of a connection — which is what let 9 and 10
    hide. It now asks the daemon (`DReq::Health`) for the socket state, whether
    a snapshot has landed, and the last dial error, and distinguishes a 401
    from unreachable.

Two of these (2 and 6) were caused by fixes to earlier ones. One intermediate
fix — treating a peer's claim as evidence of authorship — suppressed the genuine
detection case and was reverted; the honest limitation is documented instead.

12. **A relay restart began again at seq 0.** Duplicate sequence numbers in
    the one log whose purpose is to be sequenced, and every claim and session
    forgotten — so two agents could hold the same file across a restart. Found
    by building a dashboard and watching it show an empty repo that plainly was
    not. Repos are now rebuilt from the durable log on first touch.
13. **A revoked token was treated as anonymous, not refused.** On a relay with
    no configured secret it fell through to the built-in `local` identity —
    which is the identity that gates the lab's ptys. Found while writing the
    test that a registered team is not an operator.

Bug 11 is the pattern behind most of this list: a check that reports on the
thing it can see rather than the thing you asked about. `coord status` exists
because fail-open makes "off" invisible, and it was itself guessing.

The suite is now honest about its own limits: a test named
`daemon_survives_relay_restart` never restarted anything and was renamed, four
tests were passing while the daemon was unreachable until allow-assertions got a
positive control, and the Bash gap was kept as a deliberately red `#[ignore]`d
spec until it passed.

---

## Known gaps

- **Attribution under concurrent writes is inferred.** The tree is shared, so a
  peer's edit lands in our audit window too. Authorship comes from their
  `FileWritten` event and a command naming the file; if neither is conclusive,
  an ungated write can be attributed wrongly.
- **Interpreters are detected, never blocked.** `python3 -c "open(...)"` writes
  first and is recorded second.
- **Claims are file-level.** Two agents legitimately working different functions
  in one file will contend. Symbol-level claims need real parsing.
- **Same-name sessions share a mailbox**, since mail is keyed by user.
- **Unexpanded shell variables become claim targets.** A run claimed the literal
  string `$TMPDIR/smoke.js`. Harmless but wrong.
- **Fail-open is ambiguous by design.** An allowed edit and an unreachable
  daemon look identical to the agent. Tests use a positive control; humans check
  for the green dot.
- **The relay is a single point of coordination.** Deliberate — one sequencer per
  repo, fail-open when it is gone — but there is no failover.
- **Whether a brief persuades a model** cannot be unit-tested. Live evidence is
  positive across every run, and no session ever attempted a shell bypass. Small N.

---

## What is worth building next

1. **Dogfood on a real repo and read the numbers.** Started: the express run
   above is the first one, and it produced the first denial. `claim_denied` is
   contention prevented, `ungated_write` is contention caught too late. One
   collision under forced overlap is not the number that matters — a week of
   *unforced* work is, and it should be known before building anything larger.
2. **Fleet mode** — worktree isolation plus an agent-driven merge queue, where a
   rebase conflict fires a conflict brief at the agent instead of failing to a
   human. This is the enterprise half; claims are the pairing half.
3. **Symbol-level claims** via tree-sitter, once the logs show how often
   file-level granularity is the thing that bites.
4. **Governance surface** over the log — fleet visibility, policy, spend
   attribution. The log already contains it; nothing reads it that way yet.

---

## Reproducing any of this

```sh
cargo test                       # 84 tests, ~2.5s
./lab/lab.sh reset               # seed the goal repo
./lab/lab.sh web                 # four browser terminals + live log
./lab/lab.sh start               # or the four-pane tmux rig
./lab/haiku-run.sh reset         # the four-Haiku run, headless, from a clean seed
./lab/dogfood.sh                 # clone express and run four agents over it
```

Give each agent its role from `GOAL.md`, then watch. Afterwards:

```sh
source lab/metrics.sh && coord_metrics ~/.coord/relay.db   # or: ./lab/dogfood.sh report
```

`coord_metrics` holds the one copy of these queries: event counts, claims by
user, both collision tables, and every message. Note that `seq` is per-repo, so
only `ts` orders runs against each other.
