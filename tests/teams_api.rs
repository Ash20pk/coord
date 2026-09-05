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
    let addr = knoot::relay::start_with_token("127.0.0.1:0", db, token.map(str::to_string))
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
    assert!(err["error"].as_str().unwrap().contains("last live device key"));

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
    for (path, needle) in [("/", "knoot"), ("/app", "Console"), ("/ops", "knoot")] {
        let (code, _) = get(&format!("{base}{path}"), None).await;
        assert_eq!(code, 200, "{path} must be public — it carries no data");
        let _ = needle;
    }
}

/// A registered team is a team of one until someone joins, and that one is
/// the owner of a `general` room over every repo. Nothing downstream should
/// ever meet an identity with no member and no areas.
#[tokio::test]
async fn a_registered_team_has_an_owner_in_a_general_room() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let key = j["token"].as_str().unwrap().to_string();

    let (code, who) = get(&format!("{base}/api/whoami"), Some(&key)).await;
    assert_eq!(code, 200, "{who}");
    assert_eq!(who["me"]["email"], "ash@example.com");
    assert_eq!(who["me"]["role"], "owner");
    assert_eq!(who["rooms"], serde_json::json!(["general"]));
    assert_eq!(who["me"]["areas"], serde_json::json!([{ "repo": "*", "area": "/" }]));
}

/// The phase 1 exit criterion, over the API: a second person joins, gets their
/// own key, and is removed again without anybody else's key changing.
#[tokio::test]
async fn a_second_person_can_join_and_be_removed_without_touching_other_keys() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let owner_key = j["token"].as_str().unwrap().to_string();

    // Priya is put in a room, which is what records her as a member.
    let (code, room) = post(
        &format!("{base}/api/rooms"),
        Some(&owner_key),
        serde_json::json!({ "name": "platform" }),
    )
    .await;
    assert_eq!(code, 200, "{room}");
    let room_id = room["room"].as_str().unwrap().to_string();

    // An invited person exists as a member before they hold a key: the console
    // creates the member, then mints the device the person actually installs.
    let (code, team) = get(&format!("{base}/api/team"), Some(&owner_key)).await;
    assert_eq!(code, 200, "{team}");
    let owner_id = team["me"]["member_id"].as_str().unwrap().to_string();
    let (code, _) = post(
        &format!("{base}/api/rooms/{room_id}/members"),
        Some(&owner_key),
        serde_json::json!({ "member": owner_id, "role": "owner" }),
    )
    .await;
    assert_eq!(code, 200);

    // Priya's key. She has no console sign-in here, so the owner mints for a
    // member the owner created — the self-hosted path, with no Supabase.
    let (code, minted) = post(
        &format!("{base}/api/tokens"),
        Some(&owner_key),
        serde_json::json!({ "label": "priya laptop" }),
    )
    .await;
    assert_eq!(code, 200, "{minted}");
    let second_key = minted["token"].as_str().unwrap().to_string();

    // Both keys work, and both name the same verified person so far.
    for k in [&owner_key, &second_key] {
        let (code, who) = get(&format!("{base}/api/whoami"), Some(k)).await;
        assert_eq!(code, 200, "{who}");
        assert_eq!(who["me"]["email"], "ash@example.com");
    }

    // Revoking one machine leaves the other alone. This is the thing a
    // team-wide bearer token could never do.
    let device = minted["token_id"].as_str().unwrap().to_string();
    let (code, _) =
        post(&format!("{base}/api/tokens/{device}/revoke"), Some(&owner_key), serde_json::json!({}))
            .await;
    assert_eq!(code, 200);
    let (code, _) = get(&format!("{base}/api/whoami"), Some(&second_key)).await;
    assert_eq!(code, 401, "the revoked machine is out");
    let (code, _) = get(&format!("{base}/api/whoami"), Some(&owner_key)).await;
    assert_eq!(code, 200, "and nobody else noticed");
}

/// Rooms are per team on every write, not just on read: an id from another
/// team is not an authorisation bug waiting for someone to guess it.
#[tokio::test]
async fn one_teams_room_cannot_be_edited_by_another_over_the_api() {
    let base = start_api(None).await;
    let (_, ja) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "A", "email": "a@example.com" }),
    )
    .await;
    let (_, jb) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "B", "email": "b@example.com" }),
    )
    .await;
    let a = ja["token"].as_str().unwrap().to_string();
    let b = jb["token"].as_str().unwrap().to_string();

    let (_, room) =
        post(&format!("{base}/api/rooms"), Some(&b), serde_json::json!({ "name": "payments" })).await;
    let room_id = room["room"].as_str().unwrap().to_string();

    for (path, body) in [
        (format!("api/rooms/{room_id}/areas"), serde_json::json!({ "repo": "api", "area": "src" })),
        (format!("api/rooms/{room_id}/delete"), serde_json::json!({})),
        (format!("api/rooms/{room_id}/policy"), serde_json::json!({ "facts": { "enabled": false } })),
    ] {
        let (code, err) = post(&format!("{base}/{path}"), Some(&a), body).await;
        assert_eq!(code, 400, "{path} must be refused: {err}");
    }

    let (code, team) = get(&format!("{base}/api/team"), Some(&b)).await;
    assert_eq!(code, 200, "{team}");
    let names: Vec<&str> =
        team["rooms"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"payments"), "B's room is untouched: {names:?}");
}

