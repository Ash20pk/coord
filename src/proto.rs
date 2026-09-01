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

/// Everything that happens is an event on the per-repo sequenced log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { session: String, user: String, branch: String, ts: Ts },
    IntentDeclared { session: String, text: String, ts: Ts },
    ClaimAcquired { session: String, user: String, path: String, lease_until: Ts, intent: String },
    ClaimReleased { session: String, path: String, ts: Ts },
    FileWritten { session: String, path: String, ts: Ts },
    SessionEnded { session: String, ts: Ts },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { repo: String, daemon: String },
    Append { event: Event },
    ClaimReq { id: String, session: String, user: String, path: String, intent: String },
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
}

impl View {
    pub fn prune(&mut self) {
        let now = now_ms();
        self.claims.retain(|c| c.lease_until > now);
    }

    /// First live claim held by a *different* session that overlaps `path`.
    pub fn conflicting(&self, session: &str, path: &str) -> Option<&Claim> {
        let now = now_ms();
        self.claims
            .iter()
            .find(|c| c.session != session && c.lease_until > now && paths_overlap(&c.path, path))
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
            Event::IntentDeclared { session, text, ts } => {
                if let Some(s) = self.sessions.get_mut(session) {
                    s.intent = text.clone();
                    s.last_seen = *ts;
                }
            }
            Event::ClaimAcquired { session, user, path, lease_until, intent } => {
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
                    });
                }
            }
            Event::ClaimReleased { session, path, .. } => {
                self.claims.retain(|c| !(c.session == *session && c.path == *path));
            }
            Event::FileWritten { session, path, ts } => {
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
    Intent { repo_root: String, session: String, text: String },
    SessionEnd { repo_root: String, session: String },
    Who { repo_root: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum DResp {
    Decision { allow: bool, reason: Option<String> },
    Peers { sessions: Vec<SessionInfo>, claims: Vec<Claim> },
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
            intent: "i".into(),
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
        });
        assert_eq!(v.sessions["s1"].intent, "refactor auth");
        assert_eq!(v.sessions["s1"].branch, "main");
    }

    #[test]
    fn intent_for_unknown_session_is_ignored() {
        let mut v = View::default();
        v.apply(&Event::IntentDeclared { session: "ghost".into(), text: "x".into(), ts: now_ms() });
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
        v.apply(&Event::FileWritten { session: "s1".into(), path: "src/auth/session.ts".into(), ts });

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

    /// Replaying the same log in order must always yield the same view.
    #[test]
    fn log_replay_is_deterministic() {
        let log = vec![
            Event::SessionStarted { session: "s1".into(), user: "a".into(), branch: "m".into(), ts: 1 },
            Event::IntentDeclared { session: "s1".into(), text: "auth".into(), ts: 2 },
            Event::ClaimAcquired {
                session: "s1".into(), user: "a".into(), path: "src/auth.ts".into(),
                lease_until: now_ms() + LEASE_MS, intent: "auth".into(),
            },
            Event::SessionStarted { session: "s2".into(), user: "b".into(), branch: "m".into(), ts: 3 },
            Event::FileWritten { session: "s1".into(), path: "src/auth.ts".into(), ts: now_ms() },
            Event::ClaimReleased { session: "s1".into(), path: "src/auth.ts".into(), ts: 4 },
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
