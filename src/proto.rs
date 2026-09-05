use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type Ts = u64; // unix millis

pub fn now_ms() -> Ts {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub const LEASE_MS: u64 = 10 * 60 * 1000; // 10 min, renewed on activity
/// A session with no activity for this long is treated as gone. This must be
/// far longer than a human pause: a session idle at its prompt is alive, and
/// pruning it destroys identity for the rest of the run. Claims are made safe
/// by leases, not by this.
pub const SESSION_STALE_MS: u64 = 12 * 60 * 60 * 1000;

/// How far back peer writes stay worth telling an agent about. Long enough to
/// cover a slow turn, short enough that "changed under you" means recently.
pub const WRITE_WINDOW_MS: u64 = 30 * 60 * 1000;

/// A turn with no recorded predecessor looks back this far, so a session that
/// has just joined still learns what has been happening.
pub const FIRST_TURN_LOOKBACK_MS: u64 = 10 * 60 * 1000;

/// A hub file's lease. Short on purpose: a widely-shared file held for ten
/// minutes serialises every other agent's critical path, which both STORM and
/// Co-Coder name as the thing that makes parallel agents slower than one. The
/// lease is renewed by *writing*, not by being active, so a session that is
/// thinking rather than editing gives the file up.
pub const HUB_LEASE_MS: u64 = 2 * 60 * 1000;

/// How long a claim counts towards making a path a hub, and how many distinct
/// sessions in that window it takes. Three agents in one file inside half an
/// hour is not a coincidence; it is a shared dependency.
pub const HUB_WINDOW_MS: u64 = 30 * 60 * 1000;
pub const HUB_SESSIONS: usize = 3;

/// Everything that happens is an event on the per-repo sequenced log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { session: String, user: String, branch: String, ts: Ts },
    IntentDeclared {
        session: String,
        text: String,
        ts: Ts,
        /// Re-sent every turn: a session that checked out a new branch would
        /// otherwise keep claiming under the branch it started on.
        #[serde(default)]
        branch: String,
    },
    ClaimAcquired {
        session: String,
        user: String,
        path: String,
        lease_until: Ts,
        intent: String,
        #[serde(default)]
        branch: String,
        /// The only event that used to carry no timestamp, which left a blank
        /// column in the dashboard exactly on the line people look for.
        /// Defaulted, so rows written by an older relay still deserialise.
        #[serde(default)]
        ts: Ts,
    },
    ClaimReleased { session: String, path: String, ts: Ts },
    /// A write that landed on someone else's claim without being stopped —
    /// detected after the fact by diffing the working tree. Distinct from
    /// ClaimDenied: nothing was prevented, only observed.
    UngatedWrite {
        session: String,
        user: String,
        path: String,
        holder: String,
        holder_user: String,
        ts: Ts,
    },
    /// Two branches editing one file. Nothing is blocked — they are not in
    /// each other's way yet — but this is a merge conflict being born, and
    /// saying so now costs one re-plan instead of an afternoon later.
    CrossBranchOverlap {
        session: String,
        user: String,
        branch: String,
        path: String,
        peer_user: String,
        peer_branch: String,
        ts: Ts,
    },
    /// A path a peer was waiting on has been freed.
    PathFreed { path: String, by_session: String, by_user: String, intent: String, ts: Ts },
    /// A message from one session to another (or to everyone). Sessions are
    /// otherwise mute: without this, a blocked peer never learns it can go.
    Message { from_session: String, from_user: String, to: Option<String>, text: String, ts: Ts },
    /// A blocked edit attempt. Carries no state change, but it is the signal
    /// that matters: it makes collisions visible and measurable.
    ClaimDenied {
        session: String,
        user: String,
        path: String,
        holder: String,
        holder_user: String,
        ts: Ts,
    },
    /// A write we recorded. Carries the user as well as the session: joining
    /// back through `SessionStarted` to name an author is the fragile path
    /// that once blamed the wrong session for a peer's concurrent write.
    /// `default` so events logged before the field existed still replay.
    FileWritten {
        session: String,
        #[serde(default)]
        user: String,
        path: String,
        ts: Ts,
    },
    /// A path that was deleted or moved away. Deletion is the other half of a
    /// conflict claims cannot see: 26.8% of real agent conflicts are
    /// modify/delete, and a claim on an existing path says nothing about a
    /// path that has stopped existing. Delivered to every session that read
    /// or holds it, at their next hook boundary.
    PathRemoved {
        session: String,
        user: String,
        path: String,
        /// True when the path moved rather than vanished — `mv`, `git mv`.
        #[serde(default)]
        moved: bool,
        ts: Ts,
    },
    /// A session was about to write a file it had read, after somebody else
    /// changed that file. The write is not blocked: the target is not held,
    /// and a deny here would be a false-positive machine. What is wrong is the
    /// agent's *reasoning*, which was based on content that has since moved.
    ///
    /// Recorded as well as reported, because "how often was an agent working
    /// from stale reads" is the number this half of the design exists to move,
    /// and a note in a transcript cannot be counted.
    StaleRead {
        session: String,
        user: String,
        path: String,
        peer_user: String,
        /// When this session read the path, and when the peer wrote it.
        read_ts: Ts,
        write_ts: Ts,
        ts: Ts,
    },
    /// Two sessions independently creating the same new file — 15.1% of real
    /// agent conflicts, and invisible to a claim on an existing path.
    CreateCollision {
        session: String,
        user: String,
        path: String,
        peer_user: String,
        ts: Ts,
    },
    /// Two sessions declaring near-identical intent. Duplicate *tasks* were
    /// 78% of the waste grite measured, and file claims cannot see a task.
    DuplicateIntent {
        session: String,
        user: String,
        text: String,
        peer_user: String,
        peer_text: String,
        ts: Ts,
    },
    /// A publish memory would not take: a gitignored source, a path that
    /// holds credentials, a secret in the text, something too big, a room out
    /// of budget or with the kind turned off.
    ///
    /// Refused rather than warned about — a warning here is a secret in a
    /// shared store with a note attached — and *logged*, because an agent
    /// trying to publish a `.env` is exactly the information an admin wants
    /// and exactly what a silent refusal throws away. The reason is carried;
    /// the content never is.
    MemoryRefused {
        session: String,
        user: String,
        name: String,
        reason: String,
        ts: Ts,
    },
    SessionEnded { session: String, ts: Ts },
}

impl Event {
    /// The repo-relative path this event is about, or `None` when it is about
    /// a session rather than a file.
    ///
    /// This is what decides an event's area, and so who hears about it. The
    /// pathless events — presence, intent, messages — belong to no subtree and
    /// reach everyone in the repo: a peer you cannot see is worse than a peer
    /// working somewhere you do not care about.
    pub fn path(&self) -> Option<&str> {
        match self {
            Event::ClaimAcquired { path, .. }
            | Event::ClaimReleased { path, .. }
            | Event::ClaimDenied { path, .. }
            | Event::UngatedWrite { path, .. }
            | Event::CrossBranchOverlap { path, .. }
            | Event::PathFreed { path, .. }
            | Event::FileWritten { path, .. }
            | Event::PathRemoved { path, .. }
            | Event::StaleRead { path, .. }
            | Event::CreateCollision { path, .. } => Some(path),
            Event::SessionStarted { .. }
            | Event::IntentDeclared { .. }
            | Event::Message { .. }
            | Event::DuplicateIntent { .. }
            | Event::MemoryRefused { .. }
            | Event::SessionEnded { .. } => None,
        }
    }

