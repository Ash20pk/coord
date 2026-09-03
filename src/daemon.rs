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
    // KNOOT_SOCK lets tests (and multi-instance setups) use an isolated socket.
    if let Some(p) = crate::config::env_or_legacy("KNOOT_SOCK") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap().join(".knoot").join("knootd.sock")
}

struct RepoConn {
    tx: mpsc::UnboundedSender<ClientMsg>,
    /// Undelivered notes per user, in arrival order. Keyed by user rather
    /// than session because a CLI caller cannot learn its own session id.
    mail: Arc<Mutex<HashMap<String, std::collections::VecDeque<String>>>>,
    /// Consecutive turn-endings we have interrupted, per user, so a
    /// notification can never trap an agent in a loop.
    stop_holds: Arc<Mutex<HashMap<String, u32>>>,
    view: Arc<Mutex<View>>,
    connected: Arc<Mutex<bool>>,
    /// Why the last dial failed, kept so `knoot status` can say which kind of
    /// off this is rather than only that it is off.
    last_error: Arc<Mutex<Option<String>>>,
    /// True once a Welcome snapshot has been applied, i.e. the mirror is
    /// trustworthy. Until then we must not answer from an empty view.
    ready: Arc<Mutex<bool>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ServerMsg>>>>,
}

/// What a session's in-flight Bash command is expected to touch.
struct PendingBash {
    /// Working-tree fingerprint, present only when the command needed auditing.
    snapshot: Option<String>,
    taken_at: Ts,
    /// Repo-relative paths the parser predicted.
    targets: Vec<String>,
    /// The command itself. When a peer is also writing, naming a path is the
    /// evidence that a change was ours rather than theirs.
    command: String,
}

#[derive(Default)]
struct Daemon {
    repos: Mutex<HashMap<String, Arc<RepoConn>>>,
    /// Working-tree snapshots taken before an audited Bash command, keyed by
    /// (repo_root, session). Compared afterwards to catch writes the parser
    /// could not predict.
    snapshots: Mutex<HashMap<(String, String), PendingBash>>,
    /// When each session's previous turn began, keyed by (repo_root, session).
    /// "What changed under you" is meaningless without a since; this is it.
    turns: Mutex<HashMap<(String, String), Ts>>,
}

pub async fn run() -> Result<()> {
    run_on(socket_path()).await
}

