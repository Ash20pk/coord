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
    /// How this repo divides itself into areas, as last declared by a client
    /// in its Hello.
    ///
    /// The relay never sees the repo, so this is the only way it can learn
    /// the map — and it needs the map, because an event's area decides who
    /// hears about it and every connection must agree on the answer. Empty
    /// until some client says otherwise, which is the whole repo in `/`:
    /// exactly the behaviour of every repo before areas existed.
    areas: Vec<crate::config::AreaDef>,
    /// Shards published on this repo, fanned out to the connections whose keys
    /// hold the scope. Separate from the event channel because a shard is not
    /// an event: it is not sequenced with the log, and a client that ignores
    /// memory entirely must not have to skip past it.
    mem_tx: broadcast::Sender<crate::memory::Shard>,
    /// Shards that have been deleted, fanned out as ids. Ids only: a client
    /// that never held the shard has nothing to drop, and one that did knows
    /// what it is dropping.
    forget_tx: broadcast::Sender<Vec<String>>,
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
    /// Supabase, when this relay is attached to a project. `None` on a
    /// self-hosted relay, where agent tokens are the only credential.
    cloud: Option<crate::cloud::Cloud>,
    /// Which key provider this deployment seals memory with — `plaintext` for
    /// a relay inside a customer's own network, `mls` for one that hosts other
    /// people's rooms. The relay decides because sealing is a property of the
    /// deployment, and a client that chose for itself could seal shards
    /// nobody else could open.
    provider: String,
    /// Rooms whose handshake log just grew. Carries the room id and nothing
    /// else: a daemon told its room moved asks for the log itself, so the
    /// relay never has to decide who may see which blob on a fan-out.
    ///
    /// On the app, not on a repo: a **room spans repos**. Keying this per repo
    /// meant a commit made while working in one repo never woke the daemons
    /// that were in the same room but a different one, and their group never
    /// formed.
    mls_tx: broadcast::Sender<String>,
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
            areas: Vec::new(),
            mem_tx: broadcast::channel(256).0,
            forget_tx: broadcast::channel(256).0,
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
    crate::rooms::init_schema(&conn)?;
    crate::memory::init_schema(&conn)?;
    crate::mls::init_schema(&conn)?;
    // Every start, not once: a relay can be downgraded and upgraded again, and
    // the migration is keyed on `token_hash`, so running it is free when there
    // is nothing to bring forward.
    match crate::rooms::migrate_tokens(&conn) {
        Ok(0) => {}
        Ok(n) => eprintln!("knoot relay: brought {n} pre-member key(s) forward as devices"),
        // A relay that cannot migrate must still start: the old `tokens` rows
        // are untouched and the operator can be told, but refusing to serve
        // would take coordination down for everyone.
        Err(e) => eprintln!("knoot relay: could not migrate old keys ({e}) — they will not resolve"),
    }
    let app = Arc::new(App {
        repos: Mutex::new(HashMap::new()),
        db: Mutex::new(conn),
        terms: None,
        token,
        reg_limit: crate::teams::RateLimit::new(5, 60 * 60 * 1000),
        cloud: crate::cloud::Cloud::from_env(),
        provider: key_provider_name(),
        mls_tx: broadcast::channel(256).0,
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    Ok((listener, app))
}

/// Which provider this deployment seals memory with.
///
/// `plaintext` is the default and is right for the deployment that ships
/// first: a relay in the customer's own network, where the org is the trust
/// boundary. `mls` is the hosted tier, where it is not. Naming it in the
/// environment rather than inferring it means a relay never quietly changes
/// what it promises about its own storage.
fn key_provider_name() -> String {
    match crate::config::env_or_legacy("KNOOT_KEY_PROVIDER").as_deref() {
        Some(crate::proto::PROVIDER_MLS) => crate::proto::PROVIDER_MLS.into(),
        _ => crate::proto::PROVIDER_PLAINTEXT.into(),
    }
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
async fn identify(
    app: &App,
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Option<crate::teams::Identity> {
    let tok = presented(headers, query);

    if let Some(t) = tok.as_deref() {
        // A machine's token. Resolved locally, with no network, because the
        // hot path has to keep working when everything else is down.
        {
            let db = app.db.lock().unwrap();
            if let Some(id) = crate::teams::resolve(&db, t) {
                return Some(id);
            }
        }
        // A person's console session. Only shapes that are actually JWTs are
        // sent onward, so a revoked agent token is refused here rather than
        // turning into a puzzling network error.
        if let Some(cloud) = &app.cloud {
            if crate::cloud::Cloud::looks_like_jwt(t) {
                if let Some(who) = cloud.principal_for_token(t).await {
                    // The team owns rows in the local database too: repo keys
                    // are namespaced by it, and devices point at it. So does
                    // the person — rooms are enforced here, so the relay keeps
                    // its own row per member, refreshed on every console call.
                    let db = app.db.lock().unwrap();
                    crate::teams::ensure_team(&db, &who.team.id, &who.team.name);
                    let member = crate::rooms::ensure_member(
                        &db,
                        &who.team.id,
                        &who.email,
                        Some(&who.user_id),
                        &who.role,
                    )
                    .ok()?;
                    let areas = crate::rooms::areas_for_member(&db, &member.id);
                    return Some(crate::teams::Identity {
                        team_id: who.team.id,
                        team_name: who.team.name,
                        // A person is not a machine. Nothing in the console is
                        // "this device", so no device row is marked as used.
                        token_id: String::new(),
                        member,
                        areas,
                    });
                }
                return None;
            }
        }
    }

    match (&app.token, tok.as_deref()) {
        // A configured secret, presented correctly.
        (Some(expected), Some(got)) if token_matches(expected, got) => {
            Some(legacy_identity(app, "root"))
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
        (None, None) => Some(legacy_identity(app, "local")),
    }
}

/// The `root` and `local` identities: a relay with a shared secret in its
/// environment, and a loopback relay with no setup at all. Both keep working
/// exactly as they did — that is the fail-open, works-unconfigured property
/// the whole product rests on — and both get a `general` room over every repo
/// so nothing downstream has to special-case "an identity with no areas".
///
/// The member is named rather than blank so provenance is never empty, but it
/// is not *verified*: `attribute_to` is called with `None` for these, and the
/// client's own authorship string stands.
fn legacy_identity(app: &App, who: &str) -> crate::teams::Identity {
    let areas = {
        let db = app.db.lock().unwrap();
        crate::teams::ensure_team(&db, who, who);
        let _ = crate::rooms::general_room(&db, who);
        vec![crate::rooms::Area::everything()]
    };
    crate::teams::Identity {
        team_id: who.into(),
        team_name: who.into(),
        token_id: who.into(),
        member: crate::rooms::Member::legacy(who),
        areas,
    }
}

/// The web front end, built by Vite and embedded at compile time.
///
/// Keeping it inside the binary is what lets `knoot relay` serve its own
/// console with no second deployment, no CORS configuration and no static
/// host to keep in sync with the API it talks to.
static WEB: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/web/dist");

/// One page of the app. Missing means the front end was never built, which is
/// a build mistake rather than a request the visitor got wrong.
fn page(path: &str) -> axum::response::Response {
    match WEB.get_file(path) {
        Some(f) => Html(f.contents()).into_response(),
        None => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "the web front end was not built into this binary — run `npm --prefix web ci && npm --prefix web run build`, then rebuild",
        )
            .into_response(),
    }
}

/// Hashed build assets. The names carry a content hash, so they are safe to
/// cache for a long time; a new build produces new names.
async fn asset_handler(axum::extract::Path(path): axum::extract::Path<String>) -> axum::response::Response {
    let Some(file) = WEB.get_file(format!("assets/{path}")) else {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = match path.rsplit_once('.').map(|(_, e)| e) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        file.contents(),
    )
        .into_response()
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
        .route("/", get(|| async { page("index.html") }))
        .route("/docs", get(|| async { page("docs/index.html") }))
        .route("/docs/", get(|| async { page("docs/index.html") }))
        .route("/app", get(|| async { page("app/index.html") }))
        .route("/app/", get(|| async { page("app/index.html") }))
        .route("/status", get(|| async { page("status/index.html") }))
        .route("/status/", get(|| async { page("status/index.html") }))
        .route("/ops", get(|| async { page("ops/index.html") }))
        .route("/ops/", get(|| async { page("ops/index.html") }))
        .route("/lab", get(|| async { page("lab/index.html") }))
        .route("/lab/", get(|| async { page("lab/index.html") }))
        .route("/assets/*path", get(asset_handler))
        .route("/api/terms", get(terms_handler))
        .route("/term/ws/:idx", get(term_ws_handler))
        .route("/api/repos", get(repos_handler))
        .route("/api/events", get(events_handler))
        .route("/api/register", axum::routing::post(register_handler))
        .route("/api/team", get(team_handler))
        .route("/api/whoami", get(whoami_handler))
        .route("/api/tokens", axum::routing::post(mint_handler))
        .route("/api/tokens/:id/revoke", axum::routing::post(revoke_handler))
        .route("/api/members", axum::routing::post(add_member_handler))
        .route("/api/members/attach", axum::routing::post(attach_handler))
        .route("/api/members/:id/remove", axum::routing::post(remove_member_handler))
        .route("/api/rooms", axum::routing::post(create_room_handler))
        .route("/api/rooms/:id/delete", axum::routing::post(delete_room_handler))
        .route("/api/rooms/:id/areas", axum::routing::post(room_area_handler))
        .route("/api/rooms/:id/members", axum::routing::post(room_member_handler))
        .route("/api/rooms/:id/policy", axum::routing::post(room_policy_handler))
        .route("/ws", get(ws_handler))
        .with_state(app)
}

#[derive(serde::Deserialize)]
struct RegisterBody {
    team: String,
    /// Optional, and only ever used to name the team's first member. Open
    /// registration asks for nothing; a caller that already knows who is
    /// registering can say so and get a real member instead of a placeholder.
    #[serde(default)]
    email: Option<String>,
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
    match crate::teams::create_team(&db, &body.team, body.email.as_deref()) {
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
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    let db = app.db.lock().unwrap();
    let devices = crate::rooms::list_devices(&db, &id.team_id);
    Json(serde_json::json!({
        "team_id": id.team_id,
        "team": id.team_name,
        "token_id": id.token_id,
        "me": me(&id),
        // `tokens` is the name the console has always read. Devices are the
        // same rows with an owner, so the key stays and the shape grows.
        "tokens": devices,
        "members": crate::rooms::list_members(&db, &id.team_id),
        "rooms": crate::rooms::list_rooms(&db, &id.team_id),
        "repos": repos_for(&app, &db, &id),
    }))
    .into_response()
}

/// Who the caller is, as the relay verified it — not as the client described
/// itself. `knoot join` prints this, and the console shows it so a person can
/// see which member their console session speaks for.
fn me(id: &crate::teams::Identity) -> serde_json::Value {
    serde_json::json!({
        "member_id": id.member.id,
        "email": id.member.email,
        "role": id.member.role,
        "unassigned": id.member.unassigned,
        "device_id": id.token_id,
        "areas": id.areas,
    })
}

async fn whoami_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    let rooms = {
        let db = app.db.lock().unwrap();
        crate::rooms::rooms_for_member(&db, &id.member.id)
    };
    Json(serde_json::json!({
        "team_id": id.team_id,
        "team": id.team_name,
        "me": me(&id),
        "rooms": rooms,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct MintBody {
    #[serde(default)]
    label: String,
    /// Which member the key belongs to. Omitted means the caller — the common
    /// case, a person adding a second machine of their own.
    #[serde(default)]
    member: Option<String>,
}

/// Refusal for the two identities whose credential lives in the environment
/// rather than in the database. Nothing about them is editable here, and the
/// old message said exactly that; it now covers members and rooms too.
fn not_in_the_database() -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "this relay's credential is configured in its environment, not in the \
                      database. Register a team to manage members, keys and rooms here."
        })),
    )
        .into_response()
}

fn env_identity(id: &crate::teams::Identity) -> bool {
    id.team_id == "local" || id.team_id == "root"
}

/// Only an owner or an admin may change who is in a team or a room. A member
/// can still mint a key for their own machines, which is the one write that
/// does not widen anybody's access.
fn admin(id: &crate::teams::Identity) -> bool {
    id.member.role == "owner" || id.member.role == "admin"
}

fn forbidden(what: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": format!("only an owner or admin can {what}") })),
    )
        .into_response()
}

fn bad(e: impl std::fmt::Display) -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

async fn mint_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<MintBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if env_identity(&id) {
        return not_in_the_database();
    }
    let member = body.member.unwrap_or_else(|| id.member.id.clone());
    // Minting for someone else hands out a key that speaks as them, so it is
    // an admin action even though minting for yourself is not.
    if member != id.member.id && !admin(&id) {
        return forbidden("mint a key for another member");
    }
    let db = app.db.lock().unwrap();
    match crate::rooms::mint_device(&db, &id.team_id, &member, &body.label) {
        Ok(t) => Json(serde_json::json!({ "token": t.secret, "token_id": t.id, "member": member }))
            .into_response(),
        Err(e) => bad(e),
    }
}

