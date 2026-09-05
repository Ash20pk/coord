# knoot — the gap between what it is and the best product in its category

_2 September 2026, revised as each gap closed. Companion to REPORT.md, which
says what is true; this says what should be. Gaps 1–7 are closed and say so in
place, with what closed them. Gap 8 has now been **run**, and the result is
mixed: memory demonstrably changes what a weak model writes, the collision rate
under unforced work is still unmeasured, and the run found two defects that no
test had. It also records the run that measured nothing, because a measurement
harness that can fail silently is the most dangerous thing in this file._

The category — tooling for several coding agents on one repo — has one dominant
answer: **isolate them.** A worktree per agent, merge later, a human resolves
the conflicts. It is safe, and it throws away the one thing the live runs proved
is valuable: agents that can see each other write better code than agents that
cannot. The interface negotiation, the percentage-vs-fraction correction, the
rounding bug found across a file boundary by the agent that owned neither file —
none of that happens in a worktree.

So the way to be best is not to be a better lock. It is to be the only product
that lets agents **share** a codebase and makes sharing strictly better than
isolating. Every gap below is measured against that.

---

## The thesis, in one line

Everyone else is building fences between agents. knoot is a room they can work
in together, and the evidence says the room produces better code. Make the room
impossible to get hurt in, make everything in it visible without asking, and
tell people about conflicts before git does.

---

## Gaps, in the order they should close

### 1. Coordination is offered, not pushed — CLOSED

**Was.** `knoot who` and `knoot msg` were commands an agent may or may not run.
Peer intents were injected every prompt; nothing else was.

**Now.** Nothing an agent needs sits behind a command. Every turn carries mail,
peers and their branches, what each has written since this agent's last turn
(and which of those files this session had actually *read*), a peer on the same
task, a file it read that has since moved or gone, a hub it is queueing for,
what the team has decided about the code it is in, what has already been worked
out, and what peers are doing right now. On a denial the same brief rides the
refusal, which is the highest-attention surface there is. `knoot who` and
`knoot msg` remain for power users and nothing depends on them.

**Evidence.** Four Haiku agents, given `GOAL.md` and later a prompt that told
them outright to run `knoot who` before writing: zero invocations, zero
messages, two of four ended with the ownership map wrong. The same model, denied
a file on express, read the holder and remaining lease off the conflict brief
and behaved correctly on the first try. Pushed context works on the weakest
model; offered context is ignored by it.

**Left.** The measurement. `lab/haiku-run.sh` now plants a fact and a plan and
scores whether either reached a transcript, but **the run has not happened** —
see gap 8. Until it does, "pushed context works on the weakest model" remains
an inference from one earlier observation rather than a result.

### 2. Claims are blind to branches — CLOSED

**Was.** `SessionStarted` recorded the branch; `conflicting()` never read it.
Two people on two feature branches editing one file were blocked as if they
were on one.

**Why it matters.** Remote teams live on branches. As-is this is a false
positive that gets the tool switched off. Reframed, it is the most valuable
signal in the product: a merge conflict predicted hours before git would report
it, at the moment re-planning costs one turn instead of an afternoon.

**Now.** Same branch and same area → deny. Different branch → no block and a
`CrossBranchOverlap` on the log, with the brief saying these files meet at
merge rather than now. `same_branch` is consulted by the arbiter, by the local
pre-check, and by the presence brief, so all three agree.

**Left.** Nothing in the mechanism. The log records the warnings; nobody has
yet gone back to count which came true, which is the same missing week as gap 8.

### 3. Sharing is not *provably* safer than isolation — CLOSED, claim narrowed

**Was.** Write/Edit and shell writes were gated. Interpreter writes
(`python3 -c "open(...)"`) were detected after the fact and never blocked, and
that detection went to the log and nowhere else — so an agent could overwrite a
peer's work and neither of them would find out until merge.

**The claim this section asked for cannot be made.** "Interpreter writes are
gated the way shell writes are" is not implementable: `python3 -c "..."` is a
*program*, and knowing what it writes is knowing what it does. Anything that
appeared to gate it would either be a guess or would have to deny every
interpreter invocation, and a tool that stops agents running python is a tool
that gets switched off. So `ungated_write = 0` cannot be a guarantee, and
publishing it as one would be a lie with a metric attached.

