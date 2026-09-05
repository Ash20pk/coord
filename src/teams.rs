//! Teams and tokens: who a presented token speaks for.
//!
//! A relay with one shared secret can serve one team. Serving many needs three
//! things this module provides: a token that names a team, a stored form of
//! that token which is useless if the database leaks, and a repo namespace per
//! team so one team's event log cannot be read by another's.
//!
//! Registration is open by design — anyone can create a team — so the only
//! defences that matter here are the ones that hold against a stranger: hashed
//! tokens, per-IP rate limiting, and no personal data asked for or stored.
//!
//! A presented key names a *device*, and a device names a *member*: see
//! `rooms.rs` for those and for the rooms that decide which areas a key may
//! enter. This module keeps the two questions that must be answerable with no
//! network at all — which team, and is this secret live.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// Who a request speaks for. Every authenticated surface resolves to one of
/// these, and every repo key is namespaced by `team_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub team_id: String,
    pub team_name: String,
    /// Which device key was used, so the console can show "last seen" per
    /// machine and a compromised one can be told apart from the others.
    pub token_id: String,
    /// Verified: this key was minted for this person. Authorship on every
    /// event comes from here and not from what the client says about itself.
    pub member: crate::rooms::Member,
    /// The `(repo, area)` pairs this identity may enter — the union of the
    /// member's rooms. Resolved once per request, not per path.
    pub areas: Vec<crate::rooms::Area>,
}

impl Identity {
    /// Repo keys are namespaced by team. Two teams may both have a repo called
    /// `api` and must never see each other's events; keying by the pair is
    /// what makes that structural rather than a filter someone can forget.
    pub fn scope(&self, repo: &str) -> String {
        format!("{}/{}", self.team_id, repo)
    }

    /// The team-local repo id, for display. Inverse of `scope`.
    pub fn unscope<'a>(&self, key: &'a str) -> &'a str {
        key.strip_prefix(&format!("{}/", self.team_id)).unwrap_or(key)
    }

    /// May this identity work in `area` of `repo`? Until areas are declared
    /// (phase 3) every caller asks about `/`, which the `general` room grants,
    /// so this is true for everyone who is in a room at all — and false for a
    /// member who has been taken out of every room, which is the point.
    pub fn may_enter(&self, repo: &str, area: &str) -> bool {
        self.areas.iter().any(|a| a.covers(repo, area))
    }
}

/// A token as issued: shown once, never stored.
pub struct IssuedToken {
    pub id: String,
    pub secret: String,
}

pub fn hash_token(tok: &str) -> String {
    let mut h = Sha256::new();
    h.update(tok.as_bytes());
    format!("{:x}", h.finalize())
}

fn rand_hex(bytes: usize) -> String {
    // uuid v4 is already a dependency and is a CSPRNG source; two of them give
    // 256 bits, which is more than a bearer token needs.
    let mut s = String::new();
    while s.len() < bytes * 2 {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(bytes * 2);
    s
}

pub fn init_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS teams (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            created_ts INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tokens (
            id TEXT PRIMARY KEY,
            team_id TEXT NOT NULL,
            label TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_ts INTEGER NOT NULL,
            last_seen_ts INTEGER,
            revoked_ts INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tokens_hash ON tokens (token_hash);
        CREATE INDEX IF NOT EXISTS idx_tokens_team ON tokens (team_id);",
    )?;
    Ok(())
}

/// Create a team, its first member, its `general` room and that member's
/// first device key. The secret is returned once; only its hash is written
/// down.
///
/// `email` is the person registering, when anything knows who that is. Open
/// registration does not, so the owner starts as an unassigned member that an
/// admin — or the first console sign-in — attaches to a real address.
pub fn create_team(
    conn: &rusqlite::Connection,
    name: &str,
    email: Option<&str>,
) -> Result<(Identity, IssuedToken)> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a team needs a name");
    anyhow::ensure!(name.chars().count() <= 60, "team name is too long (60 max)");

    let team_id = format!("t_{}", rand_hex(8));
    let ts = crate::proto::now_ms();
    conn.execute(
        "INSERT INTO teams (id, name, created_ts) VALUES (?1, ?2, ?3)",
        rusqlite::params![team_id, name, ts],
    )
    .context("could not create team")?;

    let member = match email {
        Some(e) => crate::rooms::ensure_member(conn, &team_id, e, None, "owner")?,
        None => crate::rooms::placeholder_owner(conn, &team_id)?,
    };
    let issued = crate::rooms::mint_device(conn, &team_id, &member.id, "first key")?;
    let areas = crate::rooms::areas_for_member(conn, &member.id);
    Ok((
        Identity {
            team_id,
            team_name: name.to_string(),
            token_id: issued.id.clone(),
            member,
            areas,
        },
        issued,
    ))
}