    /// Rewrite the author of an event to the person the presented key was
    /// minted for.
    ///
    /// Authorship used to be `session_user()` — an environment variable or the
    /// OS user, chosen by the client, on a team-wide bearer token that named
    /// nobody. That is fine for a display string and not fine for anything an
    /// access decision or a memory shard's provenance rests on. The relay
    /// knows which device key arrived, so it overwrites the claim rather than
    /// trusting it.
    ///
    /// Called with `None` for the identities that genuinely have no verified
    /// person behind them — a legacy shared secret, an unconfigured loopback
    /// relay, a migrated key nobody has attached yet. Those keep the client's
    /// string, because inventing an author would be worse than an honest
    /// self-reported one.
    pub fn attribute_to(&mut self, author: Option<&str>) {
        let Some(author) = author else { return };
        match self {
            Event::SessionStarted { user, .. }
            | Event::ClaimAcquired { user, .. }
            | Event::ClaimDenied { user, .. }
            | Event::UngatedWrite { user, .. }
            | Event::CrossBranchOverlap { user, .. }
            | Event::FileWritten { user, .. }
            | Event::PathRemoved { user, .. }
            | Event::StaleRead { user, .. }
            | Event::CreateCollision { user, .. }
            | Event::DuplicateIntent { user, .. }
            | Event::MemoryRefused { user, .. } => *user = author.to_string(),
            Event::Message { from_user, .. } => *from_user = author.to_string(),
            Event::PathFreed { by_user, .. } => *by_user = author.to_string(),
            Event::IntentDeclared { .. } | Event::ClaimReleased { .. } | Event::SessionEnded { .. } => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        repo: String,
        daemon: String,
        /// How this repo divides itself into areas, from its committed
        /// `.knoot.toml`.
        ///
        /// The relay never sees the repo, so the declaration can only come
        /// from a client. Every client reads the same committed file, so they
        /// agree; the last Hello wins, which makes a change to the file take
        /// effect as soon as one session has reconnected rather than when the
        /// last one has.
        #[serde(default)]
        areas: Vec<crate::config::AreaDef>,
    },
    Append { event: Event },
    ClaimReq {
        id: String,
        session: String,
        user: String,
        path: String,
        intent: String,
        #[serde(default)]
        branch: String,
        /// The client believes this path is a hub — it is named in
        /// `.knoot.toml`. The relay decides the lease, and it also spots hubs
        /// on its own from claim history; this is the half only the client can
        /// know, since the relay never sees the repo.
        #[serde(default)]
        hub: bool,
    },
    ReleaseSession { session: String },
    /// Publish a sealed shard into an area.
    ///
    /// The shard arrives fully formed — scope, author and device included —
    /// because the seal is already bound to them and the relay cannot rewrite
    /// what it cannot open. So the relay *verifies* rather than stamps: the
    /// author must be the member the presented key was minted for, and the
    /// scope must be an area that key may enter. This is the one place where
    /// authorship is checked instead of overwritten, and the reason is that
    /// overwriting it would produce a shard nobody can open.
    MemPublish { shard: crate::memory::Shard },
    /// Everything in the areas this key holds since `since`. A daemon mirrors
    /// whole kinds; retrieval happens locally, on plaintext.
    MemSync { since: i64 },
    /// One shard by id. Exists so that the scope check on this path is a thing
    /// with a test against it: MemClaw's production leak was a fetch-by-id
    /// that skipped the check every other path performed.
    MemFetch { id: String },
    /// This device's MLS key package, so a current member of a room can add
    /// it. Public by construction; the private half never leaves the machine.
    MlsUpload { key_package: String },
    /// Ask for a device's key package, in order to add it to a room.
    MlsKeyPackage { device: String },
    /// A commit — with the welcome that goes with an Add — for the Delivery
    /// Service to order and fan out. Opaque to the relay.
    MlsCommit {
        room: String,
        epoch: u64,
        commit: String,
        #[serde(default)]
        welcome: Option<String>,
        #[serde(default)]
        for_device: Option<String>,
    },
    /// A room's handshake log since `since`.
    MlsSync { room: String, since: i64 },
    /// Re-seal an existing shard under a new key epoch.
    ///
    /// Only the sealed bytes move. The id, scope, author and author's email —
    /// everything the seal is bound to and everything provenance rests on —
    /// stay exactly as they were, which is why this is not a republish: a
    /// republish would make the rewrapper the author of somebody else's fact.
    /// After a Remove the member who removed re-seals what they can read, so
    /// a departure does not leave the room's facts readable only under a key
    /// nobody should still have.
    MemRewrap { id: String, epoch: u64, nonce: String, ciphertext: String },
    /// Delete shards. Used when a session ends: its context existed for the
    /// peers who were in the area with it, and outliving the session would
    /// make it a stale plan presented as a live one.
    MemForget { ids: Vec<String> },
    /// Which devices belong in a room, so a member can tell who is missing
    /// from the group and add them.
    MlsRoster { room: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        seq: u64,
        claims: Vec<Claim>,
        sessions: Vec<SessionInfo>,
        /// Who the presented key says this daemon is. `None` for the
        /// identities with no verified person behind them — a legacy shared
        /// secret, an unconfigured loopback relay — which is also exactly the
        /// set that may not publish memory: a shard whose provenance is a
        /// display string is worse than no shard.
        #[serde(default)]
        me: Option<Me>,
        /// The key provider this deployment seals memory with.
        #[serde(default)]
        provider: Option<String>,
    },
    Event { seq: u64, event: Event },
    ClaimResp {
        id: String,
        granted: bool,
        holder: Option<String>,
        holder_user: Option<String>,
        holder_intent: Option<String>,
        lease_until: Option<Ts>,
        /// A hub file: widely shared, leased short, queued rather than owned.
        #[serde(default)]
        hub: bool,
        /// How many sessions are already waiting on this path. Only the relay
        /// sees every session, so only the relay can answer it.
        #[serde(default)]
        queued: usize,
    },
    /// Shards, in answer to a sync or a fetch, or pushed as they are
    /// published. `more` is set when a sync was truncated and the client
    /// should ask again from its new high-water mark.
    MemShards {
        shards: Vec<crate::memory::Shard>,
        #[serde(default)]
        more: bool,
    },
    /// A publish the relay would not take. Never a reason to stop an agent —
    /// it is told, on its next boundary, and carries on.
    MemRejected { id: String, reason: String },
    /// A room's handshake log, in the order the Delivery Service assigned.
    /// `started` says whether the room has a group at all yet.
    MlsLog { room: String, msgs: Vec<crate::mls::Envelope>, #[serde(default)] started: bool },
    /// Something changed about a room — a commit landed, or a device uploaded
    /// a key package. Ask for the log.
    ///
    /// Deliberately not an empty `MlsLog`: a nudge and a sync that found
    /// nothing would then be the same message, and a daemon answering a nudge
    /// with a sync would answer its own answer forever.
    MlsWake { room: String },
    /// Shards that have been deleted. Broadcast so that a *peer's* daemon
    /// drops them too: without it, another machine would go on showing a
    /// finished session's plan as a live one, which is the failure the
    /// deletion exists to prevent.
    MemForgotten { ids: Vec<String> },
    /// A commit the Delivery Service would not take, because another daemon's
    /// commit for that epoch landed first. The loser discards and re-syncs.
    MlsRejected { room: String, reason: String },
    /// A device's key package, or `None` if it has not uploaded one.
    MlsKeyPackage { device: String, key_package: Option<String> },
    /// The devices that belong in a room.
    MlsRoster { room: String, devices: Vec<String> },
}

/// The verified identity behind a connection, as far as a client needs it: to
/// seal a shard it must bind its own member id and the team its scopes are
/// under, and it cannot learn either from anything it holds locally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Me {
    pub team_id: String,
    pub member_id: String,
    pub email: String,
    /// This machine's device row. The MLS credential identity, so a leaf in a
    /// group and a row in the console name the same laptop.
    #[serde(default)]
    pub device_id: String,
    /// The rooms this key is in, and which area of *this repo* each covers.
    /// A memory scope is sealed under the group of the room that grants it,
    /// and only the relay knows that mapping.
    #[serde(default)]
    pub rooms: Vec<(String, String)>,
}

fn yes() -> bool {
    true
}

/// Which key provider the deployment seals memory with.
///
/// A property of the *deployment*, not of the client: the relay is what knows
/// whether it sits in a customer's own network (`plaintext`, where the box is
/// the trust boundary) or hosts other people's rooms (`mls`). A client that
/// chose for itself could seal shards nobody else could open.
pub const PROVIDER_PLAINTEXT: &str = "plaintext";
pub const PROVIDER_MLS: &str = "mls";

/// The prefix `knoot present` gives its session ids.
///
/// A person in an editor and an agent are not the same kind of peer: an agent
/// can be told to stop and re-plan, and a person cannot. Anything that reports
/// presence should say which it is looking at, and the session id is the only
/// thing that travels everywhere.
pub const HUMAN_SESSION_PREFIX: &str = "human-";

pub fn is_human_session(session: &str) -> bool {
    session.starts_with(HUMAN_SESSION_PREFIX)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub session: String,
    pub user: String,
    pub path: String,
    pub lease_until: Ts,
    pub intent: String,
    /// The branch the holder is on. Two agents in one file on *different*
    /// branches are not colliding yet — git will merge them, or fail to — so
    /// this is what separates a block from a warning.
    #[serde(default)]
    pub branch: String,
}

/// A write by some session, kept just long enough to tell a peer that the
/// ground moved under it. `last_write` cannot serve this: it keeps one entry
/// per path and names a session, not a user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerWrite {
    pub session: String,
    pub user: String,
    pub path: String,
    pub ts: Ts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waiter {
    pub session: String,
    pub user: String,
    pub path: String,
    pub since: Ts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: String,
    pub user: String,
    pub branch: String,
    pub intent: String,
    pub last_seen: Ts,
}

/// Two claim paths conflict if equal, or one is a directory prefix of the other.
/// Whether two branch labels should be treated as the same branch. An unknown
/// branch on either side compares equal: knoot would rather block a write it
/// could have allowed than allow one it should have blocked.
pub fn same_branch(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
}

/// Words too common in a task sentence to be evidence of anything.
const INTENT_NOISE: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "with", "into", "from", "at",
    "by", "is", "are", "be", "it", "this", "that", "then", "so", "we", "i", "you", "please",
    "add", "make", "do", "also", "use", "using", "some", "new", "up", "out",
    // Placeholder nouns. Two sentences whose only shared words are these have
    // told us nothing: "add the thing to the file" matches everything.
    "thing", "file", "code", "stuff", "work", "part", "bit",
];