**What is true, and is now enforced.** Re-read the sentence this section was
reaching for: *no agent has ever **silently** overwritten another's work under
knoot.* The operative word is silently, and that is deliverable. An
`UngatedWrite` now reaches **both** parties at their next hook boundary — the
holder is told their file was written under them and to re-read it before
relying on their copy, and the writer is told they may have overwritten
somebody and who to tell. Before this, the event went to the log and the
dashboards; neither agent was ever informed.

So the promise is: **writes we can gate are gated; writes nobody could gate are
never quiet.** That is a stronger claim than isolation can make — a worktree
tells you at merge — and unlike `ungated_write = 0` it is one the mechanism
actually supports.

**Left.** Attribution under concurrent writes is still inferred from a peer's
`FileWritten` plus whether the command named the file. It has been right in
every live run and it is still an inference.

### 4. Only Claude Code sessions are visible — CLOSED

**Was.** Hooks fire inside Claude Code. A teammate in VS Code, or their agent
under Cursor, did not exist to knoot.

**Why it matters.** Real teams are mixed, and the coordination pain remote teams
feel is mostly *human* — who is in what. Whoever owns the shared view owns the
team.

**Now.** `knoot present` is that client. It watches the working tree — through
`git status`, so it already knows about `.gitignore` and agrees with the repo
about what a change is — and registers what you touch through exactly the
daemon requests a hook uses. You appear in `knoot who`, agents are blocked out
of your files with a brief naming you, and mail addressed to you prints as it
arrives. Leaving releases everything; a session that dies holds its claims only
until the lease expires.

One thing that had to be added beyond a watcher: a person and an agent are
**not** the same kind of peer. An agent can be asked to move and a person
cannot, so a human session is marked (`proto::HUMAN_SESSION_PREFIX`), `knoot
who` prints `person` or `agent`, and the brief says *"one of them is a person,
not an agent — do not ask them to release a file and do not wait on them, pick
different work."* A brief that blurred the two would give advice that cannot be
followed.

**Left, then closed for the agent that mattered.** Codex now speaks the hook
surface natively (`tests/codex.rs`, 13 tests against its real payload shapes),
which is the second *agent* — `knoot present` was the second *client*, and a
person, not an agent. Three things were not free: a patch that touches
several files is checked as a unit so a denial on one leaves no claim on the
others; deletions are announced once the path is really gone; and reads
through the shell are recorded, because Codex has no read tool — which turned
out to be a gap for Claude Code in auto mode as well. Cursor and Copilot
remain per-tool integrations: a matcher, a payload shape, a test file.

### 5. The log is written and never read — CLOSED

**Was.** Every event was in SQLite. Two dashboards rendered the live tail.
Nothing answered a question about the past.

**Now.** `knoot why <path>` is the flight recorder, and it prints what this
section asked for almost word for word:

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

Two passes at the relay: every event naming the path, then the session-level
events — intents, messages — of whoever touched it, because a claim without a
reason is a timestamp and a name. A message sent with `knoot msg` carries no
session id (a CLI caller cannot learn its own), so those are joined on the
*people* involved instead; dropping them lost exactly what somebody announced.
The path is matched as a whole JSON value, so asking about `src/a.rs` does not
return `src/a.rs.bak`'s history, and the team namespacing on the repo key holds
here like everywhere else.

**Left.** Nothing for a person at a terminal. The same query is the substrate
for the console view and for anything enterprise wants later — visibility,
policy, spend attribution — and none of that is built.

### 6. Fail-open is a footnote — CLOSED

**Was.** Eight tests held the line; the README mentioned it partway down.

**Why it matters.** Every tool in this space eventually gets switched off
because it got in the way once. knoot has proof that it cannot.

**Now.** It is the first line of the README, in those words, and the failure
paths are named: relay unreachable, token refused, daemon dead, key missing,
memory unreadable. Thirteen tests in `tests/failure.rs` hold the line, plus one
per subsystem added since — `a_relay_that_is_not_there_injects_no_memory_and_denies_no_write`,
`a_room_whose_group_has_not_formed_denies_no_write`,
`none_of_the_new_awareness_can_block_a_write`.

### 7. Not deployable by a team yet — CLOSED

**Was.** Relay on `127.0.0.1`, no auth, no TLS.

