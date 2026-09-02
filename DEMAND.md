# coord — is there demand for this?

_2 September 2026. Companion to REPORT.md (what is true about the code) and
GAPS.md (what would make it best). This one asks whether anyone wants it._

Method: primary sources over search summaries — GitHub's API for stars, forks
and issue threads; Hacker News' search API for story scores and full comment
threads; Anthropic's own documentation. **Reddit is a blind spot**: its API
returns 403 to unauthenticated clients and every query was blocked. Stars
measure builder enthusiasm, not paying demand; they are still good at showing
asymmetries, and the asymmetries here are large.

---

## The short version

| What coord does | External demand | Where it stands |
|---|---|---|
| Messaging and presence between sessions | **Strong** — 2,202 stars in five weeks | Absorbed by Anthropic |
| File claims / locks (the core) | **None measurable** — 0 reactions, 0–7-star tools | Open because nobody wants it |
| Cross-branch conflict warning | **Modest, real** — 63 and 125 stars | Incumbent: `clash`, single-machine |
| Cross-developer / team coordination | **None found** | Unvalidated by anyone, us included |
| Audit log / governance | **Real** — 1,071 stars | Incumbent: `bernstein` |

Two conclusions. The lock is a solution without demonstrated demand. The one
thesis left standing — that a *team's* agents on several machines collide and
need to know about each other — has no evidence for or against it anywhere.

---

## 1. Messaging between sessions: strong, proven, gone

`louislva/claude-peers-mcp` — *"Let your Claude Code instances find each other
and talk"* — reached **2,202 stars and 301 forks in five weeks** (created
2026-03-21, last push 2026-04-26). Its README's canonical prompt is *"send a
message to peer xyz: what files are you editing?"* File awareness is what
people wanted peers for.

Then it stopped. Its most-reacted open issue (#6) is *"Claude uses built-in
SendMessage instead of mcp__claude-peers__send_message"*: Anthropic shipped
cross-session messaging natively and the model preferred it.

The first-party issue behind that, anthropics/claude-code **#24798** (84
comments, Feb–Aug 2026), is the best demand document in the space. A user
running five concurrent sessions: *"The migration session is changing the
server IP, certificates, and service configs. Every other session still has the
old IP hardcoded in its context. There is no way for the migration session to
notify the others."* Several commenters had built their own HTTP messaging
protocols from scratch to get around it. Closed 2026-08-17 as shipped.

What shipped covers the whole ask: same-machine delivery over per-session
sockets, cross-machine delivery through Remote Control, idle notices, and Agent
Teams' mailboxes and shared task list. Scope is precise — **your own
sessions** — which leaves exactly one thing uncovered: sessions belonging to
different people.

**For coord:** gap 1's push mechanism was built on the right instinct and into a
market that closed while it was being built. Do not compete on messaging.

## 2. File claims: coord's core has no measurable demand

- claude-peers issue **#72 — "Add repo-scoped work claims / leases for parallel
  sessions."** Exactly coord's claims, requested in the most popular project in
  the space. **0 reactions, 0 comments.**
- Every repository whose pitch is file locking for agents:
  `file-lock-coordinator` 0★, `crew` 0★ (183 commits — a well-built tool nobody
  uses), `wingman` 1★, `axis` 5★, `parallel-sessions` 6★, `cerebra` 7★.
- Anthropic's Agent Teams doc, for the one mode that shares a directory:
  *"Two teammates editing the same file leads to overwrites. Break the work so
  each teammate owns a different set of files."* They looked at the problem and
  chose to hand partitioning back to the human.

People describe the collision constantly — *"if both agents work on the same
file, all hell breaks loose"* is the most-repeated sentence in the genre — and
nobody adopts a lock to solve it. The revealed preference is **worktree
isolation**: dozens of Show HN worktree managers (Worktrunk, wt, gw, Harness,
Amux, `ccswarm` 149★, `diri` 279★, `groundcrew`, `agetor`), almost all under
ten points, and the accepted cost is merge pain later rather than coordination
now.

This matches our own runs. Five live runs, one collision, and that one was
forced. Capable agents partition themselves; cheap ones never leave their lane.

**For coord:** the lock is a safety net, not the product. The whole
"claims with leases, 400 races, one winner" story is engineering nobody asked
for. Keep it because fail-open makes it free; stop pitching it.

## 3. Conflict prediction across branches: modest and real, with an incumbent

- `funador/claude-code-merge-queue` — **125★, 42 HN points, 22 comments**, the
  most-discussed project found. Isolation plus a local merge queue.
