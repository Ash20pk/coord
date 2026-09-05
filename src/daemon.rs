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

/// How long a recorded read stays worth checking against, and how many paths
/// per session are kept. An agent that read a file half an hour ago has
/// almost certainly moved on; one that read four hundred files is grepping,
/// not reasoning about them.
const READ_WINDOW_MS: u64 = 30 * 60 * 1000;
const READ_CAP: usize = 256;

/// How recently a peer must have created a path for "this already exists" to
/// be about them rather than about the repo's history.
const CREATE_WINDOW_MS: u64 = 10 * 60 * 1000;

/// At most this many advisory lines on one write. The brief is the highest
/// attention surface in the product and the cheapest to ruin.
const MAX_NOTES: usize = 3;

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
    /// What each session has read, and when: `session -> path -> ts`.
    ///
    /// STORM's single biggest result is that a write is stale when what the
    /// agent *read* has changed, even when the file being written is
    /// untouched — so a conflict has a semantic half that claims cannot see.
    /// knoot sees every `Read` through `PostToolUse`, so keeping this costs a
    /// hash-map insert per read.
    ///
    /// The design called this "the current turn"; it is kept for
    /// `READ_WINDOW_MS` instead, because a read from the *previous* turn is
    /// exactly what a peer's write between turns invalidates, and clearing it
    /// at the turn boundary would throw away the only case that matters.
    reads: Arc<Mutex<HashMap<String, HashMap<String, Ts>>>>,
    /// How this deployment seals memory. `Plaintext` until a Welcome says
    /// otherwise, and swapped for `Mls` behind the same interface — which is
    /// the whole point of the interface: nothing below this line changes.
    provider: Arc<Mutex<Arc<dyn crate::memory::KeyProvider>>>,
    /// This machine's MLS state, when the deployment seals with MLS. Shared
    /// with every other repo on this daemon, because a device is a machine.
    mls: Arc<Mutex<Option<Arc<MlsState>>>>,
    /// The daemon, so a connection can install the machine's MLS state the
    /// first time a relay asks for it.
    daemon: std::sync::Weak<Daemon>,
    /// The repo this connection is for, so the MLS setup can name a scope
    /// without being handed the config again.
    repo_root: String,
    /// Shared memory for this repo, opened. The daemon mirrors the ciphertext
    /// for the areas its key holds and does relevance and staleness here, on
    /// plaintext — the relay never sees a query.
    mem: Arc<Mutex<crate::memory::Cache>>,
    /// Who the relay says this key is. `None` until a Welcome lands, and
    /// permanently `None` for a key with no verified person behind it — which
    /// is also the set that may not publish, since a shard whose provenance is
    /// a display string is worse than no shard.
    me: Arc<Mutex<Option<crate::proto::Me>>>,
    connected: Arc<Mutex<bool>>,
    /// Why the last dial failed, kept so `knoot status` can say which kind of
    /// off this is rather than only that it is off.
    last_error: Arc<Mutex<Option<String>>>,
    /// True once a Welcome snapshot has been applied, i.e. the mirror is
    /// trustworthy. Until then we must not answer from an empty view.
    ready: Arc<Mutex<bool>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ServerMsg>>>>,
}

impl RepoConn {
    /// This machine's MLS state, when the deployment has one. `None` under
    /// `Plaintext`, where there is no group to reconcile.
    fn mls(&self) -> Option<Arc<MlsState>> {
        self.mls.lock().unwrap().clone()
    }
}

/// What a session's in-flight Bash command is expected to touch.
struct PendingBash {
    /// Paths the command is expected to delete or move away, and whether it
    /// was a move. Checked after the fact — a `rm` that failed deleted
    /// nothing, and announcing a deletion that did not happen is worse than
    /// missing one.
    removals: Vec<(String, bool)>,
    /// Working-tree fingerprint, present only when the command needed auditing.
    snapshot: Option<String>,
    taken_at: Ts,
    /// Repo-relative paths the parser predicted.
    targets: Vec<String>,
    /// The command itself. When a peer is also writing, naming a path is the
    /// evidence that a change was ours rather than theirs.
    command: String,
}

/// This machine's MLS identity, shared by every repo.
///
/// One per machine, because a device is a machine: a laptop is one leaf in a
/// room's group however many repos it has checked out. Giving each repo its
/// own would have several `Device`s fighting over one on-disk state and one
/// room — each proposing its own genesis, and the losers deleting the winner's
/// group out from under it.
struct MlsState {
    device: Arc<Mutex<crate::mls::Device>>,
    provider: Arc<crate::mls::Mls>,
    /// How far each room's handshake log has been applied. Per room, not per
    /// repo, for the same reason.
    seen: Mutex<HashMap<String, i64>>,
    /// Devices whose key package has been asked for, and the room they are
    /// wanted in.
    pending: Mutex<HashMap<String, String>>,
    /// Rooms whose group this machine has already started reconciling, so two
    /// repos coming up at once do not both propose genesis.
    claimed: Mutex<std::collections::HashSet<String>>,
}

