//! Layer 3½: areas — the unit of who can collide with whom.
//!
//! A repo declares its subtrees in `.knoot.toml`; a room grants `(repo, area)`
//! pairs; the relay decides an event's area from its path and delivers it only
//! to the keys that were granted it. Phase 1 carried the grants without
//! consulting them anywhere; these are the tests that say the grant now bites.
//!
//! Both tests drive the real relay over a real socket with real device keys,
//! because the interesting failure — a session hearing about work it was not
//! granted — lives in the delivery path and nowhere else.

mod common;
use common::Admin;

use futures_util::{SinkExt, StreamExt};
use knoot::config::AreaDef;
use knoot::proto::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMsg;

fn area(name: &str, paths: &[&str]) -> AreaDef {
    AreaDef { name: name.into(), paths: paths.iter().map(|p| p.to_string()).collect() }
}

/// The repo every test here divides the same way.
fn declared() -> Vec<AreaDef> {
    vec![area("auth", &["src/auth"]), area("billing", &["src/billing"])]
}

/// A relay with a team, a person confined to one area, and a person in
/// `general` who therefore sees everything. Returns the url and both keys.
///
/// Every call here goes through the relay's own API — the same calls the
/// console and `knoot member` make. It used to reach into the relay's SQLite
/// file, because there was no way to create a second person without Supabase;
/// a test that sets up state the product cannot reach is a test of something
/// nobody can do.
async fn relay_with_two_people() -> (String, String, String) {
    let admin = Admin::register("Acme", "ash@example.com").await;
    let (priya, priya_key) = admin.add_member("priya@example.com", "laptop").await;

    let room = admin.create_room("auth-team").await;
    admin.add_area(&room, "repo", "auth").await;
    admin.add_to_room(&room, &priya).await;
    // Out of `general`, or the union of her rooms would grant everything and
    // the area would be decoration.
    let general = admin.room_named("general").await;
    admin.remove_from_room(&general, &priya).await;

    (admin.url.clone(), admin.key.clone(), priya_key)
}

/// A connection that presents a device key and declares the repo's areas.
struct Keyed {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Keyed {
    async fn connect(url: &str, key: &str, repo: &str, areas: Vec<AreaDef>) -> Self {
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {key}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let hello = ClientMsg::Hello { repo: repo.into(), daemon: "test".into(), areas };
        ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
        let mut k = Self { ws };
        loop {
            if let ServerMsg::Welcome { .. } = k.recv().await {
                break;
            }
        }
        k
    }

    async fn send(&mut self, m: &ClientMsg) {
        self.ws.send(WsMsg::Text(serde_json::to_string(m).unwrap())).await.unwrap();
    }

    async fn recv(&mut self) -> ServerMsg {
        loop {
            match self.ws.next().await {
                Some(Ok(WsMsg::Text(t))) => {
                    if let Ok(m) = serde_json::from_str::<ServerMsg>(&t) {
                        return m;
                    }
                }
                Some(Ok(_)) => continue,
                other => panic!("relay connection closed: {other:?}"),
            }
        }
    }

    /// Every path this connection is told about within `ms`.
    ///
    /// A test about what does *not* arrive cannot wait for a message, so it
    /// waits for a window and asserts on what the window held. Everything the
    /// relay does here is in-process and immediate, so the window is short.
    async fn paths_within(&mut self, ms: u64) -> Vec<String> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
        while let Ok(Ok(m)) = tokio::time::timeout_at(deadline, async { Ok::<_, ()>(self.recv().await) }).await {
            if let ServerMsg::Event { event, .. } = m {
                if let Some(p) = event.path() {
                    seen.push(p.to_string());
                }
            }
        }
        seen
    }
}

fn wrote(session: &str, path: &str) -> ClientMsg {
    ClientMsg::Append {
        event: Event::FileWritten {
            session: session.into(),
            user: "ash".into(),
            path: path.into(),
            ts: now_ms(),
        },
    }
}

#[tokio::test]
async fn one_areas_events_never_reach_a_session_outside_it() {
    let (url, owner_key, priya_key) = relay_with_two_people().await;
    let mut ash = Keyed::connect(&url, &owner_key, "repo", declared()).await;
    let mut priya = Keyed::connect(&url, &priya_key, "repo", declared()).await;

    // Three writes: one in priya's area, one outside it, one in neither.
    ash.send(&wrote("s1", "src/auth/token.rs")).await;
    ash.send(&wrote("s1", "src/billing/invoice.rs")).await;
    ash.send(&wrote("s1", "README.md")).await;

    let heard = priya.paths_within(300).await;
    assert_eq!(
        heard,
        vec!["src/auth/token.rs".to_string()],
        "a key granted only `auth` hears about `auth` and nothing else"
    );

    // And the same log, seen by someone in `general`, is whole. The events
    // were never dropped — only routed.
    let all = ash.paths_within(300).await;
    assert_eq!(
        all,
        vec![
            "src/auth/token.rs".to_string(),
            "src/billing/invoice.rs".to_string(),
            "README.md".to_string()
        ],
        "`general` grants every area, so nothing is hidden from it"
    );
}

#[tokio::test]
async fn a_write_that_crosses_into_an_area_is_logged_to_both() {
    let (url, owner_key, priya_key) = relay_with_two_people().await;
    let mut ash = Keyed::connect(&url, &owner_key, "repo", declared()).await;
    let mut priya = Keyed::connect(&url, &priya_key, "repo", declared()).await;

    // Ash is working in billing and reaches into auth. Areas bound attention;
    // they never hide a write from the people it affects, so this one is on
    // both logs — ash's because `general` covers everything, priya's because
    // the path is hers.
    ash.send(&wrote("s1", "src/billing/invoice.rs")).await;
    ash.send(&wrote("s1", "src/auth/token.rs")).await;

    assert_eq!(
        priya.paths_within(300).await,
        vec!["src/auth/token.rs".to_string()],
        "the crossing write reaches the area it crossed into"
    );
    assert_eq!(
        ash.paths_within(300).await,
        vec!["src/billing/invoice.rs".to_string(), "src/auth/token.rs".to_string()],
        "and stays on the log of the area it came from"
    );
}

#[tokio::test]
async fn a_session_is_visible_across_areas_even_when_its_writes_are_not() {
    let (url, owner_key, priya_key) = relay_with_two_people().await;
    let mut priya = Keyed::connect(&url, &priya_key, "repo", declared()).await;
    let mut ash = Keyed::connect(&url, &owner_key, "repo", declared()).await;

    // Presence and intent belong to no subtree. A peer you cannot see is
    // worse than a peer working somewhere you do not care about.
    ash.send(&ClientMsg::Append {
        event: Event::SessionStarted {
            session: "s1".into(),
            user: "ash".into(),
            branch: "main".into(),
            ts: now_ms(),
        },
    })
    .await;
    ash.send(&wrote("s1", "src/billing/invoice.rs")).await;

    let mut sessions = 0;
    let mut paths = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(300);
    while let Ok(m) = tokio::time::timeout_at(deadline, priya.recv()).await {
        if let ServerMsg::Event { event, .. } = m {
            match event {
                Event::SessionStarted { .. } => sessions += 1,
                _ if event.path().is_some() => paths += 1,
                _ => {}
            }
        }
    }
    assert_eq!(sessions, 1, "presence crosses every area");
    assert_eq!(paths, 0, "the write in an area she was not granted does not");
}
