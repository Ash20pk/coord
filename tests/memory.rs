//! Phase 4 of the multiplayer design: shared memory, `Plaintext` provider.
//!
//! The claim being tested is not "facts can be stored". It is that a fact one
//! person's agent wrote reaches another person's agent, on the turn it matters,
//! without either of them running a command for it — and that the four failure
//! modes MemClaw published in production do not happen here.
//!
//! So these drive the real binary through the real hook surfaces against a
//! real relay with real device keys, and assert on what an agent would be told.
//!
//! Every test shares one relay and one team: `KNOOT_TOKEN` is process-wide and
//! the daemon is in-process, so a token per test would race. They are kept
//! apart by repo id instead, which is the same isolation the relay gives any
//! two repos.

mod common;
use common::*;

use futures_util::{SinkExt, StreamExt};
use knoot::proto::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMsg;

const BIN: &str = env!("CARGO_BIN_EXE_knoot");

/// The relay, team and daemon every test in this file shares.
struct Ctx {
    url: String,
    sock: PathBuf,
    /// A second person on the same team, for the tests about what crosses
    /// between two people.
    peer_key: String,
    peer_member: String,
    peer_email: String,
    team: String,
}

/// The shared relay, team and daemon, on a runtime of their own.
///
/// Not a `OnceCell` on a test's runtime: each `#[tokio::test]` builds and then
/// *drops* its own runtime, which would take the relay and the daemon down
/// with whichever test happened to start them. So they get a thread and a
/// runtime that outlive every test in the binary.
fn ctx() -> &'static Ctx {
    static CTX: std::sync::OnceLock<Ctx> = std::sync::OnceLock::new();
    CTX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Ctx>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                // Through the relay's own API, as the console does. This used
                // to open the relay's SQLite file to invent a colleague,
                // because until `POST /api/members` existed there was no
                // other way on a relay with no Supabase behind it.
                let admin = Admin::register("Acme", "ash@example.com").await;
                let (peer_member, peer_key) =
                    admin.add_member("priya@example.com", "priya laptop").await;

                // Read at dial time, so it must be set before any repo is
                // enrolled. Process-wide, which is the other reason every test
                // here shares one team and is kept apart by repo id instead.
                std::env::set_var("KNOOT_TOKEN", &admin.key);

                tx.send(Ctx {
                    url: admin.url.clone(),
                    sock: start_daemon().await,
                    peer_key,
                    peer_member,
                    peer_email: "priya@example.com".into(),
                    team: admin.team_id.clone(),
                })
                .unwrap();
            });
            loop {
                std::thread::park();
            }
        });
        rx.recv().expect("the shared relay and daemon must come up")
    })
}

/// A repo of this test's own, on the shared relay.
async fn repo(tag: &str) -> (&'static Ctx, PathBuf, String) {
    let c = ctx();
    let root = tmp(tag);
    let id = format!("mem-{tag}-{}", uuid::Uuid::new_v4().simple());
    init_repo(&root, &c.url, &id);
    std::fs::create_dir_all(root.join("src/http")).unwrap();
    std::fs::write(root.join("src/http/client.rs"), "fn get() {}\n").unwrap();
    (c, root, id)
}

// ------------------------------------------------------------- hook driving

fn hook_as(sock: &Path, payload: Value, user: &str) -> Option<Value> {
    let mut child = Command::new(BIN)
        .arg("hook")
        .env("KNOOT_SOCK", sock)
        .env("USER", "testuser")
        .env("KNOOT_USER", user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hook must always exit 0");
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| serde_json::from_str(&s).expect("hook output must be valid JSON"))
}