#[derive(Default)]
struct Daemon {
    repos: Mutex<HashMap<String, Arc<RepoConn>>>,
    /// Set the first time a relay says it seals with MLS. Never replaced: a
    /// reconnect must not re-key the rooms this machine is already in.
    mls: Mutex<Option<Arc<MlsState>>>,
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
        DReq::PreWrite { repo_root, session, path, creating } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::allow(); // not knoot-enabled → fail open
            };
            ensure_session(&rc, &session, &whoami());
            let path = rel_path(&repo_root, &path);

            // Awareness first, and before the verdict either way: a stale
            // read is worth saying whether or not the write is allowed, and
            // when it is denied the note rides on the denial — the highest
            // attention surface there is.
            let mut notes = stale_read_notes(&rc, &session, &path);
            if creating {
                notes.extend(create_collision_note(&rc, &repo_root, &session, &path));
            }
            // A fact naming exactly this path. The brief is the
            // highest-attention surface in the product — the agent is
            // stopped, reading, about to act — and a convention it is about
            // to break is worth one line of it.
            notes.extend(memory_note_for_path(&rc, &path));

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
                v.conflicting_on(&session, &path, &branch)
                    .map(|c| v.claim_with_live_intent(c))
                    .map(|c| (c, user, v.is_hub(&path), v.queue_len(&path, &session)))
            };
            if let Some((c, user, hub, queued)) = local {
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
                return deny(&path, &c, hub, queued, notes);
            }

            // Acquire through the relay (authoritative), fail open on timeout.
            if !*rc.connected.lock().unwrap() {
                return DResp::allow_with(notes);
            }
            let id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            rc.pending.lock().unwrap().insert(id.clone(), tx);
            // Identity and intent must come from the owning session's record,
            // not from this daemon's environment — a daemon may serve sessions
            // started under a different user, and the brief names the holder.
            let (intent, user, branch, hub) = {
                let v = rc.view.lock().unwrap();
                let hub = v.is_hub(&path);
                match v.sessions.get(&session) {
                    Some(s) => (s.intent.clone(), s.user.clone(), s.branch.clone(), hub),
                    None => (String::new(), whoami(), String::new(), hub),
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
                // Only a client can know a *declared* hub: the relay never
                // sees the repo. It spots the rest from claim history itself.
                hub,
            });
            match tokio::time::timeout(std::time::Duration::from_millis(CLAIM_TIMEOUT_MS), rx).await {
                Ok(Ok(ServerMsg::ClaimResp { granted: false, holder, holder_user, holder_intent, lease_until, hub, queued, .. })) => {
                    let c = Claim {
                        session: holder.unwrap_or_default(),
                        user: holder_user.unwrap_or_else(|| "someone".into()),
                        path: path.clone(),
                        lease_until: lease_until.unwrap_or(0),
                        intent: holder_intent.unwrap_or_default(),
                        branch: String::new(),
                    };
                    // The relay answers with the intent recorded on the claim,
                    // which may predate whatever the holder is doing now. We
                    // have their session record, so prefer it.
                    let c = rc.view.lock().unwrap().claim_with_live_intent(&c);
                    deny(&path, &c, hub, queued, notes)
                }
                Ok(Ok(ServerMsg::ClaimResp { granted: true, lease_until, hub, .. })) => {
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
                    record_claim_locally(&rc, &sess_for_warn, &path, lease_until);
                    warn_cross_branch(&rc, &sess_for_warn, &path);
                    if hub {
                        notes.push(hub_note(&rc, &path, lease_until));
                    }
                    DResp::allow_with(notes)
                }
                _ => {
                    rc.pending.lock().unwrap().remove(&id);
                    // Timed out, or the relay went away mid-request → fail
                    // open. We must *not* record a claim here: we do not know
                    // that we hold it, and a mirror that invents claims would
                    // block peers over a file nobody owns.
                    warn_cross_branch(&rc, &sess_for_warn, &path);
                    DResp::allow_with(notes)
                }
            }
        }
        DReq::FileRead { repo_root, session, path } => {
            // Cheap by construction: one map insert per `Read`, no relay call,
            // no event on the log. The log records what an agent *did*; what
            // it looked at is local knowledge, and shipping every read to the
            // relay would multiply the log's volume for no reader.
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                {
                    // Through the returned connection, never a second lookup
                    // by the caller's spelling of the path: the map is keyed by
                    // the resolved root, and a miss here would drop the read
                    // silently — which is exactly what it did.
                    let path = rel_path(&rc.repo_root, &path);
                    let now = now_ms();
                    let mut reads = rc.reads.lock().unwrap();
                    let mine = reads.entry(session).or_default();
                    mine.retain(|_, ts| now.saturating_sub(*ts) <= READ_WINDOW_MS);
                    if mine.len() >= READ_CAP {
                        // An agent grepping the tree is not reasoning about
                        // four hundred files. Drop the oldest rather than the
                        // new one: recency is the whole signal here.
                        if let Some(oldest) =
                            mine.iter().min_by_key(|(_, ts)| **ts).map(|(p, _)| p.clone())
                        {
                            mine.remove(&oldest);
                        }
                    }
                    mine.insert(path, now);
                }
            }
            DResp::Ok
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
            let memory = memory_lines(&rc, &repo_root, &[]);
            let cached = cache_lines(&rc, &repo_root, &[]);
            let context = context_lines(&rc, &session);
            let v = rc.view.lock().unwrap();
            let writes = v.writes_since(&session, since);
            DResp::Peers {
                sessions: v.sessions.values().filter(|s| s.session != session).cloned().collect(),
                claims: v.claims.iter().filter(|c| c.session != session).cloned().collect(),
                // A session that has just started has read nothing, so this is
                // empty by construction — and stays correct if a resumed
                // session ever arrives here with reads already recorded.
                depended_on: depended_on(&rc, &session, &writes),
                writes,
                mail,
                notes: Vec::new(),
                // At SessionStart the agent has read nothing, so there is no
                // "about what you are touching" to filter on. The newest
                // facts are the best available guess, and they are what a
                // person joining the repo would be told.
                memory,
                cached,
                context,
            }
        }
        DReq::Intent { repo_root, session, text, user, branch } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else { return DResp::Ok };
            ensure_session(&rc, &session, &user);
            // Before recording ours, or every session matches itself.
            let mut notes = duplicate_intent_notes(&rc, &session, &user, &text);
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
            // Before the view lock: both of these take it themselves, and the
            // brief is not worth a deadlock.
            let touched = session_paths(&rc, &session);
            let memory = memory_lines(&rc, &repo_root, &touched);
            let cached = cache_lines(&rc, &repo_root, &touched);
            // Every peer's context, not only those touching our paths: the
            // point is to learn what somebody else is doing *before* the
            // paths overlap, which is the moment it stops being avoidable.
            let context = context_lines(&rc, &session);
            let mut v = rc.view.lock().unwrap();
            v.prune();
            let writes = v.writes_since(&session, since);
            notes.truncate(MAX_NOTES);
            DResp::Peers {
                sessions: v.sessions.values().filter(|s| s.session != session).cloned().collect(),
                claims: v.claims.iter().filter(|c| c.session != session).cloned().collect(),
                // Which of those writes this session had actually read. "The
                // ground moved" matters most where the agent was standing,
                // and until now the brief could not tell the difference.
                depended_on: depended_on(&rc, &session, &writes),
                writes,
                mail,
                notes,
                // Facts about the paths this session has actually been in.
                // Everything else this repo knows is a distraction from the
                // turn about to start.
                memory,
                cached,
                context,
            }
        }
        DReq::SessionEnd { repo_root, session } => {
            if let Some(rc) = ensure_repo(d, &repo_root).await {
                // A session's context is memory in the sense that a room is a
                // memory: it exists while people are in it. Outliving the
                // session would make a finished plan look like a live one,
                // which is worse than no plan at all.
                let ids = rc
                    .mem
                    .lock()
                    .unwrap()
                    .ids_named(crate::memory::Kind::SessionContext, &session);
                if !ids.is_empty() {
                    rc.mem.lock().unwrap().forget(&ids);
                    let _ = rc.tx.send(ClientMsg::MemForget { ids });
                }
                let ev = Event::SessionEnded { session: session.clone(), ts: now_ms() };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::ReleaseSession { session });
            }
            DResp::Ok
        }
        DReq::BashPre { repo_root, session, command } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::allow();
            };
            let a = crate::bashparse::analyze(&command);
            let mut notes = Vec::new();

            // Gate every path the command is expected to write.
            for raw in &a.targets {
                let path = normalize(&repo_root, raw);
                if path.is_empty() {
                    continue; // outside the repo
                }
                notes.extend(stale_read_notes(&rc, &session, &path));
                let hit = {
                    let branch = branch_of(&rc, &session);
                    let v = rc.view.lock().unwrap();
                    v.conflicting_on(&session, &path, &branch)
                        .map(|c| v.claim_with_live_intent(c))
                        .map(|c| (c, v.is_hub(&path), v.queue_len(&path, &session)))
                };
                if let Some((c, hub, queued)) = hit {
                    report_denied(&rc, &session, &path, &c);
                    notes.truncate(MAX_NOTES);
                    return deny_bash(&path, &c, raw, hub, queued, notes);
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
            // Deletions are checked after the fact, not predicted: a `rm`
            // that failed removed nothing, and announcing a deletion that did
            // not happen is worse than missing one.
            let removals: Vec<(String, bool)> = a
                .removals
                .iter()
                .map(|(raw, moved)| (normalize(&repo_root, raw), *moved))
                .filter(|(p, _)| !p.is_empty())
                .collect();
            if snapshot.is_some() || !targets.is_empty() || !removals.is_empty() {
                d.snapshots.lock().unwrap().insert(
                    (repo_root.clone(), session.clone()),
                    PendingBash {
                        snapshot,
                        taken_at: now_ms(),
                        targets,
                        removals,
                        command: command.clone(),
                    },
                );
            }
            notes.truncate(MAX_NOTES);
            DResp::allow_with(notes)
        }
        DReq::BashPost { repo_root, session } => {
            let pending = d.snapshots.lock().unwrap().remove(&(repo_root.clone(), session.clone()));
            let Some(pending) = pending else { return DResp::Ok };
            let Some(rc) = ensure_repo(d, &repo_root).await else { return DResp::Ok };
            let taken_at = pending.taken_at;

            // Shell writes we predicted are still writes: record authorship, or
            // the log cannot tell a peer's edit from an unattributed one.
            let user = user_of(&rc, &session);
            let gone = |p: &String| {
                pending.removals.iter().any(|(r, _)| r == p)
                    && !std::path::Path::new(&repo_root).join(p).exists()
            };
            for path in pending.targets.iter().filter(|p| !gone(p)) {
                let ev = Event::FileWritten {
                    session: session.clone(),
                    user: user.clone(),
                    path: path.clone(),
                    ts: now_ms(),
                };
                rc.view.lock().unwrap().apply(&ev);
                let _ = rc.tx.send(ClientMsg::Append { event: ev });
            }

            // A path the command was expected to remove, and which is in fact
            // gone. Broadcast rather than blocked: the deletion has happened,
            // and the only useful act left is telling everyone who was
            // standing on it.
            for (path, moved) in &pending.removals {
                if std::path::Path::new(&repo_root).join(path).exists() {
                    continue;
                }
                let ev = Event::PathRemoved {
                    session: session.clone(),
                    user: user.clone(),
                    path: path.clone(),
                    moved: *moved,
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
        DReq::Remember { repo_root, session, user, name, text, paths, from } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            let root = std::path::Path::new(&repo_root);
            // `--from` is one `knoot remember` with the file's text in it, and
            // it goes through exactly the refusals a typed fact does. That is
            // why there is no `project_files` kind: a kind with its own
            // default-off switch is a design admitting it is afraid of itself.
            let (text, paths) = match &from {
                Some(f) => {
                    let rel = rel_path(&repo_root, f);
                    match crate::memory::read_publishable(root, &rel) {
                        Ok(t) => (t, vec![rel]),
                        // On the log like every other refusal. This is the
                        // case the rule exists for — an agent reaching for a
                        // `.env` — and it must not be the one that goes
                        // unrecorded because it was caught a step earlier.
                        Err(r) => {
                            let reason = r.to_string();
                            let _ = rc.tx.send(ClientMsg::Append {
                                event: Event::MemoryRefused {
                                    session: session.clone(),
                                    user: user.clone(),
                                    name: name.clone(),
                                    reason: reason.clone(),
                                    ts: now_ms(),
                                },
                            });
                            return DResp::Err { msg: reason };
                        }
                    }
                }
                None => (text, paths.iter().map(|p| rel_path(&repo_root, p)).collect()),
            };
            match publish_shard(
                &rc,
                &repo_root,
                &session,
                &user,
                crate::memory::Kind::Facts,
                &name,
                &text,
                &paths,
                &[],
            ) {
                Ok(name) => DResp::Memory {
                    items: vec![name],
                    unreadable: 0,
                    provider: rc.provider.lock().unwrap().label().to_string(),
                    ready: true,
                    identified: true,
                },
                Err(msg) => DResp::Err { msg },
            }
        }
        DReq::Plan { repo_root, session, user, text, paths, decisions } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            let paths: Vec<String> = paths.iter().map(|p| rel_path(&repo_root, p)).collect();
            // The session id is the name, so a session that replans supersedes
            // itself instead of leaving two plans standing.
            match publish_shard(
                &rc,
                &repo_root,
                &session,
                &user,
                crate::memory::Kind::SessionContext,
                &session,
                &text,
                &paths,
                &decisions,
            ) {
                Ok(_) => DResp::Ok,
                Err(msg) => DResp::Err { msg },
            }
        }
        DReq::Cache { repo_root, session, user, name, text, paths } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            let paths: Vec<String> = paths.iter().map(|p| rel_path(&repo_root, p)).collect();
            match publish_shard(
                &rc,
                &repo_root,
                &session,
                &user,
                crate::memory::Kind::RepoCache,
                &name,
                &text,
                &paths,
                &[],
            ) {
                Ok(name) => DResp::Memory {
                    items: vec![name],
                    unreadable: 0,
                    provider: rc.provider.lock().unwrap().label().to_string(),
                    ready: true,
                    identified: true,
                },
                Err(msg) => DResp::Err { msg },
            }
        }
        DReq::Recall { repo_root, query } => {
            let Some(rc) = ensure_repo(d, &repo_root).await else {
                return DResp::Err { msg: "repo not knoot-enabled (run `knoot init`)".into() };
            };
            // The mirror arrives on the same socket as everything else, so a
            // cold daemon has to be given the same beat `who` is given.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let (last_write, users) = {
                let v = rc.view.lock().unwrap();
                let users: HashMap<String, String> =
                    v.sessions.iter().map(|(k, s)| (k.clone(), s.user.clone())).collect();
                (v.last_write.clone(), users)
            };
            let lookup =
                |s: &str| users.get(s).cloned().unwrap_or_else(|| s.to_string());
            let root = std::path::Path::new(&repo_root);
            let p = rc.provider.lock().unwrap().clone();
            let provider = p.label().to_string();
            let identified = rc.me.lock().unwrap().is_some();
            // Whether this machine can seal at all: under `Mls`, the room's
            // group has to have reached it first.
            let ready = !p.confidential()
                || rc.me.lock().unwrap().as_ref().is_some_and(|me| {
                    RepoConfig::load(root).is_some_and(|cfg| {
                        p.epoch(&crate::memory::Scope {
                            team: me.team_id.clone(),
                            repo: cfg.repo,
                            area: crate::config::ROOT_AREA.into(),
                        })
                        .0 != 0
                    })
                });
            let cache = rc.mem.lock().unwrap();
            let held =
                if query.trim().is_empty() { cache.heads() } else { cache.search(&query) };
            let items = held
                .into_iter()
                .map(|h| {
                    let stale = crate::memory::staleness(h, &last_write, &lookup, Some(root));
                    format!(
                        "[{}] {}\n  {}\n  — {}, {}{}{}",
                        h.shard.kind,
                        h.fact.name,
                        h.fact.text,
                        h.shard.author_email,
                        // `ago` takes an elapsed duration, not an instant.
                        format!("{} ago", ago(now_ms().saturating_sub(h.shard.created_ts))),
                        if h.fact.paths.is_empty() {
                            String::new()
                        } else {
                            format!(", about {}", h.fact.paths.join(" "))
                        },
                        match stale {
                            Some(s) => format!("\n  ⚠ {s}"),
                            None => String::new(),
                        }
                    )
                })
                .collect();
            DResp::Memory {
                items,
                unreadable: cache.unreadable(),
                provider,
                ready,
                identified,
            }
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
                notes: Vec::new(),
                depended_on: Vec::new(),
                memory: Vec::new(),
                cached: Vec::new(),
                context: Vec::new(),
            }
        }
    }
}