/// The significant words of an intent sentence, lowercased and deduplicated.
fn intent_words(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !INTENT_NOISE.contains(w))
        .map(|w| w.trim_end_matches('s').to_string())
        .filter(|w| w.len() > 2)
        .collect()
}

/// Are two sessions plausibly doing the same task?
///
/// grite's headline number — duplicate work 78% → 0% — is about tasks, and a
/// file claim cannot see a task. knoot already has an intent sentence per
/// session per turn, so the cheap version of a task tracker is to compare
/// them. Jaccard over significant words, which is crude and deliberately so:
/// the output is a sentence saying "sam declared something similar", not a
/// decision. A false positive costs one line of context; a false negative
/// costs two agents writing the same function.
pub fn intents_overlap(a: &str, b: &str) -> bool {
    intents_overlap_beyond(a, b, &Default::default())
}

/// Words that every session in this room is saying, and which therefore say
/// nothing about who is doing what.
///
/// Found the hard way. In a live four-agent run this fired **sixteen times out
/// of sixteen**, every one of them false: the harness gave all four agents the
/// same preamble ("Read GOAL.md — it is the shared objective…"), an intent is
/// the first 160 characters of a prompt, so the four intents were nearly
/// identical by construction and the overlap was real but meaningless. A
/// warning that fires on boilerplate stops being read within a day, which is
/// exactly what this feature was supposed to avoid.
///
/// A team prompt, a project convention, a repeated instruction — anything most
/// of the room is saying is background, not a task. Needs three intents before
/// it will call anything boilerplate: with two, "the same words" and "the same
/// task" are indistinguishable.
pub fn boilerplate_words<'a>(
    intents: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeSet<String> {
    let sets: Vec<std::collections::BTreeSet<String>> =
        intents.into_iter().map(intent_words).filter(|w| !w.is_empty()).collect();
    if sets.len() < 3 {
        return Default::default();
    }
    let mut counts: HashMap<&String, usize> = HashMap::new();
    for set in &sets {
        for w in set {
            *counts.entry(w).or_default() += 1;
        }
    }
    // Strictly more than half, and never fewer than three sessions. A word
    // shared by exactly two *is the signal* — two agents on one task is the
    // case this whole feature exists for — so the bar has to sit above that
    // or the fix for the false positives would silence the true ones.
    let threshold = (sets.len() / 2 + 1).max(3);
    counts
        .into_iter()
        .filter(|(_, n)| *n >= threshold)
        .map(|(w, _)| w.clone())
        .collect()
}

/// As `intents_overlap`, ignoring words the whole room is using.
pub fn intents_overlap_beyond(
    a: &str,
    b: &str,
    boilerplate: &std::collections::BTreeSet<String>,
) -> bool {
    let strip = |t: &str| -> std::collections::BTreeSet<String> {
        intent_words(t).into_iter().filter(|w| !boilerplate.contains(w)).collect()
    };
    let (x, y) = (strip(a), strip(b));
    // Two words is not a task description; claiming a match on one shared
    // word would fire on almost every pair.
    if x.len() < 2 || y.len() < 2 {
        return false;
    }
    let shared = x.intersection(&y).count();
    if shared < 2 {
        return false;
    }
    let union = x.union(&y).count().max(1);
    shared * 2 >= union
}

pub fn paths_overlap(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).map_or(false, |r| r.starts_with('/'))
        || b.strip_prefix(a).map_or(false, |r| r.starts_with('/'))
}

/// Materialized view of the log: live claims + sessions. Used by both
/// the relay (authoritative) and the daemon (local mirror).
#[derive(Debug, Default)]
pub struct View {
    pub claims: Vec<Claim>,
    pub sessions: HashMap<String, SessionInfo>,
    /// Sessions blocked on a path, waiting for it to free up.
    pub waiters: Vec<Waiter>,
    /// Who last wrote each path, and when. The working-tree audit is blind to
    /// authorship — the tree is shared — so it consults this instead of
    /// assuming every change inside its window was its own.
    pub last_write: HashMap<String, (String, Ts)>,
    /// Recent writes, newest last, within `WRITE_WINDOW_MS`. Feeds the
    /// "changed under you since your last turn" context an agent receives
    /// without asking for it.
    pub recent_writes: Vec<PeerWrite>,
    /// Who claimed what, within `HUB_WINDOW_MS`: `(path, session, ts)`. A
    /// path claimed by enough distinct sessions is a hub, and a hub is leased
    /// short and queued. Derived from the log rather than declared, so a hub
    /// nobody thought to name in `.knoot.toml` is still found.
    pub claim_history: Vec<(String, String, Ts)>,
    /// Paths the repo declares as hubs in `.knoot.toml`. Only a client can
    /// know these — the relay never sees the repo — so they arrive on a claim
    /// request and are remembered here.
    pub declared_hubs: std::collections::BTreeSet<String>,
}