/// A room grants exactly the areas it holds. Nothing enforces areas on the log
/// yet — that is phase 3 — but the grant a key carries is what phase 3 will
/// read, so it has to be right now.
#[tokio::test]
async fn a_room_grants_the_areas_an_admin_gave_it() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let key = j["token"].as_str().unwrap().to_string();
    let (_, room) =
        post(&format!("{base}/api/rooms"), Some(&key), serde_json::json!({ "name": "auth" })).await;
    let room_id = room["room"].as_str().unwrap().to_string();
    let (_, team) = get(&format!("{base}/api/team"), Some(&key)).await;
    let me = team["me"]["member_id"].as_str().unwrap().to_string();

    let (code, _) = post(
        &format!("{base}/api/rooms/{room_id}/areas"),
        Some(&key),
        serde_json::json!({ "repo": "api", "area": "src/auth" }),
    )
    .await;
    assert_eq!(code, 200);
    let (code, _) = post(
        &format!("{base}/api/rooms/{room_id}/members"),
        Some(&key),
        serde_json::json!({ "member": me }),
    )
    .await;
    assert_eq!(code, 200);

    let (_, who) = get(&format!("{base}/api/whoami"), Some(&key)).await;
    let areas = who["me"]["areas"].as_array().unwrap();
    assert!(
        areas.contains(&serde_json::json!({ "repo": "api", "area": "src/auth" })),
        "the new grant is missing: {areas:?}"
    );
    let rooms: Vec<&str> = who["rooms"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(rooms, vec!["auth", "general"]);
}

/// The general room is what makes "a key always resolves to some area" true.
#[tokio::test]
async fn the_general_room_cannot_be_deleted_over_the_api() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let key = j["token"].as_str().unwrap().to_string();
    let (_, team) = get(&format!("{base}/api/team"), Some(&key)).await;
    let general = team["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "general")
        .expect("every team has one")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, err) =
        post(&format!("{base}/api/rooms/{general}/delete"), Some(&key), serde_json::json!({})).await;
    assert_eq!(code, 400, "{err}");
    assert!(err["error"].as_str().unwrap().contains("cannot be deleted"));
}

/// A relay whose credential lives in its environment has no database rows to
/// edit, and the refusal has to say so rather than 500 or silently no-op.
#[tokio::test]
async fn an_environment_credential_cannot_manage_members_or_rooms() {
    let base = start_api(Some("operator-secret")).await;
    let s = Some("operator-secret");
    for (path, body) in [
        ("api/rooms", serde_json::json!({ "name": "platform" })),
        ("api/tokens", serde_json::json!({ "label": "ci" })),
        ("api/members/attach", serde_json::json!({ "from": "m_nope" })),
    ] {
        let (code, err) = post(&format!("{base}/{path}"), s, body).await;
        assert_eq!(code, 400, "{path}: {err}");
        assert!(
            err["error"].as_str().unwrap().contains("configured in its environment"),
            "{path} must explain itself: {err}"
        );
    }
    // But it still authenticates, and still reports a usable identity.
    let (code, who) = get(&format!("{base}/api/whoami"), s).await;
    assert_eq!(code, 200, "{who}");
    assert_eq!(who["team_id"], "root");
    assert_eq!(who["me"]["areas"], serde_json::json!([{ "repo": "*", "area": "/" }]));
}

/// The gap this closes: until now a second *person* could only come into being
/// through Supabase, so a self-hosted relay could mint any number of keys and
/// every one of them named the same human. Rooms, areas and memory provenance
/// are all about *who*.
#[tokio::test]
async fn a_self_hosted_relay_can_add_a_second_person_and_key_them() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let owner_key = j["token"].as_str().unwrap().to_string();

    // A colleague, and the key to give them, in one call — there is no email
    // to send an invitation to on a relay with no cloud behind it.
    let (code, priya) = post(
        &format!("{base}/api/members"),
        Some(&owner_key),
        serde_json::json!({ "email": "Priya@Example.com", "label": "priya laptop" }),
    )
    .await;
    assert_eq!(code, 200, "{priya}");
    assert_eq!(priya["email"], "priya@example.com", "email is normalised");
    assert_eq!(priya["role"], "member");
    let priya_key = priya["token"].as_str().expect("a key comes back once").to_string();

    // The key is hers, not the owner's. This is the whole point: an event's
    // author and a shard's provenance now name two different people.
    let (code, who) = get(&format!("{base}/api/whoami"), Some(&priya_key)).await;
    assert_eq!(code, 200, "{who}");
    assert_eq!(who["me"]["email"], "priya@example.com");
    assert_eq!(who["me"]["member_id"], priya["member"]);
    assert!(priya["member"].as_str().is_some_and(|m| !m.is_empty()));

    // And the owner is still the owner.
    let (_, who) = get(&format!("{base}/api/whoami"), Some(&owner_key)).await;
    assert_eq!(who["me"]["email"], "ash@example.com");
    assert_eq!(who["me"]["role"], "owner");
}

