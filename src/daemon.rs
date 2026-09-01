use crate::config::RepoConfig;
use crate::proto::*;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMsg;

const CLAIM_TIMEOUT_MS: u64 = 500; // relay slower than this → fail open
const COLD_START_WAIT_MS: u64 = 400; // first-connection snapshot wait, then fail open

pub fn socket_path() -> PathBuf {
    // COORD_SOCK lets tests (and multi-instance setups) use an isolated socket.
    if let Ok(p) = std::env::var("COORD_SOCK") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap().join(".coord").join("coordd.sock")
}

struct RepoConn {
    tx: mpsc::UnboundedSender<ClientMsg>,
    view: Arc<Mutex<View>>,
    connected: Arc<Mutex<bool>>,
    /// True once a Welcome snapshot has been applied, i.e. the mirror is
    /// trustworthy. Until then we must not answer from an empty view.
    ready: Arc<Mutex<bool>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ServerMsg>>>>,
}

#[derive(Default)]
struct Daemon {
    repos: Mutex<HashMap<String, Arc<RepoConn>>>,
}

pub async fn run() -> Result<()> {
    run_on(socket_path()).await
}

/// Serve the daemon API on an explicit socket path (tests use isolated sockets).
pub async fn run_on(sock: PathBuf) -> Result<()> {
    std::fs::create_dir_all(sock.parent().unwrap())?;
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("coordd listening on {}", sock.display());

    let daemon = Arc::new(Daemon::default());
    loop {
        let (stream, _) = listener.accept().await?;
        let d = daemon.clone();
        tokio::spawn(async move {
            let _ = handle_client(stream, d).await;
        });
    }
}

async fn handle_client(stream: UnixStream, d: Arc<Daemon>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let resp = match serde_json::from_str::<DReq>(&line) {
            Ok(req) => handle_req(req, &d).await,
            Err(e) => DResp::Err { msg: e.to_string() },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

async fn handle_req(req: DReq, d: &Arc<Daemon>) -> DResp {
    match req {
        DReq::PreWrite { repo_root, session, path } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Decision { allow: true, reason: None }; // not coord-enabled → fail open
            };
            let path = rel_path(&repo_root, &path);

            // Hot path: local mirror check, microseconds.
            if let Some(c) = rc.view.lock().unwrap().conflicting(&session, &path).cloned() {
                return deny(&path, &c);
            }

            // Acquire through the relay (authoritative), fail open on timeout.
            if !*rc.connected.lock().unwrap() {
                return DResp::Decision { allow: true, reason: None };
            }
            let id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            rc.pending.lock().unwrap().insert(id.clone(), tx);
            // Identity and intent must come from the owning session's record,
            // not from this daemon's environment — a daemon may serve sessions
            // started under a different user, and the brief names the holder.
            let (intent, user) = {
                let v = rc.view.lock().unwrap();
                match v.sessions.get(&session) {
                    Some(s) => (s.intent.clone(), s.user.clone()),
                    None => (String::new(), whoami()),
                }
            };
            let _ = rc.tx.send(ClientMsg::ClaimReq { id: id.clone(), session, user, path: path.clone(), intent });
            match tokio::time::timeout(std::time::Duration::from_millis(CLAIM_TIMEOUT_MS), rx).await {
                Ok(Ok(ServerMsg::ClaimResp { granted: false, holder, holder_user, holder_intent, lease_until, .. })) => {
                    let c = Claim {
                        session: holder.unwrap_or_default(),
                        user: holder_user.unwrap_or_else(|| "someone".into()),
                        path: path.clone(),
                        lease_until: lease_until.unwrap_or(0),
                        intent: holder_intent.unwrap_or_default(),
                    };
                    deny(&path, &c)
                }
                _ => {
                    rc.pending.lock().unwrap().remove(&id);
                    DResp::Decision { allow: true, reason: None } // granted, or timeout → fail open
                }
            }
        }
        DReq::PostWrite { repo_root, session, path } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                let path = rel_path(&repo_root, &path);
                let ev = Event::FileWritten { session, path, ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::Append { event: ev });
            }
            DResp::Ok
        }
        DReq::SessionStart { repo_root, session, user, branch } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else { return DResp::Ok };
            let ev = Event::SessionStarted { session: session.clone(), user, branch, ts: now_ms() };
            rc.view.lock().unwrap().apply(&ev);
            let _ = rc.tx.send(ClientMsg::Append { event: ev });
            // Give the relay a beat to deliver the Welcome snapshot on fresh connections.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let v = rc.view.lock().unwrap();
            DResp::Peers {
                sessions: v.sessions.values().filter(|s| s.session != session).cloned().collect(),
                claims: v.claims.iter().filter(|c| c.session != session).cloned().collect(),
            }
        }
        DReq::Intent { repo_root, session, text } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                let ev = Event::IntentDeclared { session, text, ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::Append { event: ev });
            }
            DResp::Ok
        }
        DReq::SessionEnd { repo_root, session } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                let ev = Event::SessionEnded { session: session.clone(), ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::ReleaseSession { session });
            }
            DResp::Ok
        }
        DReq::Who { repo_root } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not coord-enabled (run `coord init`)".into() };
            };
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let mut v = rc.view.lock().unwrap();
            v.prune();
            DResp::Peers {
                sessions: v.sessions.values().cloned().collect(),
                claims: v.claims.clone(),
            }
        }
    }
}