**Now.** `KNOOT_RELAY_TOKEN` makes the relay require a bearer token on the
websocket, the data APIs and the lab terminals (a terminal is a shell on the
host, so it is gated hardest); browsers pass `?token=` because they cannot set
headers on a WebSocket. `knoot login --relay <url> --token <t>` stores it in
`~/.knoot/credentials.toml` at 0600, keyed by relay origin so one login serves
every repo on that relay — and deliberately **not** in the committed
`.knoot.toml`. `KNOOT_TOKEN` overrides for CI. The client speaks `wss://`.
Startup prints whether auth is on, and says so loudly when an open relay is
bound off-loopback. A rejected token fails open, with one line on stderr
naming the fix: an operator's mistake cannot become the team's outage.

**Left.** TLS termination is a proxy's job today, and nothing is deployed
anywhere yet. The other two are done: a key is a **device** belonging to a
**member**, so authorship, room membership and memory provenance all resolve to
a person and one laptop can be revoked without touching another; and under the
`mls` provider a room's key rotates on every membership change, which is token
rotation of the only thing that needed it.

**Best.** Hosted relay, bearer token per team, TLS. No product thinking; an
afternoon. Repo identity is already derived from the `origin` URL, so two
clones on two machines land on the same stream, and `knoot init` writes hooks
into a committable `.claude/settings.json`, so one person runs init, pushes,
and the whole team is enrolled. Lean on both — that is the onboarding story.

### 8. The central number — RUN, and the answer is mixed

**Run 4 September 2026.** Four Haiku agents, three turns each, headless against
`GOAL.md` on the lab repo, with a fact planted in memory beforehand. Two runs
were needed; the first was thrown away, for a reason worth keeping.

**The first run measured nothing, and reported normally.** `knoot` was not on
`PATH`, so every hook in the lab resolved to nothing and four agents worked in
perfect isolation while the script printed its usual output. `knoot status` was
the only thing that said so — *"[FAIL] binary `knoot` not found on PATH"* —
and nobody was reading it. Two fixes came out of that, and they matter more
than the numbers: `haiku-run.sh` now proves the substrate (hooks installed,
relay usable, `knoot` resolvable, planted fact actually published) and refuses
to spend anything if it cannot; and `knoot status` now says **"memory
read-only: this key names no verified person, so nothing can be published"**
rather than a bare "0 facts", which reads as an empty room when the truth is
that nothing can ever be written.

**What the real run says.**

*The one clear win — memory changes what a weak model writes.* The planted
fact was "all money in this repo is integer cents; never floats". It reached
**3 of 4** transcripts unasked. `billing.js` came out as:

```js
// Invoice calculation. All money values are in integer cents.
const tax = Math.round(afterDiscount * taxRate);
```

with `discountCents`, no `parseFloat`, no division into fractions — against a
seed that read `subtotal + subtotal * taxRate`. The control is clean: neither
the seeded file, nor `GOAL.md`, nor `TASKS.md` contains the word "cents"; the
only source was knoot's memory. `node test.js` passed 12/12 including *"Money
uses cents (no floats)"*. **This is gap 1's question answered as well** —
pushed context works on the weakest model, on evidence rather than inference.

*The central number is still not measured.* `claim_denied 0`,
`ungated_write 0`, `stale_read 0`, `create_collision 0`, `path_removed 0`,
`message 0`. Sixth run in a row with zero contention: given roles, cheap agents
stay in their lane. So the phase-2 awareness signals reached **no** transcript
— not because the mechanism failed but because nothing happened to report. The
honest reading is that **an unforced week on a real repo is still the only
thing that can answer this**, and a role-partitioned lab cannot: it is
constructed so that agents do not collide.

*A real defect, found only by running it.* `duplicate_intent` fired **16 times
out of 16, every one false.* The harness hands all four agents the same
preamble, an intent is the first 160 characters of a prompt, so the four
intents were near-identical by construction and the overlap was real and
meaningless — precisely the "warning that fires on filler stops being read
within a day" failure this feature was meant to avoid. Fixed: words that most
of the live room is using are treated as that room's background and excluded
before comparison (`proto::boilerplate_words`), with the bar set strictly above
two sessions, because a word shared by exactly two *is* the signal. Both
directions are now tested from the run's own prompts —
`a_preamble_every_agent_was_handed_is_not_a_shared_task` and
`a_shared_preamble_does_not_hide_a_genuinely_shared_task`.

