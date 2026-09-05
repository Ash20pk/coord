# knoot — multiplayer: areas, rooms and shared memory

_4 September 2026, second draft. Replaces the first draft of the same day after
a review against the literature; the section "What changed and why" says what
moved. A design the code has now caught up with: **every phase is built** (see the
phase list at the end for what that covers and what it does not).
Companion to REPORT.md (what is true of the code), GAPS.md (what would make it
best) and DEMAND.md (whether anyone wants it)._

## The one-paragraph version

knoot today coordinates *sessions* that share a secret, by claiming *existing
files* on *one repo-wide log*. The evidence says the thesis is right — agents
in one workspace with write-time conflict detection outperform isolated agents
by a wide margin — and says the mechanism is too narrow in three ways: it is
framed as a lock when what scales is awareness plus optimistic detection; it
claims writes to existing files when 42% of real agent conflicts are creations
and deletions; and it scopes to the repo when the thing that actually bounds who
can collide with whom is a subtree. So the primitive becomes `(repo, area)`,
rooms become access groups over areas, memory arrives with a published failure
taxonomy as its spec, and end-to-end encryption uses a standard group protocol
behind an interface rather than a scheme of our own.

---

## What changed and why

| First draft | This draft | Because |
|---|---|---|
| Rooms are the coordination boundary; a `repo_rooms` table stops two rooms sharing a repo | Coordination is per `(repo, area)`; rooms are access groups | A table whose only job is to stop the design doing what it naturally does is a smell. The log is already per repo. |
| Claims are the product | Awareness + write-time conflict detection is the product; the same-branch block is one output of it | Google, grite, Perforce: locks do not scale, awareness does. STORM: write-time rejection wins, and it wins by checking what the agent *read*, not only what it writes. |
| Claims on existing paths | Also creation claims, delete visibility, hub-file policy, task claims | 33,596 agent PRs: 26.8% modify/delete, 15.1% add/add. grite: duplicate *tasks* were 78% of waste. |
| Room `seal` embedded in the credential; device-wrapped keys on rotation | Key-provider interface; OpenMLS behind it for the hosted tier | RFC 9420 exists for this: groups to thousands, O(log n) rotation, forward secrecy, post-compromise security. A scheme of our own would be worse and cost more. |
| Memory design from first principles | Memory design from MemClaw's four failure modes and four primitives | Someone already ran it in production and published what broke. |
| Automatic `session_context` from `Stop`, "candidate facts held for confirmation" | Nothing published that an agent did not write on purpose, in a structured shape | Free-text conclusions derived from a transcript are an exfiltration path with no reviewer. |
| `project_files` as a memory kind | Removed; `knoot remember --from <path>` into facts, same refusal rules | A kind with a default-off switch is the design admitting it is afraid of it. |

---

## Part 0 — what is actually there today

| Thing | Where it lives | Shape |
|---|---|---|
| People, teams | Supabase (`teams`, `team_members`, RLS via `is_member_of`) | `owner` / `admin` / `member` |
| Agent tokens | Relay SQLite (`tokens`), SHA-256 | one row per token, `team_id`, label |
| Event log | Relay SQLite (`events`), sequenced per key | `team_id/repo` |
| Live state | `proto::View` — claims, sessions, waiters, `last_write`, `recent_writes` | mirrored in every daemon |
| Identity on the wire | `teams::Identity { team_id, team_name, token_id }` | resolved per request |
| Authorship | `session_user()` — **self-reported by the client** | env / git config |
| Agent-facing surfaces | `hook.rs`: `PreToolUse` deny with brief; `UserPromptSubmit`/`SessionStart` context; `Stop` mail; `PostToolUse` | already the push path |
| Shell writes | `bashparse.rs` targets; `watch.rs` tree audit → `UngatedWrite` | closes the two holes STORM admits |

Three consequences: a team has exactly one coordination space; a token is a
team-wide bearer credential so `team_members` never reaches the hot path; and
authorship is a display string, which is fine until an access decision or a
memory's provenance depends on it.

---

# Part 1 — The primitive: `(repo, area)`

An **area** is a set of path prefixes inside a repo, with a name and a set of
people. It is the unit of *who can collide with whom*, and everything that is
per-repo today becomes per-area: the sequenced log, live claims, `knoot who`,
the per-turn injection, and memory.

```
team
 ├─ member                    a person                        Supabase
 ├─ repo                      an enrolled working copy        relay
 │   └─ area                  path prefixes + people          relay
 │        └─ log, claims, who, injection, memory
 └─ room                      an access group over areas      relay
```

- **Default area is `/`.** A five-person team never sees the word. The log key
  is `team/repo//` and behaves exactly as `team/repo` does today.
- **Areas are declared in the repo**, in `.knoot.toml` (committed, like the
  repo id), and can be **imported from `CODEOWNERS`** so a large org's existing
  structure is reused rather than re-entered. Google's answer to 50,000
  engineers on one repo is OWNERS files per subtree; this is that, for agents.
- **A path belongs to the most specific area.** `src/auth/` inside `src/` is
  the auth area. Overlap is resolved the way OWNERS resolves it, so nobody has
  to learn a second rule.
- **Cross-area edits are visible to both areas.** A write to `src/auth/x.js`
  by a session working in `/` is logged to `auth` as well. Areas bound
  attention; they never hide a write from the people it affects.

Why this and not rooms-own-repos: the log is already per repo, so rooms added
no partitioning — only a hazard (two rooms on one repo could no longer see each
other) and a table to police it. Areas partition along the axis where collisions
actually cluster, and the small case is a degenerate instance rather than a
different product.

---

# Part 2 — Coordination: awareness first, then the block

The product sentence changes. Not "a lock for agents" but: **every session
knows, every turn and without asking, who is here, what they are touching, what
moved under it, and what it is about to collide with — and a write that would
collide is stopped while the agent can still change its mind.**

Gutwin and Greenberg's workspace-awareness framework — *who* is present, *what*
they are doing, *where* — is what the `UserPromptSubmit` injection already
carries. What follows adds the pieces the evidence says are missing.

## 2.1 Read snapshots: the semantic half of a conflict

STORM's mechanism, and its single biggest result, is that a write is checked
against **what the agent read**, not only what it writes. If a file the agent
based its reasoning on changed since it read it, the write is stale even when
the target file is untouched.

