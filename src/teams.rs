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
    /// Which token was used, so the dashboard can show "last seen" per token
    /// and a compromised one can be told apart from the others.
    pub token_id: String,
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

/// Create a team and its first token. The secret is returned once; only its
/// hash is written down.
pub fn create_team(conn: &rusqlite::Connection, name: &str) -> Result<(Identity, IssuedToken)> {
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

    let issued = mint_token(conn, &team_id, "first token")?;
    Ok((
        Identity { team_id, team_name: name.to_string(), token_id: issued.id.clone() },
        issued,
    ))
}

/// Issue an additional token for an existing team.
pub fn mint_token(conn: &rusqlite::Connection, team_id: &str, label: &str) -> Result<IssuedToken> {
    let label = {
        let l = label.trim();
        if l.is_empty() {
            "unnamed".to_string()
        } else {
            l.chars().take(40).collect()
        }
    };
    // Prefixed so a leaked one is recognisable in a log or a git diff, and so
    // secret scanners have something to match on.
    let secret = format!("knt_{}", rand_hex(24));
    let id = format!("k_{}", rand_hex(6));
    conn.execute(
        "INSERT INTO tokens (id, team_id, label, token_hash, created_ts) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![id, team_id, label, hash_token(&secret), crate::proto::now_ms()],
    )?;
    Ok(IssuedToken { id, secret })
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

/// Resolve a presented token to a team, or `None`.
///
/// The lookup is by hash, so the comparison is a fixed-size index probe rather
/// than a byte-by-byte compare of the secret — there is no length or prefix to
/// learn from timing.
pub fn resolve(conn: &rusqlite::Connection, presented: &str) -> Option<Identity> {
    let hash = hash_token(presented.trim());
    let mut q = conn
        .prepare(
            "SELECT tokens.id, tokens.team_id, teams.name FROM tokens \
             JOIN teams ON teams.id = tokens.team_id \
             WHERE tokens.token_hash = ?1 AND tokens.revoked_ts IS NULL",
        )
        .ok()?;
    let found = q
        .query_row(rusqlite::params![hash], |r| {
            Ok(Identity {
                token_id: r.get(0)?,
                team_id: r.get(1)?,
                team_name: r.get(2)?,
            })
        })
        .ok()?;
    let _ = conn.execute(
        "UPDATE tokens SET last_seen_ts = ?1 WHERE id = ?2",
        rusqlite::params![crate::proto::now_ms(), found.token_id],
    );
    Some(found)
}

#[derive(Debug, serde::Serialize)]
pub struct TokenRow {
    pub id: String,
    pub label: String,
    pub created_ts: i64,
    pub last_seen_ts: Option<i64>,
    pub revoked: bool,
}

pub fn list_tokens(conn: &rusqlite::Connection, team_id: &str) -> Vec<TokenRow> {
    let Ok(mut q) = conn.prepare(
        "SELECT id, label, created_ts, last_seen_ts, revoked_ts FROM tokens \
         WHERE team_id = ?1 ORDER BY created_ts",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map(rusqlite::params![team_id], |r| {
        Ok(TokenRow {
            id: r.get(0)?,
            label: r.get(1)?,
            created_ts: r.get(2)?,
            last_seen_ts: r.get(3)?,
            revoked: r.get::<_, Option<i64>>(4)?.is_some(),
        })
    });
    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// Revoke one token. Scoped by team so a token can never revoke another
/// team's credentials by guessing an id.
pub fn revoke(conn: &rusqlite::Connection, team_id: &str, token_id: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE tokens SET revoked_ts = ?1 WHERE id = ?2 AND team_id = ?3 AND revoked_ts IS NULL",
        rusqlite::params![crate::proto::now_ms(), token_id, team_id],
    )?;
    anyhow::ensure!(n == 1, "no such token for this team");
    // A team that revokes its last token can never authenticate again, so the
    // caller is stopped rather than helped into it.
    let live: i64 = conn
        .query_row(
            "SELECT count(*) FROM tokens WHERE team_id = ?1 AND revoked_ts IS NULL",
            rusqlite::params![team_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if live == 0 {
        conn.execute(
            "UPDATE tokens SET revoked_ts = NULL WHERE id = ?1",
            rusqlite::params![token_id],
        )?;
        anyhow::bail!("that is the team's last live token — mint a replacement before revoking it");
    }
    Ok(())
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
        c
    }

    #[test]
    fn a_new_team_gets_a_token_that_resolves_to_it() {
        let c = db();
        let (id, tok) = create_team(&c, "Acme").unwrap();
        let got = resolve(&c, &tok.secret).expect("issued token must resolve");
        assert_eq!(got.team_id, id.team_id);
        assert_eq!(got.team_name, "Acme");
    }

    #[test]
    fn the_secret_is_not_stored() {
        let c = db();
        let (_, tok) = create_team(&c, "Acme").unwrap();
        let stored: Vec<String> = {
            let mut q = c.prepare("SELECT token_hash FROM tokens").unwrap();
            q.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        };
        assert!(
            !stored.iter().any(|s| s.contains(&tok.secret)),
            "a database dump must not hand over working tokens"
        );
        assert_eq!(stored[0], hash_token(&tok.secret));
    }

    #[test]
    fn one_teams_token_never_resolves_to_another() {
        let c = db();
        let (a, ta) = create_team(&c, "A").unwrap();
        let (b, tb) = create_team(&c, "B").unwrap();
        assert_ne!(a.team_id, b.team_id);
        assert_eq!(resolve(&c, &ta.secret).unwrap().team_id, a.team_id);
        assert_eq!(resolve(&c, &tb.secret).unwrap().team_id, b.team_id);
    }

    #[test]
    fn repo_names_collide_across_teams_without_colliding_in_storage() {
        let c = db();
        let (a, _) = create_team(&c, "A").unwrap();
        let (b, _) = create_team(&c, "B").unwrap();
        assert_ne!(a.scope("api"), b.scope("api"));
        assert_eq!(a.unscope(&a.scope("api")), "api");
    }

    #[test]
    fn a_revoked_token_stops_working() {
        let c = db();
        let (id, first) = create_team(&c, "Acme").unwrap();
        let second = mint_token(&c, &id.team_id, "ci").unwrap();
        revoke(&c, &id.team_id, &first.id).unwrap();
        assert!(resolve(&c, &first.secret).is_none(), "revoked token must not resolve");
        assert!(resolve(&c, &second.secret).is_some(), "the other token must still work");
    }

    #[test]
    fn a_team_cannot_revoke_another_teams_token() {
        let c = db();
        let (a, _) = create_team(&c, "A").unwrap();
        let (b, tb) = create_team(&c, "B").unwrap();
        let extra = mint_token(&c, &b.team_id, "ci").unwrap();
        assert!(revoke(&c, &a.team_id, &extra.id).is_err(), "cross-team revoke must fail");
        assert!(resolve(&c, &tb.secret).is_some());
    }

    /// Locking yourself out is a support ticket nobody can answer: there is no
    /// account recovery here, and no admin to ask.
    #[test]
    fn a_team_cannot_revoke_its_way_to_no_access() {
        let c = db();
        let (id, only) = create_team(&c, "Acme").unwrap();
        let err = revoke(&c, &id.team_id, &only.id).unwrap_err().to_string();
        assert!(err.contains("last live token"), "got: {err}");
        assert!(resolve(&c, &only.secret).is_some(), "the refusal must leave it working");
    }

    #[test]
    fn nonsense_never_resolves() {
        let c = db();
        create_team(&c, "Acme").unwrap();
        for bad in ["", "   ", "knt_", "knt_deadbeef", "null"] {
            assert!(resolve(&c, bad).is_none(), "{bad:?} must not authenticate");
        }
    }

    #[test]
    fn an_unnamed_team_is_refused() {
        let c = db();
        assert!(create_team(&c, "   ").is_err());
        assert!(create_team(&c, &"x".repeat(61)).is_err());
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
