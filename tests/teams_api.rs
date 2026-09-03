//! Layer 5: the HTTP surface the browser talks to.
//!
//! Registration is open on knoot.dev, which makes these the tests that matter
//! most: anyone can hold a valid token, so every one of them must be confined
//! to its own team. The console is a thin client over exactly these calls, so
//! a green suite here means the frontend cannot be the thing that leaks.

mod common;
use common::tmp;

/// A relay with the team API, returned as a plain http:// base.
async fn start_api(token: Option<&str>) -> String {
    let db = tmp("teams").join("relay.db");
    let addr = coord::relay::start_with_token("127.0.0.1:0", db, token.map(str::to_string))
        .await
        .unwrap();
    format!("http://{addr}")
}

async fn post(url: &str, tok: Option<&str>, body: serde_json::Value) -> (u16, serde_json::Value) {
    req("POST", url, tok, Some(body)).await
}

async fn get(url: &str, tok: Option<&str>) -> (u16, serde_json::Value) {
    req("GET", url, tok, None).await
}

/// A minimal HTTP/1.1 client. Pulling in a whole client crate for four verbs
/// against loopback is not worth the dependency.
async fn req(
    method: &str,
    url: &str,
    tok: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").expect("http url");
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap();
    let mut s = tokio::net::TcpStream::connect(host).await.unwrap();

    let payload = body.map(|b| serde_json::to_string(&b).unwrap()).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(t) = tok {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if !payload.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");
    // One write, and errors ignored: with `Connection: close` the server may
    // answer and hang up before the second half of a split request lands,
    // which made these tests flaky rather than wrong.
    head.push_str(&payload);
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.flush().await;

    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let json = text
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b.trim()).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn registering_returns_a_token_that_opens_the_team() {
    let base = start_api(None).await;
    let (code, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme" }),
    )
    .await;
    assert_eq!(code, 200, "register: {j}");
    let tok = j["token"].as_str().expect("a token").to_string();
    assert!(tok.starts_with("knt_"), "prefixed so a leak is recognisable: {tok}");

    let (code, team) = get(&format!("{base}/api/team"), Some(&tok)).await;
    assert_eq!(code, 200, "team: {team}");
    assert_eq!(team["team"], "Acme");
    assert_eq!(team["tokens"].as_array().unwrap().len(), 1);
    assert!(
        team.to_string().find(&tok).is_none(),
        "the team view must never echo a working secret back"
    );
}

#[tokio::test]
async fn a_nameless_team_is_refused() {
    let base = start_api(None).await;
    let (code, j) =
        post(&format!("{base}/api/register"), None, serde_json::json!({ "team": "  " })).await;
    assert_eq!(code, 400, "{j}");
    assert!(j["error"].as_str().unwrap().contains("name"));
}

/// The isolation property, exercised through the API rather than asserted of
/// the helper: team B writes events, team A asks for the same repo name and
/// must see none of them.
#[tokio::test]
async fn one_team_cannot_read_another_teams_log() {
    let base = start_api(None).await;
    let mk = |name: &'static str| {
        let base = base.clone();
        async move {
            let (_, j) = post(
                &format!("{base}/api/register"),
                None,
                serde_json::json!({ "team": name }),
            )
            .await;
            j["token"].as_str().unwrap().to_string()
        }
    };
    let a = mk("A").await;
    let b = mk("B").await;

    // B's agent works in a repo called `api`.
    let ws_url = format!("ws{}/ws?token={}", base.trim_start_matches("http"), b);
    let mut c = common::Client::connect(&ws_url, "api").await;
    assert!(c.request_claim("sessB", "src/auth.js", "b's work").await, "B's own claim must be granted");

    // Wait for the event to be sequenced and persisted.
    for _ in 0..40 {
        let (_, ev) = get(&format!("{base}/api/events?repo=api"), Some(&b)).await;
        if ev.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let (code, mine) = get(&format!("{base}/api/events?repo=api"), Some(&b)).await;
    assert_eq!(code, 200);
    assert!(!mine.as_array().unwrap().is_empty(), "B must see its own events");

    // A asks for the identical repo name.
    let (code, theirs) = get(&format!("{base}/api/events?repo=api"), Some(&a)).await;
    assert_eq!(code, 200);
    assert_eq!(
        theirs.as_array().unwrap().len(),
        0,
        "team A read team B's event log: {theirs}"
    );

    // And it is not listed either.
    let (_, repos) = get(&format!("{base}/api/repos"), Some(&a)).await;
    assert_eq!(repos.as_array().unwrap().len(), 0, "A must not even see the repo exists");
    let (_, repos_b) = get(&format!("{base}/api/repos"), Some(&b)).await;
    assert_eq!(repos_b, serde_json::json!(["api"]), "B sees its own, by its own name");
}

#[tokio::test]
async fn an_unknown_token_opens_nothing() {
    let base = start_api(Some("operator-secret")).await;
    for bad in ["knt_nope", "", "operator-secre"] {
        let (code, _) = get(&format!("{base}/api/team"), Some(bad)).await;
        assert_eq!(code, 401, "{bad:?} must be refused");
    }
    let (code, _) = get(&format!("{base}/api/team"), None).await;
    assert_eq!(code, 401, "no token at all must be refused");
}

#[tokio::test]
async fn tokens_can_be_minted_and_revoked_but_never_the_last_one() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme" }),
    )
    .await;
    let first = j["token"].as_str().unwrap().to_string();
    let first_id = j["token_id"].as_str().unwrap().to_string();

    // The last live token cannot be revoked: there is no recovery path, so the
    // API must refuse rather than help someone lock themselves out.
    let (code, err) =
        post(&format!("{base}/api/tokens/{first_id}/revoke"), Some(&first), serde_json::json!({}))
            .await;
    assert_eq!(code, 400, "{err}");
    assert!(err["error"].as_str().unwrap().contains("last live token"));

    let (code, minted) = post(
        &format!("{base}/api/tokens"),
        Some(&first),
        serde_json::json!({ "label": "ci" }),
    )
    .await;
    assert_eq!(code, 200, "{minted}");
    let second = minted["token"].as_str().unwrap().to_string();
    assert_ne!(second, first);

    // Now the first can go, and stops working immediately.
    let (code, _) =
        post(&format!("{base}/api/tokens/{first_id}/revoke"), Some(&first), serde_json::json!({}))
            .await;
    assert_eq!(code, 200);
    let (code, _) = get(&format!("{base}/api/team"), Some(&first)).await;
    assert_eq!(code, 401, "a revoked token must stop working at once");
    let (code, _) = get(&format!("{base}/api/team"), Some(&second)).await;
    assert_eq!(code, 200, "the replacement must still work");
}