knoot sees every `Read` through `PostToolUse` and every `FileWritten` on the
log, so this is nearly free:

- The daemon keeps, per session, `read_snapshot: HashMap<path, (ts, hash)>` for
  the current turn.
- At `PreToolUse` on any write, paths in the snapshot whose `last_write` is newer
  than the read are reported on the brief: *"you read `src/session.js` 40s ago;
  priya wrote it 12s ago. Re-read before editing."* Advisory, not a deny — the
  agent's target is not held. A deny here would be a false positive machine.
- At `UserPromptSubmit`, the same set becomes the existing "changed under you"
  section, which today lists peer writes but does not know which ones the
  agent actually depended on. Now it does, and it can rank them first.

## 2.2 Creation and deletion

Of 33,596 agent pull requests, 26.8% of conflicts were **modify/delete** and
15.1% **add/add** — two agents independently creating the same new file.
Claims on existing paths see neither.

- **Creation is a claim.** A `Write` to a path that does not exist claims the
  path exactly as an edit does. Two sessions creating `src/utils/retry.ts` in
  one turn: the second is told the first exists, with its intent.
- **Deletion is broadcast.** `rm`, `git rm`, `mv` (parsed by `bashparse`
  already) on a path that appears in any other live session's read snapshot or
  claims produces a `PathRemoved` event, delivered on that session's next hook
  boundary — the same `Stop`-block path that delivers mail today.

## 2.3 Hub files

Co-Coder's first move before parallelising is *structural hub isolation*:
widely-depended files serialise every agent's critical path. STORM admits the
same: "heavily shared files become serialization bottlenecks." In knoot this is
the file every agent wants and nobody can hold for ten minutes without stalling
the room.

- A path is a **hub** when it is claimed by more than `N` distinct sessions in
  a window (start at 3 in 30 min), or is named in `.knoot.toml` (`package.json`,
  lockfiles, shared type files, route tables are the usual suspects).
- Hubs get a **short lease** (2 min, renewed on write, not on activity) and a
  **queue line** in the brief: *"held by ash, 2 behind you."* Nothing else
  changes; the queue is awareness, the short lease is the mechanism.

## 2.4 Task claims

grite's headline number — duplicate work 78% → 0% — is about *tasks*, and file
claims cannot see a task. knoot already has the intent sentence on every turn.
Make it claimable: `IntentDeclared` is matched (normalised, fuzzy) against live
intents in the same area, and a near-match is reported: *"sam declared 'add
retry to the HTTP client' 3m ago in this area."* Advisory, injected, no new
command. The cheap version of a task tracker, for the case where the whole
point is that nobody set one up.

## 2.5 The lease is advisory in spirit, blocking in one case

Same-branch, same-area, held path → **deny**, as today. Everything else —
read-snapshot staleness, cross-branch overlap, hub queueing, duplicate intents,
deletions — is **told, not enforced**. This is the Google/grite/Perforce
position: exclusive locks earn their keep only where a merge is impossible, and
for mergeable text the win is that the agent *knows* and re-plans. knoot's
deny exists because an agent, unlike a person, will not notice on its own that
the file changed — not because the lock is the product.

---

# Part 3 — Rooms, members, keys

## 3.1 Rooms are access groups

```sql
-- relay SQLite. The relay enforces membership on the hot path, so it owns it;
-- Supabase owns people and the team and nothing below this line.
CREATE TABLE rooms (
  id         TEXT PRIMARY KEY,
  team_id    TEXT NOT NULL,
  name       TEXT NOT NULL,
  policy     TEXT NOT NULL,            -- memory policy json, §4.4
  created_ts INTEGER NOT NULL
);
CREATE TABLE room_areas   (room_id TEXT, repo TEXT, area TEXT, PRIMARY KEY (room_id, repo, area));
CREATE TABLE room_members (room_id TEXT, member_id TEXT, role TEXT, PRIMARY KEY (room_id, member_id));
```

A member's key grants the **union of their rooms' areas**. Two rooms may contain
the same area; they share its log by construction. A room is what an admin
creates for "the platform team" or "the payments migration": the people, the
areas they work in, the memory policy. Creating a team creates a room
**general** holding `(every repo, /)` and everyone, so nothing is ever empty.

The first draft put rooms in Supabase with RLS *and* mirrored them in the
relay. That was two authorisation models that could drift. The relay already
verifies Supabase JWTs and the console already talks to `/api/*`; rooms live
where they are enforced, and a self-hosted relay with no Supabase gets them for
free — which the first draft failed to give it.

## 3.2 Members and devices

```sql
CREATE TABLE members (id TEXT PRIMARY KEY, team_id TEXT NOT NULL, email TEXT NOT NULL, user_id TEXT);
CREATE TABLE devices (
  id          TEXT PRIMARY KEY,
  member_id   TEXT NOT NULL,
  label       TEXT NOT NULL,
  token_hash  TEXT NOT NULL UNIQUE,    -- as tokens.token_hash today
  key_package BLOB,                    -- MLS KeyPackage, hosted tier only (§5)
  created_ts  INTEGER NOT NULL, last_seen_ts INTEGER, revoked_ts INTEGER
);
```

`tokens` becomes `devices`: one row per machine per person. The bearer secret
keeps its shape (`knt_…`, SHA-256 at rest, `credentials.toml` mode 0600 keyed by
relay origin), so `token_for()` and `resolve()` barely move. What changes is
that a row now names a **member**, and therefore:

```rust
pub struct Identity {
    pub team_id: String, pub team_name: String,
    /// Verified: this key was minted for this person. Authorship on every
    /// event, and provenance on every memory shard, comes from here and not
    /// from what the client says about itself.
    pub member: Member,
    pub device_id: String,
    pub areas: Vec<(String, String)>,   // (repo, area) this identity may enter
}
```

`user` on events stays as a display string for the legacy identities (`root`,
`local`) and becomes `member.email` everywhere else. **There is no shared room
key.** The first draft offered one as a convenience and then spent a paragraph
explaining what it cost; the cost — advisory authorship — is exactly the thing
memory cannot afford. CI gets a member of its own (`ci@team`) with its own
devices, which is also how you revoke CI without revoking a person.