impl View {
    pub fn prune(&mut self) {
        let now = now_ms();
        self.claims.retain(|c| c.lease_until > now);
        // Drop sessions that have gone quiet, and anything they still held.
        // A crashed session never sends SessionEnded, so without this its
        // presence lingers forever and peers plan around a ghost.
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.last_seen) > SESSION_STALE_MS)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.sessions.remove(&k);
            self.claims.retain(|c| c.session != k);
            self.waiters.retain(|w| w.session != k);
        }
        self.recent_writes.retain(|w| now.saturating_sub(w.ts) <= WRITE_WINDOW_MS);
        self.claim_history.retain(|(_, _, ts)| now.saturating_sub(*ts) <= HUB_WINDOW_MS);
    }

    /// Is this path a hub: declared as one, or claimed by `HUB_SESSIONS`
    /// distinct sessions inside `HUB_WINDOW_MS`?
    ///
    /// Overlap counts. A hub is usually a file, but a shared *directory*
    /// claimed by everyone is the same bottleneck, and `paths_overlap` is
    /// already how this system decides whether two claims are about the same
    /// thing.
    pub fn is_hub(&self, path: &str) -> bool {
        if self.declared_hubs.iter().any(|h| paths_overlap(h, path)) {
            return true;
        }
        let now = now_ms();
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for (p, session, ts) in &self.claim_history {
            if now.saturating_sub(*ts) <= HUB_WINDOW_MS && paths_overlap(p, path) {
                seen.insert(session.as_str());
            }
        }
        seen.len() >= HUB_SESSIONS
    }

    /// The lease a claim on `path` should get: short for a hub, normal
    /// otherwise.
    pub fn lease_for(&self, path: &str) -> Ts {
        if self.is_hub(path) { HUB_LEASE_MS } else { LEASE_MS }
    }

    /// How many *other* sessions are already waiting on a path overlapping
    /// this one. This is the queue line in the brief: "held by ash, 2 behind
    /// you" is the difference between waiting and re-planning.
    pub fn queue_len(&self, path: &str, except: &str) -> usize {
        self.waiters_for(path, except).len()
    }

    /// Peer writes since `since`, newest first, one entry per (user, path):
    /// an agent needs to know the ground moved, not how many times.
    pub fn writes_since(&self, session: &str, since: Ts) -> Vec<PeerWrite> {
        let mut out: Vec<PeerWrite> = Vec::new();
        for w in self.recent_writes.iter().rev() {
            if w.session == session || w.ts < since {
                continue;
            }
            if out.iter().any(|o| o.user == w.user && o.path == w.path) {
                continue;
            }
            out.push(w.clone());
        }
        out
    }

    /// True if a session other than `session` wrote `path` at or after `since`.
    pub fn written_by_other_since(&self, session: &str, path: &str, since: Ts) -> bool {
        self.last_write
            .get(path)
            .is_some_and(|(who, ts)| who != session && *ts + 250 >= since)
    }

    /// Sessions waiting on a path that overlaps `path`, excluding `except`.
    pub fn waiters_for(&self, path: &str, except: &str) -> Vec<Waiter> {
        self.waiters
            .iter()
            .filter(|w| w.session != except && paths_overlap(&w.path, path))
            .cloned()
            .collect()
    }

    /// First live claim held by a *different* session that overlaps `path`
    /// **on the same branch**. Only same-branch overlap is a collision: the
    /// two agents are writing the same lines of the same working tree.
    ///
    /// A claim with no recorded branch (an older client, or a session that
    /// registered before branches travelled with claims) is treated as
    /// same-branch — blocking on too little information is the safe error.
    pub fn conflicting(&self, session: &str, path: &str) -> Option<&Claim> {
        self.conflicting_on(session, path, "")
    }

    /// What the holder of a claim is *currently* doing.
    ///
    /// A claim records the intent its holder had at the moment it acquired the
    /// file, and then keeps it — but a lease is ten minutes, renewed on
    /// activity, so it routinely outlives the turn that took it. Two things
    /// follow, and both were visible in briefs: a session that claimed a file
    /// before its first prompt reported `intent: unknown` while `knoot who`
    /// showed exactly what it was doing, and a session that moved on to
    /// something else had its old intent quoted back with full confidence.
    ///
    /// The live session record is therefore the better answer where we have
    /// one. The claim's own copy is the fallback, for a holder whose session
    /// we have not seen — and an empty string means genuinely unknown, which
    /// is a claim taken before any prompt at all.
    pub fn holder_intent(&self, claim: &Claim) -> String {
        self.sessions
            .get(&claim.session)
            .map(|s| s.intent.clone())
            .filter(|i| !i.trim().is_empty())
            .unwrap_or_else(|| claim.intent.clone())
    }

    /// A copy of `claim` whose intent is the holder's current one. Convenient
    /// where a `Claim` is about to be handed to something that formats it.
    pub fn claim_with_live_intent(&self, claim: &Claim) -> Claim {
        let mut c = claim.clone();
        c.intent = self.holder_intent(claim);
        c
    }

    /// As `conflicting`, for a writer known to be on `branch`.
    pub fn conflicting_on(&self, session: &str, path: &str, branch: &str) -> Option<&Claim> {
        let now = now_ms();
        self.claims.iter().find(|c| {
            c.session != session
                && c.lease_until > now
                && paths_overlap(&c.path, path)
                && same_branch(&c.branch, branch)
        })
    }

    /// Live claims on `path` held from a *different* branch. These do not
    /// block; they are a merge conflict that has not happened yet.
    pub fn cross_branch_overlap(&self, session: &str, path: &str, branch: &str) -> Vec<Claim> {
        let now = now_ms();
        self.claims
            .iter()
            .filter(|c| {
                c.session != session
                    && c.lease_until > now
                    && paths_overlap(&c.path, path)
                    && !same_branch(&c.branch, branch)
            })
            .cloned()
            .collect()
    }

    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::SessionStarted { session, user, branch, ts } => {
                self.sessions.insert(
                    session.clone(),
                    SessionInfo {
                        session: session.clone(),
                        user: user.clone(),
                        branch: branch.clone(),
                        intent: String::new(),
                        last_seen: *ts,
                    },
                );
            }
            Event::IntentDeclared { session, text, ts, branch } => {
                if let Some(s) = self.sessions.get_mut(session) {
                    s.intent = text.clone();
                    s.last_seen = *ts;
                    if !branch.is_empty() {
                        s.branch = branch.clone();
                    }
                }
            }
            Event::ClaimAcquired { session, user, path, lease_until, intent, branch, .. } => {
                if let Some(s) = self.sessions.get_mut(session) {
                    s.last_seen = now_ms();
                }
                self.waiters.retain(|w| !(w.session == *session && w.path == *path));
                // Every acquisition, including renewals by the same session:
                // `is_hub` counts distinct sessions, so a session renewing a
                // file it already holds cannot inflate the count, and a fresh
                // timestamp is what keeps a busy hub inside the window.
                self.claim_history.push((path.clone(), session.clone(), now_ms()));
                // Renew if this session already holds it, else insert.
                if let Some(c) = self
                    .claims
                    .iter_mut()
                    .find(|c| c.session == *session && c.path == *path)
                {
                    c.lease_until = *lease_until;
                } else {
                    self.claims.push(Claim {
                        session: session.clone(),
                        user: user.clone(),
                        path: path.clone(),
                        lease_until: *lease_until,
                        intent: intent.clone(),
                        // Fall back to the session's branch: a claim minted by
                        // an older client still lands on the right branch.
                        branch: if branch.is_empty() {
                            self.sessions.get(session).map(|s| s.branch.clone()).unwrap_or_default()
                        } else {
                            branch.clone()
                        },
                    });
                }
            }
            Event::ClaimReleased { session, path, .. } => {
                self.claims.retain(|c| !(c.session == *session && c.path == *path));
            }
            Event::FileWritten { session, user, path, ts } => {
                self.last_write.insert(path.clone(), (session.clone(), *ts));
                // Authorship comes off the event now, so a peer can be told
                // who moved the file without a join back through presence.
                let user = if user.is_empty() {
                    self.sessions.get(session).map(|s| s.user.clone()).unwrap_or_default()
                } else {
                    user.clone()
                };
                self.recent_writes.push(PeerWrite {
                    session: session.clone(),
                    user,
                    path: path.clone(),
                    ts: *ts,
                });
                // Writing renews the covering lease.
                for c in self.claims.iter_mut() {
                    if c.session == *session && paths_overlap(&c.path, path) {
                        c.lease_until = ts + LEASE_MS;
                    }
                }
                if let Some(s) = self.sessions.get_mut(session) {
                    s.last_seen = *ts;
                }
            }
            Event::PathRemoved { session, path, .. } => {
                // The file is gone; a claim on it means nothing now, and
                // leaving one would block a peer from creating a replacement.
                self.claims.retain(|c| !(c.session == *session && c.path == *path));
            }
            // Observability, all three: they change how an agent is briefed,
            // never what the log says is held.
            Event::StaleRead { .. } => {}
            Event::CreateCollision { .. } => {}
            Event::DuplicateIntent { .. } => {}
            // A refusal changes nothing about who holds what. It is on the
            // log so an admin can count them, and nowhere else.
            Event::MemoryRefused { .. } => {}
            Event::ClaimDenied { session, user, path, .. } => {
                // A denial is also a subscription: this session wants the path.
                if !self
                    .waiters
                    .iter()
                    .any(|w| w.session == *session && w.path == *path)
                {
                    self.waiters.push(Waiter {
                        session: session.clone(),
                        user: user.clone(),
                        path: path.clone(),
                        since: now_ms(),
                    });
                }
            }
            Event::PathFreed { path, .. } => {
                self.waiters.retain(|w| !paths_overlap(&w.path, path));
            }
            Event::Message { .. } => {}
            Event::UngatedWrite { .. } => {} // observability only
            Event::CrossBranchOverlap { .. } => {} // a warning, not state
            Event::SessionEnded { session, .. } => {
                self.claims.retain(|c| c.session != *session);
                self.sessions.remove(session);
            }
        }
        self.prune();
    }
}

