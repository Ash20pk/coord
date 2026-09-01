//! Shared helpers for integration tests.
#![allow(dead_code)]

use coord::proto::*;
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("coord-test-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Start an in-process relay on an ephemeral port; returns its ws:// URL.
pub async fn start_relay() -> String {
    let db = tmp("relay").join("relay.db");
    let addr = coord::relay::start("127.0.0.1:0", db).await.unwrap();
    format!("ws://{addr}/ws")
}

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A raw relay client: says Hello, consumes the Welcome, then speaks ClientMsg.
pub struct Client {
    pub ws: Ws,
}

impl Client {
    pub async fn connect(url: &str, repo: &str) -> Self {
        let (mut ws, _) = connect_async(url).await.unwrap();
        let hello = ClientMsg::Hello { repo: repo.into(), daemon: "test".into() };
        ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
        let mut c = Self { ws };
        loop {
            if let ServerMsg::Welcome { .. } = c.recv().await {
                break;
            }
        }
        c
    }

    pub async fn send(&mut self, m: &ClientMsg) {
        self.ws.send(WsMsg::Text(serde_json::to_string(m).unwrap())).await.unwrap();
    }

    pub async fn recv(&mut self) -> ServerMsg {
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

    /// Wait for the ClaimResp matching `id`, ignoring interleaved events.
    pub async fn claim_resp(&mut self, id: &str) -> ServerMsg {
        loop {
            let m = self.recv().await;
            if let ServerMsg::ClaimResp { id: ref got, .. } = m {
                if got == id {
                    return m;
                }
            }
        }
    }

    pub async fn request_claim(&mut self, session: &str, path: &str, intent: &str) -> bool {
        let id = uuid::Uuid::new_v4().to_string();
        self.send(&ClientMsg::ClaimReq {
            id: id.clone(),
            session: session.into(),
            user: "u".into(),
            path: path.into(),
            intent: intent.into(),
        })
        .await;
        match self.claim_resp(&id).await {
            ServerMsg::ClaimResp { granted, .. } => granted,
            _ => unreachable!(),
        }
    }
}

/// Write a .coord.toml so the daemon treats `root` as a coord repo.
pub fn init_repo(root: &PathBuf, relay: &str, repo: &str) {
    coord::config::RepoConfig { relay: relay.into(), repo: repo.into() }
        .save(root)
        .unwrap();
}

/// Short-path temp dir for unix sockets: macOS caps socket paths at ~104 bytes,
/// and $TMPDIR under /var/folders/... is long enough to blow that on its own.
pub fn tmp_sock_dir() -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let p = PathBuf::from(format!("/tmp/cd{}", &id[..8]));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Start an in-process daemon on an isolated socket; returns the socket path.
pub async fn start_daemon() -> PathBuf {
    let sock = tmp_sock_dir().join("s");
    let s = sock.clone();
    tokio::spawn(async move {
        let _ = coord::daemon::run_on(s).await;
    });
    // Wait for the socket to accept connections.
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return sock;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("daemon did not come up");
}

/// Blocking daemon call from an async test, off the runtime threads.
pub async fn ask(sock: &PathBuf, req: DReq) -> Option<DResp> {
    let (sock, req) = (sock.clone(), req);
    tokio::task::spawn_blocking(move || coord::hook::call_daemon_at(&sock, &req))
        .await
        .unwrap()
}

pub fn allowed(r: &Option<DResp>) -> bool {
    matches!(r, Some(DResp::Decision { allow: true, .. }))
}

pub fn denied_reason(r: &Option<DResp>) -> Option<String> {
    match r {
        Some(DResp::Decision { allow: false, reason }) => reason.clone(),
        _ => None,
    }
}

/// A WebSocket server that completes the handshake then never replies.
/// Exercises the daemon's claim timeout / fail-open path.
pub async fn start_black_hole_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    // Hold the connection open, read nothing, answer nothing.
                    let (_w, mut r) = ws.split();
                    while let Some(Ok(_)) = r.next().await {}
                }
            });
        }
    });
    format!("ws://{addr}/ws")
}

/// Positive control for "allowed" assertions. Fail-open makes an empty answer
/// ambiguous — coordination working, or coordination unreachable — so an
/// allow is only meaningful if the relay actually recorded the claim.
pub async fn relay_holds_claim(url: &str, repo: &str, path: &str) -> bool {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let hello = ClientMsg::Hello { repo: repo.into(), daemon: "probe".into() };
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
    while let Some(Ok(WsMsg::Text(t))) = ws.next().await {
        if let Ok(ServerMsg::Welcome { claims, .. }) = serde_json::from_str(&t) {
            return claims.iter().any(|c| c.path == path);
        }
    }
    false
}

/// Subscribe to a repo's event stream and count UngatedWrite events as they
/// arrive. Must be armed before the write it is meant to observe.
pub fn watch_ungated(url: &str, repo: &str) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = std::sync::Arc::new(AtomicUsize::new(0));
    let (n2, url, repo) = (n.clone(), url.to_string(), repo.to_string());
    tokio::spawn(async move {
        let Ok((mut ws, _)) = connect_async(&url).await else { return };
        let hello = ClientMsg::Hello { repo, daemon: "probe".into() };
        if ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.is_err() {
            return;
        }
        while let Some(Ok(WsMsg::Text(t))) = ws.next().await {
            if let Ok(ServerMsg::Event { event: Event::UngatedWrite { .. }, .. }) =
                serde_json::from_str(&t)
            {
                n2.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    n
}
