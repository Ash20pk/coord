//! Shared helpers for integration tests.
#![allow(dead_code)]

use knoot::proto::*;
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("knoot-test-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Start an in-process relay that requires `token`; returns its ws:// URL.
pub async fn start_relay_with_token(token: &str) -> String {
    let db = tmp("relay").join("relay.db");
    let addr = knoot::relay::start_with_token("127.0.0.1:0", db, Some(token.to_string()))
        .await
        .unwrap();
    format!("ws://{addr}/ws")
}

/// Start an in-process relay on an ephemeral port; returns its ws:// URL.
pub async fn start_relay() -> String {
    let db = tmp("relay").join("relay.db");
    let addr = knoot::relay::start("127.0.0.1:0", db).await.unwrap();
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
        let hello = ClientMsg::Hello { repo: repo.into(), daemon: "test".into(), areas: Vec::new() };
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
        branch: String::new(),
        hub: false,
        })
        .await;
        match self.claim_resp(&id).await {
            ServerMsg::ClaimResp { granted, .. } => granted,
            _ => unreachable!(),
        }
    }
}

/// Write a .knoot.toml so the daemon treats `root` as a knoot repo.
pub fn init_repo(root: &PathBuf, relay: &str, repo: &str) {
    knoot::config::RepoConfig { relay: relay.into(), repo: repo.into(), hubs: Vec::new(), areas: Vec::new() }
        .save(root)
        .unwrap();
}

/// As `init_repo`, plus the hub paths the repo declares.
pub fn init_repo_with_hubs(root: &PathBuf, relay: &str, repo: &str, hubs: &[&str]) {
    knoot::config::RepoConfig {
        relay: relay.into(),
        repo: repo.into(),
        hubs: hubs.iter().map(|h| h.to_string()).collect(),
        areas: Vec::new(),
    }
    .save(root)
    .unwrap();
}

/// As `init_repo`, plus the areas the repo divides itself into.
pub fn init_repo_with_areas(
    root: &PathBuf,
    relay: &str,
    repo: &str,
    areas: &[(&str, &[&str])],
) {
    knoot::config::RepoConfig {
        relay: relay.into(),
        repo: repo.into(),
        hubs: Vec::new(),
        areas: areas
            .iter()
            .map(|(name, paths)| knoot::config::AreaDef {
                name: name.to_string(),
                paths: paths.iter().map(|p| p.to_string()).collect(),
            })
            .collect(),
    }
    .save(root)
    .unwrap();
}

/// Every live claim the relay holds for a repo, from a probe connection.
/// Used where a test needs the *lease*, not merely the fact of a claim.
pub async fn relay_claims(url: &str, repo: &str) -> Vec<Claim> {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let hello = ClientMsg::Hello { repo: repo.into(), daemon: "probe".into(), areas: Vec::new() };
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
    let mut c = Client { ws };
    loop {
        if let ServerMsg::Welcome { claims, .. } = c.recv().await {
            return claims;
        }
    }
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
        let _ = knoot::daemon::run_on(s).await;
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
    tokio::task::spawn_blocking(move || knoot::hook::call_daemon_at(&sock, &req))
        .await
        .unwrap()
}

pub fn allowed(r: &Option<DResp>) -> bool {
    matches!(r, Some(DResp::Decision { allow: true, .. }))
}

