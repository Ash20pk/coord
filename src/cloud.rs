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
    /// Browser-safe key. `sb_publishable_…`, or a legacy `anon` JWT.
    publishable_key: String,
    /// Elevated key, server-side only. `sb_secret_…`, or a legacy
    /// `service_role` JWT.
    secret_key: String,
    http: reqwest::Client,
    cache: std::sync::Arc<Mutex<HashMap<String, (Instant, Option<Team>)>>>,
}

impl Cloud {
    /// Built from the environment, or `None` when this relay is not attached
    /// to a Supabase project. All three variables are required: a URL with no
    /// service key could verify a person but never find their team, which
    /// would fail in a way that looks like a permissions bug.
    /// The key names follow Supabase's current vocabulary, and the legacy
    /// names are still read so an existing deployment does not break the day
    /// it upgrades. Supabase is retiring `anon` and `service_role` at the end
    /// of 2026; both formats work until then, and this handles either.
    pub fn from_env() -> Option<Self> {
        let url = crate::config::env_or_legacy("SUPABASE_URL")?;
        let publishable_key = crate::config::env_or_legacy("SUPABASE_PUBLISHABLE_KEY")
            .or_else(|| crate::config::env_or_legacy("SUPABASE_ANON_KEY"))?;
        let secret_key = crate::config::env_or_legacy("SUPABASE_SECRET_KEY")
            .or_else(|| crate::config::env_or_legacy("SUPABASE_SERVICE_ROLE_KEY"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        Some(Self {
            url: url.trim_end_matches('/').to_string(),
            publishable_key,
            secret_key,
            http,
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Same wiring, pointed at a server the test controls.
    #[cfg(test)]
    pub fn for_test(url: &str, secret_key: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            publishable_key: "sb_publishable_test".into(),
            secret_key: secret_key.into(),
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
            .header("apikey", &self.publishable_key)
            // Here the bearer token is the person's access token, which is a
            // real JWT. The API key rides in `apikey`, which is correct for
            // both key formats.
            .bearer_auth(access_token)
            .send()
            .await
            .ok()?;
        if !user.status().is_success() {
            return None;
        }
        let user: serde_json::Value = user.json().await.ok()?;
        let user_id = user.get("id")?.as_str()?.to_string();

        // The secret key is used only here, for a read keyed by the user id we
        // just proved. It never reaches the browser.
        let req = self
            .http
            .get(format!(
                "{}/rest/v1/team_members?user_id=eq.{}&select=team_id,teams(name)&limit=1",
                self.url, user_id
            ))
            .header("apikey", &self.secret_key);
        // A `sb_secret_…` key is not a JWT. Sent as a bearer token it is
        // parsed as one and the request is refused as an invalid JWT, so the
        // header is added only for a legacy `service_role` key, which is a JWT
        // and whose role Postgres reads out of it.
        let req = if Self::looks_like_jwt(&self.secret_key) {
            req.bearer_auth(&self.secret_key)
        } else {
            req
        };
        let rows = req.send().await.ok()?;
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
    /// Records the `Authorization` header PostgREST was sent, so the test can
    /// assert on what actually went over the wire.
    type SeenAuth = std::sync::Arc<Mutex<Option<Option<String>>>>;

    async fn stub() -> String {
        stub_recording(Default::default()).await
    }

    async fn stub_recording(seen: SeenAuth) -> String {
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
            .route("/rest/v1/team_members", get(move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    // The lookup must be keyed by the id we just proved, never
                    // by anything the caller supplied.
                    assert!(uri.query().unwrap().contains("user_id=eq.user-1"));
                    *seen.lock().unwrap() = Some(
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .map(|v| v.to_str().unwrap().to_string()),
                    );
                    Json(serde_json::json!([
                        { "team_id": "team-abc", "teams": { "name": "Platform team" } }
                    ]))
                }
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Supabase's current keys (`sb_secret_…`) are not JWTs. Sent as a bearer
    /// token they are parsed as one and the request is refused as an invalid
    /// JWT, which would break every team lookup on a project that has migrated
    /// off the legacy keys.
    #[tokio::test]
    async fn a_new_format_secret_key_is_never_sent_as_a_bearer_token() {
        let seen: SeenAuth = Default::default();
        let cloud = Cloud::for_test(&stub_recording(seen.clone()).await, "sb_secret_abcdef123456");
        cloud.team_for_token("good.token.sig").await.expect("should resolve");
        assert_eq!(
            *seen.lock().unwrap(),
            Some(None),
            "the secret key belongs in the apikey header only"
        );
    }

    /// A legacy `service_role` key is a JWT and Postgres reads the role out of
    /// it, so that one still needs the bearer header.
    #[tokio::test]
    async fn a_legacy_service_role_key_still_gets_the_bearer_header() {
        let seen: SeenAuth = Default::default();
        let legacy = "eyJhbGciOiJIUzI1NiJ9.eyJyb2xlIjoic2VydmljZV9yb2xlIn0.sig";
        let cloud = Cloud::for_test(&stub_recording(seen.clone()).await, legacy);
        cloud.team_for_token("good.token.sig").await.expect("should resolve");
        assert_eq!(*seen.lock().unwrap(), Some(Some(format!("Bearer {legacy}"))));
    }

    #[tokio::test]
    async fn a_valid_access_token_resolves_to_its_team() {
        let cloud = Cloud::for_test(&stub().await, "sb_secret_abcdef123456");
        let team = cloud.team_for_token("good.token.sig").await.expect("should resolve");
        assert_eq!(team.id, "team-abc");
        assert_eq!(team.name, "Platform team");
    }

    #[tokio::test]
    async fn a_token_supabase_rejects_authenticates_nothing() {
        let cloud = Cloud::for_test(&stub().await, "sb_secret_abcdef123456");
        assert!(cloud.team_for_token("stale.token.sig").await.is_none());
    }

    /// A refusal is cached like a success. Without that, a stale console tab
    /// retrying every few seconds becomes a request amplifier against the
    /// auth endpoint.
    #[tokio::test]
    async fn refusals_are_cached_too() {
        let cloud = Cloud::for_test("http://127.0.0.1:1", "sb_secret_abcdef123456");   // nothing listening
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