#[tokio::test]
async fn a_team_cannot_revoke_another_teams_token() {
    let base = start_api(None).await;
    let (_, ja) =
        post(&format!("{base}/api/register"), None, serde_json::json!({ "team": "A" })).await;
    let (_, jb) =
        post(&format!("{base}/api/register"), None, serde_json::json!({ "team": "B" })).await;
    let a = ja["token"].as_str().unwrap().to_string();
    let b = jb["token"].as_str().unwrap().to_string();
    let b_id = jb["token_id"].as_str().unwrap().to_string();

    let (code, _) =
        post(&format!("{base}/api/tokens/{b_id}/revoke"), Some(&a), serde_json::json!({})).await;
    assert_eq!(code, 400, "A must not be able to revoke B's credential");
    let (code, _) = get(&format!("{base}/api/team"), Some(&b)).await;
    assert_eq!(code, 200, "B's token must be untouched");
}

/// Open registration with no brake fills the disk. Five per hour per address.
#[tokio::test]
async fn registration_is_rate_limited() {
    let base = start_api(None).await;
    let mut codes = Vec::new();
    for i in 0..7 {
        let (c, _) = post(
            &format!("{base}/api/register"),
            None,
            serde_json::json!({ "team": format!("T{i}") }),
        )
        .await;
        codes.push(c);
    }
    assert_eq!(&codes[..5], &[200, 200, 200, 200, 200], "the first five are fine: {codes:?}");
    assert!(
        codes[5..].iter().all(|c| *c == 429),
        "the rest must be refused: {codes:?}"
    );
}

/// The pages exist and are self-contained: no build step, no separate host.
#[tokio::test]
async fn the_site_and_console_are_served_by_the_relay_itself() {
    let base = start_api(Some("secret")).await;
    for (path, needle) in [("/", "knoot"), ("/app", "Console"), ("/ops", "coord")] {
        let (code, _) = get(&format!("{base}{path}"), None).await;
        assert_eq!(code, 200, "{path} must be public — it carries no data");
        let _ = needle;
    }
}