/// What an agent would actually see, whichever surface carried it.
fn told(out: &Option<Value>) -> String {
    let Some(v) = out else { return String::new() };
    let hs = &v["hookSpecificOutput"];
    [hs["additionalContext"].as_str(), hs["permissionDecisionReason"].as_str(), v["reason"].as_str()]
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn joins(sock: &Path, root: &PathBuf, session: &str, user: &str) {
    hook_as(
        sock,
        json!({ "hook_event_name": "SessionStart", "session_id": session, "cwd": root.to_string_lossy() }),
        user,
    );
}

fn reads(sock: &Path, root: &PathBuf, session: &str, user: &str, rel: &str) {
    hook_as(
        sock,
        json!({
            "hook_event_name": "PostToolUse", "session_id": session,
            "cwd": root.to_string_lossy(), "tool_name": "Read",
            "tool_input": { "file_path": format!("{}/{}", root.to_string_lossy(), rel) }
        }),
        user,
    );
}

fn prompt(sock: &Path, root: &PathBuf, session: &str, user: &str, text: &str) -> String {
    told(&hook_as(
        sock,
        json!({
            "hook_event_name": "UserPromptSubmit", "session_id": session,
            "cwd": root.to_string_lossy(), "prompt": text
        }),
        user,
    ))
}

fn pre_write(sock: &Path, root: &PathBuf, session: &str, user: &str, rel: &str) -> String {
    told(&hook_as(
        sock,
        json!({
            "hook_event_name": "PreToolUse", "session_id": session,
            "cwd": root.to_string_lossy(), "tool_name": "Edit",
            "tool_input": { "file_path": format!("{}/{}", root.to_string_lossy(), rel) }
        }),
        user,
    ))
}

/// `knoot plan`, as an agent would run it: on purpose, for a named session.
fn plan(sock: &Path, root: &PathBuf, session: &str, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .arg("plan")
        .args(args)
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .env("CLAUDE_SESSION_ID", session)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn cache_entry(sock: &Path, root: &PathBuf, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .arg("cache")
        .args(args)
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn ends(sock: &Path, root: &PathBuf, session: &str, user: &str) {
    hook_as(
        sock,
        json!({ "hook_event_name": "SessionEnd", "session_id": session, "cwd": root.to_string_lossy() }),
        user,
    );
}

/// `knoot remember`, as a person or an agent would run it.
fn remember(sock: &Path, root: &PathBuf, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .arg("remember")
        .args(args)
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn recall(sock: &Path, root: &PathBuf) -> String {
    let out = Command::new(BIN)
        .arg("recall")
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

// ------------------------------------------------------------- the peer

/// A second person's machine, speaking the wire directly. Used where a test
/// needs a shard that this daemon's key did not write — which is most of the
/// point of shared memory.
struct Peer {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Peer {
    async fn connect(c: &Ctx, repo: &str) -> Self {
        let mut req = c.url.as_str().into_client_request().unwrap();
        req.headers_mut()
            .insert("Authorization", format!("Bearer {}", c.peer_key).parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let hello =
            ClientMsg::Hello { repo: repo.into(), daemon: "peer".into(), areas: Vec::new() };
        ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
        let mut p = Self { ws };
        loop {
            if let ServerMsg::Welcome { .. } = p.recv().await {
                break;
            }
        }
        p
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
                other => panic!("relay closed: {other:?}"),
            }
        }
    }

    async fn send(&mut self, m: &ClientMsg) {
        self.ws.send(WsMsg::Text(serde_json::to_string(m).unwrap())).await.unwrap();
    }

    /// Publish a fact the way a daemon does: sealed, bound to this member.
    async fn publish(&mut self, c: &Ctx, repo: &str, area: &str, name: &str, text: &str, paths: &[&str]) -> String {
        use knoot::memory::{self, KeyProvider};
        let scope = memory::Scope {
            team: c.team.clone(),
            repo: repo.to_string(),
            area: area.to_string(),
        };
        let fact = memory::Fact {
            name: name.into(),
            text: text.into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            hashes: Default::default(),
            decisions: Vec::new(),
        };
        let plain = serde_json::to_vec(&fact).unwrap();
        let id = format!("sh_{}", uuid::Uuid::new_v4().simple());
        let p = memory::Plaintext;
        let (epoch, secret) = p.epoch(&scope);
        let key = scope.key();
        let aad = memory::aad(&id, &key, "facts", &c.peer_member, &c.peer_email, epoch);
        let sealed = p.seal(&scope, &aad, &plain);
        let shard = memory::Shard {
            id: id.clone(),
            scope: key,
            kind: "facts".into(),
            author: c.peer_member.clone(),
            author_email: c.peer_email.clone(),
            device: "d".into(),
            name_blind: memory::name_blind(&secret, name),
            supersedes: None,
            epoch,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
            bytes: plain.len() as i64,
            seq: 0,
            created_ts: now_ms(),
            expires_ts: None,
        };
        self.send(&ClientMsg::MemPublish { shard }).await;
        id
    }
}

/// The relay pushes and the daemon syncs on its own schedule; this is the beat
/// that lets both happen before a test looks.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
}

// ------------------------------------------------------------------ tests

/// The whole claim of this phase in one test: a fact a teammate wrote reaches
/// an agent that never ran a command for it, on the turn it is in that code.
#[tokio::test]
async fn a_teammates_fact_reaches_an_agent_that_never_asked_for_it() {
    let (c, root, id) = repo("reaches").await;
    let mut peer = Peer::connect(c, &id).await;
    peer.publish(
        c,
        &id,
        "/",
        "http-retry",
        "the http client retries three times with jittered backoff; do not add a fourth",
        &["src/http/client.rs"],
    )
    .await;
    settle().await;

    joins(&c.sock, &root, "s1", "ash");
    reads(&c.sock, &root, "s1", "ash", "src/http/client.rs");
    let brief = prompt(&c.sock, &root, "s1", "ash", "make the http client more resilient");

    assert!(brief.contains("what this team already knows"), "no memory section:\n{brief}");
    assert!(brief.contains("jittered backoff"), "{brief}");
    assert!(brief.contains("priya@example.com"), "provenance travels with it:\n{brief}");
}

/// The highest-attention surface in the product. An agent about to edit a file
/// a convention names is told, in the one place it is certainly reading.
#[tokio::test]
async fn a_fact_about_the_exact_path_rides_the_pretooluse_brief() {
    let (c, root, id) = repo("brief").await;
    let mut peer = Peer::connect(c, &id).await;
    peer.publish(c, &id, "/", "http-client", "every request goes through one shared client", &["src/http/client.rs"])
        .await;
    settle().await;

    joins(&c.sock, &root, "s1", "ash");
    let out = pre_write(&c.sock, &root, "s1", "ash", "src/http/client.rs");
    assert!(out.contains("one shared client"), "the brief must carry it:\n{out}");

    // And not for a file it says nothing about, or the brief stops being read.
    let elsewhere = pre_write(&c.sock, &root, "s1", "ash", "src/other.rs");
    assert!(!elsewhere.contains("one shared client"), "{elsewhere}");
}

/// Nobody else has this signal. A fact that names the code it is about can be
/// told it is *wrong*, not merely old, and can name who made it so.
#[tokio::test]
async fn a_fact_is_flagged_stale_when_the_code_it_names_is_written() {
    let (c, root, id) = repo("stale").await;
    let mut peer = Peer::connect(c, &id).await;
    peer.publish(c, &id, "/", "retry", "the client retries three times", &["src/http/client.rs"])
        .await;
    settle().await;

    // Priya edits the file the fact is about.
    joins(&c.sock, &root, "s2", "priya");
    std::fs::write(root.join("src/http/client.rs"), "fn get() { retry(5) }\n").unwrap();
    pre_write(&c.sock, &root, "s2", "priya", "src/http/client.rs");
    hook_as(
        &c.sock,
        json!({
            "hook_event_name": "PostToolUse", "session_id": "s2",
            "cwd": root.to_string_lossy(), "tool_name": "Edit",
            "tool_input": { "file_path": format!("{}/src/http/client.rs", root.to_string_lossy()) }
        }),
        "priya",
    );
    settle().await;

    joins(&c.sock, &root, "s1", "ash");
    reads(&c.sock, &root, "s1", "ash", "src/http/client.rs");
    let brief = prompt(&c.sock, &root, "s1", "ash", "check the retry logic");
    assert!(brief.contains("possibly stale"), "the fact's ground moved:\n{brief}");
    assert!(brief.contains("src/http/client.rs"), "{brief}");
}

/// MemClaw's second production bug, end to end: a near-duplicate filter that
/// rejected a contradicting write. A contradiction *is* a near-duplicate, and
/// the second statement must win without the first being lost.
#[tokio::test]
async fn a_second_fact_under_one_name_supersedes_rather_than_standing_beside_it() {
    let (c, root, _) = repo("supersede").await;
    remember(&c.sock, &root, &["--name", "retry", "--path", "src/http/client.rs", "the client retries three times"]);
    settle().await;
    remember(&c.sock, &root, &["--name", "retry", "--path", "src/http/client.rs", "the client retries five times, not three"]);
    settle().await;

    let out = recall(&c.sock, &root);
    assert!(out.contains("five times"), "the current statement:\n{out}");
    assert!(!out.contains("three times"), "and only the current one:\n{out}");
}

/// A refusal, not a warning — a warning here is a secret in a shared store
/// with a note attached — and on the log, because an agent trying to publish a
/// `.env` is exactly what an admin wants to know.
#[tokio::test]
async fn a_dotenv_is_refused_and_the_refusal_is_logged() {
    let (c, root, id) = repo("dotenv").await;
    std::fs::write(root.join(".env"), "DATABASE_URL=postgres://u:p@h/db\n").unwrap();

    // Watching the log before the attempt: the relay pushes events live and
    // does not replay them, so a listener that arrives afterwards sees
    // nothing — which is a property of the log, not of the refusal.
    let mut peer = Peer::connect(c, &id).await;
    joins(&c.sock, &root, "s1", "ash");

    let out = remember(&c.sock, &root, &["--name", "env", "--from", ".env", "the db url"]);
    assert!(out.contains("not published"), "{out}");
    assert!(out.contains("credentials"), "and says why:\n{out}");
    settle().await;

    // Nothing was stored...
    assert!(!recall(&c.sock, &root).contains("postgres"), "no secret in the store");

    // ...and the attempt is on the log, where an admin can count it.
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(400);
    while let Ok(m) = tokio::time::timeout_at(deadline, peer.recv()).await {
        if let ServerMsg::Event { event, .. } = m {
            seen.push(serde_json::to_string(&event).unwrap());
        }
    }
    let refusal = seen
        .iter()
        .find(|e| e.contains("memory_refused"))
        .unwrap_or_else(|| panic!("no refusal on the log; saw {seen:?}"));
    assert!(refusal.contains("credentials"), "the reason travels: {refusal}");
    assert!(!refusal.contains("postgres"), "and the content never does: {refusal}");
}

/// Failing open is the property everything else here rests on. A relay that is
/// not there means no memory — never a write that is refused for want of it.
#[tokio::test]
async fn a_relay_that_is_not_there_injects_no_memory_and_denies_no_write() {
    let c = ctx();
    let root = tmp("offline");
    // A relay that will never answer. Nothing about memory may turn this into
    // an agent that cannot work.
    init_repo(&root, "ws://127.0.0.1:1/ws", "mem-offline");
    std::fs::create_dir_all(root.join("src")).unwrap();

    joins(&c.sock, &root, "s1", "ash");
    let out = hook_as(
        &c.sock,
        json!({
            "hook_event_name": "PreToolUse", "session_id": "s1",
            "cwd": root.to_string_lossy(), "tool_name": "Edit",
            "tool_input": { "file_path": format!("{}/src/x.rs", root.to_string_lossy()) }
        }),
        "ash",
    );
    assert_ne!(
        out.as_ref().map(|v| v["hookSpecificOutput"]["permissionDecision"].clone()),
        Some(json!("deny")),
        "a write must never be denied for want of memory"
    );
    let brief = prompt(&c.sock, &root, "s1", "ash", "do the thing");
    assert!(!brief.contains("what this team already knows"), "{brief}");
    assert!(remember(&c.sock, &root, &["--name", "x", "hello"]).contains("not published"));
}

/// Provenance comes from the key. `KNOOT_USER` is a display string a client
/// picks, and a fact attributed by one is a fact nobody can stand behind.
#[tokio::test]
async fn authorship_on_a_shard_comes_from_the_key_not_the_client() {
    let (c, root, _) = repo("authorship").await;
    // The client says it is somebody else entirely.
    let out = Command::new(BIN)
        .args(["remember", "--name", "who", "the tests run on sqlite"])
        .current_dir(&root)
        .env("KNOOT_SOCK", &c.sock)
        .env("KNOOT_USER", "someone-else")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("remembered"));
    settle().await;

    let listed = recall(&c.sock, &root);
    assert!(listed.contains("ash@example.com"), "the key's person, not the client's:\n{listed}");
    assert!(!listed.contains("someone-else"), "{listed}");
}

/// Scoped retrieval, over the wire. A key that does not hold an area is not
/// handed its memory, whether it syncs, listens, or asks for one shard by id.
#[tokio::test]
async fn shards_never_reach_a_key_that_does_not_hold_the_area() {
    let (c, _root, id) = repo("scoped").await;
    let mut peer = Peer::connect(c, &id).await;

    // A scope this peer's rooms do not grant. `general` holds `(*, /)`, which
    // covers the root area of every repo and nothing narrower is granted here.
    let refused = peer.publish(c, &id, "src/secret-area", "leak", "should not land", &[]).await;
    let ok = peer.publish(c, &id, "/", "fine", "should land", &[]).await;
    settle().await;

    peer.send(&ClientMsg::MemSync { since: 0 }).await;
    let mut ids = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while let Ok(m) = tokio::time::timeout_at(deadline, peer.recv()).await {
        if let ServerMsg::MemShards { shards, .. } = m {
            ids.extend(shards.into_iter().map(|s| s.id));
        }
    }
    assert!(ids.contains(&ok), "the granted area syncs");
    assert!(!ids.contains(&refused), "the ungranted one was never stored");

    // And fetch-by-id — MemClaw's leak — is not a way around it.
    peer.send(&ClientMsg::MemFetch { id: refused.clone() }).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while let Ok(m) = tokio::time::timeout_at(deadline, peer.recv()).await {
        if let ServerMsg::MemShards { shards, .. } = m {
            assert!(shards.iter().all(|s| s.id != refused), "fetch-by-id enforces scope too");
        }
    }
}


// ------------------------------------------- phase 6: the other two kinds

/// Phase 6's exit criterion: a peer's declared plan reaches a same-area
/// session on its next turn, within one hook boundary.
///
/// This is the thing the intent sentence cannot do. An intent is one line
/// scraped from a prompt; a plan says what the approach is and what has
/// already been settled, which is what stops a peer designing against it.
#[tokio::test]
async fn a_peers_declared_plan_reaches_the_next_turn_of_a_session_in_the_area() {
    let (c, root, _) = repo("plan").await;
    joins(&c.sock, &root, "s-ash", "ash");
    joins(&c.sock, &root, "s-priya", "priya");

    let out = plan(
        &c.sock,
        &root,
        "s-priya",
        &[
            "--path",
            "src/http/client.rs",
            "--decided",
            "one shared client, not one per module",
            "replacing the http client with a pooled one",
        ],
    );
    assert!(out.contains("noted"), "{out}");
    settle().await;

    // Ash's very next turn. No command, no asking.
    let brief = prompt(&c.sock, &root, "s-ash", "ash", "add retries to the http layer");
    assert!(brief.contains("what your peers are doing"), "no context section:\n{brief}");
    assert!(brief.contains("pooled one"), "the plan itself:\n{brief}");
    assert!(brief.contains("decided: one shared client"), "and what is settled:\n{brief}");

    // And priya is not told her own plan back — that budget is for peers'.
    let hers = prompt(&c.sock, &root, "s-priya", "priya", "carry on");
    assert!(!hers.contains("pooled one"), "an agent does not need its own plan:\n{hers}");
}

/// A session's context is memory in the sense that a room is a memory: it
/// exists while people are in it. A finished plan presented as a live one is
/// worse than no plan.
#[tokio::test]
async fn session_context_does_not_outlive_the_session() {
    let (c, root, _) = repo("ctxlife").await;
    joins(&c.sock, &root, "s-ash", "ash");
    joins(&c.sock, &root, "s-priya", "priya");
    plan(&c.sock, &root, "s-priya", &["--path", "src/http/client.rs", "rewriting the client"]);
    settle().await;
    assert!(prompt(&c.sock, &root, "s-ash", "ash", "go").contains("rewriting the client"));

    ends(&c.sock, &root, "s-priya", "priya");
    settle().await;

    let brief = prompt(&c.sock, &root, "s-ash", "ash", "go again");
    assert!(!brief.contains("rewriting the client"), "the plan outlived its session:\n{brief}");
    // And it is gone from the store, not merely hidden.
    let listed = recall(&c.sock, &root);
    assert!(!listed.contains("rewriting the client"), "still in the store:\n{listed}");
}

/// A session that replans supersedes itself. Two plans standing from one
/// session is a peer being asked which one is current.
#[tokio::test]
async fn a_session_that_replans_replaces_its_own_context() {
    let (c, root, _) = repo("replan").await;
    joins(&c.sock, &root, "s-ash", "ash");
    joins(&c.sock, &root, "s-priya", "priya");
    plan(&c.sock, &root, "s-priya", &["first approach: patch it in place"]);
    settle().await;
    plan(&c.sock, &root, "s-priya", &["second approach: extract a module"]);
    settle().await;

    let brief = prompt(&c.sock, &root, "s-ash", "ash", "go");
    assert!(brief.contains("extract a module"), "the current plan:\n{brief}");
    assert!(!brief.contains("patch it in place"), "and only the current one:\n{brief}");
}

/// Derived knowledge is *dropped* when its ground moves, not flagged. That is
/// the whole difference from a fact: a fact was written on purpose and who
/// changed it is what its reader needs; a cache entry past its files is
/// simply wrong, and it was cheap to work out.
#[tokio::test]
async fn a_cache_entry_is_dropped_once_the_files_it_came_from_change() {
    let (c, root, _) = repo("cachelife").await;
    joins(&c.sock, &root, "s-ash", "ash");
    let out = cache_entry(
        &c.sock,
        &root,
        &["--name", "how tests run", "--path", "src/http/client.rs", "cargo test --lib http"],
    );
    assert!(out.contains("cached"), "{out}");
    settle().await;

    let brief = prompt(&c.sock, &root, "s-ash", "ash", "run the tests");
    assert!(brief.contains("already worked out here"), "no cache section:\n{brief}");
    assert!(brief.contains("cargo test --lib http"), "{brief}");

    // A peer changes the file it was derived from.
    joins(&c.sock, &root, "s-priya", "priya");
    std::fs::write(root.join("src/http/client.rs"), "fn get() { /* rewritten */ }\n").unwrap();
    pre_write(&c.sock, &root, "s-priya", "priya", "src/http/client.rs");
    hook_as(
        &c.sock,
        json!({
            "hook_event_name": "PostToolUse", "session_id": "s-priya",
            "cwd": root.to_string_lossy(), "tool_name": "Edit",
            "tool_input": { "file_path": format!("{}/src/http/client.rs", root.to_string_lossy()) }
        }),
        "priya",
    );
    settle().await;

    let after = prompt(&c.sock, &root, "s-ash", "ash", "run the tests again");
    assert!(
        !after.contains("cargo test --lib http"),
        "a cache entry past its files must not be offered:\n{after}"
    );
}

/// The kinds must not supersede each other. A cache entry and a fact under
/// one name are two different things, and one replacing the other would
/// silently delete a statement somebody wrote on purpose.
#[tokio::test]
async fn a_cache_entry_and_a_fact_of_the_same_name_are_two_things() {
    let (c, root, _) = repo("kinds").await;
    joins(&c.sock, &root, "s1", "ash");
    remember(&c.sock, &root, &["--name", "retry", "the retry budget is three attempts"]);
    settle().await;
    cache_entry(&c.sock, &root, &["--name", "retry", "retry lives in src/http/retry.rs"]);
    settle().await;

    let listed = recall(&c.sock, &root);
    assert!(listed.contains("three attempts"), "the fact survives:\n{listed}");
    assert!(listed.contains("src/http/retry.rs"), "and the cache entry is there too:\n{listed}");
    assert!(listed.contains("[facts]") && listed.contains("[repo_cache]"), "{listed}");
}

/// Every kind goes through the same refusals. A cache entry is where a secret
/// is *most* likely to end up: it is derived from files, and "how the deploy
/// authenticates" is a plausible thing for an agent to work out and store.
#[tokio::test]
async fn a_secret_is_refused_in_a_plan_and_in_a_cache_entry_too() {
    let (c, root, _) = repo("kindrefuse").await;
    joins(&c.sock, &root, "s1", "ash");
    assert!(cache_entry(
        &c.sock,
        &root,
        &["--name", "deploy", "authenticate with ghp_abcdefghijklmnopqrst"]
    )
    .contains("not published"));
    assert!(plan(&c.sock, &root, "s1", &["using the key knt_2f6c9a1b3d4e5f60718293a4"])
        .contains("not published"));
    // Including in a decision, which is a second field and an easy one to
    // forget to check.
    assert!(plan(
        &c.sock,
        &root,
        "s1",
        &["--decided", "the token is ghp_zyxwvutsrqponmlkjihg", "refactoring auth"]
    )
    .contains("not published"));
}