/// Daemon unix-socket API (JSON lines).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DReq {
    PreWrite {
        repo_root: String,
        session: String,
        path: String,
        /// The tool would write the file whole (`Write`), rather than editing
        /// part of it. Creating a path a peer just created is a different
        /// failure from editing one, and only the client knows which tool ran.
        #[serde(default)]
        creating: bool,
    },
    /// A file this session has read. Kept per session per turn, and compared
    /// against the log before the next write: STORM's biggest single result is
    /// that a write is stale when what the agent *read* has changed, even when
    /// the file it is writing is untouched.
    FileRead { repo_root: String, session: String, path: String },
    PostWrite { repo_root: String, session: String, path: String },
    SessionStart { repo_root: String, session: String, user: String, branch: String },
    Intent { repo_root: String, session: String, text: String, user: String, #[serde(default)] branch: String },
    SessionEnd { repo_root: String, session: String },
    /// Send a message to a peer user, or to everyone when `to` is None.
    /// Identity travels with the request: Claude Code exposes no session id to
    /// the commands it runs, so a CLI caller can only know who it is.
    Msg { repo_root: String, from_user: String, to: Option<String>, text: String },
    /// Drain this user's mailbox.
    Poll { repo_root: String, user: String },
    /// The agent is trying to finish its turn. Pending mail is a reason to
    /// keep going, so this answers with anything undelivered.
    StopCheck { repo_root: String, user: String, already_continued: bool },
    /// A Bash command about to run: parse it for write targets and gate them.
    BashPre { repo_root: String, session: String, command: String },
    /// A Bash command that finished: diff the working tree if it was audited.
    BashPost { repo_root: String, session: String },
    Who { repo_root: String },
    /// Publish a fact into this repo's memory. `name` is the handle a later
    /// fact supersedes by; `from` reads the text out of a file, through
    /// exactly the refusals a typed fact goes through.
    Remember {
        repo_root: String,
        session: String,
        user: String,
        name: String,
        text: String,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        from: Option<String>,
    },
    /// Declare what this session is doing, for peers in the same area right
    /// now: the plan, the paths, and what has been settled.
    ///
    /// Written by an agent on purpose, in this shape. Nothing here is ever
    /// derived from a transcript: a free-text conclusion pulled out of one is
    /// an exfiltration path with no reviewer, and no amount of care about
    /// what we extract fixes that.
    Plan {
        repo_root: String,
        session: String,
        user: String,
        text: String,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        decisions: Vec<String>,
    },
    /// Cache something derived — where a symbol lives, how the tests run,
    /// what a module does — so the next agent does not work it out again.
    Cache {
        repo_root: String,
        session: String,
        user: String,
        name: String,
        text: String,
        #[serde(default)]
        paths: Vec<String>,
    },
    /// What this repo's memory holds, optionally filtered by a query. The
    /// command for people and capable models; nothing depends on it, because
    /// the injection reaches an agent whether or not it thinks to ask.
    Recall {
        repo_root: String,
        #[serde(default)]
        query: String,
    },
    /// Is this daemon actually talking to the relay? `knoot status` cannot
    /// answer that by inspecting config: a stored token proves nothing about
    /// whether the dial succeeded.
    Health { repo_root: String },
}

/// Constructors for the answer the hot path gives most often. `PreWrite` has
/// eight ways to end in "allowed"; spelling the struct out at each of them is
/// how a new advisory field ends up silently dropped on one branch.
impl DResp {
    pub fn allow() -> Self {
        DResp::Decision { allow: true, reason: None, notes: Vec::new() }
    }