// --------------------------------------------------------------------- mls
//
// The daemon's side of the Delivery Service. Everything here is best-effort:
// a room that will not converge means no memory, never a blocked write.

/// Where a device's MLS state lives. Beside the credential, because it is the
/// same kind of thing.
fn mls_dir() -> PathBuf {
    // Overridable so a test never writes group state into the user's home,
    // and so several daemons can run on one machine — the same reason
    // `KNOOT_SOCK` exists.
    if let Some(p) = crate::config::env_or_legacy("KNOOT_MLS_DIR") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".knoot").join("mls")
}

fn scope_of(key: &str) -> crate::memory::Scope {
    let mut p = key.splitn(3, '/');
    crate::memory::Scope {
        team: p.next().unwrap_or_default().to_string(),
        repo: p.next().unwrap_or_default().to_string(),
        area: p.next().unwrap_or("/").to_string(),
    }
}

/// Open this machine's MLS identity and swap the key provider for it.
///
/// The swap is the phase's whole claim about the interface: nothing that seals
/// or opens a shard changes, and the relay's code is identical either way.
fn setup_mls(rc: &Arc<RepoConn>, me: &crate::proto::Me) {
    if me.device_id.is_empty() {
        // A verified person but no device row — a console session rather than
        // a machine. Nothing to seal with, so nothing is sealed.
        return;
    }
    let Some(d) = rc.daemon.upgrade() else { return };

    // One lock across check *and* set. Two repos coming up together both
    // found no state, both opened a `Device` over the same directory, and the
    // second overwrote the first — leaving two devices on one machine, each
    // building its own group for the same room. Opening the device does file
    // I/O under this lock, which is fine: it happens once per machine.
    let mut slot = d.mls.lock().unwrap();
    if let Some(state) = slot.clone() {
        *rc.mls.lock().unwrap() = Some(state.clone());
        *rc.provider.lock().unwrap() = state.provider.clone();
        return;
    }
    let Ok(device) = crate::mls::Device::open(&mls_dir(), &me.device_id) else {
        eprintln!(
            "knoot: this relay seals memory with MLS and this machine could not open its \
             key material. Memory is OFF here; coordination is unaffected."
        );
        return;
    };
    let device = Arc::new(Mutex::new(device));
    // Scopes are bound in `mls_reconcile`, which is where the repo id is
    // known: a scope is `team/repo/area` and the room that grants the area
    // owns its key.
    let provider = Arc::new(crate::mls::Mls::new(device.clone()));
    let state = Arc::new(MlsState {
        device: device.clone(),
        provider: provider.clone(),
        seen: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        claimed: Mutex::new(Default::default()),
    });
    *slot = Some(state.clone());
    drop(slot);
    *rc.mls.lock().unwrap() = Some(state);
    *rc.provider.lock().unwrap() = provider;

    // The public half goes up so other members can add this machine; the
    // private half never leaves it.
    let kp = device.lock().unwrap().key_package();
    if let Ok(kp) = kp {
        let _ = rc.tx.send(ClientMsg::MlsUpload { key_package: crate::memory::hex(&kp) });
    }
}

/// Bind this repo's scopes to their rooms, and ask the DS where each room is.
fn mls_reconcile(rc: &Arc<RepoConn>) {
    let Some(me) = rc.me.lock().unwrap().clone() else { return };
    let Some(state) = rc.mls() else { return };
    let Some(cfg) = RepoConfig::load(std::path::Path::new(&rc.repo_root)) else { return };
    // Bind now that the repo id is known: a scope is `team/repo/area`, and the
    // room that grants the area owns its key.
    for (room, area) in &me.rooms {
        let scope = crate::memory::Scope {
            team: me.team_id.clone(),
            repo: cfg.repo.clone(),
            area: area.clone(),
        };
        state.provider.bind(&scope.key(), room);
    }
    for (room, _) in &me.rooms {
        let since = state.seen.lock().unwrap().get(room).copied().unwrap_or(0);
        let _ = rc.tx.send(ClientMsg::MlsSync { room: room.clone(), since });
    }
}

