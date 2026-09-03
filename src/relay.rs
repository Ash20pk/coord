use crate::proto::*;
use anyhow::Result;
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path as AxPath, Query, State},
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
    /// Live agent terminals. Only present when the relay was asked to host a
    /// lab; a plain relay spawns no processes.
    terms: Option<Arc<crate::term::Terms>>,
    /// Shared team secret every client must present. `None` means an open
    /// relay, which is fine on loopback and nowhere else. Predates teams and
    /// still works: it resolves to the built-in `root` team so an existing
    /// deployment keeps running across this upgrade.
    token: Option<String>,
    /// Open registration needs a brake. Five teams per hour per address is
    /// generous for a human and useless for a script.
    reg_limit: crate::teams::RateLimit,
}

impl App {
    /// Claims a session currently holds, with the intent behind them.
    fn held_by(&self, repo: &str, session: &str) -> Vec<(String, String, String)> {
        let mut repos = self.repos.lock().unwrap();
        let Some(st) = repos.get_mut(repo) else { return Vec::new() };
        st.view.prune();
        st.view
            .claims
            .iter()
            .filter(|c| c.session == session)
            .map(|c| (c.path.clone(), c.user.clone(), c.intent.clone()))
            .collect()
    }

    /// Tell anyone waiting that a path is theirs to take. Without this a
    /// blocked peer waits forever on a lease it cannot observe.
    fn announce_freed(&self, repo: &str, session: &str, freed: Vec<(String, String, String)>) {
        for (path, user, intent) in freed {
            let has_waiters = {
                let repos = self.repos.lock().unwrap();
                repos
                    .get(repo)
                    .map(|st| !st.view.waiters_for(&path, session).is_empty())
                    .unwrap_or(false)
            };
            if has_waiters {
                self.commit(
                    repo,
                    Event::PathFreed {
                        path,
                        by_session: session.to_string(),
                        by_user: user,
                        intent,
                        ts: now_ms(),
                    },
                );
            }
        }
    }

    /// Rebuild a repo's in-memory state from the durable log.
    ///
    /// Without this a restart began again at seq 0 — writing duplicate
    /// sequence numbers into a log whose whole purpose is to be sequenced —
    /// and came back with no claims and no presence, so two agents could hold
    /// the same file across a restart and the dashboard showed an empty repo
    /// that plainly was not. Leases are minutes long, so replaying a recent
    /// tail is enough to reconstruct everything still live; `prune` drops
    /// whatever expired while we were down.
    /// Must be called *without* `self.repos` held: it takes the `db` lock, and
    /// every other path takes `repos` before `db`.
    fn load_repo(&self, repo: &str) -> RepoState {
        const REPLAY: usize = 5_000;
        let mut st = RepoState {
            view: View::default(),
            seq: 0,
            tx: broadcast::channel(4096).0,
        };
        let db = self.db.lock().unwrap();
        st.seq = db
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE repo = ?1",
                rusqlite::params![repo],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as u64;
        if let Ok(mut q) = db.prepare(
            "SELECT json FROM (SELECT seq, json FROM events WHERE repo = ?1 \
             ORDER BY seq DESC LIMIT ?2) ORDER BY seq ASC",
        ) {
            if let Ok(rows) = q.query_map(rusqlite::params![repo, REPLAY], |r| r.get::<_, String>(0))
            {
                for j in rows.flatten() {
                    if let Ok(ev) = serde_json::from_str::<Event>(&j) {
                        st.view.apply(&ev);
                    }
                }
            }
        }
        st.view.prune();
        st
    }

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
    start_with_token(listen, db_path, relay_token()).await
}

/// As `start`, with the required token passed in rather than read from the
/// environment. Tests need this: a process-wide env var cannot describe two
/// relays, and reading it at construction is the right shape anyway.
pub async fn start_with_token(
    listen: &str,
    db_path: PathBuf,
    token: Option<String>,
) -> Result<std::net::SocketAddr> {
    let (listener, app) = prepare_with_token(listen, db_path, token).await?;
    let addr = listener.local_addr()?;
    let router = routes(app);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(addr)
}

async fn prepare(listen: &str, db_path: PathBuf) -> Result<(tokio::net::TcpListener, Arc<App>)> {
    prepare_with_token(listen, db_path, relay_token()).await
}