/// Serve the daemon API on an explicit socket path (tests use isolated sockets).
pub async fn run_on(sock: PathBuf) -> Result<()> {
    std::fs::create_dir_all(sock.parent().unwrap())?;
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("knootd listening on {}", sock.display());

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
                return DResp::Decision { allow: true, reason: None }; // not knoot-enabled → fail open
            };
            ensure_session(&rc, &session, &whoami());
            let path = rel_path(&repo_root, &path);

            // Hot path: local mirror check, microseconds. When it fires we
            // answer without troubling the relay — but the collision still has
            // to reach the log, or denials caught locally stay invisible.
            // The local pre-check has to know our branch too, or it denies
            // cross-branch writes before the arbiter ever sees them.
            let local = {
                let v = rc.view.lock().unwrap();
                let (user, branch) = match v.sessions.get(&session) {
                    Some(s) => (s.user.clone(), s.branch.clone()),
                    None => (whoami(), String::new()),
                };
                v.conflicting_on(&session, &path, &branch).cloned().map(|c| (c, user))
            };
            if let Some((c, user)) = local {
                let _ = rc.tx.send(ClientMsg::Append {
                    event: Event::ClaimDenied {
                        session: session.clone(),
                        user,
                        path: path.clone(),
                        holder: c.session.clone(),
                        holder_user: c.user.clone(),
                        ts: now_ms(),
                    },
                });
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
            let (intent, user, branch) = {
                let v = rc.view.lock().unwrap();
                match v.sessions.get(&session) {
                    Some(s) => (s.intent.clone(), s.user.clone(), s.branch.clone()),
                    None => (String::new(), whoami(), String::new()),
                }
            };
            let sess_for_warn = session.clone();
            let _ = rc.tx.send(ClientMsg::ClaimReq {
                id: id.clone(),
                session,
                user,
                path: path.clone(),
                intent,
                branch,
            });
            match tokio::time::timeout(std::time::Duration::from_millis(CLAIM_TIMEOUT_MS), rx).await {
                Ok(Ok(ServerMsg::ClaimResp { granted: false, holder, holder_user, holder_intent, lease_until, .. })) => {
                    let c = Claim {
                        session: holder.unwrap_or_default(),
                        user: holder_user.unwrap_or_else(|| "someone".into()),
                        path: path.clone(),
                        lease_until: lease_until.unwrap_or(0),
                        intent: holder_intent.unwrap_or_default(),
                        branch: String::new(),
                    };
                    deny(&path, &c)
                }
                Ok(Ok(ServerMsg::ClaimResp { granted: true, .. })) => {
                    rc.pending.lock().unwrap().remove(&id);
                    // Record the win in our own mirror *now*, rather than
                    // waiting for the relay to broadcast it back to us.
                    //
                    // Every mirror-only check — Bash gating, and the presence
                    // context handed to the next prompt — would otherwise read
                    // this file as free for as long as that round trip takes.
                    // Which is a real bypass, not a cosmetic lag: a peer
                    // session on this machine could `sed -i` a file we hold,
                    // because the Bash gate never asks the relay. macOS won
                    // that race and Linux lost it, so it took CI to see.
                    //
                    // Applying it twice is harmless: the relay's copy arrives
                    // shortly and `View::apply` renews a claim on the same
                    // session and path rather than duplicating it.
                    claim_locally(&rc, &sess_for_warn, &path);
                    warn_cross_branch(&rc, &sess_for_warn, &path);
                    DResp::Decision { allow: true, reason: None }
                }
                _ => {
                    rc.pending.lock().unwrap().remove(&id);
                    // Timed out, or the relay went away mid-request → fail
                    // open. We must *not* record a claim here: we do not know
                    // that we hold it, and a mirror that invents claims would
                    // block peers over a file nobody owns.
                    warn_cross_branch(&rc, &sess_for_warn, &path);
                    DResp::Decision { allow: true, reason: None }
                }
            }
        }
        DReq::PostWrite { repo_root, session, path } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                let path = rel_path(&repo_root, &path);
                let user = user_of(&rc, &session);
                let ev =
                    Event::FileWritten { session: session.clone(), user, path: path.clone(), ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::Append { event: ev });
                // Not a block and not mail: a note about work that is going to
                // meet this write at merge, delivered while the turn can still
                // act on it.
                let peers = {
                    let v = rc.view.lock().unwrap();
                    let branch = v.sessions.get(&session).map(|s| s.branch.clone()).unwrap_or_default();
                    v.cross_branch_overlap(&session, &path, &branch)
                };
                if let Some(note) = cross_branch_note(&peers, &path) {
                    return DResp::Mail { items: vec![note] };
                }
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
            let since = now_ms().saturating_sub(FIRST_TURN_LOOKBACK_MS);
            d.turns.lock().unwrap().insert((repo_root.clone(), session.clone()), now_ms());
            let mail = drain_mail(&rc, &user_of(&rc, &session));
            let v = rc.view.lock().unwrap();
            DResp::Peers {
                sessions: v.sessions.values().filter(|s| s.session != session).cloned().collect(),
                claims: v.claims.iter().filter(|c| c.session != session).cloned().collect(),
                writes: v.writes_since(&session, since),
                mail,
            }
        }
        DReq::Intent { repo_root, session, text, user, branch } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else { return DResp::Ok };
            ensure_session(&rc, &session, &user);
            let ev = Event::IntentDeclared {
                session: session.clone(),
                text,
                ts: now_ms(),
                branch,
            };
            rc.view.lock().unwrap().apply(&ev);
            let _ = rc.tx.send(ClientMsg::Append { event: ev });
            // Answer with everything the agent would otherwise have to ask
            // for: peers now (presence injected once at SessionStart goes
            // stale within minutes), what moved under it since its last turn,
            // and any mail. A cheap model will not run `knoot who` or read its
            // messages; it does not have to.
            let key = (repo_root.clone(), session.clone());
            let now = now_ms();
            let since = d
                .turns
                .lock()
                .unwrap()
                .insert(key, now)
                .unwrap_or_else(|| now.saturating_sub(FIRST_TURN_LOOKBACK_MS));
            let mail = drain_mail(&rc, &user);
            let mut v = rc.view.lock().unwrap();
            v.prune();
            DResp::Peers {
                sessions: v.sessions.values().filter(|s| s.session != session).cloned().collect(),
                claims: v.claims.iter().filter(|c| c.session != session).cloned().collect(),
                writes: v.writes_since(&session, since),
                mail,
            }
        }
        DReq::SessionEnd { repo_root, session } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                let ev = Event::SessionEnded { session: session.clone(), ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::ReleaseSession { session });
            }
            DResp::Ok
        }
        DReq::BashPre { repo_root, session, command } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Decision { allow: true, reason: None };
            };
            let a = crate::bashparse::analyze(&command);

            // Gate every path the command is expected to write.
            for raw in &a.targets {
                let path = normalize(&repo_root, raw);
                if path.is_empty() {
                    continue; // outside the repo
                }
                let hit = {
                    let branch = branch_of(&rc, &session);
                    let v = rc.view.lock().unwrap();
                    v.conflicting_on(&session, &path, &branch).cloned()
                };
                if let Some(c) = hit {
                    report_denied(&rc, &session, &path, &c);
                    return deny_bash(&path, &c, raw);
                }
            }
            // Claim them, so peers are blocked while this command runs.
            for raw in &a.targets {
                let path = normalize(&repo_root, raw);
                if path.is_empty() {
                    continue;
                }
                claim_locally(&rc, &session, &path);
            }

            // Could not prove read-only: snapshot now, diff in BashPost.
            let snapshot = if a.audit { worktree_snapshot(&repo_root).await } else { None };
            let targets: Vec<String> = a
                .targets
                .iter()
                .map(|raw| normalize(&repo_root, raw))
                .filter(|p| !p.is_empty())
                .collect();
            if snapshot.is_some() || !targets.is_empty() {
                d.snapshots.lock().unwrap().insert(
                    (repo_root.clone(), session.clone()),
                    PendingBash { snapshot, taken_at: now_ms(), targets, command: command.clone() },
                );
            }
            DResp::Decision { allow: true, reason: None }
        }
        DReq::BashPost { repo_root, session } => {
            let pending = d.snapshots.lock().unwrap().remove(&(repo_root.clone(), session.clone()));
            let Some(pending) = pending else { return DResp::Ok };
            let Some(rc) = ensure_repo(d, &repo_root).await else { return DResp::Ok };
            let taken_at = pending.taken_at;

            // Shell writes we predicted are still writes: record authorship, or
            // the log cannot tell a peer's edit from an unattributed one.
            let user = user_of(&rc, &session);
            for path in &pending.targets {
                let ev = Event::FileWritten {
                    session: session.clone(),
                    user: user.clone(),
                    path: path.clone(),
                    ts: now_ms(),
                };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::Append { event: ev });
            }

            let Some(before) = pending.snapshot else { return DResp::Ok };
            let Some(after) = worktree_snapshot(&repo_root).await else { return DResp::Ok };

            for path in changed_paths(&before, &after) {
                if pending.targets.contains(&path) {
                    continue; // already accounted for above
                }
                // Naming the file is evidence the change was ours. Without
                // this, a peer writing the same file continuously masks every
                // one of our writes to it — which is precisely the collision
                // we exist to catch.
                let we_named_it = mentions_path(&pending.command, &path);
                let held = {
                    let branch = branch_of(&rc, &session);
                    let v = rc.view.lock().unwrap();
                    // The tree is shared, so a peer's concurrent edit lands in
                    // our window too; their own write event says it was theirs.
                    if !we_named_it && v.written_by_other_since(&session, &path, taken_at) {
                        continue;
                    }
                    // Branch-scoped: writing a file another branch holds is not
                    // an ungated write, it is two trees that will meet later.
                    v.conflicting_on(&session, &path, &branch).cloned()
                };
                match held {
                    // A write landed on someone else's file. It cannot be
                    // undone, only recorded — honestly, as ungated.
                    Some(c) => {
                        let _ = rc.tx.send(ClientMsg::Append {
                            event: Event::UngatedWrite {
                                session: session.clone(),
                                user: user.clone(),
                                path: path.clone(),
                                holder: c.session.clone(),
                                holder_user: c.user.clone(),
                                ts: now_ms(),
                            },
                        });
                    }
                    None => {
                        claim_locally(&rc, &session, &path);
                        let ev = Event::FileWritten {
                            session: session.clone(),
                            user: user.clone(),
                            path: path.clone(),
                            ts: now_ms(),
                        };
                        rc.view.lock().unwrap().apply(&ev);
                        let _ = rc.tx.send(ClientMsg::Append { event: ev });
                    }
                }
            }
            DResp::Ok
        }
        DReq::Msg { repo_root, from_user, to, text } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled".into() };
            };
            let from_session = {
                let v = rc.view.lock().unwrap();
                v.sessions
                    .values()
                    .find(|s| s.user.eq_ignore_ascii_case(&from_user))
                    .map(|s| s.session.clone())
                    .unwrap_or_default()
            };
            let ev = Event::Message {
                from_session,
                from_user,
                to,
                text,
                ts: now_ms(),
            };
            let _ = rc.tx.send(ClientMsg::Append { event: ev });
            DResp::Ok
        }
        DReq::Poll { repo_root, user } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Mail { items: vec![] };
            };
            DResp::Mail { items: drain_mail(&rc, &user) }
        }
        DReq::StopCheck { repo_root, user, already_continued } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Mail { items: vec![] };
            };
            // Cap how often mail may interrupt a finish, so a chatty peer
            // cannot keep a session spinning.
            let holds = {
                let mut h = rc.stop_holds.lock().unwrap();
                let n = h.entry(user.to_lowercase()).or_insert(0);
                if already_continued {
                    *n += 1;
                } else {
                    *n = 0;
                }
                *n
            };
            if holds >= 3 {
                return DResp::Mail { items: vec![] };
            }
            DResp::Mail { items: drain_mail(&rc, &user) }
        }
        DReq::Health { repo_root } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            let connected = *rc.connected.lock().unwrap();
            let ready = *rc.ready.lock().unwrap();
            let last_error = rc.last_error.lock().unwrap().clone();
            DResp::Health { connected, ready, last_error }
        }
        DReq::Who { repo_root } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let mut v = rc.view.lock().unwrap();
            v.prune();
            DResp::Peers {
                sessions: v.sessions.values().cloned().collect(),
                claims: v.claims.clone(),
                // `who` is the explicit ask; it must not consume mail that the
                // next turn is going to be handed anyway.
                writes: v.writes_since("", now_ms().saturating_sub(FIRST_TURN_LOOKBACK_MS)),
                mail: Vec::new(),
            }
        }
    }
}