#[derive(serde::Deserialize)]
struct AddMemberBody {
    email: String,
    /// `member` or `admin`. Not `owner`: there is exactly one, made at
    /// registration, and this call is for adding colleagues rather than
    /// handing over the team.
    #[serde(default = "member_role")]
    role: String,
    /// Mint their first device key at the same time, labelled with the
    /// machine it is for.
    ///
    /// Two calls would be tidier, and one is what the situation actually
    /// needs: on a self-hosted relay there is no email to send an invitation
    /// to, so "add a colleague" means "give me a key I can pass to them".
    #[serde(default)]
    label: Option<String>,
}

/// Create a member. The call a self-hosted relay had no way to make.
///
/// Until now a second *person* could only come into being through Supabase —
/// `invite_member` and `accept_invite` — so a relay running with no cloud
/// could mint as many keys as it liked and every one of them named the same
/// human. Rooms, areas and memory provenance are all about *who*, which made
/// this the gap under three phases of work: the tests for areas, memory and
/// MLS each had to reach into the relay's database to invent a colleague.
///
/// Idempotent on the email, and it never changes an existing member's role:
/// `ensure_member` would happily rewrite it, which would let "add
/// ash@example.com as a member" quietly demote the owner. Changing a role is a
/// different operation and does not exist yet; it is not smuggled in here.
async fn add_member_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<AddMemberBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if env_identity(&id) {
        return not_in_the_database();
    }
    if !admin(&id) {
        return forbidden("add a member");
    }
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') || email.len() < 3 {
        return bad("that is not an email address");
    }
    if !matches!(body.role.as_str(), "member" | "admin") {
        return bad("a role is `member` or `admin`");
    }

    let db = app.db.lock().unwrap();
    // Already here: hand back who they are and change nothing.
    if let Some(existing) = crate::rooms::member_by_email(&db, &id.team_id, &email) {
        return Json(serde_json::json!({
            "member": existing.id,
            "email": existing.email,
            "role": existing.role,
            "existing": true,
        }))
        .into_response();
    }
    let member = match crate::rooms::ensure_member(&db, &id.team_id, &email, None, &body.role) {
        Ok(m) => m,
        Err(e) => return bad(e),
    };
    // The key, if one was asked for. Returned once and never stored in the
    // clear, exactly as `/api/tokens` does it.
    let minted = match &body.label {
        Some(label) => match crate::rooms::mint_device(&db, &id.team_id, &member.id, label) {
            Ok(t) => Some(t),
            Err(e) => return bad(e),
        },
        None => None,
    };
    Json(serde_json::json!({
        "member": member.id,
        "email": member.email,
        "role": member.role,
        "existing": false,
        "token": minted.as_ref().map(|t| t.secret.clone()),
        "token_id": minted.as_ref().map(|t| t.id.clone()),
    }))
    .into_response()
}