/// Apply a room's handshake log, then look at whether the room needs anything.
fn mls_apply(rc: &Arc<RepoConn>, room: &str, msgs: Vec<crate::mls::Envelope>, started: bool) {
    let Some(state) = rc.mls() else { return };
    let dev = state.device.clone();
    {
        let mut d = dev.lock().unwrap();
        for env in &msgs {
            match env.kind.as_str() {
                // Genesis: an empty commit whose only job was to decide, at
                // the Delivery Service, which device started the room. There
                // is nothing to apply.
                "commit" if env.blob.is_empty() => {}
                "commit" => {
                    // Our own commit comes back to us; we merged it when the
                    // DS accepted it, and processing it again is an error we
                    // want to ignore rather than a state we want to reach.
                    let _ = d.process(room, &env.blob);
                }
                "welcome" => {
                    let _ = d.join(room, &env.blob);
                }
                _ => {}
            }
            state.seen.lock().unwrap().insert(room.to_string(), env.seq);
        }
    }

    // This device's key material may just have changed — it processed a
    // commit, or was welcomed into the room. Anything it was holding and
    // could not open may open now, and nothing would re-send it.
    if !msgs.is_empty() {
        let provider = rc.provider.lock().unwrap().clone();
        let opened = rc.mem.lock().unwrap().retry(provider.as_ref(), &|k| scope_of(k));
        // A room that will not converge is otherwise completely silent, and
        // that silence cost most of a day once. `KNOOT_DEBUG_MLS=1` is the
        // only way to see an epoch that is not moving.
        if std::env::var("KNOOT_DEBUG_MLS").is_ok() {
            // Each lock taken and released in turn: two of these live at once
            // in one expression is a deadlock, and a diagnostic that hangs the
            // thing it is diagnosing is worse than none.
            let group_epoch = dev.lock().unwrap().epoch(room);
            let pending = rc.mem.lock().unwrap().unreadable_epochs();
            let left = rc.mem.lock().unwrap().unreadable();
            let mut detail = Vec::new();
            for (id, scope, ep) in pending {
                let want = provider.epoch(&scope_of(&scope)).0;
                detail.push(format!("{id}@{ep} provider_now={want}"));
            }
            eprintln!(
                "knoot: [mls] {room} epoch {group_epoch:?}, retry opened {opened}, \
                 {left} unreadable {detail:?}"
            );
        }
        let _ = opened;
    }

    // A room nobody has started: offer to start it. The DS's unique index on
    // (room, epoch) decides between two daemons doing this at once, and the
    // loser forgets the group it built.
    let in_group = dev.lock().unwrap().in_room(room);
    if !started && !in_group {
        // Once per machine, not once per repo: two repos in the same room
        // both proposing genesis would have one of them delete the other's
        // group when the DS refused it.
        //
        // The one that declines must come back, though. It is not *rejected*
        // — it never sent a commit — so nothing else would ever wake it, and
        // its rooms would stay keyless forever.
        if !state.claimed.lock().unwrap().insert(room.to_string()) {
            let (rc, room) = (rc.clone(), room.to_string());
            tokio::spawn(async move {
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if rc.mls().is_some_and(|s| s.device.lock().unwrap().in_room(&room)) {
                        return;
                    }
                    let since = rc
                        .mls()
                        .map(|s| s.seen.lock().unwrap().get(&room).copied().unwrap_or(0))
                        .unwrap_or(0);
                    let _ = rc.tx.send(ClientMsg::MlsSync { room: room.clone(), since });
                }
            });
            return;
        }
        if dev.lock().unwrap().create_room(room).is_ok() {
            let _ = rc.tx.send(ClientMsg::MlsCommit {
                room: room.into(),
                epoch: 0,
                commit: String::new(),
                welcome: None,
                for_device: None,
            });
        }
        return;
    }
    if in_group {
        let _ = rc.tx.send(ClientMsg::MlsRoster { room: room.into() });
    }
}

/// The room's roster against the group's membership: add what is missing,
/// remove what should not be there.
///
/// Any member may do this, and several will try at once. That is fine and is
/// the design: the Delivery Service serialises them, and a daemon whose commit
/// loses discards it and sees the winner's on the next sync.
fn mls_roster(rc: &Arc<RepoConn>, room: &str, devices: Vec<String>) {
    let Some(state) = rc.mls() else { return };
    let dev = state.device.clone();
    let (mine, in_group) = {
        let d = dev.lock().unwrap();
        (d.device_id.clone(), d.members(room))
    };
    // Every machine that belongs here and is not in the group yet.
    //
    // *Every*, not the first: asking for one and stopping meant a colleague
    // who had never run `knoot join` — a device row with no key package —
    // blocked everybody behind them out of the group indefinitely, because
    // nothing else would wake this room. Each answer is handled on its own,
    // and only one of the resulting commits can land per epoch; the rest are
    // refused by the Delivery Service, discarded, and retried on the next
    // wake. That is the same optimism the claim path runs on.
    let missing: Vec<&String> = devices.iter().filter(|x| !in_group.contains(x)).collect();
    if !missing.is_empty() {
        for device in missing {
            // The map only joins a request to its answer; it is not a lock.
            state.pending.lock().unwrap().insert(device.clone(), room.to_string());
            let _ = rc.tx.send(ClientMsg::MlsKeyPackage { device: device.clone() });
        }
        return;
    }
    // A leaf that is no longer on the roster — a revoked laptop, or a person
    // taken out of the room. This is the removal that moves the room to an
    // epoch the departed device cannot derive.
    if let Some(gone) = in_group.iter().find(|x| **x != mine && !devices.contains(x)) {
        let hs = { dev.lock().unwrap().remove_device(room, gone) };
        if let Ok(hs) = hs {
            let _ = rc.tx.send(ClientMsg::MlsCommit {
                room: room.into(),
                epoch: hs.epoch,
                commit: crate::memory::hex(&hs.commit),
                welcome: None,
                for_device: None,
            });
            mls_committed(rc, room, dev);
        }
    }
}

/// A device's key package came back: add it to the room it was wanted for.
fn mls_add_device(rc: &Arc<RepoConn>, device_id: &str, key_package: Option<String>) {
    let Some(state) = rc.mls() else { return };
    let Some(room) = state.pending.lock().unwrap().remove(device_id) else { return };
    // No key package means that machine has not run `knoot join` against this
    // relay yet. Nothing to do but wait for it; it will upload on connect.
    let Some(kp) = key_package else { return };
    let dev = state.device.clone();
    let hs = { dev.lock().unwrap().add_device(&room, &crate::memory::unhex(&kp)) };
    let Ok(hs) = hs else { return };
    let _ = rc.tx.send(ClientMsg::MlsCommit {
        room: room.clone(),
        epoch: hs.epoch,
        commit: crate::memory::hex(&hs.commit),
        welcome: hs.welcome.as_deref().map(crate::memory::hex),
        for_device: Some(device_id.to_string()),
    });
    mls_committed(rc, &room, dev);
}

/// Merge a commit this device proposed, and re-seal what the room holds.
///
/// Optimistic: merged as soon as it is sent, and undone by `MlsRejected` if
/// the Delivery Service took somebody else's instead. The alternative — wait
/// for the DS before merging — would leave the group unable to propose
/// anything else in the meantime, which is worse for a case that is rare.
///
/// The rewrap runs after **every** epoch change, not only after a removal.
/// §5 only asked for it on a Remove, and that is half the story: MLS gives
/// forward secrecy, so a device that joins at epoch *n* cannot derive *n-1*
/// either — a new member could see every shard in the room and open none of
/// them, which is the same bug in the other direction and worse, because it
/// looks like an empty room rather than a broken one. So whoever commits an
/// epoch change re-seals what it can read into the new epoch. Nothing is
/// orphaned: anything nobody rewraps expires on its retention.
fn mls_committed(rc: &Arc<RepoConn>, room: &str, dev: Arc<Mutex<crate::mls::Device>>) {
    if dev.lock().unwrap().merge_own(room).is_err() {
        return;
    }
    rewrap_shards(rc);
}

/// Re-seal every shard this machine can read under the room's current epoch.
///
/// Across **every** repo, not only the one whose connection made the commit.
/// MLS state is per machine and memory caches are per repo, so a room's epoch
/// change reaches shards this connection has never seen — and the ones it has
/// not seen are exactly the ones nobody else will rewrap either, because
/// nobody else is in this room on this machine.
///
/// Only the sealed bytes change: the id, the scope, the author and the author's
/// email stay exactly as they were, and they are still what the seal is bound
/// to. So a rewrap rotates the key without touching provenance — which is why
/// this is a message of its own rather than a republish under a new author.
fn rewrap_shards(rc: &Arc<RepoConn>) {
    let conns: Vec<Arc<RepoConn>> = match rc.daemon.upgrade() {
        Some(d) => d.repos.lock().unwrap().values().cloned().collect(),
        None => vec![rc.clone()],
    };
    for conn in conns {
        rewrap_one(&conn);
    }
}