async fn prepare_with_token(
    listen: &str,
    db_path: PathBuf,
    token: Option<String>,
) -> Result<(tokio::net::TcpListener, Arc<App>)> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&db_path)?;
    configure_sqlite(&conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
            repo TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_repo_seq ON events (repo, seq);",
    )?;
    crate::teams::init_schema(&conn)?;
    let app = Arc::new(App {
        repos: Mutex::new(HashMap::new()),
        db: Mutex::new(conn),
        terms: None,
        token,
        reg_limit: crate::teams::RateLimit::new(5, 60 * 60 * 1000),
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    Ok((listener, app))
}

/// Durability settings for the event log.
///
/// WAL is not a performance tweak here: continuous replication (Litestream and
/// everything like it) reads the write-ahead log, and against a rollback-
/// journal database it silently replicates nothing at all. A relay whose log
/// is not replicable is a relay whose log is one disk away from gone, so this
/// is asserted by a test rather than left to a comment.
pub fn configure_sqlite(conn: &rusqlite::Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "could not enable WAL (got {mode})");
    // Safe with WAL: a crash can lose the tail of the last transaction group,
    // never the database. Full fsync per commit would put a disk flush on the
    // claim path, which is the one path that must stay in single-digit ms.
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 1000;",
    )?;
    Ok(())
}

pub struct LabOpts {
    pub dir: PathBuf,
    pub agents: Vec<String>,
    pub program: String,
}

/// Leases expire without anyone acting, so nothing would announce those paths.
/// This sweeps for them and notifies whoever was waiting.
fn spawn_expiry_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            let expired: Vec<(String, String, String, String, String)> = {
                let mut repos = app.repos.lock().unwrap();
                let now = now_ms();
                let mut out = Vec::new();
                for (repo, st) in repos.iter_mut() {
                    for c in st.view.claims.iter().filter(|c| c.lease_until <= now) {
                        if !st.view.waiters_for(&c.path, &c.session).is_empty() {
                            out.push((
                                repo.clone(),
                                c.session.clone(),
                                c.path.clone(),
                                c.user.clone(),
                                c.intent.clone(),
                            ));
                        }
                    }
                    st.view.prune();
                }
                out
            };
            for (repo, session, path, user, intent) in expired {
                app.commit(
                    &repo,
                    Event::PathFreed {
                        path,
                        by_session: session,
                        by_user: user,
                        intent: format!("{intent} (lease expired)"),
                        ts: now_ms(),
                    },
                );
            }
        }
    });
}

pub async fn run(listen: String, db_path: PathBuf, lab: Option<LabOpts>) -> Result<()> {
    let (listener, mut app) = prepare(&listen, db_path.clone()).await?;
    if let Some(l) = lab {
        let terms = crate::term::Terms::spawn(&l.dir, &l.agents, &l.program)?;
        eprintln!("  lab terminals: {} in {}", l.agents.join(", "), l.dir.display());
        Arc::get_mut(&mut app).expect("sole owner before serving").terms = Some(terms);
    }
    let has_terms = app.terms.is_some();
    spawn_expiry_sweeper(app.clone());
    let router = routes(app);
    let shown = listen.replace("0.0.0.0", "127.0.0.1");
    eprintln!("knoot relay listening on ws://{listen}/ws (audit log: {})", db_path.display());
    match relay_token() {
        Some(_) => eprintln!("  auth:      token required (KNOOT_RELAY_TOKEN)"),
        None => {
            let loopback = listen.starts_with("127.0.0.1") || listen.starts_with("localhost");
            if loopback {
                eprintln!("  auth:      none (loopback only)");
            } else {
                // Not a hard failure: an operator may have a proxy in front.
                // But an unauthenticated relay on a public interface hands
                // anyone the event log and, in lab mode, a shell.
                eprintln!(
                    "  auth:      NONE, and {listen} is not loopback. Set KNOOT_RELAY_TOKEN \
                     unless something in front of this is doing authentication."
                );
            }
        }
    }
    eprintln!("  dashboard: http://{shown}/");
    if has_terms {
        eprintln!("  lab:       http://{shown}/lab");
    }
    axum::serve(listener, router).await?;
    Ok(())
}

/// The token this relay requires, if any. A relay with no token set is open —
/// which is right for `127.0.0.1` and wrong for anything hosted, so `serve`
/// says so out loud at startup.
pub fn relay_token() -> Option<String> {
    crate::config::env_or_legacy("KNOOT_RELAY_TOKEN")
}

