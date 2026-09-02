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
    SessionEnded { session: String, ts: Ts },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { repo: String, daemon: String },
    Append { event: Event },
    ClaimReq { id: String, session: String, user: String, path: String, intent: String, #[serde(default)] branch: String },
    ReleaseSession { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { seq: u64, claims: Vec<Claim>, sessions: Vec<SessionInfo> },
    Event { seq: u64, event: Event },
    ClaimResp {
        id: String,
        granted: bool,
        holder: Option<String>,
        holder_user: Option<String>,
        holder_intent: Option<String>,
        lease_until: Option<Ts>,
    },
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
/// branch on either side compares equal: coord would rather block a write it
/// could have allowed than allow one it should have blocked.
pub fn same_branch(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
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
            Event::ClaimAcquired { session, user, path, lease_until, intent, branch } => {
                if let Some(s) = self.sessions.get_mut(session) {
                    s.last_seen = now_ms();
                }
                self.waiters.retain(|w| !(w.session == *session && w.path == *path));
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
    PreWrite { repo_root: String, session: String, path: String },
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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum DResp {
    Decision { allow: bool, reason: Option<String> },
    Mail { items: Vec<String> },
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
    },
    Ok,
    Err { msg: String },
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
                lease_until: now_ms() + LEASE_MS, intent: "auth".into(), branch: String::new(),
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