/// A team as it was created before members existed: a `tokens` row and
/// nothing else. Only the migration tests need to build one.
#[cfg(test)]
pub fn create_team_legacy(conn: &rusqlite::Connection, name: &str) -> Result<(Identity, IssuedToken)> {
    let team_id = format!("t_{}", rand_hex(8));
    conn.execute(
        "INSERT INTO teams (id, name, created_ts) VALUES (?1, ?2, ?3)",
        rusqlite::params![team_id, name, crate::proto::now_ms()],
    )?;
    let secret = format!("knt_{}", rand_hex(24));
    let id = format!("k_{}", rand_hex(6));
    conn.execute(
        "INSERT INTO tokens (id, team_id, label, token_hash, created_ts) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![id, team_id, "first token", hash_token(&secret), crate::proto::now_ms()],
    )?;
    Ok((
        Identity {
            team_id,
            team_name: name.to_string(),
            token_id: id.clone(),
            member: crate::rooms::Member::legacy("legacy"),
            areas: Vec::new(),
        },
        IssuedToken { id, secret },
    ))
}

/// Record a team that was authenticated elsewhere, so local rows can point at
/// it. Identity lives in Supabase; this is the relay's own copy of the name,
/// which `resolve` joins against when listing a team's tokens.
pub fn ensure_team(conn: &rusqlite::Connection, team_id: &str, name: &str) {
    let _ = conn.execute(
        "INSERT INTO teams (id, name, created_ts) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        rusqlite::params![team_id, name, crate::proto::now_ms()],
    );
}

/// Resolve a presented key to a team, a member and their areas, or `None`.
///
/// The lookup is by hash, so the comparison is a fixed-size index probe rather
/// than a byte-by-byte compare of the secret — there is no length or prefix to
/// learn from timing.
pub fn resolve(conn: &rusqlite::Connection, presented: &str) -> Option<Identity> {
    let hash = hash_token(presented.trim());
    let mut q = conn
        .prepare(
            "SELECT devices.id, devices.team_id, teams.name, \
                    members.id, members.email, members.role, members.unassigned \
             FROM devices \
             JOIN teams   ON teams.id = devices.team_id \
             JOIN members ON members.id = devices.member_id \
             WHERE devices.token_hash = ?1 AND devices.revoked_ts IS NULL",
        )
        .ok()?;
    let (found, member_id) = q
        .query_row(rusqlite::params![hash], |r| {
            let member = crate::rooms::Member {
                id: r.get(3)?,
                email: r.get(4)?,
                role: r.get(5)?,
                unassigned: r.get::<_, i64>(6)? != 0,
            };
            let id = member.id.clone();
            Ok((
                Identity {
                    token_id: r.get(0)?,
                    team_id: r.get(1)?,
                    team_name: r.get(2)?,
                    member,
                    areas: Vec::new(),
                },
                id,
            ))
        })
        .ok()?;
    let _ = conn.execute(
        "UPDATE devices SET last_seen_ts = ?1 WHERE id = ?2",
        rusqlite::params![crate::proto::now_ms(), found.token_id],
    );
    Some(Identity { areas: crate::rooms::areas_for_member(conn, &member_id), ..found })
}

/// Per-IP registration limiter. Open registration on a public relay is an
/// invitation to fill the disk; this makes it a slow one.
pub struct RateLimit {
    hits: Mutex<HashMap<String, Vec<u64>>>,
    max: usize,
    window_ms: u64,
}

impl RateLimit {
    pub fn new(max: usize, window_ms: u64) -> Self {
        Self { hits: Mutex::new(HashMap::new()), max, window_ms }
    }

    /// True when the caller may proceed. Records the attempt either way, so a
    /// caller cannot retry its way out of the window.
    pub fn check(&self, key: &str) -> bool {
        let now = crate::proto::now_ms();
        let mut hits = self.hits.lock().unwrap();
        // Opportunistic sweep: without it a bot cycling source addresses grows
        // this map forever.
        if hits.len() > 10_000 {
            hits.retain(|_, v| v.iter().any(|t| now.saturating_sub(*t) < self.window_ms));
        }
        let v = hits.entry(key.to_string()).or_default();
        v.retain(|t| now.saturating_sub(*t) < self.window_ms);
        v.push(now);
        v.len() <= self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        crate::rooms::init_schema(&c).unwrap();
        c
    }

    fn team(c: &rusqlite::Connection, name: &str) -> (Identity, IssuedToken) {
        create_team(c, name, Some(&format!("{}@example.com", name.to_lowercase()))).unwrap()
    }

    #[test]
    fn a_new_team_gets_a_key_that_resolves_to_it() {
        let c = db();
        let (id, tok) = team(&c, "Acme");
        let got = resolve(&c, &tok.secret).expect("issued key must resolve");
        assert_eq!(got.team_id, id.team_id);
        assert_eq!(got.team_name, "Acme");
    }