/// Translate events other sessions caused into notes for our own sessions.
/// Must run before the view applies the event, since PathFreed clears waiters.
fn deliver(rc: &Arc<RepoConn>, ev: &Event) {
    let notes: Vec<(String, String)> = {
        let v = rc.view.lock().unwrap();
        match ev {
            Event::PathFreed { path, by_user, intent, by_session, .. } => v
                .waiters_for(path, by_session)
                .into_iter()
                .map(|w| {
                    let key = w.user.to_lowercase();
                    let why = if intent.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" Their task was: \"{}\".", truncate(intent, 160))
                    };
                    (
                        key,
                        format!(
                            "knoot: `{path}` is free now — {by_user} released it.{why} \
                             You were blocked on this file; you can proceed with it."
                        ),
                    )
                })
                .collect(),
            Event::Message { from_user, to, text, .. } => {
                let scope = if to.is_some() { "" } else { " (to everyone)" };
                let note = format!("knoot: message from {from_user}{scope}: {text}");
                match to {
                    // Addressed: deliver even if that user has no live session
                    // yet, so the note is waiting when they arrive.
                    Some(t) if !t.eq_ignore_ascii_case(from_user) => {
                        vec![(t.to_lowercase(), note)]
                    }
                    Some(_) => Vec::new(),
                    None => v
                        .sessions
                        .values()
                        .map(|s| s.user.to_lowercase())
                        .filter(|u| !u.eq_ignore_ascii_case(from_user))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|u| (u, note.clone()))
                        .collect(),
                }
            }
            _ => Vec::new(),
        }
    };
    if notes.is_empty() {
        return;
    }
    let mut mail = rc.mail.lock().unwrap();
    for (session, note) in notes {
        let q = mail.entry(session).or_default();
        if q.len() < 32 {
            q.push_back(note);
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one.chars().count() <= n {
        one
    } else {
        format!("{}…", one.chars().take(n - 1).collect::<String>())
    }
}

fn drain_mail(rc: &Arc<RepoConn>, user: &str) -> Vec<String> {
    rc.mail
        .lock()
        .unwrap()
        .get_mut(&user.to_lowercase())
        .map(|q| q.drain(..).collect())
        .unwrap_or_default()
}

/// A session that activity arrives for but the view has never heard of must be
/// re-registered, not mislabelled. This is what a long idle gap used to break:
/// presence was pruned and every later claim was attributed to the OS user.
fn ensure_session(rc: &Arc<RepoConn>, session: &str, user: &str) {
    let known = rc.view.lock().unwrap().sessions.contains_key(session);
    if known {
        return;
    }
    let ev = Event::SessionStarted {
        session: session.to_string(),
        user: user.to_string(),
        branch: String::new(),
        ts: now_ms(),
    };
    rc.view.lock().unwrap().apply(&ev);
    let _ = rc.tx.send(ClientMsg::Append { event: ev });
}

/// Record a denial so collisions caught by the local mirror still reach the log.
fn report_denied(rc: &Arc<RepoConn>, session: &str, path: &str, c: &Claim) {
    let user = user_of(rc, session);
    let _ = rc.tx.send(ClientMsg::Append {
        event: Event::ClaimDenied {
            session: session.to_string(),
            user,
            path: path.to_string(),
            holder: c.session.clone(),
            holder_user: c.user.clone(),
            ts: now_ms(),
        },
    });
}

fn user_of(rc: &Arc<RepoConn>, session: &str) -> String {
    rc.view
        .lock()
        .unwrap()
        .sessions
        .get(session)
        .map(|s| s.user.clone())
        .unwrap_or_else(whoami)
}

/// The branch a session is on, per its own record. Empty when unknown, which
/// `same_branch` treats as "assume same branch and block".
fn branch_of(rc: &Arc<RepoConn>, session: &str) -> String {
    rc.view.lock().unwrap().sessions.get(session).map(|s| s.branch.clone()).unwrap_or_default()
}

/// The note handed back with an allowed write. Names the branch and the peer,
/// because "you will conflict" is only actionable if you know with whom.
fn cross_branch_note(peers: &[Claim], path: &str) -> Option<String> {
    if peers.is_empty() {
        return None;
    }
    let who: Vec<String> = peers
        .iter()
        .map(|p| format!("{} on branch {}", p.user, if p.branch.is_empty() { "?" } else { &p.branch }))
        .collect();
    Some(format!(
        "knoot: {} is also editing {} right now. Nothing is blocked — you are on different \
         branches — but these edits will meet at merge. Keep your change tight and scoped, and \
         consider `knoot msg` to agree who owns which part.",
        who.join(" and "),
        path
    ))
}

/// A write allowed onto a file someone else holds on another branch. Nothing
/// is blocked — the trees are separate until a merge — but this is the moment
/// re-planning is cheap, and the only moment anyone can be told.
fn warn_cross_branch(rc: &Arc<RepoConn>, session: &str, path: &str) -> Vec<Claim> {
    let (branch, user, peers) = {
        let v = rc.view.lock().unwrap();
        let (branch, user) = match v.sessions.get(session) {
            Some(s) => (s.branch.clone(), s.user.clone()),
            None => (String::new(), whoami()),
        };
        let peers = v.cross_branch_overlap(session, path, &branch);
        (branch, user, peers)
    };
    for p in &peers {
        let _ = rc.tx.send(ClientMsg::Append {
            event: Event::CrossBranchOverlap {
                session: session.to_string(),
                user: user.clone(),
                branch: branch.clone(),
                path: path.to_string(),
                peer_user: p.user.clone(),
                peer_branch: p.branch.clone(),
                ts: now_ms(),
            },
        });
    }
    peers
}

/// Optimistically claim locally and tell the relay. Used where we have already
/// decided to allow the write, so a synchronous round-trip buys nothing.
fn claim_locally(rc: &Arc<RepoConn>, session: &str, path: &str) {
    let (intent, user, branch) = {
        let v = rc.view.lock().unwrap();
        match v.sessions.get(session) {
            Some(s) => (s.intent.clone(), s.user.clone(), s.branch.clone()),
            None => (String::new(), whoami(), String::new()),
        }
    };
    let ev = Event::ClaimAcquired {
        session: session.to_string(),
        user,
        path: path.to_string(),
        lease_until: now_ms() + LEASE_MS,
        intent,
        branch,
        ts: now_ms(),
    };
    rc.view.lock().unwrap().apply(&ev);
    let _ = rc.tx.send(ClientMsg::Append { event: ev });
}

/// Does this command name the given repo-relative path, in full or by file
/// name? Used only to attribute a change we already know happened.
fn mentions_path(command: &str, path: &str) -> bool {
    if command.contains(path) {
        return true;
    }
    match path.rsplit('/').next() {
        Some(base) if base.len() >= 3 => command.contains(base),
        _ => false,
    }
}

/// Repo-relative path for a target as written on a command line. Empty string
/// when it falls outside the repo (those are none of our business).
fn normalize(repo_root: &str, raw: &str) -> String {
    let root = std::path::Path::new(repo_root.trim_end_matches('/'));
    let p = std::path::Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    // Resolve . and .. without touching the filesystem (the file may not exist).
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_string_lossy().to_string()),
        }
    }
    let abs = parts.join("/").replace("//", "/");
    let root_s = root.to_string_lossy().to_string();
    match abs.strip_prefix(&format!("{root_s}/")) {
        Some(rel) => rel.to_string(),
        None => String::new(),
    }
}