async fn revoke_handler(
    State(app): State<Arc<App>>,
    AxPath(device_id): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    let db = app.db.lock().unwrap();
    // A person may always kill their own machine's key — a stolen laptop must
    // not wait for an admin — but not somebody else's.
    let owner = crate::rooms::list_devices(&db, &id.team_id)
        .into_iter()
        .find(|d| d.id == device_id)
        .map(|d| d.member_id);
    if owner.as_deref() != Some(id.member.id.as_str()) && !admin(&id) {
        return forbidden("revoke another member's key");
    }
    let owner_member = crate::rooms::list_devices(&db, &id.team_id)
        .into_iter()
        .find(|d| d.id == device_id)
        .map(|d| d.member_id);
    let r = crate::rooms::revoke_device(&db, &id.team_id, &device_id);
    let rooms = owner_member
        .map(|m| crate::rooms::rooms_of_member(&db, &m))
        .unwrap_or_default();
    drop(db);
    match r {
        Ok(()) => {
            // A revoked laptop must leave the groups it was a leaf in, or the
            // key it already holds still opens everything the room writes.
            for room in rooms {
                let _ = app.mls_tx.send(room);
            }
            Json(serde_json::json!({ "revoked": device_id })).into_response()
        }
        Err(e) => bad(e),
    }
}

// ------------------------------------------------------------- members

#[derive(serde::Deserialize)]
struct AttachBody {
    /// The migrated, unassigned member whose keys are being adopted.
    from: String,
    /// The real person taking them over. Omitted means the caller.
    #[serde(default)]
    to: Option<String>,
}

/// Adopt a pre-member key. The console shows migrated keys as "unassigned";
/// this is the button that turns one into a named person's device.
async fn attach_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<AttachBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if env_identity(&id) {
        return not_in_the_database();
    }
    let to = body.to.unwrap_or_else(|| id.member.id.clone());
    if to != id.member.id && !admin(&id) {
        return forbidden("attach a key to another member");
    }
    let db = app.db.lock().unwrap();
    match crate::rooms::attach_devices(&db, &id.team_id, &body.from, &to) {
        Ok(n) => Json(serde_json::json!({ "attached": n, "member": to })).into_response(),
        Err(e) => bad(e),
    }
}

async fn remove_member_handler(
    State(app): State<Arc<App>>,
    AxPath(member_id): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if env_identity(&id) {
        return not_in_the_database();
    }
    if !admin(&id) {
        return forbidden("remove a member");
    }
    let removed = {
        let db = app.db.lock().unwrap();
        crate::rooms::remove_member(&db, &id.team_id, &member_id)
    };
    match removed {
        Ok(rooms) => {
            // Every room they were in is now in the wrong epoch. Waking them
            // is what makes a departure actually rotate a key rather than
            // leave the room sealed under one the departed laptop holds.
            for room in rooms {
                let _ = app.mls_tx.send(room);
            }
            Json(serde_json::json!({ "removed": member_id })).into_response()
        }
        Err(e) => bad(e),
    }
}

// --------------------------------------------------------------- rooms

#[derive(serde::Deserialize)]
struct RoomBody {
    name: String,
}

async fn create_room_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<RoomBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if env_identity(&id) {
        return not_in_the_database();
    }
    if !admin(&id) {
        return forbidden("create a room");
    }
    let db = app.db.lock().unwrap();
    match crate::rooms::create_room(&db, &id.team_id, &body.name) {
        Ok(room) => Json(serde_json::json!({ "room": room })).into_response(),
        Err(e) => bad(e),
    }
}