## 3.3 Joining

- Console → **Members** → invite by email. Supabase holds the pending invite
  (`invites`, as in the first draft) and `accept_invite(token)` joins the
  existing team in one transaction — `create_team()` today is the only way in
  and it refuses a second team per user, so this function is not optional.
- Console → **Rooms** → add member. Relay API, Supabase JWT.
- `knoot join <device-key>` on a machine → writes the credential, prints team,
  member, rooms, areas. In the hosted tier it also generates the device's MLS
  key package and uploads it (§5).

## 3.4 Migration

- A `tokens` row with no member → migrated to a device of a synthetic member
  named after its label, in **general**. Old keys keep working; the console
  shows them as "unassigned" and lets an admin attach them to a person.
- `root` and `local` identities → their own `general` room with `/`. The
  fail-open, works-offline, works-unconfigured properties in REPORT.md are
  load-bearing and this change must not touch them.
- Event keys stay `team/repo` on disk; the reader treats the absent area as
  `/`. The log is append-only and a migration that rewrites it can corrupt it.

---

# Part 4 — Shared memory

## 4.1 The spec is a failure taxonomy

MemClaw (Governed Shared Memory, June 2026) ran multi-tenant shared memory for
agent fleets in production and published four failure modes and the four
primitives that answer them. That is the spec; anything here that does not map
to one of these rows is decoration.

| Failure mode | Primitive | In knoot |
|---|---|---|
| Unauthorized leakage | **Scoped retrieval** | Shards scoped to `(repo, area)`; the scope check runs on *every* access path — search, list, **and fetch-by-id**. MemClaw's own production leak was a GET-by-id that skipped the check. |
| Stale propagation | **Temporal supersession** | Facts carry the paths they are about and each file's hash at authoring; `FileWritten` on those paths marks the fact *possibly stale*, naming who changed what and when. Nobody else has this signal. |
| Contradiction persistence | **Supersession chains** | Facts are append-only with `supersedes`. Injection shows the latest per name with author and age. **Never dedupe before supersession**: MemClaw's other production bug was a near-duplicate filter rejecting a contradicting write before contradiction detection saw it. |
| Provenance collapse | **Provenance** | `author = Identity.member`, verified (§3.2), plus device and session. Immutable on the shard. |
| — | **Policy-governed propagation** | The room's memory policy (§4.4) decides what kinds propagate, how long, and to which areas. |

Plus one from Collaborative Memory (2025): **two tiers**. *Private* memory
never leaves the machine — the daemon's own notes, the agent's scratch.
*Shared* memory is what was deliberately published into an area. Nothing moves
from the first tier to the second without an explicit act.

## 4.2 The shape: sharded, sealed, indexed

**Sharded** by `(repo, area, kind, author, name)`. One member's shards drop
when they leave without touching anyone else's; each kind has its own switch
and retention; a fetch pulls what an agent needs, not the room's history.

**Sealed** on the authoring machine by the key provider (§5). The relay stores
ciphertext plus what it needs to route and collect:

```sql
CREATE TABLE memory_shards (
  id          TEXT PRIMARY KEY,           -- random
  scope       TEXT NOT NULL,              -- team/repo/area
  kind        TEXT NOT NULL,              -- facts | repo_cache | session_context
  author      TEXT NOT NULL,              -- member id, verified
  device      TEXT NOT NULL,
  name_blind  TEXT NOT NULL,              -- HMAC(epoch_secret, name): uniqueness only
  supersedes  TEXT,                       -- shard id
  epoch       INTEGER NOT NULL,           -- key epoch the ciphertext is under
  nonce       BLOB NOT NULL, ciphertext BLOB NOT NULL, bytes INTEGER NOT NULL,
  seq INTEGER NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER,
  UNIQUE (scope, kind, author, name_blind, supersedes)
);
```

AEAD with AAD = `id ‖ scope ‖ kind ‖ author ‖ epoch`, so a relay that swaps
two shards' metadata produces a decryption failure, not a silent lie. The relay
is the one component the room cannot audit; the client must be able to catch it
misbehaving.

**Retrieval is local.** A daemon mirrors the ciphertext of every kind for the
areas its member is in — facts and repo_cache for one area are kilobytes, the
same order as the claim mirror — decrypts into its own cache, and does
relevance and staleness on plaintext. No blinded tag index; `name_blind` exists
for the uniqueness constraint and nothing else. The relay learns shard counts,
sizes, kinds, authors, epochs, and which shards share a name. It is told so, in
the docs, in those words.

## 4.3 Three kinds

| Kind | What | Written by | Default | Retention |
|---|---|---|---|---|
| `facts` | Durable statements: interface decisions, conventions, gotchas; each names the paths it is about | agent or person, on purpose: `knoot remember` / a tool call / `--from <path>` | on | 90 d, superseded chains kept |
| `repo_cache` | Derived knowledge: where a symbol lives, how tests run, what a module does | daemon, on request, structured | on | 14 d, invalidated by `FileWritten` |
| `session_context` | `{plan, paths_touched, decisions[]}` — structured, for peers in the area *now* | agent, via tool; **or** composed by the daemon from the intent and claims that session already declared, marked `derived`. Never from the transcript. | on | session; deleted when the session ends |

`project_files` is gone. Its honest use is one `knoot remember --from
CLAUDE.md`, through the same refusal rules as everything else.

`session_context` is deliberately the narrowest thing that still carries the
value: what this agent is doing at a depth the intent sentence cannot, so a peer
in the same area does not duplicate it or design against it. It is memory in
the sense that a room is a memory: it exists while people are in it.

## 4.4 Policy

Per room, in `rooms.policy`, edited in the console:

```json
{ "facts":           {"enabled": true, "retain_days": 90},
  "repo_cache":      {"enabled": true, "retain_days": 14},
  "session_context": {"enabled": true},
  "budget_bytes":    8388608,
  "propagate_to":    ["same_area"] }
```

`propagate_to` is MemClaw's policy-governed propagation: `same_area` (default),
or a list of areas a fact may also surface in — the platform team's facts about
`src/http/` are worth reading from `src/payments/`. Budget is per room, 8 MB
default, enforced on write with the oldest superseded shards evicted first.

## 4.5 How it reaches an agent