    pub fn allow_with(notes: Vec<String>) -> Self {
        DResp::Decision { allow: true, reason: None, notes }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum DResp {
    Decision {
        allow: bool,
        reason: Option<String>,
        /// Things worth telling the agent that are not grounds to stop it:
        /// a file it read has moved, a path it is creating already exists, a
        /// hub it is queueing for. Advisory by construction — the doctrine
        /// here is awareness first, and a deny only where a merge is
        /// impossible.
        #[serde(default)]
        notes: Vec<String>,
    },
    Mail { items: Vec<String> },
    /// Facts, rendered for a human or an agent that asked.
    Memory {
        items: Vec<String>,
        /// Shards that arrived and would not open. Reported rather than
        /// hidden: unreadable memory that says nothing is indistinguishable
        /// from a working install with nothing to say.
        #[serde(default)]
        unreadable: usize,
        /// Which key provider sealed them, so `knoot status` can say which
        /// deployment this machine is in without the human reading config.
        #[serde(default)]
        provider: String,
        /// Whether this machine can seal at all. Under `Mls` it cannot until
        /// the room's group has reached it, and "waiting for the room's key"
        /// is a different kind of quiet from "nothing to say".
        #[serde(default)]
        ready: bool,
        /// Whether this key names a verified person. Memory needs one — a
        /// shard whose provenance is a display string is worse than no shard
        /// — so an unconfigured relay can *read* memory and never write it.
        /// Silent before, which cost a whole lab run: `knoot status` said
        /// "0 facts" where it should have said "no key, so none can be
        /// written".
        #[serde(default = "yes")]
        identified: bool,
    },
    /// Everything an agent should know at the start of a turn without having
    /// run a command for it. `default` on the pushed fields so a running
    /// daemon from an older build still answers something usable.
    Peers {
        sessions: Vec<SessionInfo>,
        claims: Vec<Claim>,
        #[serde(default)]
        writes: Vec<PeerWrite>,
        #[serde(default)]
        mail: Vec<String>,
        /// Advisory lines for the start of a turn: a peer declaring the same
        /// task, a file this session read that has since moved, a deletion.
        #[serde(default)]
        notes: Vec<String>,
        /// What peers in this area are doing right now, from the
        /// `session_context` they published on purpose.
        #[serde(default)]
        context: Vec<String>,
        /// Derived knowledge about the paths this session is in.
        #[serde(default)]
        cached: Vec<String>,
        /// Of `writes`, the paths this session had actually read — the ones
        /// its reasoning rests on. Ranked first in the brief, because "the
        /// ground moved" matters most where you were standing.
        #[serde(default)]
        depended_on: Vec<String>,
        /// What this repo has learned, for the areas this session works in.
        /// Capped hard: the rest of this brief arrived unasked too, and an
        /// injection nobody reads coordinates nothing.
        #[serde(default)]
        memory: Vec<String>,
    },
    Ok,
    Err { msg: String },
    /// `connected` is the socket; `ready` means a Welcome snapshot has landed,
    /// so the mirror can be trusted. `last_error` is the most recent dial
    /// failure, which is what turns "off" into something a human can fix.
    Health {
        connected: bool,
        ready: bool,
        #[serde(default)]
        last_error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(session: &str, path: &str, lease_until: Ts) -> Claim {
        Claim {
            session: session.into(),
            user: "u".into(),
            path: path.into(),
            lease_until,
            intent: "i".into(), branch: String::new(),
        }
    }

    // ---------- intents_overlap ----------

    /// The case that motivates this: two agents, one task, and no file claim
    /// that could ever see it.
    #[test]
    fn two_phrasings_of_one_task_overlap() {
        assert!(intents_overlap(
            "add retry with backoff to the http client",
            "please add retry and backoff to our HTTP client"
        ));
        assert!(intents_overlap(
            "fix the rounding in invoice tax",
            "fix invoice tax rounding bug"
        ));
    }

    #[test]
    fn different_tasks_do_not_overlap() {
        assert!(!intents_overlap(
            "add retry with backoff to the http client",
            "fix the rounding in the invoice tax calculation"
        ));
        assert!(!intents_overlap("write the auth tests", "update the readme"));
    }

    /// The regression from the four-agent run: sixteen notices, all false.
    ///
    /// Every agent was handed the same preamble by the harness, and an intent
    /// is the first 160 characters of a prompt — so the four intents were
    /// nearly identical and the overlap was real and meaningless. The words
    /// that distinguish these agents are the ones after the shared part.
    #[test]
    fn a_preamble_every_agent_was_handed_is_not_a_shared_task() {
        let preamble = "Read GOAL.md — it is the shared objective and you are one of four \
                        agents working this repo at the same time. You are user";
        let intents: Vec<String> = ["ash", "priya", "sam", "ci-bot"]
            .iter()
            .map(|n| format!("{preamble} \"{n}\". You own src/{n}.js"))
            .collect();
        let refs: Vec<&str> = intents.iter().map(|s| s.as_str()).collect();

        // As it was: every pair "overlaps", which is what fired sixteen times.
        assert!(
            intents_overlap(refs[0], refs[1]),
            "the bug, kept as a fact: raw overlap on boilerplate is real"
        );

        // As it is: the shared preamble is recognised as the room's
        // background and only the distinguishing words are compared.
        let boilerplate = boilerplate_words(refs.clone());
        assert!(boilerplate.contains("agent"), "the preamble is boilerplate: {boilerplate:?}");
        for (i, a) in refs.iter().enumerate() {
            for (j, b) in refs.iter().enumerate() {
                if i != j {
                    assert!(
                        !intents_overlap_beyond(a, b, &boilerplate),
                        "{a:?} and {b:?} are different tasks under one preamble"
                    );
                }
            }
        }
    }

    /// And the feature must still work. Boilerplate stripping that also
    /// stripped the signal would be a silent regression in the other
    /// direction, which is worse: a false negative is two agents writing the
    /// same function.
    #[test]
    fn a_shared_preamble_does_not_hide_a_genuinely_shared_task() {
        let preamble = "Read GOAL.md — it is the shared objective and you are one of four \
                        agents working this repo at the same time.";
        let a = format!("{preamble} Add retry with backoff to the http client.");
        let b = format!("{preamble} Please add retry and backoff to our HTTP client.");
        let c = format!("{preamble} Fix the rounding in the invoice tax calculation.");
        let d = format!("{preamble} Update the readme with install steps.");
        let boilerplate = boilerplate_words(vec![a.as_str(), b.as_str(), c.as_str(), d.as_str()]);

        assert!(
            intents_overlap_beyond(&a, &b, &boilerplate),
            "two phrasings of one task still overlap once the preamble is gone"
        );
        assert!(!intents_overlap_beyond(&a, &c, &boilerplate));
        assert!(!intents_overlap_beyond(&c, &d, &boilerplate));
    }

    /// Two intents cannot tell "the same words" from "the same task", so
    /// nothing is called boilerplate until there are three to compare.
    #[test]
    fn two_sessions_are_too_few_to_call_anything_boilerplate() {
        let a = "add retry with backoff to the http client";
        let b = "please add retry and backoff to our HTTP client";
        assert!(boilerplate_words(vec![a, b]).is_empty());
        assert!(intents_overlap_beyond(a, b, &boilerplate_words(vec![a, b])));
    }

    /// Everything an agent says shares "the", "add", "fix". A warning that
    /// fires on filler stops being read within a day.
    #[test]
    fn shared_filler_is_not_a_shared_task() {
        assert!(!intents_overlap(
            "add the thing to the file",
            "add the other thing to the file"
        ));
        assert!(!intents_overlap("fix it", "fix it"), "two words is not a task description");
        assert!(!intents_overlap("", "add retry to the http client"));
    }

    #[test]
    fn one_shared_word_is_not_enough() {
        assert!(!intents_overlap(
            "refactor the billing module",
            "delete the billing tests"
        ));
    }

    // ---------- hubs ----------

    fn acquire(v: &mut View, session: &str, path: &str) {
        v.sessions.insert(
            session.into(),
            SessionInfo {
                session: session.into(),
                user: session.into(),
                branch: "main".into(),
                intent: String::new(),
                last_seen: now_ms(),
            },
        );
        v.apply(&Event::ClaimAcquired {
            session: session.into(),
            user: session.into(),
            path: path.into(),
            lease_until: now_ms() + LEASE_MS,
            intent: String::new(),
            branch: "main".into(),
            ts: now_ms(),
        });
        // Released again, so the next claimant is not merely blocked: a hub is
        // a file everyone takes in turn, not one somebody is holding.
        v.apply(&Event::ClaimReleased {
            session: session.into(),
            path: path.into(),
            ts: now_ms(),
        });
    }

    #[test]
    fn a_path_claimed_by_enough_sessions_is_a_hub() {
        let mut v = View::default();
        acquire(&mut v, "s1", "src/routes.ts");
        acquire(&mut v, "s2", "src/routes.ts");
        assert!(!v.is_hub("src/routes.ts"), "two is a coincidence");
        acquire(&mut v, "s3", "src/routes.ts");
        assert!(v.is_hub("src/routes.ts"), "three inside the window is a shared dependency");
        assert_eq!(v.lease_for("src/routes.ts"), HUB_LEASE_MS);
        assert_eq!(v.lease_for("src/auth.ts"), LEASE_MS);
    }

    /// One session claiming a file all afternoon is a busy session, not a hub.
    #[test]
    fn one_session_reclaiming_a_path_never_makes_it_a_hub() {
        let mut v = View::default();
        for _ in 0..10 {
            acquire(&mut v, "s1", "src/routes.ts");
        }
        assert!(!v.is_hub("src/routes.ts"));
    }

    #[test]
    fn a_declared_hub_is_a_hub_from_the_first_claim() {
        let mut v = View::default();
        v.declared_hubs.insert("package.json".into());
        assert!(v.is_hub("package.json"));
        assert_eq!(v.lease_for("package.json"), HUB_LEASE_MS);
        assert!(!v.is_hub("package-lock.json"), "the prefix trap applies here too");
    }

    /// A shared *directory* everyone claims is the same bottleneck as a
    /// shared file, and overlap is already how this system decides whether
    /// two claims are about the same thing.
    #[test]
    fn a_hub_directory_covers_the_files_under_it() {
        let mut v = View::default();
        v.declared_hubs.insert("src/types".into());
        assert!(v.is_hub("src/types/user.ts"));
        assert!(!v.is_hub("src/typescript.ts"));
    }

    #[test]
    fn hub_claims_age_out_of_the_window() {
        let mut v = View::default();
        let old = now_ms().saturating_sub(HUB_WINDOW_MS + 60_000);
        for s in ["s1", "s2", "s3"] {
            v.claim_history.push(("src/routes.ts".into(), s.into(), old));
        }
        assert!(!v.is_hub("src/routes.ts"), "yesterday's crowd is not today's hub");
        v.claim_history.push(("src/routes.ts".into(), "s4".into(), now_ms()));
        assert!(!v.is_hub("src/routes.ts"));
    }

    /// The queue is what turns "wait" into a decision an agent can make.
    #[test]
    fn the_queue_counts_everyone_waiting_but_you() {
        let mut v = View::default();
        for (s, u) in [("s2", "priya"), ("s3", "sam")] {
            v.apply(&Event::ClaimDenied {
                session: s.into(),
                user: u.into(),
                path: "package.json".into(),
                holder: "s1".into(),
                holder_user: "ash".into(),
                ts: now_ms(),
            });
        }
        assert_eq!(v.queue_len("package.json", "s2"), 1);
        assert_eq!(v.queue_len("package.json", ""), 2);
    }

    // ---------- removal ----------

    /// A claim on a file that no longer exists blocks a peer from creating a
    /// replacement, over a file nobody can edit.
    #[test]
    fn removing_a_path_drops_the_claim_on_it() {
        let mut v = View::default();
        acquire(&mut v, "s1", "src/old.ts");
        v.apply(&Event::ClaimAcquired {
            session: "s1".into(),
            user: "ash".into(),
            path: "src/old.ts".into(),
            lease_until: now_ms() + LEASE_MS,
            intent: String::new(),
            branch: "main".into(),
            ts: now_ms(),
        });
        assert!(v.conflicting("s2", "src/old.ts").is_some());
        v.apply(&Event::PathRemoved {
            session: "s1".into(),
            user: "ash".into(),
            path: "src/old.ts".into(),
            moved: false,
            ts: now_ms(),
        });
        assert!(
            v.conflicting("s2", "src/old.ts").is_none(),
            "nobody can hold a file that is gone"
        );
    }

    // ---------- paths_overlap ----------

    #[test]
    fn overlap_identical() {
        assert!(paths_overlap("src/auth.ts", "src/auth.ts"));
    }

    #[test]
    fn overlap_dir_contains_file_both_directions() {
        assert!(paths_overlap("src/auth", "src/auth/session.ts"));
        assert!(paths_overlap("src/auth/session.ts", "src/auth"));
    }

    #[test]
    fn overlap_deep_nesting() {
        assert!(paths_overlap("src", "src/a/b/c/d.ts"));
    }

    /// The classic prefix trap: `src/auth` must NOT claim `src/auth2`.
    #[test]
    fn no_overlap_on_partial_segment() {
        assert!(!paths_overlap("src/auth", "src/auth2"));
        assert!(!paths_overlap("src/auth2", "src/auth"));
        assert!(!paths_overlap("src/auth.ts", "src/auth.tsx"));
        assert!(!paths_overlap("a/b", "a/bc/d"));
    }

    #[test]
    fn no_overlap_siblings() {
        assert!(!paths_overlap("src/auth.ts", "src/billing.ts"));
        assert!(!paths_overlap("src/a/x.ts", "src/b/x.ts"));
    }

    #[test]
    fn overlap_is_symmetric_over_samples() {
        let samples = [
            "src", "src/auth", "src/auth2", "src/auth/session.ts", "src/auth.ts", "lib/x", "",
        ];
        for a in samples {
            for b in samples {
                assert_eq!(
                    paths_overlap(a, b),
                    paths_overlap(b, a),
                    "asymmetric for {a:?} / {b:?}"
                );
            }
        }
    }

    // ---------- lease expiry ----------

    #[test]
    fn expired_claim_is_invisible_and_pruned() {
        let mut v = View::default();
        v.claims.push(claim("other", "src/auth.ts", now_ms() - 1));
        assert!(v.conflicting("me", "src/auth.ts").is_none(), "expired lease must not block");
        v.prune();
        assert!(v.claims.is_empty(), "expired lease must be pruned");
    }

    #[test]
    fn live_claim_by_other_blocks_but_own_does_not() {
        let mut v = View::default();
        v.claims.push(claim("other", "src/auth.ts", now_ms() + LEASE_MS));
        assert!(v.conflicting("me", "src/auth.ts").is_some());
        assert!(v.conflicting("other", "src/auth.ts").is_none(), "own claim must not block self");
    }

    #[test]
    fn dir_claim_blocks_nested_file() {
        let mut v = View::default();
        v.claims.push(claim("other", "src/auth", now_ms() + LEASE_MS));
        assert!(v.conflicting("me", "src/auth/session.ts").is_some());
        assert!(v.conflicting("me", "src/auth2/session.ts").is_none());
    }

    // ---------- View::apply ----------

    #[test]
    fn session_lifecycle_and_intent() {
        let mut v = View::default();
        v.apply(&Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: now_ms(),
        });
        assert_eq!(v.sessions.len(), 1);
        v.apply(&Event::IntentDeclared {
            session: "s1".into(),
            text: "refactor auth".into(),
            ts: now_ms(),
            branch: String::new(),
        });
        assert_eq!(v.sessions["s1"].intent, "refactor auth");
        assert_eq!(v.sessions["s1"].branch, "main");
    }

    #[test]
    fn intent_for_unknown_session_is_ignored() {
        let mut v = View::default();
        v.apply(&Event::IntentDeclared { session: "ghost".into(), text: "x".into(), ts: now_ms() , branch: String::new()});
        assert!(v.sessions.is_empty());
    }

    #[test]
    fn claim_acquired_twice_renews_rather_than_duplicates() {
        let mut v = View::default();
        let first = now_ms() + 1_000;
        let second = now_ms() + 60_000;
        for lease_until in [first, second] {
            v.apply(&Event::ClaimAcquired {
                session: "s1".into(),
                user: "ash".into(),
                path: "src/auth.ts".into(),
                lease_until,
                intent: "i".into(),
                branch: String::new(),
                ts: now_ms(),
            });
        }
        assert_eq!(v.claims.len(), 1, "same session+path must renew, not duplicate");
        assert_eq!(v.claims[0].lease_until, second);
    }

    #[test]
    fn release_removes_only_that_sessions_claim() {
        let mut v = View::default();
        v.claims.push(claim("s1", "src/a.ts", now_ms() + LEASE_MS));
        v.claims.push(claim("s2", "src/b.ts", now_ms() + LEASE_MS));
        v.apply(&Event::ClaimReleased { session: "s1".into(), path: "src/a.ts".into(), ts: now_ms() });
        assert_eq!(v.claims.len(), 1);
        assert_eq!(v.claims[0].session, "s2");
    }

    #[test]
    fn release_of_nonexistent_claim_is_a_noop() {
        let mut v = View::default();
        v.claims.push(claim("s1", "src/a.ts", now_ms() + LEASE_MS));
        v.apply(&Event::ClaimReleased { session: "s1".into(), path: "nope.ts".into(), ts: now_ms() });
        assert_eq!(v.claims.len(), 1);
    }

    #[test]
    fn file_written_renews_only_covering_leases_of_that_session() {
        let mut v = View::default();
        let soon = now_ms() + 1_000;
        v.claims.push(claim("s1", "src/auth", soon));      // covers the write
        v.claims.push(claim("s1", "lib/other.ts", soon));  // does not cover
        v.claims.push(claim("s2", "src/auth", soon));       // other session
        let ts = now_ms();
        v.apply(&Event::FileWritten { session: "s1".into(), user: "u".into(), path: "src/auth/session.ts".into(), ts });

        let get = |s: &str, p: &str| {
            v.claims.iter().find(|c| c.session == s && c.path == p).unwrap().lease_until
        };
        assert_eq!(get("s1", "src/auth"), ts + LEASE_MS, "covering lease must renew");
        assert_eq!(get("s1", "lib/other.ts"), soon, "unrelated lease must not renew");
        assert_eq!(get("s2", "src/auth"), soon, "other session's lease must not renew");
    }

    #[test]
    fn session_ended_sweeps_all_its_claims_and_presence() {
        let mut v = View::default();
        v.apply(&Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: now_ms(),
        });
        v.claims.push(claim("s1", "src/a.ts", now_ms() + LEASE_MS));
        v.claims.push(claim("s1", "src/b.ts", now_ms() + LEASE_MS));
        v.claims.push(claim("s2", "src/c.ts", now_ms() + LEASE_MS));
        v.apply(&Event::SessionEnded { session: "s1".into(), ts: now_ms() });
        assert!(v.sessions.is_empty());
        assert_eq!(v.claims.len(), 1);
        assert_eq!(v.claims[0].session, "s2");
    }

    // ---------- branch-aware claims ----------

    fn claim_on(session: &str, path: &str, branch: &str) -> Claim {
        Claim {
            session: session.into(),
            user: "u".into(),
            path: path.into(),
            lease_until: now_ms() + LEASE_MS,
            intent: "i".into(),
            branch: branch.into(),
        }
    }

    #[test]
    fn one_branch_one_file_is_a_collision() {
        let mut v = View::default();
        v.claims.push(claim_on("theirs", "lib/response.js", "main"));
        assert!(v.conflicting_on("mine", "lib/response.js", "main").is_some());
        assert!(v.cross_branch_overlap("mine", "lib/response.js", "main").is_empty());
    }

    #[test]
    fn two_branches_one_file_is_a_warning_not_a_collision() {
        let mut v = View::default();
        v.claims.push(claim_on("theirs", "lib/response.js", "main"));

        assert!(
            v.conflicting_on("mine", "lib/response.js", "feat/discounts").is_none(),
            "different branches must not block"
        );
        let warn = v.cross_branch_overlap("mine", "lib/response.js", "feat/discounts");
        assert_eq!(warn.len(), 1, "but it must still be reported");
        assert_eq!(warn[0].branch, "main");
    }

    /// Blocking on too little information is the safe error, so an unknown
    /// branch on either side compares equal.
    #[test]
    fn an_unknown_branch_blocks_rather_than_slipping_through() {
        let mut v = View::default();
        v.claims.push(claim_on("theirs", "lib/response.js", ""));
        assert!(v.conflicting_on("mine", "lib/response.js", "feat/x").is_some());

        let mut v2 = View::default();
        v2.claims.push(claim_on("theirs", "lib/response.js", "main"));
        assert!(v2.conflicting_on("mine", "lib/response.js", "").is_some());
    }

    #[test]
    fn a_claim_inherits_the_branch_of_its_session() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "feat/discounts".into(),
            ts: t0,
        });
        // An older client sends no branch on the claim itself.
        v.apply(&Event::ClaimAcquired {
            session: "s1".into(),
            user: "ash".into(),
            path: "lib/response.js".into(),
            lease_until: t0 + LEASE_MS,
            intent: "i".into(),
            branch: String::new(),
            ts: t0,
        });
        assert_eq!(v.claims[0].branch, "feat/discounts");
    }

    /// A session that checks out a different branch mid-run must claim under
    /// the new one; the branch travels with every turn for this reason.
    #[test]
    fn checking_out_a_branch_updates_presence() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: t0,
        });
        v.apply(&Event::IntentDeclared {
            session: "s1".into(),
            text: "add discounts".into(),
            ts: t0 + 1,
            branch: "feat/discounts".into(),
        });
        assert_eq!(v.sessions["s1"].branch, "feat/discounts");
    }

    #[test]
    fn an_empty_branch_on_intent_does_not_erase_a_known_one() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: t0,
        });
        v.apply(&Event::IntentDeclared {
            session: "s1".into(),
            text: "x".into(),
            ts: t0 + 1,
            branch: String::new(),
        });
        assert_eq!(v.sessions["s1"].branch, "main", "silence is not a branch change");
    }

    // ---------- pushed context: writes_since ----------

    fn wrote(session: &str, user: &str, path: &str, ts: Ts) -> Event {
        Event::FileWritten {
            session: session.into(),
            user: user.into(),
            path: path.into(),
            ts,
        }
    }

    #[test]
    fn writes_since_excludes_our_own_and_names_the_author() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&wrote("mine", "ash", "src/auth.js", t0));
        v.apply(&wrote("theirs", "priya", "src/billing.js", t0 + 1));

        let out = v.writes_since("mine", t0);
        assert_eq!(out.len(), 1, "our own writes are not news to us");
        assert_eq!(out[0].user, "priya");
        assert_eq!(out[0].path, "src/billing.js");
    }

    #[test]
    fn writes_since_ignores_anything_before_the_bookmark() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&wrote("theirs", "priya", "src/old.js", t0));
        v.apply(&wrote("theirs", "priya", "src/new.js", t0 + 100));

        let out = v.writes_since("mine", t0 + 50);
        assert_eq!(out.len(), 1, "only what happened since the last turn");
        assert_eq!(out[0].path, "src/new.js");
    }

    /// Ten edits to one file are one fact: the file moved.
    #[test]
    fn repeated_writes_to_one_path_collapse_to_the_latest() {
        let mut v = View::default();
        let t0 = now_ms();
        for i in 0..10 {
            v.apply(&wrote("theirs", "priya", "src/billing.js", t0 + i));
        }
        let out = v.writes_since("mine", t0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, t0 + 9, "the newest one survives");
    }

    #[test]
    fn two_peers_on_one_path_are_both_reported() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&wrote("s1", "priya", "src/billing.js", t0));
        v.apply(&wrote("s2", "sam", "src/billing.js", t0 + 1));

        assert_eq!(v.writes_since("mine", t0).len(), 2);
    }

    #[test]
    fn the_write_window_is_pruned() {
        let mut v = View::default();
        let old = now_ms() - WRITE_WINDOW_MS - 1;
        v.apply(&wrote("theirs", "priya", "src/billing.js", old));
        assert!(v.recent_writes.is_empty(), "stale writes must not accumulate");
    }

    /// Pre-`user` rows still name an author when presence can supply one.
    #[test]
    fn a_user_less_write_falls_back_to_presence() {
        let mut v = View::default();
        let t0 = now_ms();
        v.apply(&Event::SessionStarted {
            session: "theirs".into(),
            user: "priya".into(),
            branch: "main".into(),
            ts: t0,
        });
        v.apply(&wrote("theirs", "", "src/billing.js", t0 + 1));

        let out = v.writes_since("mine", t0);
        assert_eq!(out[0].user, "priya");
    }

    // ---------- file_written attribution ----------

    /// The event describes itself instead of requiring a join back through
    /// SessionStarted — the fragile path that once blamed the wrong session.
    #[test]
    fn file_written_carries_its_user() {
        let ev = Event::FileWritten {
            session: "s1".into(),
            user: "ash".into(),
            path: "src/auth.js".into(),
            ts: now_ms(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""user":"ash""#), "{json}");
    }

    /// Rows written before the field existed must still replay.
    #[test]
    fn file_written_without_a_user_still_deserializes() {
        let old = r#"{"type":"file_written","session":"s1","path":"src/auth.js","ts":1}"#;
        let ev: Event = serde_json::from_str(old).unwrap();
        match ev {
            Event::FileWritten { user, session, .. } => {
                assert_eq!(session, "s1");
                assert_eq!(user, "", "missing user defaults empty, not a parse failure");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn quiet_sessions_are_pruned_with_their_claims() {
        let mut v = View::default();
        let stale_ts = now_ms() - SESSION_STALE_MS - 1;
        v.sessions.insert(
            "ghost".into(),
            SessionInfo {
                session: "ghost".into(),
                user: "crashed".into(),
                branch: "main".into(),
                intent: "died".into(),
                last_seen: stale_ts,
            },
        );
        v.claims.push(claim("ghost", "src/auth.ts", now_ms() + LEASE_MS));
        v.prune();
        assert!(v.sessions.is_empty(), "a session gone quiet must stop showing as present");
        assert!(v.claims.is_empty(), "and must not keep holding files");
    }

    #[test]
    fn active_sessions_survive_pruning() {
        let mut v = View::default();
        v.apply(&Event::SessionStarted {
            session: "live".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: now_ms(),
        });
        v.prune();
        assert_eq!(v.sessions.len(), 1);
    }

    /// Replaying the same log in order must always yield the same view.
    #[test]
    fn log_replay_is_deterministic() {
        let t0 = now_ms(); // must be recent: stale sessions are pruned
        let log = vec![
            Event::SessionStarted { session: "s1".into(), user: "a".into(), branch: "m".into(), ts: t0 },
            Event::IntentDeclared { session: "s1".into(), text: "auth".into(), ts: t0 + 1 , branch: String::new()},
            Event::ClaimAcquired {
                session: "s1".into(), user: "a".into(), path: "src/auth.ts".into(),
                lease_until: now_ms() + LEASE_MS, intent: "auth".into(),
                branch: String::new(), ts: t0 + 1,
            },
            Event::SessionStarted { session: "s2".into(), user: "b".into(), branch: "m".into(), ts: t0 + 2 },
            Event::FileWritten { session: "s1".into(), user: "u".into(), path: "src/auth.ts".into(), ts: now_ms() },
            Event::ClaimReleased { session: "s1".into(), path: "src/auth.ts".into(), ts: t0 + 3 },
        ];
        let build = || {
            let mut v = View::default();
            for e in &log {
                v.apply(e);
            }
            (v.claims.len(), v.sessions.len())
        };
        assert_eq!(build(), build());
        assert_eq!(build(), (0, 2));
    }
}