    /// The whole reason devices exist: a key names a person, so authorship and
    /// provenance do not have to trust what the client says about itself.
    #[test]
    fn a_key_names_the_person_it_was_minted_for() {
        let c = db();
        let (_, tok) = team(&c, "Acme");
        let got = resolve(&c, &tok.secret).unwrap();
        assert_eq!(got.member.email, "acme@example.com");
        assert_eq!(got.member.role, "owner");
        assert!(!got.member.unassigned);
        assert!(!got.token_id.is_empty(), "and the machine it was minted for");
    }

    #[test]
    fn a_key_carries_the_areas_its_rooms_grant() {
        let c = db();
        let (_, tok) = team(&c, "Acme");
        let got = resolve(&c, &tok.secret).unwrap();
        assert!(got.may_enter("api", "/"), "the general room covers the whole repo");
        assert!(got.may_enter("anything", "src/auth"));
    }

    /// Open registration knows no email, and inventing one would be a lie on
    /// every event this key ever writes.
    #[test]
    fn a_team_registered_with_no_email_has_an_unassigned_owner() {
        let c = db();
        let (_, tok) = create_team(&c, "Acme", None).unwrap();
        let got = resolve(&c, &tok.secret).unwrap();
        assert!(got.member.unassigned);
        assert!(got.member.email.ends_with("@unassigned.invalid"));
    }

    #[test]
    fn the_secret_is_not_stored() {
        let c = db();
        let (_, tok) = team(&c, "Acme");
        let stored: Vec<String> = {
            let mut q = c.prepare("SELECT token_hash FROM devices").unwrap();
            q.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        };
        assert!(
            !stored.iter().any(|s| s.contains(&tok.secret)),
            "a database dump must not hand over working keys"
        );
        assert_eq!(stored[0], hash_token(&tok.secret));
    }

    #[test]
    fn one_teams_key_never_resolves_to_another() {
        let c = db();
        let (a, ta) = team(&c, "A");
        let (b, tb) = team(&c, "B");
        assert_ne!(a.team_id, b.team_id);
        assert_eq!(resolve(&c, &ta.secret).unwrap().team_id, a.team_id);
        assert_eq!(resolve(&c, &tb.secret).unwrap().team_id, b.team_id);
    }

    #[test]
    fn repo_names_collide_across_teams_without_colliding_in_storage() {
        let c = db();
        let (a, _) = team(&c, "A");
        let (b, _) = team(&c, "B");
        assert_ne!(a.scope("api"), b.scope("api"));
        assert_eq!(a.unscope(&a.scope("api")), "api");
    }

    #[test]
    fn a_revoked_key_stops_working() {
        let c = db();
        let (id, first) = team(&c, "Acme");
        let second =
            crate::rooms::mint_device(&c, &id.team_id, &id.member.id, "desktop").unwrap();
        crate::rooms::revoke_device(&c, &id.team_id, &first.id).unwrap();
        assert!(resolve(&c, &first.secret).is_none(), "revoked key must not resolve");
        assert!(resolve(&c, &second.secret).is_some(), "the other machine still works");
    }

    #[test]
    fn a_team_cannot_revoke_another_teams_key() {
        let c = db();
        let (a, _) = team(&c, "A");
        let (b, tb) = team(&c, "B");
        let extra = crate::rooms::mint_device(&c, &b.team_id, &b.member.id, "ci").unwrap();
        assert!(
            crate::rooms::revoke_device(&c, &a.team_id, &extra.id).is_err(),
            "cross-team revoke must fail"
        );
        assert!(resolve(&c, &tb.secret).is_some());
    }

    /// Locking yourself out is a support ticket nobody can answer: there is no
    /// account recovery here, and no admin to ask.
    #[test]
    fn a_team_cannot_revoke_its_way_to_no_access() {
        let c = db();
        let (id, only) = team(&c, "Acme");
        let err = crate::rooms::revoke_device(&c, &id.team_id, &only.id).unwrap_err().to_string();
        assert!(err.contains("last live device key"), "got: {err}");
        assert!(resolve(&c, &only.secret).is_some(), "the refusal must leave it working");
    }

    #[test]
    fn nonsense_never_resolves() {
        let c = db();
        team(&c, "Acme");
        for bad in ["", "   ", "knt_", "knt_deadbeef", "null"] {
            assert!(resolve(&c, bad).is_none(), "{bad:?} must not authenticate");
        }
    }

    #[test]
    fn an_unnamed_team_is_refused() {
        let c = db();
        assert!(create_team(&c, "   ", None).is_err());
        assert!(create_team(&c, &"x".repeat(61), None).is_err());
    }

    #[test]
    fn registration_is_rate_limited_per_key() {
        let rl = RateLimit::new(3, 60_000);
        for i in 1..=3 {
            assert!(rl.check("1.2.3.4"), "attempt {i} should pass");
        }
        assert!(!rl.check("1.2.3.4"), "the fourth in the window must be refused");
        assert!(rl.check("5.6.7.8"), "a different caller is unaffected");
    }
}
