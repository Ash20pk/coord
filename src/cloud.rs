//! Supabase-backed identity for people.
//!
//! Two credentials reach this relay and they are not the same kind of thing.
//! A machine presents an agent token: minted here, stored as a hash, verified
//! against local SQLite with no network, because the hot path must keep
//! working when nothing else does. A person presents a Supabase access token
//! from the console, which this module exchanges for a user and a team.
//!
//! Identity lives in Supabase; the event log and token hashes stay on the
//! relay. That split is deliberate. A self-hosted relay with no Supabase
//! configuration behaves exactly as it did before: `from_env` returns `None`
//! and nothing here is ever called.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a verified access token is trusted without asking Supabase again.
/// Short enough that a deleted user loses access promptly, long enough that a
/// console refreshing its log does not make a call per request.
const TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Team {
    pub id: String,
    pub name: String,
}

#[derive(Clone)]
pub struct Cloud {
    url: String,
    anon_key: String,
    service_key: String,
    http: reqwest::Client,
    cache: std::sync::Arc<Mutex<HashMap<String, (Instant, Option<Team>)>>>,
}

impl Cloud {
    /// Built from the environment, or `None` when this relay is not attached
    /// to a Supabase project. All three variables are required: a URL with no
    /// service key could verify a person but never find their team, which
    /// would fail in a way that looks like a permissions bug.
    pub fn from_env() -> Option<Self> {
        let url = crate::config::env_or_legacy("SUPABASE_URL")?;
        let anon_key = crate::config::env_or_legacy("SUPABASE_ANON_KEY")?;
        let service_key = crate::config::env_or_legacy("SUPABASE_SERVICE_ROLE_KEY")?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        Some(Self {
            url: url.trim_end_matches('/').to_string(),
            anon_key,
            service_key,
            http,
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Same wiring, pointed at a server the test controls.
    #[cfg(test)]
    pub fn for_test(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            anon_key: "anon".into(),
            service_key: "service".into(),
            http: reqwest::Client::new(),
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A Supabase access token is a JWT: three base64url segments. Agent
    /// tokens never look like this, so the shape alone is enough to route a
    /// credential to the right verifier without trying both.
    pub fn looks_like_jwt(tok: &str) -> bool {
        let mut parts = tok.split('.');
        let (a, b, c, rest) = (parts.next(), parts.next(), parts.next(), parts.next());
        rest.is_none()
            && [a, b, c].iter().all(|p| {
                p.map(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
                    .unwrap_or(false)
            })
    }

    /// Verify an access token and resolve the team it speaks for.
    ///
    /// Returns `None` for a token Supabase rejects, and also for a valid user
    /// who belongs to no team — there is nothing for them to read yet, and
    /// inventing a team here would let the console diverge from the database
    /// that owns team membership.
    pub async fn team_for_token(&self, access_token: &str) -> Option<Team> {
        if let Some((at, team)) = self.cache.lock().unwrap().get(access_token) {
            if at.elapsed() < TTL {
                return team.clone();
            }
        }
        let team = self.lookup(access_token).await;
        self.cache
            .lock()
            .unwrap()
            .insert(access_token.to_string(), (Instant::now(), team.clone()));
        team
    }

    async fn lookup(&self, access_token: &str) -> Option<Team> {
        let user = self
            .http
            .get(format!("{}/auth/v1/user", self.url))
            .header("apikey", &self.anon_key)
            .bearer_auth(access_token)
            .send()
            .await
            .ok()?;
        if !user.status().is_success() {
            return None;
        }
        let user: serde_json::Value = user.json().await.ok()?;
        let user_id = user.get("id")?.as_str()?.to_string();

        // The service key is used only here, for a read keyed by the user id
        // we just proved. It never reaches the browser.
        let rows = self
            .http
            .get(format!(
                "{}/rest/v1/team_members?user_id=eq.{}&select=team_id,teams(name)&limit=1",
                self.url, user_id
            ))
            .header("apikey", &self.service_key)
            .bearer_auth(&self.service_key)
            .send()
            .await
            .ok()?;
        if !rows.status().is_success() {
            return None;
        }
        let rows: serde_json::Value = rows.json().await.ok()?;
        let row = rows.as_array()?.first()?;
        let id = row.get("team_id")?.as_str()?.to_string();
        let name = row
            .get("teams")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("team")
            .to_string();
        Some(Team { id, name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// A stand-in for the two Supabase endpoints this module calls, so the
    /// exchange is exercised for real rather than mocked at the seam.
    async fn stub() -> String {
        use axum::{routing::get, Router, Json};
        let app = Router::new()
            .route("/auth/v1/user", get(|headers: axum::http::HeaderMap| async move {
                let ok = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    == Some("Bearer good.token.sig");
                if ok {
                    Json(serde_json::json!({ "id": "user-1" })).into_response()
                } else {
                    (axum::http::StatusCode::UNAUTHORIZED, "no").into_response()
                }
            }))
            .route("/rest/v1/team_members", get(|uri: axum::http::Uri| async move {
                // The lookup must be keyed by the id we just proved, never by
                // anything the caller supplied.
                assert!(uri.query().unwrap().contains("user_id=eq.user-1"));
                Json(serde_json::json!([
                    { "team_id": "team-abc", "teams": { "name": "Platform team" } }
                ]))
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_valid_access_token_resolves_to_its_team() {
        let cloud = Cloud::for_test(&stub().await);
        let team = cloud.team_for_token("good.token.sig").await.expect("should resolve");
        assert_eq!(team.id, "team-abc");
        assert_eq!(team.name, "Platform team");
    }

    #[tokio::test]
    async fn a_token_supabase_rejects_authenticates_nothing() {
        let cloud = Cloud::for_test(&stub().await);
        assert!(cloud.team_for_token("stale.token.sig").await.is_none());
    }

    /// A refusal is cached like a success. Without that, a stale console tab
    /// retrying every few seconds becomes a request amplifier against the
    /// auth endpoint.
    #[tokio::test]
    async fn refusals_are_cached_too() {
        let cloud = Cloud::for_test("http://127.0.0.1:1");   // nothing listening
        assert!(cloud.team_for_token("good.token.sig").await.is_none());
        assert!(cloud.cache.lock().unwrap().contains_key("good.token.sig"));
    }

    #[test]
    fn jwt_shape_is_distinguishable_from_an_agent_token() {
        assert!(Cloud::looks_like_jwt("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln"));
        // Agent tokens are one hex run with a prefix; they must not be sent to
        // Supabase, or a revoked token would produce a confusing network error
        // instead of a clean refusal.
        assert!(!Cloud::looks_like_jwt("knt_2f6c9a1b3d4e5f60718293a4b5c6d7e8"));
        assert!(!Cloud::looks_like_jwt("a.b"));
        assert!(!Cloud::looks_like_jwt("a.b.c.d"));
        assert!(!Cloud::looks_like_jwt("a..c"));
        assert!(!Cloud::looks_like_jwt("a.b!.c"));
    }
}