Through the surfaces `hook.rs` already owns, never through a command an agent
must think of (GAPS.md #1: cheap models do not run `knoot who` when told to).

- `UserPromptSubmit` — a **memory** section after mail and peer writes: facts
  naming paths this session has read, claimed or touched, newest-supersession
  first, stale ones flagged with who changed what; then `session_context` of
  peers in the area. Hard cap ~1.5 KB, on the same argument the existing
  injection is kept short.
- `PreToolUse` — a fact naming the exact path rides on the claim or the
  denial. The brief is the highest-attention surface in the product.
- `Stop` — if the agent wrote `session_context` this turn, the daemon publishes
  it. It does not compose one for the agent.
- `knoot remember`, `knoot recall`, `knoot memory ls` exist for people and for
  capable models, as `knoot who` does. Nothing depends on them.

## 4.6 Refusals

Publishing is refused — not warned about — and the refusal is an event on the
log, when the content or its source path is `.gitignore`d or matches `.env*`,
`*.pem`, `*.key`, `id_*`, `credentials*`, `*.tfvars`; matches the token
patterns knoot already knows (`knt_`, `sb_secret_`, AWS/GitHub/Slack prefixes,
PEM headers, long high-entropy runs); exceeds 64 KB; would exceed the room
budget; or is of a kind the room disabled. The attempt is information an admin
wants.

---

# Part 5 — The key provider

Sealing is a property of the *deployment*, not of the protocol. The daemon
seals shards through one interface; the relay code is identical whichever
provider is behind it.

```rust
pub trait KeyProvider {
    /// The current epoch and its secret for an area's memory.
    fn epoch(&self, scope: &Scope) -> (u64, Secret);
    /// A past epoch's secret, if this device still holds it (for re-reading
    /// older shards until they are rewrapped or expire).
    fn epoch_secret(&self, scope: &Scope, epoch: u64) -> Option<Secret>;
}
```

- **`Plaintext`** — `Secret` is a fixed zero key; shards are stored readable.
  For a relay in the customer's own VPC, where the org is the trust boundary and
  the question "can the vendor read it" has already been answered by where the
  box is. This is the enterprise answer, and it is simpler than any cryptography.
- **`Mls`** — the hosted tier. Each **room** is an MLS group (RFC 9420, via
  OpenMLS); each device is a leaf; the epoch secret is `MLS-Exporter("knoot
  memory", scope)`. Adding a member is an Add proposal by any current member's
  daemon; removing one is a Remove, after which the group is in a new epoch and
  the departed device cannot derive it. Cost is O(log n) per change, groups
  scale to thousands, and forward secrecy and post-compromise security come from
  the protocol rather than from a rotation ceremony we would have to design and
  then get wrong. The relay is the MLS Delivery Service: it stores key packages
  and forwards handshake messages, and it cannot read either.
- **`Kms`** — later, if an enterprise wants sealed *and* self-hosted: epoch
  secrets from their KMS. Same interface; a week of work when it is paid for.

Old shards after a Remove: the member who removed re-encrypts live shards from
their local plaintext cache into the new epoch (they can decrypt everything,
not just their own). Shards nobody rewraps expire on their retention. Nothing
is orphaned unreadable.

The first draft carried the seal inside the credential string and then had
nowhere for a *second* member to get it from without the relay or the browser
holding it — which would have made the central claim false. MLS is the standard
answer to exactly that problem, and there is a Rust implementation.

---

# Part 6 — Relay seams for scale

Not built now; decided now, because they are cheap today and expensive later.

1. **Log key is `team/repo/area`** with area defaulting to `/`. When a repo
   with two thousand sessions needs its log split across writers, it splits on
   a key that already exists.
2. **Websocket subscriptions are by `(repo, area-prefix)`** from day one, even
   though every client subscribes to `/` today. Fan-out becomes proportional to
   who can collide with you, not to headcount.
3. **`View` is per area** in the daemon, so the claim mirror and the memory
   cache for a large repo are the size of the areas this member is in.

---

# Part 7 — Threat model, honestly

**Relay operator sees:** teams, members, devices, rooms, areas, every event on
the log (paths, intents, who, when — as today), and for memory: shard counts,
kinds, sizes, authors, epochs, name equality. Under `Mls`: no shard plaintext,
no epoch secret. Under `Plaintext`: everything, by design, on a box you run.

**A room member sees:** every shard in the areas they are in, decrypted. There
is no per-member read scoping inside an area; a person who should not read
something belongs in a different area.

**Not claimed:** zero-knowledge (the log itself is metadata-rich and always has
been — paths and intents are the product); protection against a member's own
compromised machine; protection against a relay that *withholds* a shard, which
it can do without reading it, and withholding a fact is a way to influence an
agent. Under `Mls` we get forward secrecy and post-compromise security; under
`Plaintext` we get neither and say so.

---

# Part 8 — Failing open

Nothing here may ever be the reason an agent cannot write. Relay down, provider
missing, epoch unknown, decryption failure, policy off: every path ends in *no
memory injected, no awareness beyond what the local mirror has*, and the deny
path is unchanged from today. `knoot status` tells a working install from a
quiet one:

```
[ok  ] room      platform   (12 members, areas: src/http, src/auth)
[ok  ] area      src/auth   (this repo, 3 live sessions)
[ok  ] memory    facts 41, repo cache 12, epoch 7 (mls)
[warn] memory    2 shards unreadable: written under epoch 6, awaiting rewrap
```

---

## Phases, each with an exit criterion

1. **Members, devices, rooms as access groups; area = `/`.** ✅ **Built.**
   Supabase `invites` + `accept_invite` + `revoke_invite` + `remove_member`
   (`supabase/migrations/0002_invites.sql`); relay
   `members`/`devices`/`rooms`/`room_areas`/`room_members` in `src/rooms.rs`;
   `Identity` carries a verified member and the areas their rooms grant;
   `knoot join <key>`, which asks the relay who a key is for rather than
   believing it; console Members, Agent keys and Rooms. *Exit:* a second person
   can join a team and be revoked without touching anyone else's key —
   `a_second_person_can_join_and_be_removed_without_touching_other_keys` in
   `tests/teams_api.rs`, and `removing_one_member_leaves_everyone_elses_keys_working`
   in `src/rooms.rs`.

   Three things went in beyond the letter of the phase, each because leaving it
   out would have been a lie somewhere:

   - **Authorship already comes from the key.** `Event::attribute_to` rewrites
     the author of every event at the relay, before anything reads it. It was
     cheap once `Identity` carried a member, and `KNOOT_USER` was otherwise a
     way to write an event as a colleague. Identities with no verified person
     behind them — the legacy shared secret, an unconfigured loopback relay, a
     migrated key nobody has adopted — keep the client's self-reported string,
     because inventing an author is worse than an honest guess.
   - **`rooms.policy` is written but never read.** The memory policy of §4.4 is
     stored with its defaults from the day a room is created, so phase 4 needs
     no migration. An admin editing it today changes nothing.
   - **Areas are carried, not enforced.** A key resolves to its `(repo, area)`
     grants and `Identity::may_enter` answers questions about them, but no log
     or claim path consults it yet: every caller asks about `/`, which
     `general` grants. Phase 3 is what makes the grant bite. Until then a room
     with narrow areas restricts what the console shows about a member, not
     what their agent may write.

   Not built here: SCIM/SSO group sync, and any mail. `invite_member` returns a
   secret link once and whoever invites passes it on themselves.

   **Since closed:** a self-hosted relay could not create a second *person* at
   all — `invite_member`/`accept_invite` are Supabase RPCs, so a relay with no
   cloud behind it could mint any number of keys and every one of them named
   the same human. Rooms, areas and memory provenance are all about *who*, so
   this was the gap under phases 3, 4 and 5, and their tests each reached into
   the relay's SQLite file to invent a colleague. `POST /api/members` now
   makes one; `knoot member add|ls|rm` is the terminal surface; the console's
   Members tab falls back to it when there is no Supabase, instead of being a
   panel that cannot work. The tests go through the API — `Admin` in
   `tests/common/mod.rs` — so nothing sets up state the product cannot reach.
2. **Awareness upgrades.** ✅ **Built.** Read snapshots
   (`RepoConn::reads`, fed by `Read` through `PostToolUse`), creation claims,
   delete broadcast (`PathRemoved`, from `bashparse`'s new `removals`), hub
   policy (`View::is_hub`/`lease_for`, `hubs` in `.knoot.toml`),
   duplicate-intent notice (`intents_overlap`) — all inside the existing hook
   surfaces, and all advisory. Four new events carry the signals to the log:
   `StaleRead`, `CreateCollision`, `PathRemoved`, `DuplicateIntent`.
   *Exit:* `tests/awareness.rs` drives the real binary with hook payloads and
   asserts on what an agent would be told, one property per test — the add/add
   case (`two_sessions_creating_one_new_file_are_told_about_each_other`) and
   the stale-dependency case
   (`a_stale_read_is_reported_before_the_write_and_does_not_deny_it`) both
   pass, and both were silent before. `lab/haiku-run.sh report` now counts the
   four events and greps the transcripts for the five phrases they produce.

   Four decisions worth writing down, three of them departures:

   - **A read snapshot keeps a timestamp, not a hash.** §2.1 said `(ts,
     hash)`. The comparison is against the log's `last_write` timestamp, so
     the hash would only earn its keep by re-reading the file at `PreToolUse`
     to prove the content really changed — I/O on the hot path to suppress the
     rare case where a peer wrote a file back identically.
   - **The snapshot is not cleared at the turn boundary.** §2.1 called it "the
     current turn"; it is kept for 30 minutes, capped at 256 paths. A read
     from the *previous* turn is exactly what a peer's write between turns
     invalidates, so clearing it per turn would throw away the only case that
     matters.
   - **Reporting acknowledges.** After a stale read is reported, the recorded
     read is advanced to the peer's write, so the same news is not repeated on
     every later write in the turn. A *newer* write says something new and is
     reported again.
   - **An allowed write's advisory rides `additionalContext`, never
     `permissionDecision: allow`.** Deciding the permission would auto-approve
     the edit and override whatever the human configured about confirming
     writes. So an advisory is context and the tool call takes its normal
     course — which does mean that on a client that ignores
     `additionalContext` for `PreToolUse`, an *allowed* write's note is not
     shown. On a denial the note rides the reason string, which every client
     renders.

   Also true, and worth knowing before phase 3: a `rm` no longer logs a
   `FileWritten` for a path that is in fact gone. "priya wrote legacy.ts" for
   a file that does not exist was the log's own version of a stale read.
3. **Areas.** ✅ **Built.** `[[areas]]` in `.knoot.toml` (`name` + `paths`),
   `knoot areas` and `knoot areas --import-codeowners --write`, most-specific
   prefix resolution (`config::area_of`), and delivery scoped to the areas a
   key's rooms grant: the relay decides an event's area from its path and a
   connection is sent only what `Identity::may_enter` allows — the Welcome
   snapshot's claims included. *Exit:* `tests/areas.rs` puts two areas in one
   repo and a person in exactly one of them, over a real socket with a real
   device key —
   `one_areas_events_never_reach_a_session_outside_it` and
   `a_write_that_crosses_into_an_area_is_logged_to_both` both pass, and both
   were vacuous before: phase 1 carried the grants and consulted them nowhere.

   Four decisions, three of them departures:

   - **The relay learns the map from `Hello`, not from the repo.** The relay
     never sees a working copy, so the only place `.knoot.toml` can reach it
     is a client. Every client reads the same committed file, so they agree;
     the last Hello wins, which makes a re-division take effect once one
     session has reconnected rather than when the last one has. A client that
     declares *nothing* leaves the stored map alone — otherwise a colleague on
     an older binary would silently undo the division for everyone.
   - **Pathless events cross every area.** Presence, intent, messages and
     `DuplicateIntent` name no subtree. §1 said areas bound attention; a peer
     you cannot see is worse than a peer working somewhere you do not care
     about, and duplicate *intent* is the case where the two agents are by
     definition not yet in the same files.
   - **Crossing is not a second delivery, it is the same one.** §1 said a
     cross-area write is "logged to both". An event's area is its path's area,
     so a write into `auth` by a session working in `/` reaches the auth room
     because it *is* an auth event, and reaches `/` because `/` covers
     everything. No event is duplicated, and nothing had to decide which area
     a session "is in".
   - **`View` is scoped by what arrives, not by a split.** §6.3 wanted a
     per-area `View` in the daemon. The daemon mirrors what the relay sends,
     and the relay now sends only the granted areas, so a narrow member's
     mirror is already the size of their areas — `knoot who` and the per-turn
     injection narrowed for free. A literal split buys nothing until one
     daemon holds several areas' worth of a repo it was granted in full.

   Not built here, and deliberately: the log key on disk is still
   `team/repo` with the area derived on read, because §3.4 is right that a
   migration which rewrites an append-only log can corrupt it, and §6.1's
   split-by-key is a change to *writers*, which nobody needs at this size.
   Also not built: a self-hosted way to create a second member — a room with
   narrow areas needs somebody to put in it, and outside Supabase there is no
   call that makes one. `tests/areas.rs` reaches into the relay's database to
   play the console. That is the next thing to close.
4. **Facts, `Plaintext` provider.** ✅ **Built.** `src/memory.rs`: the
   `memory_shards` table, the `KeyProvider` interface with `Plaintext` behind
   it, whole-kind sync (`MemSync`/`MemShards` on the existing socket), local
   retrieval over plaintext in the daemon's `Cache`, staleness from
   `FileWritten`, supersession chains, refusals, and injection through
   `UserPromptSubmit` and the `PreToolUse` brief. `knoot remember`,
   `knoot recall`, and a `memory` line in `knoot status`. `rooms.policy`,
   written since phase 1, is now read: budget, per-kind switches and retention
   are enforced on every publish.

   *Exit:* the haiku arm is planted and scored — `lab/haiku-run.sh` writes a
   fact saying money is integer cents before the run and the report says
   whether it reached a transcript and whether `billing.js` still used floats.
   **That arm has not been run here**; it needs four live Haiku sessions, and
   an unrun experiment reported as a result is worse than an honest gap. What
   *is* verified is everything the arm depends on: `tests/memory.rs` drives the
   real binary through the real hooks against a real relay with real device
   keys and asserts that a teammate's fact reaches an agent that never asked
   for it, with its provenance, on the turn it is in that code.

   Five decisions, four of them departures:

   - **The seal binds the author, and the relay verifies instead of
     rewriting.** Everywhere else the relay overwrites authorship
     (`Event::attribute_to`). It cannot here: the AAD covers
     `id ‖ scope ‖ kind ‖ author ‖ author_email ‖ epoch`, so a relay that
     "corrected" a shard's author would produce one nobody can open. So the
     client binds and the relay *checks* — and a key with no verified person
     behind it may not publish at all, because a fact whose provenance is a
     display string is worse than no fact. Which is why `Welcome` now carries
     `me`: a client cannot learn its own member id from an opaque secret.
   - **`Plaintext` still authenticates.** §5 says the plaintext provider stores
     shards readable, and it does. It also carries an HMAC tag under a zero
     key, which costs nothing (`sha2` was already a dependency, and no crypto
     crate was added) and buys
     `a_shard_whose_metadata_was_tampered_with_fails_to_open` in the
     deployment that ships first rather than only under `Mls`. Under a public
     key that catches a relay which swaps or loses rows, not one that
     deliberately forges — said plainly in the module docs, because the
     alternative is a security claim that is not true.
   - **A fact records each named file's hash, and the hash is checked off the
     hot path.** Phase 2 rejected hashes for read snapshots because proving
     content really changed meant I/O at `PreToolUse`. A fact is different:
     hashing happens once at authoring, and the check happens at
     `UserPromptSubmit`, where a peer who reverted a file must not make every
     fact about it look stale. That is the flag that would otherwise teach
     agents to ignore the flag.
   - **Budget takes the largest room's, kinds take the strictest.** Two rooms
     sharing an area share one store. Enforcing the stricter budget would let
     one room silently evict the other's facts — a policy reaching outside its
     own room. A disabled kind is the opposite: a room that turned it off said
     something about the area, and a permissive neighbour is not grounds to
     overrule it.
   - **Retention is not in the seal.** Deliberately, so a room can shorten it
     without making every existing shard unreadable. The relay stamps
     `expires_ts` from the room's policy on accept.

   Not built here: `Mls` (phase 5), `repo_cache` and `session_context`
   (phase 6) — the `Kind` enum names them so the store needs no migration —
   `propagate_to` beyond `same_area`, and console UI for memory. Also still
   open from phase 3: no self-hosted way to create a second member, so
   `tests/memory.rs` reaches into the relay's database to make one.
5. **`Mls` provider.** ✅ **Built.** `src/mls.rs`: a room is an OpenMLS group
   (RFC 9420, ciphersuite `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`), a
   device is a leaf, and the memory secret is
   `MLS-Exporter("knoot memory", scope)`. The relay is the Delivery Service —
   `mls_key_packages` and `mls_log`, membership enforced, content opaque. The
   daemon reconciles: genesis, Add, Remove, and rewrap. `KNOOT_KEY_PROVIDER`
   on the relay chooses; `knoot status` says which deployment you are in.

   *Exit:* `a_relay_dump_under_mls_yields_no_plaintext_no_secret_and_no_working_credential`
   passes — it publishes a fact through the hosted configuration and then
   greps the relay's whole database file, WAL included, for the sentence, for
   the fact's *name*, and for anything key-shaped. Verified live too: the
   relay holds one 121-byte ciphertext, an HMAC of the name, and a
   zero-length genesis commit. `a_removed_device_cannot_derive_the_next_epoch`
   passes, and so does the test the others only imply —
   `a_second_laptop_is_added_to_the_group_and_can_read_the_rooms_facts`, which
   is the whole chain: priya's laptop uploads a key package, ash's daemon adds
   it, priya joins from the welcome, derives the key from the protocol, and
   opens a fact she never had a key for.

   Six decisions, four of them departures:

   - **MLS agrees the key; the AEAD is ours.** §5 says the epoch secret is the
     exporter output, and that is exactly how far MLS reaches: the shard is
     then sealed with ChaCha20-Poly1305 under that secret, with the routing
     metadata as associated data. So the relay stores ordinary sealed bytes
     and knows nothing about MLS beyond forwarding blobs — and `Plaintext` and
     `Mls` differ in one method, `confidential()`.
   - **The Delivery Service's whole ordering job is one unique index.**
     `(room_id, epoch) WHERE kind = 'commit'`. Two daemons proposing from one
     epoch race, exactly one lands, and the loser discards its pending commit
     and re-syncs. No leader, no lock, no election — the same shape as the
     claim arbitration one layer up.
   - **Group creation needed a genesis message that MLS does not have.**
     Building a group is local and silent, so two machines would each build
     one for the same room and each think it was the room's. So the creator
     appends an *empty commit at epoch zero*: it carries nothing, and exists
     only so the index above can decide who started the room. The loser
     forgets the group it built and waits to be welcomed into the real one.
   - **The device is per machine, not per repo.** Found by a test that would
     not stop failing: several repos on one daemon each opened their own
     `Device` over one on-disk state and one room, each proposed genesis, and
     the losers deleted the winner's group out from under it. A laptop is one
     leaf however many repos it has checked out, so the state moved onto the
     `Daemon`.
   - **Publishing with no key is refused, not silently unreadable.** Sealing
     under a zero secret at epoch zero stored a fact that not even its author
     could read, and printed "remembered". Now `knoot remember` says the
     room's key has not arrived yet, and `knoot status` carries a `waiting for
     this room's key` line — a different kind of quiet from nothing to say.
   - **Rewrap is its own message, not a republish.** `MemRewrap` replaces a
     shard's sealed bytes and nothing else: id, scope, kind, author and
     author's email stay exactly as they were, and they are still what the new
     seal is bound to. A republish would have made whoever removed a member
     the author of everybody else's facts.

   Not verified end to end: **rewrap after a removal**. The relay path, the
   scope check and the "cannot move backwards, cannot launder authorship"
   properties have tests; the daemon calling it after a Remove commit does
   not, because that needs two live daemons and one process can hold one
   device. The code is there and reviewed, and that is not the same as tested
   — a room that removes a member today may end up with facts readable only
   from the epoch they were sealed in, until their retention expires. Also not
   built: `Kms` (nobody has asked with a budget), and MLS for a console
   session, which has no device and therefore no leaf.
6. **`repo_cache`, `session_context`.** ✅ **Built.** Both kinds ride the phase-4
   shard machinery — same scoping, same provider, same refusals — and differ
   in retention, in what invalidates them, and in how they are shown.
   `knoot plan --path <f> --decided <d> "<plan>"` publishes a session's
   context; `knoot cache --name <n> --path <f> "<answer>"` publishes derived
   knowledge; `knoot recall` labels every entry with its kind. Three sections
   in the brief now, in the order that matters: what peers are doing, what the
   team knows, what has already been worked out.

   *Exit:* `a_peers_declared_plan_reaches_the_next_turn_of_a_session_in_the_area`
   passes, and it is visible by hand — priya's plan and her settled decision
   appear at the top of ash's very next `UserPromptSubmit`, with no command
   run for them. `session_context_does_not_outlive_the_session` passes too.

   Five decisions, three of them departures:

   - **A plan publishes immediately, not at `Stop`.** §4.5 said the daemon
     publishes `session_context` at the `Stop` boundary. But the value of a
     plan is highest before the work happens, and holding it until the turn
     ends means a peer starting a turn in that minute is told nothing. So
     `knoot plan` publishes when it is run.

   - **The daemon composes one after all — from declarations, never from the
     turn.** This draft said it never would, and the lab run of 4 September
     said the draft was wrong: `plans published 0`, because no Haiku agent
     ran the command. A feature the weakest model in the room cannot reach is
     not a feature. So on every `UserPromptSubmit` the daemon publishes what
     the session appears to be doing, composed from its declared intent and
     the paths it holds.

     The rule §4.5 was protecting is untouched, and it is worth being exact
     about why. What that rule forbids is a *free-text conclusion pulled out
     of a transcript* — unreviewed text, that no one chose to publish,
     leaving the machine. The composer reads neither the transcript nor the
     turn: its two inputs were declared by the agent and broadcast to every
     peer before it ran, so it discloses nothing that was not already shared,
     and it summarises nothing — the text is the intent, verbatim. If that
     ever stops being true, this becomes the exfiltration path the design
     refused.

     Three guards: a session that ran `knoot plan` is left alone, because
     both supersede by session id and a scrape must never replace a plan; an
     unchanged intent and path set republishes nothing; and the shard carries
     `derived`, so a peer is told *"appears to be working on (from their
     intent and claims, not a declared plan)"*. A guess in a plan's voice
     would be worse than no plan.
   - **A cache entry is dropped when its files move, not flagged.** §4.1 gives
     every kind temporal supersession, and for a fact the flag is right — a
     human wrote it on purpose and "priya changed this since" is what its
     reader needs. Derived knowledge past its files is simply wrong, and it
     was cheap to work out, so showing it with a warning spends attention on
     something that should just be regenerated.
   - **Supersession is keyed by kind as well as name.** A `repo_cache` entry
     called `retry` and a fact called `retry` are two different things, and
     one silently replacing the other would delete a statement somebody wrote
     on purpose. `a_cache_entry_and_a_fact_of_the_same_name_are_two_things`.
   - **A session's context is keyed by its session id**, so a session that
     replans supersedes itself. Two plans standing from one session is a peer
     being asked which one is current.
   - **Deletion is broadcast.** §4.3 says a session's context is deleted when
     the session ends, which turned out to need a `MemForgotten` fan-out: a
     delete removes a row, and a sync keyed on a high-water mark can never
     mention a row that is not there — so a *peer's* daemon would have gone on
     showing a finished session's plan as a live one.

   Two real bugs fell out of building this, both older than the phase:

   - **A repo under a symlink got two of everything.** A hook payload carries
     the cwd Claude Code was started in; the CLI resolves its own. On macOS,
     where `/tmp` and `/var` are symlinks, the same repo arrived as
     `/var/folders/…` and `/private/var/folders/…` and got two connections,
     two claim mirrors and two memory caches. They agreed on anything the
     relay pushed and diverged the moment one dropped something locally.
     `ensure_repo` now keys on the resolved path, and `rel_path` resolves the
     incoming path symmetrically — including for a file that does not exist
     yet, which is the creation case.
   - **A room's wake was broadcast per repo, but a room spans repos.** A
     commit made while working in one repo never woke the daemons that were in
     the same room but a different one, so their group never formed and their
     memory stayed keyless. The fan-out moved onto the `App`. Alongside it, two
     smaller phase-5 faults: `setup_mls` had a check-then-set race that let two
     repos each open a `Device` over one on-disk state, and a daemon that
     declined to propose genesis because another repo had already claimed it
     never came back to look.

   Not built: `propagate_to` beyond `same_area`, and console UI for memory.

---

## What is still open

**Rewrap is now tested end to end** —
`a_removal_rotates_the_rooms_key_and_rewraps_what_it_holds`, on a fixture of
its own with two subprocess daemons, because a device is a machine and one
process holds one. Writing that test found eight faults, all of them real and
none of them visible from inside the phase that shipped them:

1. Removing somebody from a room **never rotated the key** — nothing woke the
   room on a membership change, so the departure sat there until an unrelated
   commit happened to move the group.
2. `memory::forget_author` was written in phase 4 and **called by nothing**, so
   `knoot member rm`'s own promise that "their memory goes" was false.
3. Rewrap ran only on a Remove. MLS gives forward secrecy in both directions,
   so a device that **joins** at epoch *n* cannot derive *n-1* either — a new
   member could see every shard in the room and open none of them, which looks
   like an empty room rather than a broken one.
4. Rewrap only covered the cache of the repo whose connection made the commit.
   MLS state is per machine and memory caches are per repo, so the shards it
   did not visit were exactly the ones nobody else would visit either.
5. `epoch_secret` only returned secrets already derived, and only *publishing*
   derived one — so a read-only member could open nothing, ever. An earlier
   test was papering over this with an explicit derive call I had written
   without realising what it was hiding.
6. A rewrap changed the row and told nobody. A sync is keyed on a high-water
   mark and a rewrap does not move it, so the new ciphertext reached no one.
7. Unreadable shards were discarded rather than retried, so a shard that
   arrived one handshake message before the key was a permanent hole.
8. `mls_roster` gave up for the whole round when the first missing device had
   no key package, so one colleague who had never run `knoot join` blocked
   every other machine out of the group indefinitely.

Left: `propagate_to` beyond `same_area`; console UI for memory; SCIM/SSO group
sync; the `Kms` provider; cross-repo facts. Each waits for someone to ask with
a budget.

Not in any phase: SCIM/SSO group sync, `Kms` provider, cross-repo facts. Each
waits for someone to ask with a budget.

## Tests that must exist before this is believable

One property per test, named for the thing that would be wrong, in the style
of `teams.rs`.

- `one_areas_events_never_reach_a_session_outside_it`, and the same for shards
- `a_write_that_crosses_into_an_area_is_logged_to_both`
- `a_stale_read_is_reported_before_the_write_and_does_not_deny_it`
- `two_sessions_creating_one_new_file_are_told_about_each_other`
- `a_deletion_reaches_every_session_that_read_the_path`
- `a_hub_lease_is_short_and_the_queue_is_reported`
- `a_legacy_token_still_authenticates_and_lands_in_general`
- `authorship_on_events_and_shards_comes_from_the_key_not_the_client`
- `fetch_by_id_enforces_scope` (MemClaw's leak)
- `a_contradicting_fact_is_stored_as_a_supersession_not_rejected_as_a_duplicate` (MemClaw's other bug)
- `a_fact_about_a_path_is_marked_stale_when_that_path_is_written`
- `a_shard_whose_metadata_was_tampered_with_fails_to_decrypt`
- `a_relay_dump_under_mls_yields_no_plaintext_no_secret_and_no_working_credential`
- `a_removed_device_cannot_derive_the_next_epoch`
- `a_dotenv_is_refused_and_the_refusal_is_logged`
- `session_context_does_not_outlive_the_session`
- `a_session_that_never_ran_plan_still_tells_its_peers_what_it_is_doing`
- `a_composed_context_is_the_declared_intent_and_nothing_else`
- `a_declared_plan_is_never_replaced_by_a_composed_one`
- `a_missing_provider_injects_no_memory_and_denies_no_write`

## Sources

- STORM — *Multi-agent Collaboration with State Management*, May 2026. Shared
  workspace 46.2 vs worktrees 24.6 vs single 20.7 (Commit0-Lite, weighted);
  read-snapshot OCC; admits terminal bypass, shell commands, hub serialisation.
  https://arxiv.org/html/2605.20563v1
- *AI Agent Pull Requests on GitHub*, July 2026. 33,596 PRs; cross-agent
  conflicts 41.7% vs intra 19.8%; modify/delete 26.8%, add/add 15.1%.
  https://arxiv.org/html/2607.04697v2
- grite — *Before the Pull Request*, June 2026. Git-native, advisory leases,
  CRDT log; duplicate work 78% → 0%. https://arxiv.org/abs/2606.19616v1
- Co-Coder — *When Parallelism Pays Off*, May 2026. Structural hub isolation;
  cohesion clustering; Agent Teams baseline fastest and least correct.
  https://arxiv.org/html/2606.00953v1
- MemClaw — *Governed Shared Memory for Multi-Agent LLM Systems*, June 2026.
  Four failure modes, four primitives, two production bugs.
  https://arxiv.org/abs/2606.24535
- *Collaborative Memory: Multi-User Memory Sharing with Dynamic Access
  Control*, May 2025. Private/shared tiers, immutable provenance.
  https://arxiv.org/abs/2505.18279
- *Software Engineering at Google*, ch. 16. OWNERS per subtree; locking
  dismissed as unscalable. https://abseil.io/resources/swe-book/html/ch16.html
- Gutwin & Greenberg, *A Descriptive Framework of Workspace Awareness for
  Real-Time Groupware*, CSCW 2002. https://dl.acm.org/doi/10.1023/A:1021271517844
- RFC 9420 (MLS protocol) and RFC 9750 (MLS architecture).
  https://www.rfc-editor.org/info/rfc9420/ · https://www.ietf.org/rfc/rfc9750.html
- Burrows, *The Chubby Lock Service*, OSDI 2006 — coarse leases, advisory
  use in practice. https://research.google.com/archive/chubby.html