fn rewrap_one(rc: &Arc<RepoConn>) {
    let provider = rc.provider.lock().unwrap().clone();
    let held: Vec<(crate::memory::Shard, crate::memory::Fact)> = {
        let cache = rc.mem.lock().unwrap();
        cache.all().map(|h| (h.shard.clone(), h.fact.clone())).collect()
    };
    for (shard, fact) in held {
        let scope = scope_of(&shard.scope);
        let Ok(plain) = serde_json::to_vec(&fact) else { continue };
        let (epoch, _) = provider.epoch(&scope);
        if epoch == shard.epoch || epoch == 0 {
            continue;
        }
        let aad = crate::memory::aad(
            &shard.id,
            &shard.scope,
            &shard.kind,
            &shard.author,
            &shard.author_email,
            epoch,
        );
        let sealed = provider.seal(&scope, &aad, &plain);
        let _ = rc.tx.send(ClientMsg::MemRewrap {
            id: shard.id.clone(),
            epoch: sealed.epoch,
            nonce: crate::memory::hex(&sealed.nonce),
            ciphertext: crate::memory::hex(&sealed.ciphertext),
        });
    }
}

// ------------------------------------------------------------------ memory

/// How much of the turn's brief memory may take. The rest of the injection is
/// already fighting for attention; a memory section that runs on stops being
/// read, and then the section that would have mattered is not read either.
const MEMORY_BUDGET_BYTES: usize = 1_500;
const MEMORY_MAX_LINES: usize = 6;