/// Constant-time-ish comparison. Tokens are short and this is not the weak
/// point of the system, but there is no reason to leak length or prefix.
fn token_matches(expected: &str, got: &str) -> bool {
    if expected.len() != got.len() {
        return false;
    }
    expected.bytes().zip(got.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The token a request presents: `Authorization: Bearer`, or `?token=` for a
/// browser, which cannot set headers on a WebSocket or an `EventSource`.
fn presented(headers: &axum::http::HeaderMap, query: Option<&str>) -> Option<String> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string());
    bearer.filter(|b| !b.is_empty()).or_else(|| {
        query.and_then(|q| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "token")
                .and_then(|(_, v)| urldecode(v))
                .filter(|v| !v.is_empty())
        })
    })
}

/// `?token=` arrives percent-encoded. Only `%XX` and `+` matter here.
fn urldecode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Who this request speaks for, or `None` if it may not proceed.
///
/// Three ways in, in order: a team token from the database, the legacy shared
/// secret (which is the `root` team), or — on a relay started with no secret
/// at all — the built-in `local` team, because a loopback relay must keep
/// working with no setup whatsoever.
fn identify(
    app: &App,
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Option<crate::teams::Identity> {
    let tok = presented(headers, query);

    if let Some(t) = tok.as_deref() {
        let db = app.db.lock().unwrap();
        if let Some(id) = crate::teams::resolve(&db, t) {
            return Some(id);
        }
    }

    match (&app.token, tok.as_deref()) {
        // A configured secret, presented correctly.
        (Some(expected), Some(got)) if token_matches(expected, got) => {
            Some(crate::teams::Identity {
                team_id: "root".into(),
                team_name: "root".into(),
                token_id: "root".into(),
            })
        }
        // A configured secret, and this is not it.
        (Some(_), _) => None,
        // No secret configured, and a token was presented anyway: it did not
        // resolve above, so it is wrong — a revoked one, or a typo. Falling
        // back to the anonymous identity here would mean a *revoked* token
        // still opened a console, and would hand it the `local` identity that
        // gates the lab's ptys. Presenting a bad credential is a refusal;
        // only presenting none is anonymous.
        (None, Some(_)) => None,
        // No secret configured and nothing presented: an open relay, which is
        // what makes a loopback relay work with no setup at all.
        (None, None) => Some(crate::teams::Identity {
            team_id: "local".into(),
            team_name: "local".into(),
            token_id: "local".into(),
        }),
    }
}

/// The caller's address for rate-limiting. Behind Caddy the socket is always
/// loopback, so the forwarded header is the only thing that distinguishes
/// callers; a direct connection falls back to the peer address.
fn caller_key(headers: &axum::http::HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "unknown".into()))
}

fn routes(app: Arc<App>) -> Router {
    Router::new()
        // The public site. `/` is what someone who was sent a link sees, so it
        // explains the thing; `/app` is the team console; `/ops` is the
        // original single-team operator view, kept because deployments and
        // muscle memory point at it.
        .route("/", get(|| async { Html(include_str!("site.html")) }))
        .route("/app", get(|| async { Html(include_str!("app.html")) }))
        .route("/ops", get(|| async { Html(include_str!("dashboard.html")) }))
        .route("/lab", get(|| async { Html(include_str!("lab.html")) }))
        .route("/api/terms", get(terms_handler))
        .route("/term/ws/:idx", get(term_ws_handler))
        .route("/api/repos", get(repos_handler))
        .route("/api/events", get(events_handler))
        .route("/api/register", axum::routing::post(register_handler))
        .route("/api/team", get(team_handler))
        .route("/api/tokens", axum::routing::post(mint_handler))
        .route("/api/tokens/:id/revoke", axum::routing::post(revoke_handler))
        .route("/ws", get(ws_handler))
        .with_state(app)
}

#[derive(serde::Deserialize)]
struct RegisterBody {
    team: String,
}