*And a negative result about phase 6 — now closed.* `plans published 0`,
`peers' plans seen 0`. Not one agent ran `knoot plan`, though the prompt asked
it to — which is gap 1's original finding recurring: **a cheap model does not
run a command it is told to run.** Publishing session context was a command, so
on these models it was a feature that did not exist.

The fix is gap 1's fix applied to phase 6: **the daemon composes it.** On every
turn it publishes what a session appears to be doing, from the intent that
session declared and the paths it holds — both of which were already on the log
and already in every peer's `knoot who` before the composer ran, which is what
makes this compatible with the rule that nothing is derived from a transcript.
There is no summarisation and there must never be; the composed text is the
intent, verbatim. Three things keep it from becoming noise: a session that ran
`knoot plan` is left alone (a composed context supersedes by session id, so
continuing would overwrite a real plan with a scrape of the same session's
prompt), an unchanged intent and path set republishes nothing, and the shard is
marked `derived` so a peer reads *"appears to be working on (from their intent
and claims, not a declared plan)"* rather than a guess in a plan's voice.
`knoot plan` remains, and is now the thing it should always have been: how a
capable model says what the *approach* is and what has been settled, which no
intent sentence can carry.

*One thing that is not a bug.* Every claim is attributed to
`lab@knoot.local`, because four agents on one machine share one device key and
authorship comes from the key. That is phase 1 working as designed — one
laptop is one person — but it means per-agent attribution in the lab is not
available, and the metrics table's "by user" column cannot separate them.
Report by session if that ever matters.

## What to say no to

- **Symbol-level claims** until the log shows file-level granularity is what
  bites. Most expensive item in the backlog; no evidence yet that it is needed.
- **Fleet mode / merge queues.** That is building the competitor's product
  inside this one. If sharing works, isolation is the fallback, not the roadmap.
- **A model in the arbiter.** The 4.1 ms, the determinism and the replayable log
  *are* the product. Intelligence stays in the agents; knoot stays the honest
  referee.
- **Enterprise features before one team has used it for a week.** Governance is
  a query over the log. It will be there when the buyer is.
- **"Remote teams" as a technical framing.** Two agents in one room collide
  identically. Remote teams are the audience that feels the pain — the right
  wedge — but the product is "teams running several agents on one repo." Market
  it as remote; build it as concurrent.

---

## The habit that should not change

REPORT.md names the bugs the fixes caused and the test that never restarted
anything. Keep shipping numbers with every claim, run the lab in CI, publish
collision rates from real repos. In a category full of demos, the product with
receipts wins by default.

---

## Sequence

_Revised 4 September 2026, after gaps 1–7 were built and gap 8 was run once.
The research found no external evidence for the team thesis this whole document
rests on, so validation still comes before any further building._

1. ~~Push coordination into every turn (gap 1)~~ — done, and now *measured*: a
   planted fact reached 3 of 4 Haiku agents and changed what they wrote.
   Anthropic has since shipped native cross-session messaging, so the delivery
   channel is table stakes; the memory that rides it is not.
2. ~~Branch-aware claims (gap 2)~~ — done; the one direction the market agrees
   with (`clash`, 63★, does it for one machine's worktrees)
3. ~~Hosted relay with token auth (gap 7)~~ — done, plus per-person device
   keys and MLS key rotation
4. ~~Gaps 3, 4, 5, 6~~ — done: ungated writes now reach both parties,
   `knoot present` puts a human in the room, `knoot why` reads the log back,
   fail-open is the first line of the README
5. **Five team interviews** — does a colleague's agent ever step on yours, and
   what does it cost? See DEMAND.md. **This is now the only thing in the way.**
   Six lab runs have produced zero unforced collisions, which means the lab
   cannot answer it: a role-partitioned repo is built so that agents do not
   collide. Only a real team's week can.
6. If yes: one design-partner team, one week, read `claim_denied`,
   `cross_branch_overlap` and `ungated_write` — and now also whether anybody
   ran `knoot plan`, because on cheap models nobody did.

~~**One thing to fix before that week, found by the run:** publishing session
context is a *command*, and no Haiku agent ran it despite being told to.~~
Done: the daemon now composes a session's context from the intent and claims it
has already declared, marked as derived so it is never read as a declared plan,
and standing down for any session that ran `knoot plan` itself. Phase 6 no
longer depends on a command the weakest model in the room will not run — so the
week can read `plans seen` and learn something from the answer.