/// The paths this session has been in: what it holds, and what it has read.
/// This is what makes the memory section about *this* turn rather than about
/// the repo.
fn session_paths(rc: &Arc<RepoConn>, session: &str) -> Vec<String> {
    let mut out: Vec<String> = rc
        .view
        .lock()
        .unwrap()
        .claims
        .iter()
        .filter(|c| c.session == session)
        .map(|c| c.path.clone())
        .collect();
    if let Some(reads) = rc.reads.lock().unwrap().get(session) {
        out.extend(reads.keys().cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// The memory section of a brief: facts about these paths, newest supersession
/// first, stale ones flagged with who changed what.
///
/// With no paths — a session that has just started — the newest facts stand in,
/// because a repo's most recent conventions are the best available guess at
/// what somebody arriving needs.
fn memory_lines(rc: &Arc<RepoConn>, repo_root: &str, paths: &[String]) -> Vec<String> {
    use crate::memory::Kind;
    let (last_write, users) = write_context(rc);
    let root = std::path::Path::new(repo_root);
    let lookup = |session: &str| users.get(session).cloned().unwrap_or_else(|| session.to_string());

    let cache = rc.mem.lock().unwrap();
    let held: Vec<&crate::memory::Held> = if paths.is_empty() {
        cache.heads_of(Kind::Facts)
    } else {
        cache.about(Kind::Facts, paths)
    };
    let mut out = Vec::new();
    let mut used = 0;
    for h in held {
        let stale = crate::memory::staleness(h, &last_write, &lookup, Some(root));
        let line = format!(
            "{} — {} ({}{}){}",
            h.fact.name,
            truncate(&h.fact.text, 240),
            h.shard.author_email,
            if h.fact.paths.is_empty() {
                String::new()
            } else {
                format!(", about {}", h.fact.paths.join(" "))
            },
            match &stale {
                Some(s) => format!("  ⚠ {s}"),
                None => String::new(),
            }
        );
        used += line.len();
        if used > MEMORY_BUDGET_BYTES || out.len() >= MEMORY_MAX_LINES {
            break;
        }
        out.push(line);
    }
    out
}

/// Derived knowledge about the paths this session is in — where something
/// lives, how the tests run, what a module does.
///
/// Anything whose ground has moved is **dropped**, not flagged. That is the
/// whole difference from a fact: a fact was written on purpose and "priya
/// changed this since" is what its reader needs, whereas derived knowledge
/// past its files is simply wrong, and it was cheap to work out.
fn cache_lines(rc: &Arc<RepoConn>, repo_root: &str, paths: &[String]) -> Vec<String> {
    use crate::memory::Kind;
    let (last_write, users) = write_context(rc);
    let root = std::path::Path::new(repo_root);
    let lookup = |session: &str| users.get(session).cloned().unwrap_or_else(|| session.to_string());

    let cache = rc.mem.lock().unwrap();
    let held: Vec<&crate::memory::Held> = if paths.is_empty() {
        cache.heads_of(Kind::RepoCache)
    } else {
        cache.about(Kind::RepoCache, paths)
    };
    let mut out = Vec::new();
    let mut used = 0;
    for h in held {
        if Kind::RepoCache.invalidated_by_writes()
            && crate::memory::staleness(h, &last_write, &lookup, Some(root)).is_some()
        {
            continue;
        }
        let line = format!("{} — {}", h.fact.name, truncate(&h.fact.text, 240));
        used += line.len();
        if used > MEMORY_BUDGET_BYTES || out.len() >= MEMORY_MAX_LINES {
            break;
        }
        out.push(line);
    }
    out
}

/// What peers in this area are doing, at a depth the intent sentence cannot
/// reach: the plan, the paths, and what has already been settled.
///
/// This is memory in the sense that a room is a memory — it exists while
/// people are in it. `mine` is dropped: an agent does not need its own plan
/// read back, and that budget is what a peer's plan needs.
fn context_lines(rc: &Arc<RepoConn>, mine: &str) -> Vec<String> {
    let cache = rc.mem.lock().unwrap();
    let mut out = Vec::new();
    let mut used = 0;
    for h in cache.peer_context(mine) {
        let mut line = format!(
            "{} is working on: {}",
            h.shard.author_email,
            truncate(&h.fact.text, 240)
        );
        if !h.fact.paths.is_empty() {
            line.push_str(&format!("  [in: {}]", h.fact.paths.join(", ")));
        }
        for d in h.fact.decisions.iter().take(3) {
            line.push_str(&format!("\n  decided: {}", truncate(d, 160)));
        }
        used += line.len();
        if used > MEMORY_BUDGET_BYTES || out.len() >= MEMORY_MAX_LINES {
            break;
        }
        out.push(line);
    }
    out
}

/// The log's view of what has been written, and who by. Shared by every
/// staleness check so they cannot disagree.
fn write_context(
    rc: &Arc<RepoConn>,
) -> (HashMap<String, (String, Ts)>, HashMap<String, String>) {
    let v = rc.view.lock().unwrap();
    let users: HashMap<String, String> =
        v.sessions.iter().map(|(k, s)| (k.clone(), s.user.clone())).collect();
    (v.last_write.clone(), users)
}

/// A fact naming exactly this path, for the `PreToolUse` brief.
///
/// One line, and only for an exact match: the brief is the highest-attention
/// surface in the product and the fastest way to lose it is to spend it on
/// something that was merely nearby.
fn memory_note_for_path(rc: &Arc<RepoConn>, path: &str) -> Option<String> {
    let cache = rc.mem.lock().unwrap();
    let h = cache
        .heads()
        .into_iter()
        .find(|h| h.fact.paths.iter().any(|p| p.trim_matches('/') == path.trim_matches('/')))?;
    Some(format!(
        "knoot: what this team knows about {path} — {} ({})",
        truncate(&h.fact.text, 240),
        h.shard.author_email
    ))
}

/// Seal a fact and publish it, or say why not.
///
/// Every refusal is also an event on the log. An admin wants to know that an
/// agent tried to publish a `.env`, and the attempt is exactly the information
/// a warning would have thrown away.
#[allow(clippy::too_many_arguments)]
fn publish_shard(
    rc: &Arc<RepoConn>,
    repo_root: &str,
    session: &str,
    user: &str,
    kind: crate::memory::Kind,
    name: &str,
    text: &str,
    paths: &[String],
    decisions: &[String],
) -> Result<String, String> {
    use crate::memory::{self};

    let root = std::path::Path::new(repo_root);
    let refuse = |rc: &Arc<RepoConn>, r: memory::Refusal| -> String {
        let why = r.to_string();
        let _ = rc.tx.send(ClientMsg::Append {
            event: Event::MemoryRefused {
                session: session.to_string(),
                user: user.to_string(),
                name: name.to_string(),
                reason: why.clone(),
                ts: now_ms(),
            },
        });
        why
    };

    // Every kind goes through the same refusals. A `repo_cache` entry is
    // where a secret is *most* likely to end up — it is derived from files,
    // and "how the deploy authenticates" is a plausible thing for an agent to
    // work out and cache.
    if let Some(r) = memory::refuse_text(text) {
        return Err(refuse(rc, r));
    }
    for d in decisions {
        if let Some(r) = memory::refuse_text(d) {
            return Err(refuse(rc, r));
        }
    }
    for p in paths {
        if let Some(r) = memory::refuse_path(root, p) {
            return Err(refuse(rc, r));
        }
    }

    let Some(me) = rc.me.lock().unwrap().clone() else {
        return Err("this key names no verified person, so it may not publish memory \
                    (run `knoot join <key>`)"
            .into());
    };
    let cfg = RepoConfig::load(root).ok_or_else(|| "repo is not enrolled".to_string())?;
    // A fact belongs to the area of the code it is about. With no paths it is
    // about the repo, which is the root area.
    let area = paths
        .first()
        .map(|p| cfg.area_for(p))
        .unwrap_or_else(|| crate::config::ROOT_AREA.to_string());
    let scope = memory::Scope { team: me.team_id.clone(), repo: cfg.repo.clone(), area };

    let hashes = paths
        .iter()
        .filter_map(|p| memory::hash_file(root, p).map(|h| (p.clone(), h)))
        .collect();
    let fact = memory::Fact {
        name: name.to_string(),
        text: text.to_string(),
        paths: paths.to_vec(),
        hashes,
        decisions: decisions.to_vec(),
    };
    let plain = serde_json::to_vec(&fact).map_err(|e| e.to_string())?;

    // A second fact under the same name supersedes the first rather than
    // standing beside it. This is the contradiction path, and it must never be
    // a dedupe: the case that matters is precisely a near-duplicate that says
    // the opposite.
    let supersedes =
        rc.mem.lock().unwrap().head_of(&me.member_id, kind, name).map(|h| h.shard.id.clone());

    // Whatever this deployment seals with. The whole point of the interface:
    // this function does not know, and does not change, when the answer does.
    let provider = rc.provider.lock().unwrap().clone();
    let id = format!("sh_{}", uuid::Uuid::new_v4().simple());
    let (epoch, secret) = provider.epoch(&scope);
    // No key for this scope. Under `Mls` that means the room's group has not
    // reached this machine yet — a fresh join, or a relay that is still
    // catching up. Refused, and said plainly: sealing under a key nobody has
    // would store a fact that not even the author can read, which looks like
    // success and is worse than a refusal.
    if epoch == 0 && provider.confidential() {
        return Err(
            "this room's key has not reached this machine yet — try again in a moment".into()
        );
    }
    let scope_key = scope.key();
    let aad = memory::aad(&id, &scope_key, kind.as_str(), &me.member_id, &me.email, epoch);
    let sealed = provider.seal(&scope, &aad, &plain);
    let now = now_ms();
    let shard = memory::Shard {
        id,
        scope: scope_key,
        kind: kind.as_str().into(),
        author: me.member_id.clone(),
        author_email: me.email.clone(),
        device: String::new(),
        name_blind: memory::name_blind(&secret, name),
        supersedes,
        epoch,
        nonce: sealed.nonce,
        ciphertext: sealed.ciphertext,
        bytes: plain.len() as i64,
        seq: 0,
        created_ts: now,
        // A backstop only: the room's policy is what actually decides, and
        // the relay stamps it on accept.
        expires_ts: kind.ttl_ms().map(|ttl| now + ttl),
    };
    // Into our own cache immediately: a fact this machine just wrote must be
    // readable on the next turn whether or not the relay answered.
    rc.mem.lock().unwrap().apply(provider.as_ref(), &scope, shard.clone());
    rc.tx
        .send(ClientMsg::MemPublish { shard })
        .map_err(|_| "not connected to the relay".to_string())?;
    Ok(name.to_string())
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
            // A deletion is only news to a session that was standing on the
            // path: one that read it, or holds a claim on it. Everybody else
            // gets nothing, which is what keeps this from being noise.
            Event::PathRemoved { path, user, moved, session: by_session, .. } => {
                let verb = if *moved { "moved" } else { "deleted" };
                let mut who: std::collections::BTreeSet<String> = v
                    .claims
                    .iter()
                    .filter(|c| c.session != *by_session && paths_overlap(&c.path, path))
                    .map(|c| c.user.to_lowercase())
                    .collect();
                for (sess, paths) in rc.reads.lock().unwrap().iter() {
                    if sess == by_session || !paths.contains_key(path) {
                        continue;
                    }
                    if let Some(s) = v.sessions.get(sess) {
                        who.insert(s.user.to_lowercase());
                    }
                }
                who.remove(&user.to_lowercase());
                who.into_iter()
                    .map(|u| {
                        (
                            u,
                            format!(
                                "knoot: `{path}` has been {verb} by {user}. You had read or \
                                 claimed it — anything you planned around it is now wrong. \
                                 Do not recreate it without asking: knoot msg {user} \"why did \
                                 `{path}` go?\""
                            ),
                        )
                    })
                    .collect()
            }
            // An ungated write: somebody wrote a file a peer holds, and
            // nothing stopped it. Interpreter writes cannot be predicted —
            // `python3 -c "open(p,'w')"` is a program, not a command line —
            // so this path exists and always will. What it must never be is
            // *silent*: the holder is told at once, because the sentence that
            // lets a team turn off worktrees is not "nothing can go wrong",
            // it is "nothing goes wrong without you hearing about it".
            Event::UngatedWrite { path, user, holder, holder_user, session, .. } => {
                let mut out = Vec::new();
                // Compared by *session*, not by user: one person's two agents
                // colliding is the ordinary case, and both halves of the news
                // land in that one person's mailbox, which is right — they are
                // the one who has to sort it out.
                if session != holder {
                    out.push((
                        holder_user.to_lowercase(),
                        format!(
                            "knoot: `{path}` was written by {user} while you held it. Nothing \
                             stopped them — it was not an Edit or a shell command we could \
                             gate. Re-read the file before you rely on your copy of it, and \
                             check nothing of yours was lost: knoot msg {user} \"did you mean \
                             to write `{path}`?\""
                        ),
                    ));
                    // And the writer, who probably has no idea. An agent that
                    // is told immediately can put it back; one that finds out
                    // at merge cannot.
                    out.push((
                        user.to_lowercase(),
                        format!(
                            "knoot: you wrote `{path}`, which {holder_user} is holding. That \
                             write was not gated, so you may have overwritten their work. \
                             Check the file and tell them: knoot msg {holder_user} \"I wrote \
                             `{path}`\""
                        ),
                    ));
                }
                out
            }
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
/// Build the ClaimAcquired this daemon would record for a session and path.
fn local_claim_event(
    rc: &Arc<RepoConn>,
    session: &str,
    path: &str,
    lease_until: Option<Ts>,
) -> Event {
    let (intent, user, branch, lease) = {
        let v = rc.view.lock().unwrap();
        // A hub gets a short lease here too, or the Bash gate — which never
        // asks the relay — would hand out ten-minute holds on the one kind of
        // file that must not be held for ten minutes.
        let lease = lease_until.unwrap_or_else(|| now_ms() + v.lease_for(path));
        match v.sessions.get(session) {
            Some(s) => (s.intent.clone(), s.user.clone(), s.branch.clone(), lease),
            None => (String::new(), whoami(), String::new(), lease),
        }
    };
    Event::ClaimAcquired {
        session: session.to_string(),
        user,
        path: path.to_string(),
        lease_until: lease,
        intent,
        branch,
        ts: now_ms(),
    }
}

/// Record a claim in the local mirror **only**.
///
/// For a claim the relay has already granted, it has already sequenced the
/// event too: appending here as well would write it to the durable log twice.
/// That is not cosmetic — the log is the audit surface, the dashboard reads it,
/// and a claim that appears twice invites the reader to wonder what happened
/// in between.
fn record_claim_locally(rc: &Arc<RepoConn>, session: &str, path: &str, lease_until: Option<Ts>) {
    let ev = local_claim_event(rc, session, path, lease_until);
    rc.view.lock().unwrap().apply(&ev);
}

/// Record a claim locally *and* tell the relay about it.
///
/// For paths the Bash gate takes, which never went through arbitration: the
/// relay has not heard of them, so somebody has to say so.
fn claim_locally(rc: &Arc<RepoConn>, session: &str, path: &str) {
    let ev = local_claim_event(rc, session, path, None);
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

fn deny_bash(path: &str, c: &Claim, raw: &str, hub: bool, queued: usize, notes: Vec<String>) -> DResp {
    let (base, notes) = match deny(path, c, hub, queued, notes) {
        DResp::Decision { reason: Some(r), notes, .. } => (r, notes),
        other => return other,
    };
    DResp::Decision {
        allow: false,
        reason: Some(format!(
            "{base} This Bash command would write `{raw}`. Editing it by shell does not bypass the \
             claim."
        )),
        notes,
    }
}

fn deny(path: &str, c: &Claim, hub: bool, queued: usize, notes: Vec<String>) -> DResp {
    // Rounded *up*: "expires in ~1m" for a two-minute hub lease read as a
    // contradiction of the "~2m" in the queue line right after it, and for a
    // ten-minute lease it was a minute pessimistic all day.
    let mins = c.lease_until.saturating_sub(now_ms()).div_ceil(60_000);
    let intent = if c.intent.is_empty() { "unknown".to_string() } else { format!("\"{}\"", c.intent) };
    // A hub is the case where waiting is the wrong instinct: the file is
    // shared, the lease is short, and there is a line. Saying how long the
    // line is turns "wait" into a decision the agent can actually make.
    let queue = if hub {
        format!(
            " This is a shared hub file: everyone needs it, so it is leased short and queued \
             rather than owned. {} Do the rest of your task first and come back to it.",
            if queued == 0 {
                "Nobody else is waiting — you are next.".to_string()
            } else {
                format!("{queued} session(s) are ahead of you in the queue.")
            }
        )
    } else {
        String::new()
    };
    DResp::Decision {
        allow: false,
        reason: Some(format!(
            "knoot: `{path}` is currently claimed by {} (session {}…) — intent: {}. Lease expires in ~{}m.{queue} \
             Do not edit this file now: work on something else, or wait — you will be told automatically when it is released. \
             To coordinate directly, run: knoot msg {} \"your question\". `knoot who` lists all active sessions.",
            c.user,
            &c.session[..c.session.len().min(8)],
            intent,
            mins.max(1),
            c.user,
        )),
        notes,
    }
}

/// Every path this session read that somebody else has written since.
///
/// This is the semantic half of a conflict: the file being written may be
/// nobody's, and the write still wrong, because the agent reasoned from
/// content that has moved. Advisory only — a deny here would fire on every
/// harmless read of a busy file, and STORM reports the same trade.
///
/// Reporting also *acknowledges*: the recorded read is advanced to the peer's
/// write, so the same write is not announced again on every subsequent edit in
/// the turn. A newer write says something new and is reported again.
fn stale_read_notes(rc: &Arc<RepoConn>, session: &str, target: &str) -> Vec<String> {
    let mine: Vec<(String, Ts)> = {
        let reads = rc.reads.lock().unwrap();
        match reads.get(session) {
            Some(m) => m.iter().map(|(p, ts)| (p.clone(), *ts)).collect(),
            None => return Vec::new(),
        }
    };
    let mut stale: Vec<(String, String, Ts, Ts)> = {
        let v = rc.view.lock().unwrap();
        mine.iter()
            .filter_map(|(path, read_ts)| {
                let (writer, write_ts) = v.last_write.get(path)?;
                if writer == session || write_ts <= read_ts {
                    return None;
                }
                let who = v
                    .sessions
                    .get(writer)
                    .map(|s| s.user.clone())
                    .or_else(|| {
                        v.recent_writes
                            .iter()
                            .rev()
                            .find(|w| &w.path == path && &w.session == writer)
                            .map(|w| w.user.clone())
                    })
                    .unwrap_or_else(|| "someone".into());
                Some((path.clone(), who, *read_ts, *write_ts))
            })
            .collect()
    };
    // Newest first: if the cap bites, the agent should lose the oldest news.
    stale.sort_by_key(|(_, _, _, write_ts)| std::cmp::Reverse(*write_ts));
    stale.truncate(MAX_NOTES);

    let user = user_of(rc, session);
    let now = now_ms();
    let mut out = Vec::new();
    for (path, who, read_ts, write_ts) in stale {
        let _ = rc.tx.send(ClientMsg::Append {
            event: Event::StaleRead {
                session: session.to_string(),
                user: user.clone(),
                path: path.clone(),
                peer_user: who.clone(),
                read_ts,
                write_ts,
                ts: now,
            },
        });
        let same = path == target;
        out.push(format!(
            "knoot: you read `{path}` {} ago; {who} wrote it {} ago. {}",
            ago(now.saturating_sub(read_ts)),
            ago(now.saturating_sub(write_ts)),
            if same {
                "Re-read it before editing — your version is behind."
            } else {
                "Your plan for this write may rest on it. Re-read it before you rely on what it said."
            }
        ));
        rc.reads
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .insert(path, write_ts);
    }
    out
}

/// A `Write` of a whole file onto a path that already exists, created by
/// somebody else and recently.
///
/// 15.1% of real agent conflicts are add/add: two agents independently
/// creating the same new file. A claim on an existing path sees none of them,
/// and by the time the second agent writes, the first agent's file is there —
/// so the check is "does it exist, and did a peer just make it".
fn create_collision_note(
    rc: &Arc<RepoConn>,
    repo_root: &str,
    session: &str,
    path: &str,
) -> Option<String> {
    if !std::path::Path::new(repo_root).join(path).exists() {
        return None; // a genuinely new file: nothing to collide with
    }
    let now = now_ms();
    let (peer_user, peer_intent, when) = {
        let v = rc.view.lock().unwrap();
        let (writer, ts) = v.last_write.get(path)?;
        if writer == session || now.saturating_sub(*ts) > CREATE_WINDOW_MS {
            // Either ours, or old enough that "it already exists" is a fact
            // about the repository rather than about a peer.
            return None;
        }
        // The creator has usually *finished* — that is why nothing holds the
        // path any more — so their session record may be gone. The write
        // event carries the author, and outliving the session is the whole
        // reason it does.
        let user = v
            .sessions
            .get(writer)
            .map(|s| s.user.clone())
            .or_else(|| {
                v.recent_writes
                    .iter()
                    .rev()
                    .find(|w| w.path == path && &w.session == writer && !w.user.is_empty())
                    .map(|w| w.user.clone())
            })
            .unwrap_or_else(|| "someone".into());
        let intent = v.sessions.get(writer).map(|s| s.intent.clone()).unwrap_or_default();
        (user, intent, *ts)
    };
    let _ = rc.tx.send(ClientMsg::Append {
        event: Event::CreateCollision {
            session: session.to_string(),
            user: user_of(rc, session),
            path: path.to_string(),
            peer_user: peer_user.clone(),
            ts: now,
        },
    });
    let why = if peer_intent.trim().is_empty() {
        String::new()
    } else {
        format!(" Their task: \"{}\".", truncate(&peer_intent, 120))
    };
    Some(format!(
        "knoot: `{path}` already exists — {peer_user} created it {} ago.{why} Writing the whole \
         file will overwrite their work. Read it first and edit instead, or agree who owns it: \
         knoot msg {peer_user} \"…\".",
        ago(now.saturating_sub(when))
    ))
}

/// The note that goes with an allowed claim on a hub file.
fn hub_note(rc: &Arc<RepoConn>, path: &str, lease_until: Option<Ts>) -> String {
    let mins = lease_until
        .map(|t| (t.saturating_sub(now_ms()) / 60_000).max(1))
        .unwrap_or(2);
    let behind = rc.view.lock().unwrap().queue_len(path, "");
    let queue = if behind > 0 {
        format!(" {behind} session(s) are waiting on it.")
    } else {
        String::new()
    };
    format!(
        "knoot: `{path}` is a hub — several sessions depend on it, so your lease is ~{mins}m and \
         renews when you write, not while you think.{queue} Make the smallest change that works \
         and move on."
    )
}

/// Live sessions whose declared intent looks like this one's.
///
/// grite measured duplicate work at 78% of the waste, and it is duplicate
/// *tasks*, which no file claim can see. knoot already collects an intent
/// sentence every turn, so this is the cheap version of a task tracker — for
/// the case where the whole point is that nobody set one up. Advisory, and
/// deliberately crude: the output is a sentence, not a decision.
fn duplicate_intent_notes(
    rc: &Arc<RepoConn>,
    session: &str,
    user: &str,
    text: &str,
) -> Vec<String> {
    let peers: Vec<(String, String)> = {
        let v = rc.view.lock().unwrap();
        // What the whole room is saying says nothing about who is doing what.
        // Without this, four agents handed the same preamble by their harness
        // all "overlap" with each other and the notice fires on every turn.
        let live: Vec<&str> = std::iter::once(text)
            .chain(v.sessions.values().map(|s| s.intent.as_str()))
            .filter(|t| !t.trim().is_empty())
            .collect();
        let boilerplate = crate::proto::boilerplate_words(live);
        v.sessions
            .values()
            .filter(|s| s.session != session && !s.intent.trim().is_empty())
            .filter(|s| crate::proto::intents_overlap_beyond(text, &s.intent, &boilerplate))
            .map(|s| (s.user.clone(), s.intent.clone()))
            .collect()
    };
    let now = now_ms();
    peers
        .into_iter()
        .map(|(peer_user, peer_text)| {
            let _ = rc.tx.send(ClientMsg::Append {
                event: Event::DuplicateIntent {
                    session: session.to_string(),
                    user: user.to_string(),
                    text: truncate(text, 160),
                    peer_user: peer_user.clone(),
                    peer_text: truncate(&peer_text, 160),
                    ts: now,
                },
            });
            format!(
                "knoot: {peer_user} is already on something very like this — \"{}\". Two agents \
                 doing one task is the most expensive kind of collision, and nothing will block \
                 it. Check with them before you start: knoot msg {peer_user} \"are you taking \
                 this?\"",
                truncate(&peer_text, 160)
            )
        })
        .collect()
}

/// Of these peer writes, the paths this session had actually read.
fn depended_on(rc: &Arc<RepoConn>, session: &str, writes: &[PeerWrite]) -> Vec<String> {
    let reads = rc.reads.lock().unwrap();
    let Some(mine) = reads.get(session) else { return Vec::new() };
    let mut out: Vec<String> = writes
        .iter()
        .filter(|w| mine.contains_key(&w.path))
        .map(|w| w.path.clone())
        .collect();
    out.dedup();
    out
}

/// A duration a person would say out loud. The briefs are read by a model
/// with no clock, so "40s ago" carries information that a timestamp does not.
fn ago(ms: u64) -> String {
    let s = ms / 1000;
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// An absolute path as this repo names it.
///
/// The strip has to survive symlinks. `repo_root` is resolved (see
/// `ensure_repo`), while an absolute path from a hook payload is whatever the
/// client had — `/tmp/x` where the root is `/private/tmp/x`. So when the plain
/// strip fails, resolve the path first: the deepest ancestor that exists gets
/// canonicalized and the rest is re-joined, because the file being written may
/// not exist yet, which is exactly the creation case.
fn rel_path(repo_root: &str, path: &str) -> String {
    let root = repo_root.trim_end_matches('/');
    if let Some(rel) = path.strip_prefix(&format!("{root}/")) {
        return rel.to_string();
    }
    if !path.starts_with('/') {
        return path.to_string();
    }
    let resolved = resolve_lexically(std::path::Path::new(path));
    let resolved = resolved.to_string_lossy();
    resolved.strip_prefix(&format!("{root}/")).unwrap_or(path).to_string()
}

/// A path with its existing prefix canonicalized and the rest appended.
fn resolve_lexically(path: &std::path::Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        if !cur.pop() {
            return path.to_path_buf();
        }
    }
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

/// Get or create the relay connection for a repo (keyed by repo root).
///
/// The key is the *resolved* path. A hook payload carries the cwd Claude Code
/// was started in and the CLI resolves its own, so on any machine where the
/// repo sits under a symlink — `/tmp` and `/var` on macOS, most obviously —
/// the same repo arrived under two names and got two connections, two claim
/// mirrors and two memory caches. They agreed on anything the relay pushed
/// and disagreed the moment one of them dropped something locally.
async fn ensure_repo(d: &Arc<Daemon>, repo_root: &str) -> Option<Arc<RepoConn>> {
    let resolved = std::fs::canonicalize(repo_root).ok()?;
    let repo_root = &resolved.to_string_lossy().to_string()[..];
    if let Some(rc) = d.repos.lock().unwrap().get(repo_root) {
        return Some(rc.clone());
    }
    let cfg = RepoConfig::load(std::path::Path::new(repo_root))?;
    let (tx, rx) = mpsc::unbounded_channel::<ClientMsg>();
    // Declared hubs come from the repo, which only a client can read. Seeded
    // here so every hub check — local gate, Bash gate, brief — agrees, and
    // sent to the relay on the first claim so its arbitration agrees too.
    let view = View {
        declared_hubs: cfg.hubs.iter().map(|h| h.trim().trim_matches('/').to_string()).collect(),
        ..Default::default()
    };
    let rc = Arc::new(RepoConn {
        tx,
        view: Arc::new(Mutex::new(view)),
        reads: Arc::new(Mutex::new(HashMap::new())),
        provider: Arc::new(Mutex::new(Arc::new(crate::memory::Plaintext))),
        mls: Arc::new(Mutex::new(None)),
        daemon: Arc::downgrade(d),
        repo_root: repo_root.to_string(),
        mem: Arc::new(Mutex::new(crate::memory::Cache::default())),
        me: Arc::new(Mutex::new(None)),
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
                let hello = ClientMsg::Hello {
                    repo: cfg.repo.clone(),
                    daemon: whoami(),
                    areas: cfg.areas.clone(),
                };
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
                                ServerMsg::Welcome { claims, sessions, me, provider, .. } => {
                                    {
                                        let mut v = rc.view.lock().unwrap();
                                        v.claims = claims;
                                        v.sessions = sessions.into_iter().map(|s| (s.session.clone(), s)).collect();
                                    }
                                    // Sealing is the deployment's choice, so the
                                    // relay's answer is what selects it. A
                                    // hosted relay whose client could not do
                                    // MLS is better off with no memory than
                                    // with memory the room cannot read.
                                    if provider.as_deref() == Some(crate::proto::PROVIDER_MLS) {
                                        if let Some(m) = &me {
                                            setup_mls(&rc, m);
                                        }
                                    }
                                    *rc.me.lock().unwrap() = me;
                                    *rc.ready.lock().unwrap() = true;
                                    mls_reconcile(&rc);
                                    // Resume from the high-water mark rather
                                    // than re-fetching the room's history on
                                    // every reconnect.
                                    let since = rc.mem.lock().unwrap().seq;
                                    let _ = rc.tx.send(ClientMsg::MemSync { since });
                                }
                                // ---- the Delivery Service, from this side.
                                ServerMsg::MlsLog { room, msgs, started } => {
                                    mls_apply(&rc, &room, msgs, started);
                                }
                                ServerMsg::MlsWake { room } => {
                                    let since = rc
                                        .mls()
                                        .map(|s| {
                                            s.seen.lock().unwrap().get(&room).copied().unwrap_or(0)
                                        })
                                        .unwrap_or(0);
                                    let _ = rc.tx.send(ClientMsg::MlsSync { room, since });
                                }
                                ServerMsg::MlsRejected { room, .. } => {
                                    // Somebody else's commit for that epoch
                                    // landed first. Drop ours and come back
                                    // from where the room actually is.
                                    let since = match rc.mls() {
                                        Some(state) => {
                                            let mut d = state.device.lock().unwrap();
                                            if d.epoch(&room) == Some(0)
                                                && d.members(&room).len() == 1
                                            {
                                                // Our genesis lost: the room
                                                // was started elsewhere, so
                                                // the group we built is not
                                                // the room's.
                                                d.forget_room(&room);
                                            } else {
                                                let _ = d.discard_own(&room);
                                            }
                                            state.seen.lock().unwrap().get(&room).copied().unwrap_or(0)
                                        }
                                        None => 0,
                                    };
                                    let _ = rc.tx.send(ClientMsg::MlsSync { room, since });
                                }
                                ServerMsg::MlsKeyPackage { device, key_package } => {
                                    mls_add_device(&rc, &device, key_package);
                                }
                                ServerMsg::MlsRoster { room, devices } => {
                                    mls_roster(&rc, &room, devices);
                                }
                                ServerMsg::MemForgotten { ids } => {
                                    rc.mem.lock().unwrap().forget(&ids);
                                }
                                ServerMsg::MemShards { shards, more } => {
                                    let mut last = 0;
                                    {
                                        let provider = rc.provider.lock().unwrap().clone();
                                        let mut cache = rc.mem.lock().unwrap();
                                        for sh in shards {
                                            last = last.max(sh.seq);
                                            // The scope comes off the shard,
                                            // because under `Mls` it selects
                                            // the group and the exporter
                                            // context. `Plaintext` ignores it,
                                            // and the seal binds it either way.
                                            let scope = scope_of(&sh.scope);
                                            cache.apply(provider.as_ref(), &scope, sh);
                                        }
                                    }
                                    if more && last > 0 {
                                        let _ = rc.tx.send(ClientMsg::MemSync { since: last });
                                    }
                                }
                                // A refusal the relay made — over budget, or a
                                // scope this key does not hold. Told, never
                                // enforced: nothing here may stop an agent.
                                ServerMsg::MemRejected { reason, .. } => {
                                    let user = whoami();
                                    rc.mail
                                        .lock()
                                        .unwrap()
                                        .entry(user)
                                        .or_default()
                                        .push_back(format!("knoot: a fact was not published — {reason}"));
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