async fn delete_room_handler(
    State(app): State<Arc<App>>,
    AxPath(room): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if !admin(&id) {
        return forbidden("delete a room");
    }
    let db = app.db.lock().unwrap();
    match crate::rooms::delete_room(&db, &id.team_id, &room) {
        Ok(()) => Json(serde_json::json!({ "deleted": room })).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(serde::Deserialize)]
struct AreaBody {
    /// A team-local repo id, or `*` for every repo in the team.
    repo: String,
    /// A path prefix, or `/` for the whole repo.
    #[serde(default = "root_area")]
    area: String,
    /// True to take the area out of the room again.
    #[serde(default)]
    remove: bool,
}

fn root_area() -> String {
    "/".into()
}

async fn room_area_handler(
    State(app): State<Arc<App>>,
    AxPath(room): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<AreaBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if !admin(&id) {
        return forbidden("change a room's areas");
    }
    let db = app.db.lock().unwrap();
    let r = if body.remove {
        crate::rooms::remove_area(&db, &id.team_id, &room, &body.repo, &body.area)
    } else {
        crate::rooms::add_area(&db, &id.team_id, &room, &body.repo, &body.area)
    };
    match r {
        Ok(()) => Json(serde_json::json!({ "room": room, "repo": body.repo, "area": body.area }))
            .into_response(),
        Err(e) => bad(e),
    }
}

#[derive(serde::Deserialize)]
struct RoomMemberBody {
    member: String,
    #[serde(default = "member_role")]
    role: String,
    #[serde(default)]
    remove: bool,
}

fn member_role() -> String {
    "member".into()
}

async fn room_member_handler(
    State(app): State<Arc<App>>,
    AxPath(room): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<RoomMemberBody>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if !admin(&id) {
        return forbidden("change who is in a room");
    }
    let db = app.db.lock().unwrap();
    let r = if body.remove {
        crate::rooms::remove_room_member(&db, &id.team_id, &room, &body.member)
    } else {
        crate::rooms::add_member(&db, &id.team_id, &room, &body.member, &body.role)
    };
    drop(db);
    match r {
        Ok(()) => {
            // The room's membership just changed, so its MLS group is now
            // wrong: somebody is in it who should not be, or is missing.
            // Waking the room is what turns an admin's click into a key
            // rotation — without it a removal sat there until an unrelated
            // commit happened to move the group.
            let _ = app.mls_tx.send(room.clone());
            Json(serde_json::json!({ "room": room, "member": body.member })).into_response()
        }
        Err(e) => bad(e),
    }
}

/// The room's memory policy (§4.4 of the multiplayer design). Stored now,
/// read in phase 4 — an admin editing it before memory exists changes nothing,
/// which is better than a migration later.
async fn room_policy_handler(
    State(app): State<Arc<App>>,
    AxPath(room): AxPath<String>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    Json(policy): Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    if !admin(&id) {
        return forbidden("change a room's memory policy");
    }
    let db = app.db.lock().unwrap();
    match crate::rooms::set_policy(&db, &id.team_id, &room, &policy) {
        Ok(()) => Json(serde_json::json!({ "room": room })).into_response(),
        Err(e) => bad(e),
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
    match identify(&app, &headers, uri.query()).await {
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
    match identify(&app, &headers, uri.query()).await {
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
    let Some(id) = identify(&app, &headers, uri.query()).await else {
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
    /// Narrow to one file's story: everything about this path, plus the
    /// session-level events — intents, messages — of the sessions that
    /// touched it. Without them a claim is a timestamp and a name; with them
    /// it is a reason.
    #[serde(default)]
    path: Option<String>,
}

/// Recent history, so the page is useful the moment it is opened rather than
/// only from the connection onwards.
async fn events_handler(
    State(app): State<Arc<App>>,
    Query(q): Query<EventsQuery>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
        return unauthorized();
    };
    // The client asks for `api`; storage is keyed `t_xxxx/api`. A caller
    // cannot reach another team's log by naming it, because the name it sends
    // is always rewritten with its own team id.
    let scoped = EventsQuery { repo: id.scope(&q.repo), limit: q.limit, path: q.path };
    events_body(app, scoped).await.into_response()
}

async fn events_body(app: Arc<App>, q: EventsQuery) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(400).min(2000);
    let db = app.db.lock().unwrap();
    if let Some(path) = &q.path {
        return Json(events_for_path(&db, &q.repo, path, limit));
    }
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

/// One file's history: every event that names it, and the session-level
/// events of whoever touched it.
///
/// The log has always held this and nothing ever answered a question about the
/// past — the events were written and read only as a live tail. Two passes
/// rather than one join, because the second pass's key is a set the first pass
/// discovers, and a `LIKE` over a JSON column is not a thing to be clever
/// with.
fn events_for_path(
    db: &rusqlite::Connection,
    repo: &str,
    path: &str,
    limit: usize,
) -> Vec<serde_json::Value> {
    let path = path.trim_start_matches('/');
    // The exact JSON the writer emits. Matching on the quoted pair rather than
    // the bare path is what stops `src/a.rs` finding `src/a.rs.bak` and what
    // stops a path matching an intent that merely mentions it.
    let needle = format!("\"path\":\"{path}\"");
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut sessions: std::collections::HashSet<String> = Default::default();
    // Users as well as sessions: a message sent with `knoot msg` carries no
    // session id, because a CLI caller cannot learn its own. Joining only on
    // sessions dropped exactly the messages a person sent about the file.
    let mut users: std::collections::HashSet<String> = Default::default();

    if let Ok(mut q) = db.prepare(
        "SELECT json FROM (SELECT seq, json FROM events \
         WHERE repo = ?1 AND instr(json, ?2) > 0 ORDER BY seq DESC LIMIT ?3) ORDER BY seq ASC",
    ) {
        if let Ok(rows) = q.query_map(rusqlite::params![repo, needle, limit], |r| {
            r.get::<_, String>(0)
        }) {
            for j in rows.flatten() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) {
                    for key in ["session", "by_session", "from_session"] {
                        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                            sessions.insert(s.to_string());
                        }
                    }
                    for key in ["user", "holder_user", "peer_user", "by_user"] {
                        if let Some(u) = v.get(key).and_then(|x| x.as_str()) {
                            if !u.is_empty() {
                                users.insert(u.to_string());
                            }
                        }
                    }
                    out.push(v);
                }
            }
        }
    }
    if sessions.is_empty() && users.is_empty() {
        return out;
    }

    // What those sessions said. Bounded by the same limit, because a chatty
    // room should not be able to turn one question into the whole log.
    if let Ok(mut q) = db.prepare(
        "SELECT json FROM (SELECT seq, json FROM events WHERE repo = ?1 \
         AND (instr(json, '\"type\":\"intent_declared\"') > 0 \
              OR instr(json, '\"type\":\"message\"') > 0 \
              OR instr(json, '\"type\":\"session_started\"') > 0) \
         ORDER BY seq DESC LIMIT ?2) ORDER BY seq ASC",
    ) {
        if let Ok(rows) = q.query_map(rusqlite::params![repo, limit], |r| r.get::<_, String>(0)) {
            for j in rows.flatten() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) else { continue };
                let by_session = ["session", "from_session"]
                    .iter()
                    .filter_map(|k| v.get(*k).and_then(|x| x.as_str()))
                    .any(|s| sessions.contains(s));
                // A message counts when either end of it is somebody who
                // touched the file — including one addressed to `all`, which
                // is how a person announces they are taking something.
                let by_user = v.get("type").and_then(|t| t.as_str()) == Some("message")
                    && (v
                        .get("from_user")
                        .and_then(|x| x.as_str())
                        .is_some_and(|u| users.contains(u))
                        || v.get("to").and_then(|x| x.as_str()).is_some_and(|u| users.contains(u))
                        || v.get("to").is_some_and(|t| t.is_null()));
                if by_session || by_user {
                    out.push(v);
                }
            }
        }
    }
    // Back into the order things happened. Two passes produce two runs; the
    // reader wants one story.
    out.sort_by_key(|v| v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0));
    out
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let Some(id) = identify(&app, &headers, uri.query()).await else {
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

/// The memory scopes a key holds on this repo.
///
/// A grant of `/` — which `general` gives everyone — covers the repo, so it
/// yields the root scope plus every declared area. A narrow grant yields only
/// what it names. There is no wildcard at the storage layer: a scope is a
/// string, and a key either holds it or does not.
fn scopes_for(id: &crate::teams::Identity, repo_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for a in &id.areas {
        if a.repo != "*" && a.repo != repo_name {
            continue;
        }
        out.push(
            crate::memory::Scope {
                team: id.team_id.clone(),
                repo: repo_name.to_string(),
                area: a.area.clone(),
            }
            .key(),
        );
    }
    out
}

/// Take a published shard, or say why not.
///
/// Two checks, and the order matters. The author must be the member this key
/// was minted for — provenance is the whole point of a shard, and a key that
/// could write as a colleague makes every fact unattributable. The scope must
/// be one this key may enter — the same grant that decides which events reach
/// it. Neither is a rewrite: the seal already binds both, so a relay that
/// "corrected" them would produce a shard nobody could open.
fn accept_shard(
    app: &Arc<App>,
    repo_key: &str,
    repo_name: &str,
    id: &crate::teams::Identity,
    shard: &crate::memory::Shard,
) -> Result<()> {
    anyhow::ensure!(
        !id.member.unassigned && !id.member.id.is_empty(),
        "this key names no verified person, so it may not publish memory"
    );
    anyhow::ensure!(
        shard.author == id.member.id && shard.author_email == id.member.email,
        "a shard's author must be the member the key was minted for"
    );
    anyhow::ensure!(
        scopes_for(id, repo_name).contains(&shard.scope),
        "this key does not hold that area"
    );
    let mut shard = shard.clone();
    {
        let db = app.db.lock().unwrap();
        anyhow::ensure!(
            crate::rooms::kind_enabled_for_scope(&db, &shard.scope, &shard.kind),
            "a room over this area has {} turned off",
            shard.kind
        );
        // Retention is the room's to decide, not the publisher's. It is not
        // bound into the seal precisely so that a room can shorten it without
        // making every shard unreadable.
        if let Some(days) = crate::rooms::retain_days_for_scope(&db, &shard.scope, &shard.kind) {
            shard.expires_ts = Some(shard.created_ts + days * 24 * 60 * 60 * 1000);
        }
        let budget = crate::rooms::budget_for_scope(&db, &shard.scope);
        crate::memory::put(&db, &shard, budget)?;
    }
    // Everyone else in the area hears about it now, not at their next sync.
    if let Some(st) = app.repos.lock().unwrap().get(repo_key) {
        let _ = st.mem_tx.send(shard);
    }
    Ok(())
}

/// Whether an event reaches a connection, given the areas its key grants.
///
/// The map is read per event rather than captured at Hello so that a repo
/// which re-divides itself takes effect on live connections, not only on ones
/// opened afterwards.
fn visible_to(
    app: &Arc<App>,
    repo_key: &str,
    repo_name: &str,
    id: &crate::teams::Identity,
    event: &Event,
) -> bool {
    let Some(path) = event.path() else { return true };
    let area = {
        let repos = app.repos.lock().unwrap();
        match repos.get(repo_key) {
            Some(st) => crate::config::area_of(&st.areas, path),
            None => return true,
        }
    };
    id.may_enter(repo_name, &area)
}

async fn client(sock: WebSocket, app: Arc<App>, id: crate::teams::Identity) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = sock.split();

    // The author every event from this connection is stamped with, or `None`
    // for an identity with no verified person behind it — a legacy shared
    // secret, an unconfigured loopback relay, a migrated key nobody has
    // attached yet. Resolved once per connection: it cannot change while the
    // socket is open, because the key that opened it cannot change.
    let author: Option<String> = (!id.member.unassigned
        && id.member.email.contains('@'))
        .then(|| id.member.email.clone());

    // First message must be Hello.
    let (repo, repo_name, declared) = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t)? {
                // Namespaced here, once, so nothing downstream can address a
                // repo outside the caller's team.
                ClientMsg::Hello { repo, areas, .. } => break (id.scope(&repo), repo, areas),
                _ => anyhow::bail!("expected Hello"),
            },
            Some(Ok(_)) => continue,
            _ => return Ok(()),
        }
    };

    // Who this key says we are. A client cannot learn its own member id or
    // team from anything it holds — the credential is an opaque secret — and
    // it needs both to seal a shard, because the seal binds them.
    let me = author.as_ref().map(|email| crate::proto::Me {
        team_id: id.team_id.clone(),
        member_id: id.member.id.clone(),
        email: email.clone(),
        device_id: id.token_id.clone(),
        rooms: {
            let db = app.db.lock().unwrap();
            crate::rooms::room_grants_for_repo(&db, &id.member.id, &repo_name)
        },
    });
    let mut mem_rx;

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
        // A client that declares areas is stating what the committed file
        // says; one that declares none may simply be older than areas, and
        // must not silently undo a division the rest of the team is working
        // under.
        if !declared.is_empty() {
            st.areas = declared;
        }
        let areas = st.areas.clone();
        mem_rx = st.mem_tx.subscribe();
        (
            ServerMsg::Welcome {
                seq: st.seq,
                claims: st
                    .view
                    .claims
                    .iter()
                    .filter(|c| {
                        id.may_enter(&repo_name, &crate::config::area_of(&areas, &c.path))
                    })
                    .cloned()
                    .collect(),
                sessions: st.view.sessions.values().cloned().collect(),
                me: me.clone(),
                provider: Some(app.provider.clone()),
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
    // An event about a path this identity may not enter is not delivered.
    // Areas bound who can collide with whom, and a room that grants `src/auth`
    // and nothing else should not have its sessions told about — or woken by —
    // every write in the repo. Pathless events (presence, intent, messages)
    // reach everyone: a peer you cannot see is worse than a peer working
    // somewhere you do not care about.
    let pump_app = app.clone();
    let pump_repo = repo.clone();
    let pump_name = repo_name.clone();
    let pump_id = id.clone();
    let pump = tokio::spawn(async move {
        while let Ok((seq, event)) = rx.recv().await {
            if !visible_to(&pump_app, &pump_repo, &pump_name, &pump_id, &event) {
                continue;
            }
            if pump_tx.send(ServerMsg::Event { seq, event }).is_err() {
                break;
            }
        }
    });

    // Shards this key's scopes cover, pushed as they are published. Filtered
    // the same way events are and for the same reason: a room granted one
    // area must not be handed another area's memory.
    // A room this key is in moved; tell it so, and let it ask for the log.
    let mut mls_rx = Some(app.mls_tx.subscribe());
    let mls_tx_out = out_tx.clone();
    let mls_rooms: Vec<String> =
        me.as_ref().map(|m| m.rooms.iter().map(|(r, _)| r.clone()).collect()).unwrap_or_default();
    let mls_pump = tokio::spawn(async move {
        let Some(rx) = mls_rx.as_mut() else { return };
        while let Ok(room) = rx.recv().await {
            if !mls_rooms.contains(&room) {
                continue;
            }
            if mls_tx_out.send(ServerMsg::MlsWake { room }).is_err() {
                break;
            }
        }
    });

    let mut forget_rx = {
        let repos = app.repos.lock().unwrap();
        repos.get(&repo).map(|st| st.forget_tx.subscribe())
    };
    let forget_out = out_tx.clone();
    let forget_pump = tokio::spawn(async move {
        let Some(rx) = forget_rx.as_mut() else { return };
        while let Ok(ids) = rx.recv().await {
            if forget_out.send(ServerMsg::MemForgotten { ids }).is_err() {
                break;
            }
        }
    });

    let mem_tx_out = out_tx.clone();
    let mem_scopes = scopes_for(&id, &repo_name);
    let mem_pump = tokio::spawn(async move {
        while let Ok(shard) = mem_rx.recv().await {
            if !mem_scopes.contains(&shard.scope) {
                continue;
            }
            if mem_tx_out.send(ServerMsg::MemShards { shards: vec![shard], more: false }).is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(t) = msg else { continue };
        let Ok(cm) = serde_json::from_str::<ClientMsg>(&t) else { continue };
        match cm {
            ClientMsg::Hello { .. } => {}
            ClientMsg::MemPublish { shard } => {
                if let Err(why) = accept_shard(&app, &repo, &repo_name, &id, &shard) {
                    let _ = out_tx.send(ServerMsg::MemRejected {
                        id: shard.id.clone(),
                        reason: why.to_string(),
                    });
                }
            }
            ClientMsg::MemForget { ids } => {
                let scopes = scopes_for(&id, &repo_name);
                let gone: Vec<String> = {
                    let db = app.db.lock().unwrap();
                    ids.into_iter()
                        .filter(|i| crate::memory::forget(&db, std::slice::from_ref(i), &scopes) > 0)
                        .collect()
                };
                // Everyone else in the area drops it now, not at their next
                // sync — a sync is keyed on a high-water mark and would never
                // mention a row that is no longer there.
                if !gone.is_empty() {
                    if let Some(st) = app.repos.lock().unwrap().get(&repo) {
                        let _ = st.forget_tx.send(gone);
                    }
                }
            }
            ClientMsg::MemRewrap { id: want, epoch, nonce, ciphertext } => {
                // Any member who holds the scope may rewrap — that is the
                // point: after a Remove, whoever removed re-seals what the
                // room can still read. Provenance is untouched, and the seal
                // is still bound to the original author, so a rewrap cannot
                // become a way to write as somebody else.
                let scopes = scopes_for(&id, &repo_name);
                let done = {
                    let db = app.db.lock().unwrap();
                    crate::memory::rewrap(
                        &db,
                        &want,
                        &scopes,
                        epoch,
                        &crate::memory::unhex(&nonce),
                        &crate::memory::unhex(&ciphertext),
                    )
                };
                match done {
                    // Out to the area, so a device that could not open this
                    // shard gets the version it can.
                    Ok(shard) => {
                        if let Some(st) = app.repos.lock().unwrap().get(&repo) {
                            let _ = st.mem_tx.send(shard);
                        }
                    }
                    Err(e) => {
                        let _ = out_tx
                            .send(ServerMsg::MemRejected { id: want, reason: e.to_string() });
                    }
                }
            }
            ClientMsg::MemSync { since } => {
                const PAGE: usize = 500;
                let scopes = scopes_for(&id, &repo_name);
                let shards = {
                    let db = app.db.lock().unwrap();
                    crate::memory::since(&db, &scopes, since, PAGE)
                };
                let more = shards.len() == PAGE;
                let _ = out_tx.send(ServerMsg::MemShards { shards, more });
            }
            // ---- Delivery Service. Every blob below is opaque here: the
            // relay orders and forwards, and RFC 9750 is explicit that a DS
            // need not be trusted with content. What it *does* enforce is
            // membership — a key that is not in a room may not read its
            // handshake log or write to it.
            ClientMsg::MlsUpload { key_package } => {
                if !id.member.unassigned && !id.token_id.is_empty() {
                    let rooms = {
                        let db = app.db.lock().unwrap();
                        let _ = crate::mls::put_key_package(
                            &db,
                            &id.team_id,
                            &id.token_id,
                            &crate::memory::unhex(&key_package),
                        );
                        crate::rooms::room_grants_for_repo(&db, &id.member.id, &repo_name)
                    };
                    // A machine that has just become addable is a reason for
                    // the room's other members to look again. Without this,
                    // the second laptop to arrive waits for an unrelated
                    // commit before anyone adds it.
                    for (room, _) in rooms {
                        let _ = app.mls_tx.send(room);
                    }
                }
            }
            ClientMsg::MlsKeyPackage { device } => {
                let kp = {
                    let db = app.db.lock().unwrap();
                    crate::mls::key_package_for(&db, &id.team_id, &device)
                };
                let _ = out_tx.send(ServerMsg::MlsKeyPackage {
                    device,
                    key_package: kp.as_deref().map(crate::memory::hex),
                });
            }
            ClientMsg::MlsCommit { room, epoch, commit, welcome, for_device } => {
                let outcome = {
                    let db = app.db.lock().unwrap();
                    if !crate::rooms::member_in_room(&db, &room, &id.member.id) {
                        Err(anyhow::anyhow!("this key is not in that room"))
                    } else {
                        crate::mls::append(
                            &db,
                            &room,
                            &crate::mls::Envelope {
                                seq: 0,
                                epoch,
                                kind: "commit".into(),
                                blob: crate::memory::unhex(&commit),
                                for_device: None,
                            },
                        )
                        .and_then(|_| {
                            // The welcome rides the same acceptance as the
                            // commit that produced it. Storing it separately
                            // would let a rejected commit leave a welcome
                            // behind, and a device would join a group the room
                            // never entered.
                            if let (Some(w), Some(dev)) = (&welcome, &for_device) {
                                crate::mls::append(
                                    &db,
                                    &room,
                                    &crate::mls::Envelope {
                                        seq: 0,
                                        epoch,
                                        kind: "welcome".into(),
                                        blob: crate::memory::unhex(w),
                                        for_device: Some(dev.clone()),
                                    },
                                )?;
                            }
                            Ok(())
                        })
                    }
                };
                match outcome {
                    Ok(()) => {
                        let _ = app.mls_tx.send(room.clone());
                    }
                    Err(e) => {
                        let _ = out_tx
                            .send(ServerMsg::MlsRejected { room, reason: e.to_string() });
                    }
                }
            }
            ClientMsg::MlsSync { room, since } => {
                let (msgs, started) = {
                    let db = app.db.lock().unwrap();
                    if !crate::rooms::member_in_room(&db, &room, &id.member.id) {
                        (Vec::new(), false)
                    } else {
                        (
                            crate::mls::log_since(&db, &room, &id.token_id, since),
                            crate::mls::has_group(&db, &room),
                        )
                    }
                };
                let _ = out_tx.send(ServerMsg::MlsLog { room, msgs, started });
            }
            ClientMsg::MlsRoster { room } => {
                let devices = {
                    let db = app.db.lock().unwrap();
                    if crate::rooms::member_in_room(&db, &room, &id.member.id) {
                        crate::rooms::devices_in_room(&db, &id.team_id, &room)
                    } else {
                        Vec::new()
                    }
                };
                let _ = out_tx.send(ServerMsg::MlsRoster { room, devices });
            }
            ClientMsg::MemFetch { id: want } => {
                // The scope check is inside `get`, which cannot be called
                // without saying which scopes the caller holds.
                let scopes = scopes_for(&id, &repo_name);
                let shards = {
                    let db = app.db.lock().unwrap();
                    crate::memory::get(&db, &want, &scopes).into_iter().collect()
                };
                let _ = out_tx.send(ServerMsg::MemShards { shards, more: false });
            }
            ClientMsg::Append { mut event } => {
                // Authorship comes from the key, not from what the client says
                // about itself. Before anything reads the event, so no code
                // path downstream can see the client's version.
                event.attribute_to(author.as_deref());
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
            ClientMsg::ClaimReq { id, session, user, path, intent, branch, hub } => {
                let user = author.clone().unwrap_or(user);
                // Arbitration: the one decision only the relay may make.
                let (verdict, is_hub, queued, lease_ms) = {
                    let mut repos = app.repos.lock().unwrap();
                    let st = repos.get_mut(&repo).unwrap();
                    st.view.prune();
                    // A declared hub is knowledge only the client has — the
                    // relay never sees the repo — so it is remembered here the
                    // first time somebody claims one. History-detected hubs
                    // the relay finds on its own.
                    if hub {
                        st.view.declared_hubs.insert(path.clone());
                    }
                    let is_hub = st.view.is_hub(&path);
                    let queued = st.view.queue_len(&path, &session);
                    let lease_ms = st.view.lease_for(&path);
                    // Resolved under the same lock as the lookup, so the pair
                    // cannot disagree about who holds what.
                    let verdict = st
                        .view
                        .conflicting_on(&session, &path, &branch)
                        .map(|c| (c.clone(), st.view.holder_intent(c)));
                    (verdict, is_hub, queued, lease_ms)
                };
                match verdict {
                    Some((holder, live_intent)) => {
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
                            // The holder's current intent, not the one frozen
                            // into the claim: the relay sees every session, so
                            // it is the best-placed to answer this, and a
                            // daemon on another machine has nothing else.
                            holder_intent: Some(live_intent),
                            lease_until: Some(holder.lease_until),
                            hub: is_hub,
                            queued,
                        });
                    }
                    None => {
                        // A hub is leased for two minutes rather than ten.
                        // Locks do not scale and awareness does; a widely
                        // shared file held for a whole turn is the one case
                        // where that stops being a slogan and starts being
                        // every other agent's critical path.
                        let lease_until = now_ms() + lease_ms;
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
                            hub: is_hub,
                            queued,
                        });
                    }
                }
            }
        }
    }

    pump.abort();
    mem_pump.abort();
    mls_pump.abort();
    forget_pump.abort();
    writer.abort();
    Ok(())
}