/// Open registration: a name in, a team and its first token out. No email, no
/// password, nothing to reset — the token *is* the account, which is the same
/// trade the CLI already makes.
async fn register_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterBody>,
) -> axum::response::Response {
    if !app.reg_limit.check(&caller_key(&headers, None)) {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "too many teams from this address — try again in an hour"
            })),
        )
            .into_response();
    }
    let db = app.db.lock().unwrap();
    match crate::teams::create_team(&db, &body.team) {
        Ok((id, tok)) => Json(serde_json::json!({
            "team_id": id.team_id,
            "team": id.team_name,
            "token": tok.secret,
            "token_id": tok.id,
        }))
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Everything the console needs about the caller's own team. Never another's.
async fn team_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    let db = app.db.lock().unwrap();
    let tokens = crate::teams::list_tokens(&db, &id.team_id);
    Json(serde_json::json!({
        "team_id": id.team_id,
        "team": id.team_name,
        "token_id": id.token_id,
        "tokens": tokens,
        "repos": repos_for(&app, &db, &id),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct MintBody {
    #[serde(default)]
    label: String,
}

async fn mint_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<MintBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    if id.team_id == "local" || id.team_id == "root" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "this relay's token is configured in its environment, not in the \
                          database. Register a team to manage tokens here."
            })),
        )
            .into_response();
    }
    let db = app.db.lock().unwrap();
    match crate::teams::mint_token(&db, &id.team_id, &body.label) {
        Ok(t) => Json(serde_json::json!({ "token": t.secret, "token_id": t.id })).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn revoke_handler(
    State(app): State<Arc<App>>,
    AxPath(token_id): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    let db = app.db.lock().unwrap();
    match crate::teams::revoke(&db, &id.team_id, &token_id) {
        Ok(()) => Json(serde_json::json!({ "revoked": token_id })).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// This team's repos, by their team-local names.
fn repos_for(
    app: &App,
    db: &rusqlite::Connection,
    id: &crate::teams::Identity,
) -> Vec<serde_json::Value> {
    let prefix = format!("{}/", id.team_id);
    let mut keys: Vec<String> = app
        .repos
        .lock()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    if let Ok(mut q) = db.prepare("SELECT DISTINCT repo FROM events WHERE repo LIKE ?1") {
        if let Ok(rows) = q.query_map(rusqlite::params![format!("{prefix}%")], |r| {
            r.get::<_, String>(0)
        }) {
            for k in rows.flatten() {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
    }
    keys.sort();
    keys.iter()
        .map(|k| {
            let live = app.repos.lock().unwrap().get(k).map(|st| st.seq).unwrap_or(0);
            serde_json::json!({ "repo": id.unscope(k), "seq": live })
        })
        .collect()
}

/// The agent terminals this relay is hosting, if any.
async fn terms_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    // Same rule as the pty itself: a registered team has no business seeing
    // the operator's terminals, let alone attaching to one.
    match identify(&app, &headers, uri.query()) {
        Some(id) if id.team_id == "root" || id.team_id == "local" => {}
        _ => return unauthorized(),
    }
    terms_body(app).await.into_response()
}

async fn terms_body(app: Arc<App>) -> impl IntoResponse {
    match &app.terms {
        Some(t) => Json(serde_json::json!({ "dir": t.dir, "agents": t.names() })),
        None => Json(serde_json::json!({ "dir": null, "agents": [] })),
    }
}

/// Bridge a browser terminal to its pty. Binary frames carry keystrokes and
/// output; text frames carry control messages (resize).
async fn term_ws_handler(
    ws: WebSocketUpgrade,
    AxPath(idx): AxPath<usize>,
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    // A terminal is a shell on the host. If anything on this relay is gated,
    // this is — and since anyone may register a team, being authenticated is
    // not enough: only the operator's own credential reaches a pty.
    match identify(&app, &headers, uri.query()) {
        Some(id) if id.team_id == "root" || id.team_id == "local" => {}
        _ => return unauthorized(),
    }
    let term = app.terms.as_ref().and_then(|t| t.get(idx));
    ws.on_upgrade(move |sock| async move {
        let Some(term) = term else { return };
        let (mut tx_ws, mut rx_ws) = sock.split();
        let (history, mut rx) = term.subscribe();

        // Replay scrollback so a reload shows the session as it stands.
        if !history.is_empty() && tx_ws.send(Message::Binary(history)).await.is_err() {
            return;
        }
        let pump = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(chunk) => {
                        if tx_ws.send(Message::Binary(chunk)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });

        while let Some(Ok(msg)) = rx_ws.next().await {
            match msg {
                Message::Binary(b) => term.write_input(&b),
                Message::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        match (v["cols"].as_u64(), v["rows"].as_u64()) {
                            (Some(c), Some(r)) => term.resize(c as u16, r as u16),
                            _ => term.write_input(t.as_bytes()),
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        pump.abort();
    })
}

/// Repos this relay has seen, live ones first.
async fn repos_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    let db = app.db.lock().unwrap();
    // Team-local names only. The unscoped list would name other teams' repos.
    let names: Vec<String> = repos_for(&app, &db, &id)
        .into_iter()
        .filter_map(|v| v["repo"].as_str().map(str::to_string))
        .collect();
    Json(names).into_response()
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
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    // The client asks for `api`; storage is keyed `t_xxxx/api`. A caller
    // cannot reach another team's log by naming it, because the name it sends
    // is always rewritten with its own team id.
    let scoped = EventsQuery { repo: id.scope(&q.repo), limit: q.limit };
    events_body(app, scoped).await.into_response()
}

async fn events_body(app: Arc<App>, q: EventsQuery) -> impl IntoResponse {
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()) else {
        return unauthorized();
    };
    ws.on_upgrade(move |sock| async move {
        let _ = client(sock, app, id).await;
    })
    .into_response()
}

/// A refusal an operator can read in a log and a client can act on. Never a
/// silent drop: a daemon that cannot tell "rejected" from "unreachable" cannot
/// tell the human why coordination stopped.
fn unauthorized() -> axum::response::Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        "knoot relay: missing or invalid token. Run `knoot login --relay <url> --token <token>`, \
         or set KNOOT_TOKEN.\n",
    )
        .into_response()
}

async fn client(sock: WebSocket, app: Arc<App>, id: crate::teams::Identity) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = sock.split();

    // First message must be Hello.
    let repo = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t)? {
                // Namespaced here, once, so nothing downstream can address a
                // repo outside the caller's team.
                ClientMsg::Hello { repo, .. } => break id.scope(&repo),
                _ => anyhow::bail!("expected Hello"),
            },
            Some(Ok(_)) => continue,
            _ => return Ok(()),
        }
    };

    // Recover the repo from the durable log *before* taking the map lock.
    // Doing it inside the closure would hold `repos` while locking `db`, and
    // the team API locks those two the other way round — a deadlock that would
    // hang the whole relay the first time a console call raced a new session.
    let recovered = if app.repos.lock().unwrap().contains_key(&repo) {
        None
    } else {
        Some(app.load_repo(&repo))
    };

    // Register repo, snapshot state, subscribe to its broadcast.
    let (welcome, mut rx) = {
        let mut repos = app.repos.lock().unwrap();
        if let Some(fresh) = recovered {
            // `entry` rather than `insert`: another session may have won the
            // race between the check above and this lock.
            repos.entry(repo.clone()).or_insert(fresh);
        }
        let st = repos.get_mut(&repo).expect("just inserted");
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
                // A release frees a path someone may be blocked on.
                let freed = match &event {
                    Event::ClaimReleased { session, path, .. } => {
                        let held = app.held_by(&repo, session);
                        held.into_iter().filter(|(p, _, _)| p == path).collect()
                    }
                    Event::SessionEnded { session, .. } => app.held_by(&repo, session),
                    _ => Vec::new(),
                };
                let who = match &event {
                    Event::ClaimReleased { session, .. } | Event::SessionEnded { session, .. } => {
                        session.clone()
                    }
                    _ => String::new(),
                };
                app.commit(&repo, event);
                if !freed.is_empty() {
                    app.announce_freed(&repo, &who, freed);
                }
            }
            ClientMsg::ReleaseSession { session } => {
                let freed = app.held_by(&repo, &session);
                app.commit(&repo, Event::SessionEnded { session: session.clone(), ts: now_ms() });
                app.announce_freed(&repo, &session, freed);
            }
            ClientMsg::ClaimReq { id, session, user, path, intent, branch } => {
                // Arbitration: the one decision only the relay may make.
                let verdict = {
                    let mut repos = app.repos.lock().unwrap();
                    let st = repos.get_mut(&repo).unwrap();
                    st.view.prune();
                    st.view.conflicting_on(&session, &path, &branch).cloned()
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
                                branch,
                                ts: now_ms(),
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


#[cfg(test)]
mod auth_tests {
    use super::*;

    fn app_with(token: Option<&str>) -> App {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::teams::init_schema(&conn).unwrap();
        App {
            repos: Mutex::new(HashMap::new()),
            db: Mutex::new(conn),
            terms: None,
            token: token.map(str::to_string),
            reg_limit: crate::teams::RateLimit::new(5, 60_000),
        }
    }

    fn bearer(v: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, v.parse().unwrap());
        h
    }

    fn team_of(app: &App, headers: &axum::http::HeaderMap, q: Option<&str>) -> Option<String> {
        identify(app, headers, q).map(|i| i.team_id)
    }

    #[test]
    fn an_open_relay_accepts_anything_as_the_local_team() {
        let app = app_with(None);
        assert_eq!(team_of(&app, &axum::http::HeaderMap::new(), None).as_deref(), Some("local"));
    }

    #[test]
    fn a_token_relay_needs_the_token() {
        let app = app_with(Some("sekrit"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, None).is_none());
        assert_eq!(team_of(&app, &bearer("Bearer sekrit"), None).as_deref(), Some("root"));
        assert!(identify(&app, &bearer("Bearer nope"), None).is_none());
        assert!(
            identify(&app, &bearer("sekrit"), None).is_none(),
            "must be a Bearer token"
        );
    }

    /// Browsers cannot set headers on a websocket or an <img>, so the query
    /// form exists; it must be exactly as strict.
    #[test]
    fn the_query_form_works_and_is_just_as_strict() {
        let app = app_with(Some("sekrit"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, Some("token=sekrit")).is_some());
        assert!(identify(&app, &none, Some("repo=x&token=sekrit")).is_some());
        assert!(identify(&app, &none, Some("token=nope")).is_none());
        assert!(identify(&app, &none, Some("repo=x")).is_none());
        assert!(identify(&app, &none, Some("token=sekritextra")).is_none());
    }

    #[test]
    fn a_percent_encoded_query_token_still_works() {
        let app = app_with(Some("a b+c"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, Some("token=a%20b%2Bc")).is_some());
    }

    #[test]
    fn comparison_does_not_short_circuit_on_length() {
        assert!(token_matches("abcd", "abcd"));
        assert!(!token_matches("abcd", "abc"));
        assert!(!token_matches("abcd", "abcde"));
        assert!(!token_matches("abcd", "abce"));
    }

    /// A registered team's token works on a relay that also has a configured
    /// secret, and resolves to that team rather than to root.
    #[test]
    fn a_registered_team_token_resolves_to_its_own_team() {
        let app = app_with(Some("sekrit"));
        let (id, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme").unwrap()
        };
        let got = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).unwrap();
        assert_eq!(got.team_id, id.team_id);
        assert_ne!(got.team_id, "root");
    }

    /// The property the whole multi-team story rests on: two teams naming the
    /// same repo address different storage keys.
    #[test]
    fn two_teams_naming_one_repo_never_share_a_log() {
        let app = app_with(None);
        let (a, ta) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "A").unwrap()
        };
        let (b, tb) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "B").unwrap()
        };
        let ia = identify(&app, &bearer(&format!("Bearer {}", ta.secret)), None).unwrap();
        let ib = identify(&app, &bearer(&format!("Bearer {}", tb.secret)), None).unwrap();
        assert_eq!(ia.team_id, a.team_id);
        assert_eq!(ib.team_id, b.team_id);
        assert_ne!(ia.scope("api"), ib.scope("api"));
    }

    /// Registration is open, so a stranger holding a valid team token must not
    /// thereby hold a shell on the host. Only the operator's own credential
    /// reaches the lab.
    #[test]
    fn a_registered_team_is_not_an_operator() {
        let app = app_with(Some("sekrit"));
        let (_, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Stranger").unwrap()
        };
        let id = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).unwrap();
        assert!(
            id.team_id != "root" && id.team_id != "local",
            "a registered team must not pass the operator check that gates ptys"
        );
    }

    #[test]
    fn a_revoked_token_is_refused_not_downgraded() {
        let app = app_with(None);
        let (id, first) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme").unwrap()
        };
        {
            let db = app.db.lock().unwrap();
            crate::teams::mint_token(&db, &id.team_id, "second").unwrap();
            crate::teams::revoke(&db, &id.team_id, &first.id).unwrap();
        }
        // Not a downgrade to the anonymous identity — a refusal. Anything
        // else would let a revoked token keep opening a console, and hand it
        // the identity that gates the lab's terminals.
        assert!(
            identify(&app, &bearer(&format!("Bearer {}", first.secret)), None).is_none(),
            "a revoked token must be refused outright, not treated as anonymous"
        );
    }
}