/// `git status` of the working tree, used as a cheap change fingerprint.
async fn worktree_snapshot(repo_root: &str) -> Option<String> {
    let root = repo_root.to_string();
    tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new("git")
            .args(["-C", &root, "status", "--porcelain", "--untracked-files=all"])
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Paths whose status differs between two snapshots.
fn changed_paths(before: &str, after: &str) -> Vec<String> {
    use std::collections::HashSet;
    let lines = |s: &str| -> HashSet<String> { s.lines().map(str::to_string).collect() };
    let (b, a) = (lines(before), lines(after));
    let mut out: Vec<String> = a
        .symmetric_difference(&b)
        .filter_map(|l| {
            let rest = l.get(3..)?.trim();
            // Renames appear as "old -> new"; the new path is what was written.
            let p = rest.rsplit(" -> ").next().unwrap_or(rest);
            Some(p.trim_matches('"').to_string())
        })
        .filter(|p| !p.is_empty() && !p.starts_with(".knoot") && !p.starts_with(".claude"))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn deny_bash(path: &str, c: &Claim, raw: &str) -> DResp {
    let base = match deny(path, c) {
        DResp::Decision { reason: Some(r), .. } => r,
        other => return other,
    };
    DResp::Decision {
        allow: false,
        reason: Some(format!(
            "{base} This Bash command would write `{raw}`. Editing it by shell does not bypass the \
             claim."
        )),
    }
}

fn deny(path: &str, c: &Claim) -> DResp {
    let mins = (c.lease_until.saturating_sub(now_ms())) / 60_000;
    let intent = if c.intent.is_empty() { "unknown".to_string() } else { format!("\"{}\"", c.intent) };
    DResp::Decision {
        allow: false,
        reason: Some(format!(
            "knoot: `{path}` is currently claimed by {} (session {}…) — intent: {}. Lease expires in ~{}m. \
             Do not edit this file now: work on something else, or wait — you will be told automatically when it is released. \
             To coordinate directly, run: knoot msg {} \"your question\". `knoot who` lists all active sessions.",
            c.user,
            &c.session[..c.session.len().min(8)],
            intent,
            mins.max(1),
            c.user,
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
        mail: Arc::new(Mutex::new(HashMap::new())),
        stop_holds: Arc::new(Mutex::new(HashMap::new())),
        connected: Arc::new(Mutex::new(false)),
        last_error: Arc::new(Mutex::new(None)),
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

/// Dial the relay, presenting this user's token when one is known. A relay
/// with no token configured ignores the header, so the same client works
/// against a loopback relay and a hosted one.
pub(crate) async fn connect_authed(
    url: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    crate::install_tls_provider();
    let mut req = url.into_client_request()?;
    if let Some(tok) = crate::config::token_for(url) {
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {tok}").parse().map_err(|_| anyhow::anyhow!("bad token"))?,
        );
    }
    Ok(tokio_tungstenite::connect_async(req).await?)
}

/// Owns the WebSocket to the relay. Reconnects forever with backoff.
async fn relay_loop(cfg: RepoConfig, rc: Arc<RepoConn>, mut rx: mpsc::UnboundedReceiver<ClientMsg>) {
    let mut announced_auth_failure = false;
    loop {
        match connect_authed(&cfg.relay).await {
            Ok((ws, _)) => {
                announced_auth_failure = false;
                *rc.connected.lock().unwrap() = true;
                *rc.last_error.lock().unwrap() = None;
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
                                    deliver(&rc, &event);
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
            Err(e) => {
                *rc.connected.lock().unwrap() = false;
                // Rejected and unreachable look identical to an agent — both
                // fail open — but they are not the same thing to the human, so
                // say which, once, rather than every three seconds.
                let msg = e.to_string();
                *rc.last_error.lock().unwrap() = Some(msg.clone());
                if !announced_auth_failure && msg.contains("401") {
                    eprintln!(
                        "knoot: relay {} rejected this daemon's token. Coordination is OFF \
                         (edits are allowed, as always when the relay is unavailable). Fix with: \
                         knoot login --relay {} --token <token>",
                        cfg.relay, cfg.relay
                    );
                    announced_auth_failure = true;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