- `clash-sh/clash` — **63★**, Rust, Claude Code plugin, February 2026. *"Avoid
  merge conflicts across git worktrees for parallel AI coding agents."* Its
  problem statement is gap 2's: agents in separate worktrees are *"blind to
  each other's changes and inevitably touch overlapping parts of the codebase.
  Conflicts only surface at feature completion."*

One HN commenter, unprompted: *"set up a channel for agents to communicate and
you shouldn't have issues with messy merges."* That is coord's thesis, said by
a stranger.

**For coord:** this is the one direction where the market agrees. `clash` does
it for one machine's worktrees; coord does it across machines on a shared tree
with live leases. Small niche, validated, occupied at the single-machine end.

## 4. Team coordination — the GTM wedge: nothing found

Every issue, thread and repository above is **one developer running many
agents**. The single time a team came up — in the merge-queue thread, *"have
you tried this on a team with multiple humans?"* — the author answered: *"I
have not used this on a team… you'd end up with a giant PR that no one wants
to review… that would probably be the end of the package at that workplace."*

The one project explicitly aimed at cross-user coordination, `beadhub`
(*"real time coord for coding agents across different minders"*), has **5
stars**. `scrubjay`, cross-machine sync, has 14.

This does not prove the need is absent. It may be early: teams where several
developers each run agents daily are new, and their tooling conversation has
not started. But there is, today, **zero external evidence** for "remote teams'
agents collide and want a coordinator." The thesis is ours alone.

## 5. Adjacent and validated: receipts

`sipyourdrink-ltd/bernstein` — **1,071★, 147 forks, 332 open issues, pushed
today.** *"The open-source governance layer for AI agents. No model in the
coordination loop… offline-verifiable run receipts, signed lineage + an opt-in
HMAC audit chain."* Worktree per task, forty-plus agents supported, enterprise
tone.

That is GAPS.md's items 4 and 5 — the log as product — built by someone else on
the isolation model. It shows the log has a market. It also means that market
has an incumbent with a thousand stars and a head start.

---

## What this means

**Stop building.** Three of GAPS.md's eight items are done and the code is in
good shape. Nothing further should be written until the question below has an
answer, because every remaining item assumes a customer who has not been shown
to exist.

**Validate the only thesis left, and do it by talking, not shipping.** Five
conversations with teams where more than two developers run coding agents
daily. One question: *has one of your agents ever stepped on a colleague's
work — not your own other session, a colleague's — and what did it cost?*
Follow-ups: how did you find out, how long after, what do you do about it now.
If four of five shrug, that is the answer, and it was cheaper than a deploy.

**If the demand is there, the positioning that survives this research is
narrow:** `clash` across machines, plus receipts. Early warning that a
colleague's agent is in the file you are about to change — on their branch,
hours before git can say so — with a log that shows what happened afterward.
Not a lock. Not chat. Both of those are either unwanted or first-party.

**What to say no to, with new reasons:**

- **Messaging features.** Anthropic owns this now, including cross-machine.
- **The lock as a headline.** Zero demonstrated demand; keep it as the safety
  net it already is.
- **Single-developer orchestration.** Crowded (a dozen worktree managers,
  `diri`, `ccswarm`), commoditized, and now first-party via Agent Teams.
- **A general governance layer.** `bernstein` has a year and a thousand stars.
  If receipts matter for coord, they are *team* receipts — who touched what
  across people — which is the one slice bernstein's per-developer worktree
  model does not produce.

---

## Revised sequence

1. Five team interviews. One week. Write down the answers verbatim.
2. If the collisions are real: fix the shipping blocker (`coord init` writes an
   absolute path to the binary into the committed hook config), one hosted
   relay, one design-partner team, one week of unforced work, read the log.
3. If they are not: the honest options are to shelve coord, or to reposition it
   around what the log can say about a team's agents — and that is a different
   product with a different first customer, to be decided then, not now.

---

## Sources

- anthropics/claude-code #24798 — Inter-session communication for multi-Claude
  workflows (84 comments, closed as shipped 2026-08-17)
- anthropics/claude-code #48965 — Multi-session coordination primitives (closed)
- louislva/claude-peers-mcp — 2,202★, issue #72 (work claims, 0 reactions),
  issue #6 (built-in SendMessage preferred)
- sipyourdrink-ltd/bernstein — 1,071★
- funador/claude-code-merge-queue — 125★; HN "Show HN: A local merge queue for
  parallel Claude Code agents", 42 points, 22 comments
- clash-sh/clash — 63★
- rchaz/git-stint 35★, henba1/scrubjay 14★, beadhub/bdh 5★, and the file-lock
  tools listed in §2
- Anthropic docs: Agent Teams; Cross-session messaging; Worktrees
- Hacker News search API, queries on parallel/multi-agent coding, Sept 2026
- Reddit: blocked (HTTP 403), not consulted
