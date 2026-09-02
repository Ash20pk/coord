//! Layer 3: fail-open is a product promise, not an implementation detail.
//! Every degraded path must end in "allow, silently" — coord may never be
//! the reason an agent can't work.

mod common;
use common::*;
use coord::proto::*;

const REPO_ID: &str = "failure-tests";

fn prewrite(root: &std::path::Path, session: &str, path: &str) -> DReq {
    DReq::PreWrite {
        repo_root: root.to_string_lossy().to_string(),
        session: session.into(),
        path: format!("{}/{}", root.to_string_lossy(), path),
    }
}

#[tokio::test]
async fn no_daemon_running_means_allow() {
    let ghost = tmp_sock_dir().join("nope");
    let root = tmp("repo");
    let r = ask(&ghost, prewrite(&root, "s1", "src/auth.ts")).await;
    assert!(r.is_none(), "unreachable daemon must yield no verdict (hook then allows)");
}

#[tokio::test]
async fn repo_without_coord_config_is_allowed() {
    let sock = start_daemon().await;
    let root = tmp("uninit"); // no .coord.toml
    let r = ask(&sock, prewrite(&root, "s1", "src/auth.ts")).await;
    assert!(allowed(&r), "a non-coord repo must never be gated");
}

#[tokio::test]
async fn relay_down_means_allow() {
    let sock = start_daemon().await;
    let root = tmp("relay-down");
    // Point at a port nobody is listening on.
    init_repo(&root, "ws://127.0.0.1:1/ws", REPO_ID);
    let r = ask(&sock, prewrite(&root, "s1", "src/auth.ts")).await;
    assert!(allowed(&r), "relay down must fail open");
}

#[tokio::test]
async fn relay_that_never_answers_times_out_and_allows() {
    let sock = start_daemon().await;
    let root = tmp("black-hole");
    init_repo(&root, &start_black_hole_relay().await, REPO_ID);

    // Let the daemon establish the connection so `connected` is true —
    // this is the dangerous case: reachable but unresponsive.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let start = std::time::Instant::now();
    let r = ask(&sock, prewrite(&root, "s1", "src/auth.ts")).await;
    let elapsed = start.elapsed();

    assert!(allowed(&r), "an unresponsive relay must fail open, not hang");
    assert!(
        elapsed < std::time::Duration::from_millis(2000),
        "must give up quickly, took {elapsed:?}"
    );
}

#[tokio::test]
async fn malformed_daemon_request_is_an_error_not_a_hang() {
    use std::io::{BufRead, BufReader, Write};
    let sock = start_daemon().await;
    let out = tokio::task::spawn_blocking(move || {
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        s.write_all(b"{ not json at all }\n").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line
    })
    .await
    .unwrap();
    assert!(out.contains("err"), "daemon must answer malformed input, got: {out}");
}

#[tokio::test]
async fn daemon_mirror_reflects_claims_made_by_other_clients() {
    // A claim taken by some other daemon must show up in this daemon's local
    // mirror and be enforced from there. (Relay *restart* is a different
    // property — see tests/e2e.rs::relay_restart_is_survived_by_the_daemon.)
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp("mirror");
    init_repo(&root, &url, "mirror-repo");

    let mut a = Client::connect(&url, "mirror-repo").await;
    assert!(a.request_claim("sessA", "src/auth.ts", "refactor auth").await);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let r = ask(&sock, prewrite(&root, "sessB", "src/auth.ts")).await;
    assert!(denied_reason(&r).is_some(), "daemon mirror should reflect the peer's claim");
    let r2 = ask(&sock, prewrite(&root, "sessB", "src/other.ts")).await;
    assert!(allowed(&r2), "unrelated files stay editable");
}

#[tokio::test]
async fn expired_lease_unblocks_a_peer_without_any_release() {
    // A crashed session can never wedge the repo: the lease is the safety net.
    let mut v = View::default();
    v.apply(&Event::ClaimAcquired {
        session: "crashed".into(),
        user: "ash".into(),
        path: "src/auth.ts".into(),
        lease_until: now_ms() + 40, // about to expire
        intent: "died mid-turn".into(),
        branch: String::new(),
    });
    assert!(v.conflicting("peer", "src/auth.ts").is_some(), "blocked while lease is live");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(
        v.conflicting("peer", "src/auth.ts").is_none(),
        "lease expiry must free the file with no release event"
    );
}

#[tokio::test]
async fn a_claim_holder_can_keep_editing_its_own_file() {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp("self");
    init_repo(&root, &url, "self-repo");

    for i in 0..3 {
        let r = ask(&sock, prewrite(&root, "sessA", "src/auth.ts")).await;
        assert!(allowed(&r), "own file must stay editable across turns (attempt {i})");
    }
}

/// A relay that requires a token, and a client that has none, is a relay this
/// daemon cannot reach. The agent must still be able to work: an operator's
/// auth mistake cannot become an outage for every developer on the team.
#[tokio::test]
async fn a_relay_that_refuses_us_still_fails_open() {
    let url = start_relay_with_token("the-real-token").await;
    let sock = start_daemon().await;
    let root = tmp("badtoken");
    // Credentials are keyed by relay origin, and this one is an ephemeral
    // port, so there is nothing on disk or in the environment that matches.
    init_repo(&root, &url, "failure-authed");

    let r = ask(&sock, prewrite(&root, "s1", "src/auth.ts")).await;
    match r {
        None => {}
        Some(DResp::Decision { allow, .. }) => {
            assert!(allow, "a relay that rejected us must never block an edit");
        }
        other => panic!("unexpected verdict: {other:?}"),
    }
}

/// And the gate is real: the socket itself is refused without a token.
#[tokio::test]
async fn an_unauthenticated_client_is_refused_by_the_relay() {
    let url = start_relay_with_token("sekrit").await;

    assert!(
        tokio_tungstenite::connect_async(&url).await.is_err(),
        "a tokenless client must not get a socket"
    );

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert("Authorization", "Bearer sekrit".parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(req).await.is_ok(),
        "the right token must still get in"
    );

    let mut wrong = url.as_str().into_client_request().unwrap();
    wrong.headers_mut().insert("Authorization", "Bearer nope".parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(wrong).await.is_err(),
        "a wrong token must be refused, not merely logged"
    );
}
