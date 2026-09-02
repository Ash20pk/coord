# coord — state of the project

_As of 2 September 2026. Nine commits, `2b4dd0e`..`a33719d`._

Realtime coordination for coding agents. Multiple Claude Code sessions work one
repo without overwriting each other, and — the part that makes it multiplayer
rather than just a lock — they tell each other what they are doing.

Roughly 3,400 lines of Rust plus two web pages. 82 tests, all passing, ~2.5s.

---

## What works

| Capability | Mechanism | Verified |
|---|---|---|
| Claims with leases | Per-repo sequenced log, single arbiter | 400+ concurrent races, exactly one winner each |
| Conflict briefs | `PreToolUse` deny carrying holder + intent | Live: a real session re-planned instead of writing |
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
| Unit — protocol | 39 | path-overlap boundaries, lease expiry, log-replay determinism, staleness |
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

Two of these (2 and 6) were caused by fixes to earlier ones. One intermediate
fix — treating a peer's claim as evidence of authorship — suppressed the genuine
detection case and was reverted; the honest limitation is documented instead.

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

1. **Dogfood on a real repo and read the numbers.** `claim_denied` is contention
   prevented, `ungated_write` is contention caught too late. If both stay near
   zero across a week of ordinary work, that is the real answer about timing —
   and it should be known before building anything larger.
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
cargo test                       # 82 tests, ~2.5s
./lab/lab.sh reset               # seed the goal repo
./lab/lab.sh web                 # four browser terminals + live log
./lab/lab.sh start               # or the four-pane tmux rig
```

Give each agent its role from `GOAL.md`, then watch. Afterwards:

```sh
sqlite3 ~/.coord/relay.db \
  "SELECT json_extract(json,'\$.type'), count(*) FROM events GROUP BY 1 ORDER BY 2 DESC;"
```
