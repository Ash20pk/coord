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