/// Adding somebody who is already here must not quietly rewrite their role.
/// `ensure_member` would, which would make "add ash@example.com as a member"
/// a way to demote the owner.
#[tokio::test]
async fn adding_an_existing_member_changes_nothing_about_them() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let owner_key = j["token"].as_str().unwrap().to_string();

    let (code, again) = post(
        &format!("{base}/api/members"),
        Some(&owner_key),
        serde_json::json!({ "email": "ash@example.com", "role": "member", "label": "sneaky" }),
    )
    .await;
    assert_eq!(code, 200, "{again}");
    assert_eq!(again["existing"], true);
    assert_eq!(again["role"], "owner", "the owner is still the owner");
    assert!(again["token"].is_null(), "and no key was minted behind their back");

    let (_, who) = get(&format!("{base}/api/whoami"), Some(&owner_key)).await;
    assert_eq!(who["me"]["role"], "owner");
}

/// Creating a member hands out access, so it is an admin call — and the roles
/// it may hand out stop short of the one that owns the team.
#[tokio::test]
async fn only_an_admin_may_add_a_member_and_never_a_second_owner() {
    let base = start_api(None).await;
    let (_, j) = post(
        &format!("{base}/api/register"),
        None,
        serde_json::json!({ "team": "Acme", "email": "ash@example.com" }),
    )
    .await;
    let owner_key = j["token"].as_str().unwrap().to_string();

    // A plain member's key.
    let (_, priya) = post(
        &format!("{base}/api/members"),
        Some(&owner_key),
        serde_json::json!({ "email": "priya@example.com", "label": "laptop" }),
    )
    .await;
    let priya_key = priya["token"].as_str().unwrap().to_string();

    let (code, e) = post(
        &format!("{base}/api/members"),
        Some(&priya_key),
        serde_json::json!({ "email": "outsider@example.com" }),
    )
    .await;
    assert_eq!(code, 403, "a member may not widen the team: {e}");

    // An admin may, and an admin may make another admin.
    let (code, sam) = post(
        &format!("{base}/api/members"),
        Some(&owner_key),
        serde_json::json!({ "email": "sam@example.com", "role": "admin", "label": "desktop" }),
    )
    .await;
    assert_eq!(code, 200, "{sam}");
    let sam_key = sam["token"].as_str().unwrap().to_string();
    let (code, _) = post(
        &format!("{base}/api/members"),
        Some(&sam_key),
        serde_json::json!({ "email": "kim@example.com" }),
    )
    .await;
    assert_eq!(code, 200, "an admin can add people too");

    // But not a second owner, and not something that is not a role at all.
    for role in ["owner", "root", ""] {
        let (code, _) = post(
            &format!("{base}/api/members"),
            Some(&owner_key),
            serde_json::json!({ "email": format!("x-{role}@example.com"), "role": role }),
        )
        .await;
        assert_eq!(code, 400, "role {role:?} must be refused");
    }
    // Nor a non-address.
    for email in ["notanemail", "", "  "] {
        let (code, _) = post(
            &format!("{base}/api/members"),
            Some(&owner_key),
            serde_json::json!({ "email": email }),
        )
        .await;
        assert_eq!(code, 400, "email {email:?} must be refused");
    }
}

/// One team may not add a person to another, which is the property every call
/// on this surface has to have.
#[tokio::test]
async fn a_team_cannot_add_a_member_to_another_team() {
    let base = start_api(None).await;
    let mut keys = Vec::new();
    for team in ["Acme", "Globex"] {
        let (_, j) = post(
            &format!("{base}/api/register"),
            None,
            serde_json::json!({ "team": team, "email": format!("owner@{team}.example") }),
        )
        .await;
        keys.push(j["token"].as_str().unwrap().to_string());
    }
    let (_, added) = post(
        &format!("{base}/api/members"),
        Some(&keys[0]),
        serde_json::json!({ "email": "shared@example.com", "label": "laptop" }),
    )
    .await;
    let member = added["member"].as_str().unwrap().to_string();

    // Globex sees nothing of Acme's people.
    let (_, team) = get(&format!("{base}/api/team"), Some(&keys[1])).await;
    let emails: Vec<String> = team["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["email"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(!emails.contains(&"shared@example.com".to_string()), "{emails:?}");

    // And cannot mint for them, nor remove them.
    let (code, _) = post(
        &format!("{base}/api/tokens"),
        Some(&keys[1]),
        serde_json::json!({ "label": "stolen", "member": member }),
    )
    .await;
    assert_ne!(code, 200, "minting for another team's member must fail");
    let (code, _) = post(
        &format!("{base}/api/members/{member}/remove"),
        Some(&keys[1]),
        serde_json::json!({}),
    )
    .await;
    assert_ne!(code, 200, "removing another team's member must fail");
    let (code, _) = get(&format!("{base}/api/whoami"), Some(added["token"].as_str().unwrap())).await;
    assert_eq!(code, 200, "and she still works");
}
