//! Layer 2: the relay is the sole arbiter. These tests validate the core
//! product claim — concurrent claims on overlapping paths yield exactly one
//! winner, and every loser learns who holds it and why.

mod common;
use common::*;
use knoot::proto::*;

#[tokio::test]
async fn concurrent_claims_on_same_path_yield_exactly_one_winner() {
    const N: usize = 8;
    let url = start_relay().await;

    for round in 0..50 {
        let repo = format!("repo-{round}");
        let path = "src/auth.ts";

        // N independent clients, all racing for the same path.
        let mut tasks = Vec::new();
        for i in 0..N {
            let url = url.clone();
            let repo = repo.clone();
            tasks.push(tokio::spawn(async move {
                let mut c = Client::connect(&url, &repo).await;
                c.request_claim(&format!("s{i}"), path, "race").await
            }));
        }

        let mut granted = 0;
        for t in tasks {
            if t.await.unwrap() {
                granted += 1;
            }
        }
        assert_eq!(granted, 1, "round {round}: expected exactly 1 grant, got {granted}");
    }
}

#[tokio::test]
async fn concurrent_claims_on_overlapping_dir_and_file_yield_one_winner() {
    let url = start_relay().await;
    // These paths all overlap each other; only one may win.
    let paths = ["src", "src/auth", "src/auth/session.ts"];

    for round in 0..30 {
        let repo = format!("overlap-{round}");
        let mut tasks = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let (url, repo, p) = (url.clone(), repo.clone(), p.to_string());
            tasks.push(tokio::spawn(async move {
                let mut c = Client::connect(&url, &repo).await;
                c.request_claim(&format!("s{i}"), &p, "race").await
            }));
        }
        let mut granted = 0;
        for t in tasks {
            if t.await.unwrap() {
                granted += 1;
            }
        }
        assert_eq!(granted, 1, "round {round}: overlapping paths must serialize");
    }
}

#[tokio::test]
async fn non_overlapping_claims_all_succeed() {
    let url = start_relay().await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(&url, "parallel").await;
            c.request_claim(&format!("s{i}"), &format!("src/mod{i}.ts"), "work").await
        }));
    }
    for t in tasks {
        assert!(t.await.unwrap(), "disjoint paths must not block each other");
    }
}

#[tokio::test]
async fn loser_receives_holder_identity_and_intent() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "brief").await;
    let mut b = Client::connect(&url, "brief").await;

    assert!(a.request_claim("sessA", "src/auth.ts", "refactor auth").await);

    let id = "req-b".to_string();
    b.send(&ClientMsg::ClaimReq {
        id: id.clone(),
        session: "sessB".into(),
        user: "priya".into(),
        path: "src/auth.ts".into(),
        intent: "add logging".into(),
        branch: String::new(),
    })
    .await;

    match b.claim_resp(&id).await {
        ServerMsg::ClaimResp { granted, holder, holder_intent, lease_until, .. } => {
            assert!(!granted);
            assert_eq!(holder.as_deref(), Some("sessA"));
            assert_eq!(holder_intent.as_deref(), Some("refactor auth"));
            assert!(lease_until.unwrap() > now_ms(), "brief must carry a live lease");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn same_session_reclaiming_its_own_path_is_granted() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "renew").await;
    assert!(a.request_claim("sessA", "src/auth.ts", "work").await);
    assert!(
        a.request_claim("sessA", "src/auth.ts", "work").await,
        "a session must be able to keep editing its own claimed file"
    );
}

#[tokio::test]
async fn releasing_a_session_frees_its_claims_for_others() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "release").await;
    let mut b = Client::connect(&url, "release").await;

    assert!(a.request_claim("sessA", "src/auth.ts", "work").await);
    assert!(!b.request_claim("sessB", "src/auth.ts", "work").await);

    a.send(&ClientMsg::ReleaseSession { session: "sessA".into() }).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        b.request_claim("sessB", "src/auth.ts", "work").await,
        "claims must be freed when the holding session ends"
    );
}

#[tokio::test]
async fn repos_are_isolated_from_each_other() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "repo-one").await;
    let mut b = Client::connect(&url, "repo-two").await;
    assert!(a.request_claim("sessA", "src/auth.ts", "work").await);
    assert!(
        b.request_claim("sessB", "src/auth.ts", "work").await,
        "same path in a different repo must not conflict"
    );
}

#[tokio::test]
async fn late_joiner_receives_existing_claims_in_welcome() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "welcome").await;
    a.send(&ClientMsg::Append {
        event: Event::SessionStarted {
            session: "sessA".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: now_ms(),
        },
    })
    .await;
    assert!(a.request_claim("sessA", "src/auth.ts", "refactor auth").await);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // A fresh connection must be told the current state, not an empty one.
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    use futures_util::{SinkExt, StreamExt};
    let hello = ClientMsg::Hello { repo: "welcome".into(), daemon: "late".into() };
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&hello).unwrap(),
    ))
    .await
    .unwrap();

    loop {
        if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) = ws.next().await {
            if let Ok(ServerMsg::Welcome { claims, sessions, seq }) = serde_json::from_str(&t) {
                assert!(seq > 0, "sequencer must have advanced");
                assert_eq!(claims.len(), 1);
                assert_eq!(claims[0].path, "src/auth.ts");
                assert_eq!(claims[0].intent, "refactor auth");
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].user, "ash");
                break;
            }
        }
    }
}

#[tokio::test]
async fn claim_grant_is_broadcast_to_peers() {
    let url = start_relay().await;
    let mut a = Client::connect(&url, "broadcast").await;
    let mut b = Client::connect(&url, "broadcast").await;

    assert!(a.request_claim("sessA", "src/auth.ts", "refactor").await);

    // B must learn about it via the event stream, so its local mirror stays warm.
    loop {
        if let ServerMsg::Event { event: Event::ClaimAcquired { path, session, .. }, seq } =
            b.recv().await
        {
            assert_eq!(path, "src/auth.ts");
            assert_eq!(session, "sessA");
            assert!(seq > 0);
            break;
        }
    }
}
