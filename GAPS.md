# coord — the gap between what it is and the best product in its category

_2 September 2026. Companion to REPORT.md, which says what is true; this says
what should be._

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

Everyone else is building fences between agents. coord is a room they can work
in together, and the evidence says the room produces better code. Make the room
impossible to get hurt in, make everything in it visible without asking, and
tell people about conflicts before git does.

---

## Gaps, in the order they should close

### 1. Coordination is offered, not pushed

**Today.** `coord who` and `coord msg` are commands an agent may or may not run.
Peer intents are injected every prompt; nothing else is.

**Evidence.** Four Haiku agents, given `GOAL.md` and later a prompt that told
them outright to run `coord who` before writing: zero invocations, zero
messages, two of four ended with the ownership map wrong. The same model, denied
a file on express, read the holder and remaining lease off the conflict brief
and behaved correctly on the first try. Pushed context works on the weakest
model; offered context is ignored by it.

**Best.** Zero commands an agent must know. Every turn's context already carries
who is here, what each peer has touched since this agent's last turn, what was
said to it or to `all`, and what it is about to collide with. `coord who` and
`coord msg` remain for power users; the default agent never types either and
coordinates anyway. Everything needed is already in `View`; the injection path
is `hook.rs` `UserPromptSubmit`.

**Cost.** Days. Cheapest item here and the one that turns a feature Opus uses
into a product every model uses. **Do this first.** Then re-run
`lab/haiku-run.sh` — if the message count moves off zero, the mechanism is
found; if it does not, the product is Opus-only and should say so.

### 2. Claims are blind to branches

**Today.** `SessionStarted` records the branch; `conflicting()` never reads it.
Two people on two feature branches editing one file are blocked as if they were
on one branch.

**Why it matters.** Remote teams live on branches. As-is this is a false
positive that gets the tool switched off. Reframed, it is the most valuable
signal in the product: a merge conflict predicted hours before git would report
it, at the moment re-planning costs one turn instead of an afternoon.

**Best.** Same branch → hard block, as now. Different branch → no block, and
presence says *"priya is in this file on `feat/discounts`; you will conflict at
merge."* Over time the log records which warnings came true. Nobody in the
category has this. It is the feature a user describes to a colleague.

**Cost.** About a day. Should come before any GTM conversation.

### 3. Sharing is not yet *provably* safer than isolation

**Today.** Write/Edit and shell writes are gated. Interpreter writes
(`python3 -c "open(...)"`) are detected after the fact, never blocked.
Attribution under concurrent writes is inferred from a peer's `FileWritten`
plus whether the command named the file.

**Isolation's whole argument** is "nothing can go wrong." Beat it on its own
terms.

**Best.** `ungated_write = 0` is a published guarantee, not a metric. Interpreter
writes are gated the way shell writes are. The sentence that lets a team turn
off worktrees is: *no agent has ever silently overwritten another's work under
coord.* Every live run so far supports it; make it a promise and instrument it.

### 4. Only Claude Code sessions are visible

**Today.** Hooks fire inside Claude Code. A teammate in VS Code, or their agent
under Cursor, does not exist to coord.

**Why it matters.** Real teams are mixed, and the coordination pain remote teams
feel is mostly *human* — who is in what. Whoever owns the shared view owns the
team.

**Best.** The relay protocol is client-agnostic already. A plain file-watcher
client registers a human editor's touches as claims; a Cursor hook adapter
follows. The presence list shows a Claude session, a Cursor session and a
human, side by side. Start with the watcher; it is small.

### 5. The log is written and never read

**Today.** Every event is in SQLite. Two dashboards render the live tail.
Nothing answers a question about the past.

**Best.** A flight recorder for agent work. *"Why is `response.js` like this?"*
→ `coord why lib/response.js`: sam claimed it with intent X, priya was denied,
they exchanged two messages, sam wrote it, the suite went green. Every team
with more than one agent will need this and none has it. Also the substrate for
everything enterprise wants later — visibility, policy, spend attribution — so
nothing built here is wasted.

### 6. Fail-open is a footnote

**Today.** Eight tests hold the line; the README mentions it partway down.

**Why it matters.** Every tool in this space eventually gets switched off
because it got in the way once. coord has proof that it cannot.

**Best.** First line of the README: *if coord is broken, your agents do not
know.* The distinction from every alternative, stated where people decide.

### 7. Not deployable by a team yet

**Today.** Relay on `127.0.0.1`, no auth, no TLS.

**Best.** Hosted relay, bearer token per team, TLS. No product thinking; an
afternoon. Repo identity is already derived from the `origin` URL, so two
clones on two machines land on the same stream, and `coord init` writes hooks
into a committable `.claude/settings.json`, so one person runs init, pushes,
and the whole team is enrolled. Lean on both — that is the onboarding story.

### 8. The central number is still unmeasured

**Today.** Five live runs. Collisions: one, and only because the express tasks
were written to collide. Capable agents negotiate ownership up front and never
contend; cheap agents stay in their lane and never contend.

**Best.** A week of *unforced* work on a real repo by a real team, then read
`claim_denied`, cross-branch warnings and `ungated_write`. One design-partner
startup, 3–5 developers, is both the dogfood week and the first GTM
conversation. Either answer is fine. Not knowing is the only bad state, and it
is the current one.

---

## What to say no to

- **Symbol-level claims** until the log shows file-level granularity is what
  bites. Most expensive item in the backlog; no evidence yet that it is needed.
- **Fleet mode / merge queues.** That is building the competitor's product
  inside this one. If sharing works, isolation is the fallback, not the roadmap.
- **A model in the arbiter.** The 4.1 ms, the determinism and the replayable log
  *are* the product. Intelligence stays in the agents; coord stays the honest
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

1. Push coordination into every turn (gap 1) — days
2. Branch-aware claims (gap 2) — a day
3. Hosted relay with token auth (gap 7) — an afternoon
4. One design-partner team, one week, read the numbers (gap 8)
5. Then, and only then, decide between gaps 3, 4, 5 on what the log says