fn deny(path: &str, c: &Claim) -> DResp {
    let mins = (c.lease_until.saturating_sub(now_ms())) / 60_000;
    let intent = if c.intent.is_empty() { "unknown".to_string() } else { format!("\"{}\"", c.intent) };
    DResp::Decision {
        allow: false,
        reason: Some(format!(
            "coord: `{path}` is currently claimed by {} (session {}…) — intent: {}. Lease expires in ~{}m. \
             Do not edit this file now: re-plan around it, work on something else, or tell the user so they can coordinate. \
             `coord who` shows all active sessions.",
            c.user,
            &c.session[..c.session.len().min(8)],
            intent,
            mins.max(1)
        )),
    }
}

fn rel_path(repo_root: &str, path: &str) -> String {
    let root = repo_root.trim_end_matches('/');
    path.strip_prefix(&format!("{root}/")).unwrap_or(path).to_string()
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

/// Get or create the relay connection for a repo (keyed by repo root).
async fn ensure_repo(d: &Arc<Daemon>, repo_root: &str) -> Option<Arc<RepoConn>> {
    if let Some(rc) = d.repos.lock().unwrap().get(repo_root) {
        return Some(rc.clone());
    }
    let cfg = RepoConfig::load(std::path::Path::new(repo_root))?;
    let (tx, rx) = mpsc::unbounded_channel::<ClientMsg>();
    let rc = Arc::new(RepoConn {
        tx,
        view: Arc::new(Mutex::new(View::default())),
        connected: Arc::new(Mutex::new(false)),
        ready: Arc::new(Mutex::new(false)),
        pending: Arc::new(Mutex::new(HashMap::new())),
    });
    d.repos.lock().unwrap().insert(repo_root.to_string(), rc.clone());
    tokio::spawn(relay_loop(cfg, rc.clone(), rx));

    // Cold start: give the relay a bounded moment to deliver the first
    // snapshot, so the very first edit in a repo is still arbitrated. If the
    // relay is unreachable we fall through and fail open, as designed.
    for _ in 0..COLD_START_WAIT_MS / 10 {
        if *rc.ready.lock().unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Some(rc)
}

/// Owns the WebSocket to the relay. Reconnects forever with backoff.
async fn relay_loop(cfg: RepoConfig, rc: Arc<RepoConn>, mut rx: mpsc::UnboundedReceiver<ClientMsg>) {
    loop {
        match tokio_tungstenite::connect_async(&cfg.relay).await {
            Ok((ws, _)) => {
                *rc.connected.lock().unwrap() = true;
                let (mut w, mut r) = ws.split();
                let hello = ClientMsg::Hello { repo: cfg.repo.clone(), daemon: whoami() };
                if w.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.is_err() {
                    *rc.connected.lock().unwrap() = false;
                    continue;
                }
                loop {
                    tokio::select! {
                        out = rx.recv() => {
                            let Some(msg) = out else { return };
                            if w.send(WsMsg::Text(serde_json::to_string(&msg).unwrap())).await.is_err() { break; }
                        }
                        inc = r.next() => {
                            let Some(Ok(WsMsg::Text(t))) = inc else { break };
                            let Ok(sm) = serde_json::from_str::<ServerMsg>(&t) else { continue };
                            match sm {
                                ServerMsg::Welcome { claims, sessions, .. } => {
                                    {
                                        let mut v = rc.view.lock().unwrap();
                                        v.claims = claims;
                                        v.sessions = sessions.into_iter().map(|s| (s.session.clone(), s)).collect();
                                    }
                                    *rc.ready.lock().unwrap() = true;
                                }
                                ServerMsg::Event { event, .. } => {
                                    rc.view.lock().unwrap().apply(&event);
                                }
                                ServerMsg::ClaimResp { ref id, .. } => {
                                    if let Some(tx) = rc.pending.lock().unwrap().remove(id) {
                                        let _ = tx.send(sm);
                                    }
                                }
                            }
                        }
                    }
                }
                *rc.connected.lock().unwrap() = false;
            }
            Err(_) => {
                *rc.connected.lock().unwrap() = false;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