pub fn denied_reason(r: &Option<DResp>) -> Option<String> {
    match r {
        Some(DResp::Decision { allow: false, reason, .. }) => reason.clone(),
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
    let hello = ClientMsg::Hello { repo: repo.into(), daemon: "probe".into(), areas: Vec::new() };
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
        let hello = ClientMsg::Hello { repo, daemon: "probe".into(), areas: Vec::new() };
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

/// Blocking daemon call from a sync test context.
pub fn ask_daemon(sock: &PathBuf, req: DReq) -> Option<DResp> {
    knoot::hook::call_daemon_at(sock, &req)
}

/// A relay that grants every claim and then says nothing else — no event
/// broadcast, ever.
///
/// Its purpose is to make one specific race deterministic. The daemon learns
/// about its own granted claims twice: once from the ClaimResp, and again when
/// the relay broadcasts the ClaimAcquired event back. If the daemon relies on
/// the second, then between the two its own mirror reads the file as free —
/// and the Bash gate consults only that mirror, so a peer session can write a
/// file this daemon holds. Against this double the second delivery never
/// comes, so a mirror that waits for it stays empty forever and the bypass
/// shows up on every platform rather than only on a slow one.
pub async fn start_granting_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
                let (mut w, mut r) = ws.split();
                while let Some(Ok(WsMsg::Text(t))) = r.next().await {
                    let Ok(msg) = serde_json::from_str::<ClientMsg>(&t) else { continue };
                    let reply = match msg {
                        // The daemon will not answer from an empty mirror until
                        // a snapshot has landed, so it still needs a Welcome.
                        ClientMsg::Hello { .. } => Some(ServerMsg::Welcome {
                            seq: 0,
                            claims: vec![],
                            sessions: vec![],
                            me: None,
                            provider: None,
                        }),
                        ClientMsg::ClaimReq { id, .. } => Some(ServerMsg::ClaimResp {
                            hub: false,
                            queued: 0,
                            id,
                            granted: true,
                            holder: None,
                            holder_user: None,
                            holder_intent: None,
                            lease_until: Some(now_ms() + 600_000),
                        }),
                        // Appends are swallowed: no echo, no broadcast.
                        _ => None,
                    };
                    if let Some(m) = reply {
                        if w.send(WsMsg::Text(serde_json::to_string(&m).unwrap())).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    format!("ws://{addr}/ws")
}

// ------------------------------------------------------------------ admin
//
// The team surface, over HTTP, as the console and `knoot member` use it.
//
// This exists because for three phases the tests for areas, memory and MLS
// each reached into the relay's SQLite file to invent a colleague — there was
// no call that made one on a relay with no Supabase behind it. That was the
// gap, not the workaround: a test that sets up state the product cannot is a
// test of something nobody can actually do. Now there is `POST /api/members`,
// and these tests go through it.

/// An admin's key against a relay, with the calls that shape a team.
pub struct Admin {
    /// http:// base, for the API.
    pub base: String,
    /// ws:// url, for a client.
    pub url: String,
    pub key: String,
    pub team_id: String,
}

impl Admin {
    /// A fresh relay with a registered team, and the owner's key.
    pub async fn register(team: &str, email: &str) -> Self {
        let db = tmp("admin").join("relay.db");
        Self::register_on(db, team, email).await
    }

    /// As `register`, on a database path the caller keeps — for a test that
    /// wants to look at what the relay stored.
    pub async fn register_on(db: PathBuf, team: &str, email: &str) -> Self {
        let addr = knoot::relay::start_with_token("127.0.0.1:0", db, None).await.unwrap();
        let base = format!("http://{addr}");
        let (code, j) = http(
            "POST",
            &format!("{base}/api/register"),
            None,
            Some(serde_json::json!({ "team": team, "email": email })),
        )
        .await;
        assert_eq!(code, 200, "register failed: {j}");
        let key = j["token"].as_str().expect("a key").to_string();
        let (_, who) = http("GET", &format!("{base}/api/whoami"), Some(&key), None).await;
        Self {
            url: format!("ws://{addr}/ws"),
            team_id: who["team_id"].as_str().unwrap_or_default().to_string(),
            base,
            key,
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        http("POST", &format!("{}{path}", self.base), Some(&self.key), Some(body)).await
    }

    pub async fn get(&self, path: &str) -> serde_json::Value {
        http("GET", &format!("{}{path}", self.base), Some(&self.key), None).await.1
    }

    /// Add a colleague and mint their first key. Returns `(member_id, key)`.
    pub async fn add_member(&self, email: &str, label: &str) -> (String, String) {
        let (code, v) = self
            .post("/api/members", serde_json::json!({ "email": email, "label": label }))
            .await;
        assert_eq!(code, 200, "add_member failed: {v}");
        (
            v["member"].as_str().expect("a member id").to_string(),
            v["token"].as_str().expect("a key").to_string(),
        )
    }

    /// The device id a key was minted against, which is its MLS credential.
    pub async fn device_of(&self, member: &str) -> String {
        self.get("/api/team").await["tokens"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["member_id"].as_str() == Some(member)))
            .and_then(|t| t["id"].as_str().map(str::to_string))
            .expect("a device for that member")
    }

    pub async fn my_member(&self) -> String {
        self.get("/api/whoami").await["me"]["member_id"].as_str().unwrap_or_default().to_string()
    }

    pub async fn create_room(&self, name: &str) -> String {
        let (code, v) = self.post("/api/rooms", serde_json::json!({ "name": name })).await;
        assert_eq!(code, 200, "create_room failed: {v}");
        v["room"].as_str().expect("a room id").to_string()
    }

    pub async fn room_named(&self, name: &str) -> String {
        self.get("/api/team").await["rooms"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["name"].as_str() == Some(name)))
            .and_then(|r| r["id"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("no room called {name}"))
    }

    pub async fn add_area(&self, room: &str, repo: &str, area: &str) {
        let (code, v) = self
            .post(
                &format!("/api/rooms/{room}/areas"),
                serde_json::json!({ "repo": repo, "area": area }),
            )
            .await;
        assert_eq!(code, 200, "add_area failed: {v}");
    }

    pub async fn add_to_room(&self, room: &str, member: &str) {
        let (code, v) = self
            .post(&format!("/api/rooms/{room}/members"), serde_json::json!({ "member": member }))
            .await;
        assert_eq!(code, 200, "add_to_room failed: {v}");
    }

    /// Take somebody out of a room. Needed to give anyone a *narrow* grant:
    /// `general` holds everybody over every repo, so a key's areas are the
    /// union of that and anything else until they leave it.
    pub async fn remove_from_room(&self, room: &str, member: &str) {
        let (code, v) = self
            .post(
                &format!("/api/rooms/{room}/members"),
                serde_json::json!({ "member": member, "remove": true }),
            )
            .await;
        assert_eq!(code, 200, "remove_from_room failed: {v}");
    }
}

/// A minimal HTTP/1.1 client. Pulling in a whole client crate for two verbs
/// against loopback is not worth the dependency.
pub async fn http(
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
    // answer and hang up before the second half of a split request lands.
    head.push_str(&payload);
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.flush().await;

    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 =
        text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let json = text
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b.trim()).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

// -------------------------------------------------------------- a 2nd daemon
//
// One process holds one MLS device, because a device *is* a machine. So a
// property about two machines — the one this exists for is "a removal rotates
// the room's key and the departed laptop cannot follow" — needs a daemon in a
// subprocess with its own socket, its own credential and its own key
// material. Nothing else in the suite is shaped like this, and nothing else
// needs to be.

pub struct Daemon2 {
    child: std::process::Child,
    pub sock: PathBuf,
    pub mls_dir: PathBuf,
    /// A checkout of its own, enrolled against the same repo id — which is
    /// what makes it the same coordination space and a different machine.
    pub root: PathBuf,
}

impl Daemon2 {
    /// Start a second daemon holding `key`, enrolled in `repo` on `relay`.
    pub async fn start(bin: &str, relay: &str, repo: &str, key: &str, tag: &str) -> Self {
        let sock = tmp_sock_dir().join("s2");
        let mls_dir = tmp(&format!("{tag}-mls"));
        let root = tmp(&format!("{tag}-checkout"));
        init_repo(&root, relay, repo);
        std::fs::create_dir_all(root.join("src")).unwrap();

        let child = std::process::Command::new(bin)
            .arg("daemon")
            .env("KNOOT_SOCK", &sock)
            .env("KNOOT_TOKEN", key)
            .env("KNOOT_MLS_DIR", &mls_dir)
            .env("KNOOT_USER", "priya")
            .stdout(std::process::Stdio::from(
                std::fs::File::create(mls_dir.join("daemon.out")).unwrap(),
            ))
            .stderr(std::process::Stdio::from(
                std::fs::File::create(mls_dir.join("daemon.err")).unwrap(),
            ))
            .spawn()
            .expect("second daemon must start");

        // Owned before the wait, so a daemon that never comes up is still
        // killed by `Drop` rather than left running after the panic.
        let me = Self { child, sock, mls_dir, root };
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(&me.sock).is_ok() {
                return me;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("second daemon did not come up: {}", me.stderr());
    }

    /// Run a `knoot` command against this daemon, in its own checkout.
    pub fn run(&self, bin: &str, args: &[&str]) -> String {
        let out = std::process::Command::new(bin)
            .args(args)
            .current_dir(&self.root)
            .env("KNOOT_SOCK", &self.sock)
            .env("KNOOT_MLS_DIR", &self.mls_dir)
            .env("KNOOT_USER", "priya")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Feed it a hook payload, as Claude Code would.
    pub fn hook(&self, bin: &str, payload: serde_json::Value) {
        use std::io::Write;
        let mut child = std::process::Command::new(bin)
            .arg("hook")
            .env("KNOOT_SOCK", &self.sock)
            .env("KNOOT_MLS_DIR", &self.mls_dir)
            .env("KNOOT_USER", "priya")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let _ = child.stdin.as_mut().unwrap().write_all(payload.to_string().as_bytes());
        let _ = child.wait();
    }

    /// Whatever the daemon said on stderr. A subprocess that fails open says
    /// nothing to the caller, so this is the only way to see why.
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.mls_dir.join("daemon.err")).unwrap_or_default()
    }

    /// The device id this daemon's key was minted against, read out of its own
    /// MLS state directory — which is named by it.
    pub fn device_id(&self) -> Option<String> {
        std::fs::read_dir(&self.mls_dir)
            .ok()?
            .flatten()
            .find(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
    }
}

impl Drop for Daemon2 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