#[cfg(test)]
mod auth_tests {
    use super::*;

    fn app_with(token: Option<&str>) -> App {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::teams::init_schema(&conn).unwrap();
        crate::rooms::init_schema(&conn).unwrap();
        App {
            repos: Mutex::new(HashMap::new()),
            db: Mutex::new(conn),
            terms: None,
            token: token.map(str::to_string),
            reg_limit: crate::teams::RateLimit::new(5, 60_000),
            cloud: None,
            provider: crate::proto::PROVIDER_PLAINTEXT.into(),
            mls_tx: broadcast::channel(16).0,
        }
    }

    fn bearer(v: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, v.parse().unwrap());
        h
    }

    async fn team_of(app: &App, headers: &axum::http::HeaderMap, q: Option<&str>) -> Option<String> {
        identify(app, headers, q).await.map(|i| i.team_id)
    }

    #[tokio::test]
    async fn an_open_relay_accepts_anything_as_the_local_team() {
        let app = app_with(None);
        assert_eq!(team_of(&app, &axum::http::HeaderMap::new(), None).await.as_deref(), Some("local"));
    }

    #[tokio::test]
    async fn a_token_relay_needs_the_token() {
        let app = app_with(Some("sekrit"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, None).await.is_none());
        assert_eq!(team_of(&app, &bearer("Bearer sekrit"), None).await.as_deref(), Some("root"));
        assert!(identify(&app, &bearer("Bearer nope"), None).await.is_none());
        assert!(
            identify(&app, &bearer("sekrit"), None).await.is_none(),
            "must be a Bearer token"
        );
    }

    /// Browsers cannot set headers on a websocket or an <img>, so the query
    /// form exists; it must be exactly as strict.
    #[tokio::test]
    async fn the_query_form_works_and_is_just_as_strict() {
        let app = app_with(Some("sekrit"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, Some("token=sekrit")).await.is_some());
        assert!(identify(&app, &none, Some("repo=x&token=sekrit")).await.is_some());
        assert!(identify(&app, &none, Some("token=nope")).await.is_none());
        assert!(identify(&app, &none, Some("repo=x")).await.is_none());
        assert!(identify(&app, &none, Some("token=sekritextra")).await.is_none());
    }

    #[tokio::test]
    async fn a_percent_encoded_query_token_still_works() {
        let app = app_with(Some("a b+c"));
        let none = axum::http::HeaderMap::new();
        assert!(identify(&app, &none, Some("token=a%20b%2Bc")).await.is_some());
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
    #[tokio::test]
    async fn a_registered_team_token_resolves_to_its_own_team() {
        let app = app_with(Some("sekrit"));
        let (id, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme", Some("acme@example.com")).unwrap()
        };
        let got = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).await.unwrap();
        assert_eq!(got.team_id, id.team_id);
        assert_ne!(got.team_id, "root");
    }

    /// The property the whole multi-team story rests on: two teams naming the
    /// same repo address different storage keys.
    #[tokio::test]
    async fn two_teams_naming_one_repo_never_share_a_log() {
        let app = app_with(None);
        let (a, ta) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "A", Some("a@example.com")).unwrap()
        };
        let (b, tb) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "B", Some("b@example.com")).unwrap()
        };
        let ia = identify(&app, &bearer(&format!("Bearer {}", ta.secret)), None).await.unwrap();
        let ib = identify(&app, &bearer(&format!("Bearer {}", tb.secret)), None).await.unwrap();
        assert_eq!(ia.team_id, a.team_id);
        assert_eq!(ib.team_id, b.team_id);
        assert_ne!(ia.scope("api"), ib.scope("api"));
    }

    /// Registration is open, so a stranger holding a valid team token must not
    /// thereby hold a shell on the host. Only the operator's own credential
    /// reaches the lab.
    #[tokio::test]
    async fn a_registered_team_is_not_an_operator() {
        let app = app_with(Some("sekrit"));
        let (_, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Stranger", Some("stranger@example.com")).unwrap()
        };
        let id = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).await.unwrap();
        assert!(
            id.team_id != "root" && id.team_id != "local",
            "a registered team must not pass the operator check that gates ptys"
        );
    }

    #[tokio::test]
    async fn a_revoked_token_is_refused_not_downgraded() {
        let app = app_with(None);
        let (id, first) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme", Some("acme@example.com")).unwrap()
        };
        {
            let db = app.db.lock().unwrap();
            crate::rooms::mint_device(&db, &id.team_id, &id.member.id, "second").unwrap();
            crate::rooms::revoke_device(&db, &id.team_id, &first.id).unwrap();
        }
        // Not a downgrade to the anonymous identity — a refusal. Anything
        // else would let a revoked token keep opening a console, and hand it
        // the identity that gates the lab's terminals.
        assert!(
            identify(&app, &bearer(&format!("Bearer {}", first.secret)), None).await.is_none(),
            "a revoked token must be refused outright, not treated as anonymous"
        );
    }

    /// A key resolves to the person it was minted for, and to the areas their
    /// rooms grant. Everything in phase 1 rests on this one resolution.
    #[tokio::test]
    async fn a_presented_key_resolves_to_a_verified_member_and_their_areas() {
        let app = app_with(None);
        let (_, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme", Some("ash@example.com")).unwrap()
        };
        let id = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).await.unwrap();
        assert_eq!(id.member.email, "ash@example.com");
        assert!(!id.member.unassigned);
        assert!(id.may_enter("api", "/"), "the general room covers the whole repo");
    }

    /// The fail-open, works-unconfigured property in REPORT.md is load-bearing
    /// and members must not have touched it: a loopback relay with no setup
    /// still hands out an identity, and that identity can still work.
    #[tokio::test]
    async fn an_unconfigured_relay_still_answers_with_a_usable_identity() {
        let app = app_with(None);
        let id = identify(&app, &axum::http::HeaderMap::new(), None).await.unwrap();
        assert_eq!(id.team_id, "local");
        assert_eq!(id.member.email, "local");
        assert!(id.may_enter("anything", "/"), "an unconfigured relay gates nothing");
    }

    /// A key with no verified person behind it — the legacy secret, the
    /// unconfigured relay, a migrated key nobody has adopted — must keep the
    /// client's own authorship string. Inventing an author would be worse
    /// than an honest self-reported one.
    #[tokio::test]
    async fn a_legacy_identity_does_not_get_a_fabricated_author() {
        let app = app_with(Some("sekrit"));
        let id = identify(&app, &bearer("Bearer sekrit"), None).await.unwrap();
        assert_eq!(id.team_id, "root");
        let author: Option<String> =
            (!id.member.unassigned && id.member.email.contains('@')).then(|| id.member.email.clone());
        assert!(author.is_none(), "nothing here proves who is at the keyboard");

        let mut ev = Event::FileWritten {
            session: "s1".into(),
            user: "ash".into(),
            path: "a.rs".into(),
            ts: 1,
        };
        ev.attribute_to(author.as_deref());
        let Event::FileWritten { user, .. } = ev else { unreachable!() };
        assert_eq!(user, "ash", "the client's string stands when nothing better is known");
    }

    /// And the converse, which is the reason devices exist: with a verified
    /// member, whatever the client claims about itself is overwritten.
    #[tokio::test]
    async fn authorship_on_events_comes_from_the_key_not_the_client() {
        let app = app_with(None);
        let (_, tok) = {
            let db = app.db.lock().unwrap();
            crate::teams::create_team(&db, "Acme", Some("ash@example.com")).unwrap()
        };
        let id = identify(&app, &bearer(&format!("Bearer {}", tok.secret)), None).await.unwrap();
        let author = Some(id.member.email.clone());

        // KNOOT_USER is a display convenience, and on its own it was also a
        // way to write an event as somebody else.
        let mut ev = Event::FileWritten {
            session: "s1".into(),
            user: "priya".into(),
            path: "a.rs".into(),
            ts: 1,
        };
        ev.attribute_to(author.as_deref());
        let Event::FileWritten { user, .. } = ev else { unreachable!() };
        assert_eq!(user, "ash@example.com");
    }

    // ------------------------------------------------------ knoot why

    fn log_db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE events (repo TEXT, seq INTEGER, ts INTEGER, json TEXT);",
        )
        .unwrap();
        c
    }

    fn log(c: &rusqlite::Connection, seq: i64, ts: u64, ev: serde_json::Value) {
        c.execute(
            "INSERT INTO events (repo, seq, ts, json) VALUES ('t/r', ?1, ?2, ?3)",
            rusqlite::params![seq, ts as i64, ev.to_string()],
        )
        .unwrap();
    }

    /// The flight recorder. The log has held this since the first version and
    /// nothing read it back.
    #[test]
    fn one_files_story_is_its_own_events_plus_what_its_people_said() {
        let c = log_db();
        log(&c, 1, 100, serde_json::json!({
            "type": "intent_declared", "session": "s1", "text": "normalise errors", "ts": 100
        }));
        log(&c, 2, 200, serde_json::json!({
            "type": "claim_acquired", "session": "s1", "user": "sam",
            "path": "src/response.js", "intent": "normalise errors", "ts": 200
        }));
        log(&c, 3, 300, serde_json::json!({
            "type": "claim_denied", "session": "s2", "user": "priya",
            "path": "src/response.js", "holder_user": "sam", "ts": 300
        }));
        log(&c, 4, 400, serde_json::json!({
            "type": "message", "from_session": "", "from_user": "sam",
            "to": serde_json::Value::Null, "text": "taking it", "ts": 400
        }));
        // Another file entirely, and an intent from a session that never
        // touched ours.
        log(&c, 5, 500, serde_json::json!({
            "type": "claim_acquired", "session": "s9", "user": "kim",
            "path": "src/other.js", "intent": "unrelated", "ts": 500
        }));
        log(&c, 6, 600, serde_json::json!({
            "type": "intent_declared", "session": "s9", "text": "unrelated work", "ts": 600
        }));

        let story = events_for_path(&c, "t/r", "src/response.js", 100);
        let kinds: Vec<&str> = story.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec!["intent_declared", "claim_acquired", "claim_denied", "message"],
            "in the order things happened, and nothing from the unrelated session"
        );
        assert!(
            !story.iter().any(|e| e["text"].as_str() == Some("unrelated work")),
            "a session that never touched the file has nothing to say about it"
        );
        // A message with no session id still lands: `knoot msg` cannot know
        // its own session, and dropping those loses what a person announced.
        assert_eq!(story[3]["text"], "taking it");
    }

    /// A path is matched as a whole value, not as a substring — or asking
    /// about `src/a.rs` would return `src/a.rs.bak`'s history as well.
    #[test]
    fn a_path_does_not_match_its_own_prefix() {
        let c = log_db();
        for (seq, path) in [(1, "src/a.rs"), (2, "src/a.rs.bak"), (3, "vendor/src/a.rs")] {
            log(&c, seq, seq as u64 * 100, serde_json::json!({
                "type": "file_written", "session": "s1", "user": "sam",
                "path": path, "ts": seq * 100
            }));
        }
        let story = events_for_path(&c, "t/r", "src/a.rs", 100);
        assert_eq!(story.len(), 1, "one file, not three: {story:?}");
        assert_eq!(story[0]["path"], "src/a.rs");

        // And a leading slash is the same question.
        assert_eq!(events_for_path(&c, "t/r", "/src/a.rs", 100).len(), 1);
    }

    /// One team's question may not read another team's log — the repo key is
    /// namespaced by team and this path must respect it like every other.
    #[test]
    fn one_teams_file_story_cannot_reach_another_teams_log() {
        let c = log_db();
        c.execute(
            "INSERT INTO events (repo, seq, ts, json) VALUES ('other/r', 1, 100, ?1)",
            rusqlite::params![serde_json::json!({
                "type": "file_written", "session": "s1", "user": "kim",
                "path": "src/response.js", "ts": 100
            })
            .to_string()],
        )
        .unwrap();
        assert!(events_for_path(&c, "t/r", "src/response.js", 100).is_empty());
        assert_eq!(events_for_path(&c, "other/r", "src/response.js", 100).len(), 1);
    }

    /// A file nobody has touched is an empty story, not an error and not the
    /// whole log.
    #[test]
    fn a_file_with_no_history_has_an_empty_story() {
        let c = log_db();
        log(&c, 1, 100, serde_json::json!({
            "type": "session_started", "session": "s1", "user": "sam",
            "branch": "main", "ts": 100
        }));
        assert!(events_for_path(&c, "t/r", "src/never-touched.js", 100).is_empty());
    }
}
