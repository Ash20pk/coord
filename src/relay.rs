use crate::proto::*;
use anyhow::Result;
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Query, State},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

struct RepoState {
    view: View,
    seq: u64,
    tx: broadcast::Sender<(u64, Event)>,
}

struct App {
    repos: Mutex<HashMap<String, RepoState>>,
    db: Mutex<rusqlite::Connection>,
}

impl App {
    /// Sequence, persist, apply, broadcast. The heart of the relay.
    fn commit(&self, repo: &str, ev: Event) -> u64 {
        let seq = {
            let mut repos = self.repos.lock().unwrap();
            let st = repos.get_mut(repo).expect("repo registered");
            st.seq += 1;
            st.view.apply(&ev);
            let _ = st.tx.send((st.seq, ev.clone()));
            st.seq
        };
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO events (repo, seq, ts, json) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![repo, seq, now_ms(), serde_json::to_string(&ev).unwrap()],
        );
        seq
    }
}

/// Bind and serve in the background; returns the actual bound address.
/// Used by tests (port 0) and by `run`.
pub async fn start(listen: &str, db_path: PathBuf) -> Result<std::net::SocketAddr> {
    let (listener, app) = prepare(listen, db_path).await?;
    let addr = listener.local_addr()?;
    let router = routes(app);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(addr)
}

async fn prepare(listen: &str, db_path: PathBuf) -> Result<(tokio::net::TcpListener, Arc<App>)> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
            repo TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_repo_seq ON events (repo, seq);",
    )?;
    let app = Arc::new(App { repos: Mutex::new(HashMap::new()), db: Mutex::new(conn) });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    Ok((listener, app))
}

pub async fn run(listen: String, db_path: PathBuf) -> Result<()> {
    let (listener, app) = prepare(&listen, db_path.clone()).await?;
    let router = routes(app);
    let shown = listen.replace("0.0.0.0", "127.0.0.1");
    eprintln!("coord relay listening on ws://{listen}/ws (audit log: {})", db_path.display());
    eprintln!("  dashboard: http://{shown}/");
    axum::serve(listener, router).await?;
    Ok(())
}

fn routes(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("dashboard.html")) }))
        .route("/api/repos", get(repos_handler))
        .route("/api/events", get(events_handler))
        .route("/ws", get(ws_handler))
        .with_state(app)
}

/// Repos this relay has seen, live ones first.
async fn repos_handler(State(app): State<Arc<App>>) -> impl IntoResponse {
    let mut live: Vec<String> = app.repos.lock().unwrap().keys().cloned().collect();
    live.sort();
    let db = app.db.lock().unwrap();
    if let Ok(mut q) = db.prepare("SELECT DISTINCT repo FROM events") {
        if let Ok(rows) = q.query_map([], |r| r.get::<_, String>(0)) {
            for repo in rows.flatten() {
                if !live.contains(&repo) {
                    live.push(repo);
                }
            }
        }
    }
    Json(live)
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    repo: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// Recent history, so the page is useful the moment it is opened rather than
/// only from the connection onwards.
async fn events_handler(
    State(app): State<Arc<App>>,
    Query(q): Query<EventsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(400).min(2000);
    let db = app.db.lock().unwrap();
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT json FROM (SELECT seq, json FROM events WHERE repo = ?1 \
         ORDER BY seq DESC LIMIT ?2) ORDER BY seq ASC",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![q.repo, limit], |r| r.get::<_, String>(0))
        {
            for j in rows.flatten() {
                if let Ok(v) = serde_json::from_str(&j) {
                    out.push(v);
                }
            }
        }
    }
    Json(out)
}

async fn ws_handler(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> impl IntoResponse {
    ws.on_upgrade(move |sock| async move {
        let _ = client(sock, app).await;
    })
}

async fn client(sock: WebSocket, app: Arc<App>) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = sock.split();

    // First message must be Hello.
    let repo = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t)? {
                ClientMsg::Hello { repo, .. } => break repo,
                _ => anyhow::bail!("expected Hello"),
            },
            Some(Ok(_)) => continue,
            _ => return Ok(()),
        }
    };

    // Register repo, snapshot state, subscribe to its broadcast.
    let (welcome, mut rx) = {
        let mut repos = app.repos.lock().unwrap();
        let st = repos.entry(repo.clone()).or_insert_with(|| RepoState {
            view: View::default(),
            seq: 0,
            tx: broadcast::channel(4096).0,
        });
        st.view.prune();
        (
            ServerMsg::Welcome {
                seq: st.seq,
                claims: st.view.claims.clone(),
                sessions: st.view.sessions.values().cloned().collect(),
            },
            st.tx.subscribe(),
        )
    };

    // Single writer task; both the broadcast pump and request handling feed it.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    out_tx.send(welcome)?;
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let txt = serde_json::to_string(&msg).unwrap();
            if ws_tx.send(Message::Text(txt)).await.is_err() {
                break;
            }
        }
    });
    let pump_tx = out_tx.clone();
    let pump = tokio::spawn(async move {
        while let Ok((seq, event)) = rx.recv().await {
            if pump_tx.send(ServerMsg::Event { seq, event }).is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(t) = msg else { continue };
        let Ok(cm) = serde_json::from_str::<ClientMsg>(&t) else { continue };
        match cm {
            ClientMsg::Hello { .. } => {}
            ClientMsg::Append { event } => {
                app.commit(&repo, event);
            }
            ClientMsg::ReleaseSession { session } => {
                app.commit(&repo, Event::SessionEnded { session, ts: now_ms() });
            }
            ClientMsg::ClaimReq { id, session, user, path, intent } => {
                // Arbitration: the one decision only the relay may make.
                let verdict = {
                    let mut repos = app.repos.lock().unwrap();
                    let st = repos.get_mut(&repo).unwrap();
                    st.view.prune();
                    st.view.conflicting(&session, &path).cloned()
                };
                match verdict {
                    Some(holder) => {
                        // Record the collision before answering — this is the
                        // number the whole product exists to reduce.
                        app.commit(
                            &repo,
                            Event::ClaimDenied {
                                session: session.clone(),
                                user: user.clone(),
                                path: path.clone(),
                                holder: holder.session.clone(),
                                holder_user: holder.user.clone(),
                                ts: now_ms(),
                            },
                        );
                        let _ = out_tx.send(ServerMsg::ClaimResp {
                            id,
                            granted: false,
                            holder: Some(holder.session),
                            holder_user: Some(holder.user),
                            holder_intent: Some(holder.intent),
                            lease_until: Some(holder.lease_until),
                        });
                    }
                    None => {
                        let lease_until = now_ms() + LEASE_MS;
                        app.commit(
                            &repo,
                            Event::ClaimAcquired {
                                session: session.clone(),
                                user,
                                path,
                                lease_until,
                                intent,
                            },
                        );
                        let _ = out_tx.send(ServerMsg::ClaimResp {
                            id,
                            granted: true,
                            holder: None,
                            holder_user: None,
                            holder_intent: None,
                            lease_until: Some(lease_until),
                        });
                    }
                }
            }
        }
    }

    pump.abort();
    writer.abort();
    Ok(())
}
