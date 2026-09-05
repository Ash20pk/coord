//! Phase 5 of the multiplayer design: the `Mls` key provider.
//!
//! One question decides whether the hosted tier is honest: with the relay
//! configured for MLS, what does a copy of its database give an attacker who
//! has it? The answer has to be "the shape of the traffic and nothing else" —
//! no shard plaintext, no epoch secret, no credential that still works.
//!
//! So the headline test dumps every byte the relay persisted, after a real
//! group has formed over a real socket and a real fact has been published
//! through it, and looks for things that must not be there.
//!
//! Everything here shares one relay, because `KNOOT_KEY_PROVIDER` and
//! `KNOOT_TOKEN` are process-wide and the daemon is in-process. Tests are kept
//! apart by repo id, as in `tests/memory.rs`.

mod common;
use common::*;

use futures_util::{SinkExt, StreamExt};
use knoot::proto::*;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMsg;

const BIN: &str = env!("CARGO_BIN_EXE_knoot");

struct Ctx {
    url: String,
    db: PathBuf,
    sock: PathBuf,
    /// The owner's key, which the in-process daemon holds.
    owner_key: String,
    owner_device: String,
    /// A second person's key and device, driven over the raw wire.
    peer_key: String,
    peer_device: String,
    peer_member: String,
    peer_email: String,
    room: String,
    /// The owner's key against the API, for a test that needs to shape the
    /// team rather than only read it.
    admin: Admin,
}

/// The relay, team and daemon, on a runtime that outlives every test — a
/// `#[tokio::test]` drops its own runtime, which would take them down.
fn ctx() -> &'static Ctx {
    static CTX: std::sync::OnceLock<Ctx> = std::sync::OnceLock::new();
    CTX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Ctx>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                // The deployment's choice, and read at relay start.
                std::env::set_var("KNOOT_KEY_PROVIDER", "mls");
                std::env::set_var("KNOOT_MLS_DIR", tmp("mlsdir"));

                // Through the relay's own API, as the console does. The
                // database path is kept only because the headline test has to
                // read what the relay actually persisted — that is the
                // assertion, not the setup.
                let db = tmp("mls").join("relay.db");
                let admin =
                    Admin::register_on(db.clone(), "Acme", "ash@example.com").await;
                let owner_member = admin.my_member().await;
                let owner_device = admin.device_of(&owner_member).await;
                let (peer_member, peer_key) =
                    admin.add_member("priya@example.com", "priya laptop").await;
                let peer_device = admin.device_of(&peer_member).await;
                let room = admin.room_named("general").await;

                std::env::set_var("KNOOT_TOKEN", &admin.key);

                tx.send(Ctx {
                    url: admin.url.clone(),
                    db,
                    sock: start_daemon().await,
                    owner_key: admin.key.clone(),
                    owner_device,
                    peer_key,
                    peer_device,
                    peer_member,
                    peer_email: "priya@example.com".into(),
                    room,
                    admin,
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

async fn repo(tag: &str) -> (&'static Ctx, PathBuf, String) {
    let c = ctx();
    let root = tmp(tag);
    let id = format!("mls-{tag}-{}", uuid::Uuid::new_v4().simple());
    init_repo(&root, &c.url, &id);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/billing.rs"), "fn total() {}\n").unwrap();
    (c, root, id)
}

// ------------------------------------------------------------- driving

fn hook_as(sock: &Path, payload: serde_json::Value, user: &str) -> Option<serde_json::Value> {
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
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| serde_json::from_str(&s).unwrap())
}

fn joins(sock: &Path, root: &Path, session: &str, user: &str) {
    hook_as(
        sock,
        json!({ "hook_event_name": "SessionStart", "session_id": session, "cwd": root.to_string_lossy() }),
        user,
    );
}

fn remember(sock: &Path, root: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .arg("remember")
        .args(args)
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn recall(sock: &Path, root: &Path) -> String {
    let out = Command::new(BIN)
        .arg("recall")
        .current_dir(root)
        .env("KNOOT_SOCK", sock)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A beat for the relay and the daemon to exchange what they owe each other.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
}

/// Wait until this machine actually holds the room's key.
///
/// Not a sleep: the group has to form over the socket first, and under a
/// loaded test binary that takes as long as it takes. Publishing before the
/// key arrives is *refused* — which is the behaviour being relied on here, so
/// polling for it is testing it.
async fn key_ready(sock: &Path, root: &Path) {
    for _ in 0..80 {
        let out = Command::new(BIN)
            .arg("status")
            .current_dir(root)
            .env("KNOOT_SOCK", sock)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.contains("provider mls") && !text.contains("waiting for this room") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the room's MLS group never reached this machine");
}

/// Every byte the relay persisted, as one lump. What an attacker with a copy
/// of the database, or a backup of it, actually holds.
fn relay_dump(c: &Ctx) -> Vec<u8> {
    // The file itself, not a SELECT: a WAL, a freelist page or a column
    // somebody forgot to look at is exactly where a leak would hide.
    let mut out = std::fs::read(&c.db).unwrap_or_default();
    for suffix in ["-wal", "-shm"] {
        let p = c.db.with_file_name(format!(
            "{}{suffix}",
            c.db.file_name().unwrap().to_string_lossy()
        ));
        out.extend(std::fs::read(p).unwrap_or_default());
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// ------------------------------------------------------------------ tests

/// The exit criterion. A fact goes in through the hosted configuration, and
/// then the relay's whole database is searched for the three things it must
/// never hold.
#[tokio::test]
async fn a_relay_dump_under_mls_yields_no_plaintext_no_secret_and_no_working_credential() {
    let (c, root, _) = repo("dump").await;
    joins(&c.sock, &root, "s1", "ash");
    key_ready(&c.sock, &root).await;

    // Something distinctive enough that finding it in a dump is unambiguous.
    let secret_sentence = "zarquon-nine-invoices-round-half-up";
    let out = remember(
        &c.sock,
        &root,
        &["--name", "rounding", "--path", "src/billing.rs", secret_sentence],
    );
    assert!(out.contains("remembered"), "the fact must actually publish: {out}");
    settle().await;

    // It is readable by the machine that wrote it — otherwise this test could
    // pass by never having stored anything.
    assert!(recall(&c.sock, &root).contains(secret_sentence), "the room can read its own fact");

    let dump = relay_dump(c);
    assert!(!dump.is_empty(), "there must be a database to search");

    // 1. No plaintext.
    assert!(!contains(&dump, secret_sentence), "the fact's text is in the relay's database");
    assert!(!contains(&dump, "round-half-up"), "nor any distinctive fragment of it");
    // The fact's *handle* is blinded too: `name_blind` is an HMAC, and the
    // name itself is inside the sealed payload.
    assert!(!contains(&dump, "rounding"), "the fact's name is in the relay's database");

    // 2. No epoch secret. The relay stores key packages and opaque handshake
    //    blobs; the group secret is derived on the devices and sent nowhere.
    let db = rusqlite::Connection::open(&c.db).unwrap();
    let secret_cols: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memory_shards') \
             WHERE name IN ('secret','key','epoch_secret','plaintext')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(secret_cols, 0, "the schema has nowhere to put a secret, which is the point");
    // And the shard opens only under the group's key: the stored ciphertext is
    // not the plaintext with a tag on it, as it would be under `plaintext`.
    let (ct, bytes): (Vec<u8>, i64) = db
        .query_row("SELECT ciphertext, bytes FROM memory_shards LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("a shard was stored");
    assert!(ct.len() > bytes as usize, "sealed, so longer than its plaintext");
    assert!(!contains(&ct, secret_sentence));

    // 3. No working credential. Keys are stored as SHA-256 and nothing in the
    //    dump is presentable to the relay.
    assert!(!contains(&dump, &c.owner_key), "the owner's key is in the dump");
    assert!(!contains(&dump, &c.peer_key), "a member's key is in the dump");
    assert!(!contains(&dump, "knt_"), "nothing key-shaped is in the dump at all");
}

/// The interface is the claim: the relay's code does not know which provider
/// is behind it, and a client is told which one the deployment uses rather
/// than choosing. A client that chose could seal shards the room cannot open.
#[tokio::test]
async fn the_deployment_chooses_the_provider_and_says_so() {
    let (c, root, id) = repo("provider").await;
    joins(&c.sock, &root, "s1", "ash");
    key_ready(&c.sock, &root).await;

    // What the relay tells a fresh connection.
    let mut req = c.url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", c.owner_key).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = ClientMsg::Hello { repo: id.clone(), daemon: "probe".into(), areas: Vec::new() };
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
    loop {
        if let Some(Ok(WsMsg::Text(t))) = ws.next().await {
            if let Ok(ServerMsg::Welcome { provider, me, .. }) = serde_json::from_str(&t) {
                assert_eq!(provider.as_deref(), Some("mls"), "the relay names its provider");
                let me = me.expect("a verified key names a person");
                assert_eq!(me.device_id, c.owner_device, "and the machine it was minted for");
                assert!(!me.rooms.is_empty(), "and the rooms whose groups hold its keys");
                break;
            }
        }
    }

    // And the status line a human reads says the same.
    let out = Command::new(BIN)
        .arg("status")
        .current_dir(&root)
        .env("KNOOT_SOCK", &c.sock)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("provider mls"), "status must say which deployment this is:\n{text}");
}

/// The Delivery Service orders commits and nothing else. Two daemons
/// proposing from one epoch race; exactly one lands, and the loser is told.
#[tokio::test]
async fn the_delivery_service_takes_one_commit_per_epoch() {
    let c = ctx();
    let db = rusqlite::Connection::open(&c.db).unwrap();
    let room = format!("rm-race-{}", uuid::Uuid::new_v4().simple());

    let env = |epoch: u64, blob: &str| knoot::mls::Envelope {
        seq: 0,
        epoch,
        kind: "commit".into(),
        blob: blob.as_bytes().to_vec(),
        for_device: None,
    };
    assert!(knoot::mls::append(&db, &room, &env(1, "first")).is_ok());
    assert!(
        knoot::mls::append(&db, &room, &env(1, "second")).is_err(),
        "a second commit for one epoch is what the DS exists to refuse"
    );
    assert!(knoot::mls::append(&db, &room, &env(2, "next")).is_ok(), "the next epoch is fine");

    // And a welcome is addressed, so one device's invitation is not fanned
    // out to the whole room.
    let welcome = knoot::mls::Envelope {
        seq: 0,
        epoch: 2,
        kind: "welcome".into(),
        blob: b"for-d2".to_vec(),
        for_device: Some("d2".into()),
    };
    knoot::mls::append(&db, &room, &welcome).unwrap();
    let d1 = knoot::mls::log_since(&db, &room, "d1", 0);
    let d2 = knoot::mls::log_since(&db, &room, "d2", 0);
    assert_eq!(d1.iter().filter(|e| e.kind == "welcome").count(), 0, "not d1's welcome");
    assert_eq!(d2.iter().filter(|e| e.kind == "welcome").count(), 1, "and it is d2's");
    assert_eq!(
        d1.iter().filter(|e| e.kind == "commit").count(),
        2,
        "every commit reaches every device"
    );
}

/// Membership is enforced even though content is not readable. A relay that
/// forwards blobs it cannot read is still not a free-for-all: a key outside a
/// room may not read its handshake log or write to it.
#[tokio::test]
async fn a_key_outside_a_room_cannot_touch_its_handshake_log() {
    let (c, _root, id) = repo("outsider").await;

    // A room priya is not in. Created the way an admin creates one.
    let closed = c.admin.create_room("closed").await;
    c.admin.add_area(&closed, "*", "/").await;
    let rooms = c.admin.get("/api/team").await["rooms"].clone();
    let members_of_closed = rooms
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["id"].as_str() == Some(&closed)))
        .and_then(|r| r["members"].as_array().cloned())
        .unwrap_or_default();
    assert!(
        !members_of_closed.iter().any(|m| m["id"].as_str() == Some(&c.peer_member)),
        "priya must not be in it"
    );

    let mut req = c.url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", c.peer_key).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = ClientMsg::Hello { repo: id, daemon: "priya".into(), areas: Vec::new() };
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
    ws.send(WsMsg::Text(
        serde_json::to_string(&ClientMsg::MlsSync { room: closed.clone(), since: 0 }).unwrap(),
    ))
    .await
    .unwrap();
    ws.send(WsMsg::Text(
        serde_json::to_string(&ClientMsg::MlsRoster { room: closed.clone() }).unwrap(),
    ))
    .await
    .unwrap();
    ws.send(WsMsg::Text(
        serde_json::to_string(&ClientMsg::MlsCommit {
            room: closed.clone(),
            epoch: 1,
            commit: knoot::memory::hex(b"i should not be here"),
            welcome: None,
            for_device: None,
        })
        .unwrap(),
    ))
    .await
    .unwrap();

    let mut refused = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(700);
    while let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout_at(deadline, ws.next()).await {
        match serde_json::from_str::<ServerMsg>(&t) {
            Ok(ServerMsg::MlsLog { msgs, started, .. }) => {
                assert!(msgs.is_empty() && !started, "an outsider learns nothing about the room");
            }
            Ok(ServerMsg::MlsRoster { devices, .. }) => {
                assert!(devices.is_empty(), "not even who is in it");
            }
            Ok(ServerMsg::MlsRejected { .. }) => refused = true,
            _ => {}
        }
    }
    assert!(refused, "and may not write to its log");
    let db = rusqlite::Connection::open(&c.db).unwrap();
    assert!(
        !knoot::mls::has_group(&db, &closed),
        "so the room's log is still empty afterwards"
    );
}

/// A key package is verified where it is used, not where it is carried. The
/// Delivery Service is not trusted with content and is not trusted with this.
#[tokio::test]
async fn a_forged_key_package_does_not_get_a_device_into_a_room() {
    let dir = tmp("forge");
    let mut ash = knoot::mls::Device::open(&dir, "d-ash").unwrap();
    let priya = knoot::mls::Device::open(&dir, "d-priya").unwrap();
    ash.create_room("rm1").unwrap();

    let mut kp = priya.key_package().unwrap();
    // Flip a byte in the middle: the signature no longer covers the contents.
    let mid = kp.len() / 2;
    kp[mid] ^= 0xff;
    assert!(
        ash.add_device("rm1", &kp).is_err(),
        "a key package that does not verify must not become a leaf"
    );
    assert!(ash.add_device("rm1", b"not a key package at all").is_err());
    assert_eq!(ash.members("rm1").len(), 1, "and the group is untouched");
}

/// Rewrapping rotates the key and cannot launder authorship. This is why a
/// rewrap is its own message rather than a republish: a republish would make
/// whoever removed a member the author of everybody else's facts.
#[tokio::test]
async fn a_rewrap_moves_the_key_forward_and_leaves_provenance_alone() {
    let (c, root, _) = repo("rewrap").await;
    joins(&c.sock, &root, "s1", "ash");
    key_ready(&c.sock, &root).await;
    remember(&c.sock, &root, &["--name", "convention", "commit messages are imperative"]);
    settle().await;

    let db = rusqlite::Connection::open(&c.db).unwrap();
    let (id, scope, epoch, author, email): (String, String, i64, String, String) = db
        .query_row(
            "SELECT id, scope, epoch, author, author_email FROM memory_shards \
             ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("a shard was stored");

    let held = std::slice::from_ref(&scope);
    knoot::memory::rewrap(&db, &id, held, epoch as u64 + 1, b"newnonce1234", b"newct")
        .expect("a member holding the scope may rewrap");
    let (e2, a2, m2): (i64, String, String) = db
        .query_row(
            "SELECT epoch, author, author_email FROM memory_shards WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(e2, epoch + 1, "the key moved forward");
    assert_eq!((a2, m2), (author, email), "and the author did not move at all");

    // Not backwards, and not from outside the scope.
    assert!(knoot::memory::rewrap(&db, &id, held, 1, b"n", b"c").is_err());
    assert!(
        knoot::memory::rewrap(&db, &id, &["someone/elses/scope".into()], 99, b"n", b"c").is_err(),
        "the scope check is on this path too"
    );
}

/// Failing open, at the top. A hosted deployment whose group has not formed
/// must still let an agent work.
#[tokio::test]
async fn a_room_whose_group_has_not_formed_denies_no_write() {
    let c = ctx();
    let root = tmp("unformed");
    // A relay that will never answer, so no group can ever form.
    init_repo(&root, "ws://127.0.0.1:1/ws", "mls-unformed");
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
        "no key, no memory — and never a blocked write"
    );
    assert!(remember(&c.sock, &root, &["--name", "x", "hello"]).contains("not published"));
    let _ = (&c.peer_device, &c.peer_email, &c.room);
}

/// The whole chain, with two machines. Everything else here proves something
/// is *absent*; this proves the mechanism works — a second laptop is added to
/// the group by the first, derives the same key from the protocol, and opens a
/// fact it never had the key for.
///
/// Priya's daemon is played by hand: one in-process daemon can only hold one
/// device, because `KNOOT_TOKEN` is process-wide. Everything she does is the
/// same wire her daemon would use.
#[tokio::test]
async fn a_second_laptop_is_added_to_the_group_and_can_read_the_rooms_facts() {
    let (c, root, id) = repo("twodevices").await;
    let secret_sentence = "vogon-seven-retries-are-idempotent";

    // Ash's daemon comes up and starts the room's group.
    joins(&c.sock, &root, "s1", "ash");
    key_ready(&c.sock, &root).await;

    // Priya's machine. Her MLS credential is her relay device id, which is
    // what the roster names and what a Remove would name.
    // Held behind the same lock the provider wants, for the whole test: the
    // device and the provider are one thing on a real machine.
    let priya = std::sync::Arc::new(std::sync::Mutex::new(
        knoot::mls::Device::open(&tmp("priyadev"), &c.peer_device).unwrap(),
    ));
    let kp = priya.lock().unwrap().key_package().unwrap();

    let mut ws = {
        let mut req = c.url.as_str().into_client_request().unwrap();
        req.headers_mut()
            .insert("Authorization", format!("Bearer {}", c.peer_key).parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws
    };
    macro_rules! send {
        ($m:expr) => {
            ws.send(WsMsg::Text(serde_json::to_string(&$m).unwrap())).await.unwrap()
        };
    }
    send!(ClientMsg::Hello { repo: id.clone(), daemon: "priya".into(), areas: Vec::new() });

    // Uploading the key package is what makes her addable, and it wakes the
    // room's other members to look at the roster again.
    send!(ClientMsg::MlsUpload { key_package: knoot::memory::hex(&kp) });
    settle().await;
    settle().await;

    // Ash publishes, under whatever epoch the room is now in.
    let out = remember(
        &c.sock,
        &root,
        &["--name", "retries", "--path", "src/billing.rs", secret_sentence],
    );
    assert!(out.contains("remembered"), "{out}");
    settle().await;

    // Priya pulls her room's handshake log and the room's shards, and keeps
    // pulling until she can open one.
    //
    // A loop, not a single round, because that is what a daemon does: the
    // room's epoch moves whenever anybody's membership changes, and a device
    // stays readable by continuing to process the log. A test that synced
    // once would be asserting that the room never moves again.
    let mut shards: Vec<knoot::memory::Shard> = Vec::new();
    let mut welcomed = false;
    let mut opened: Option<String> = None;

    for _ in 0..40 {
        send!(ClientMsg::MlsSync { room: c.room.clone(), since: 0 });
        send!(ClientMsg::MemSync { since: 0 });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(400);
        while let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout_at(deadline, ws.next()).await
        {
            match serde_json::from_str::<ServerMsg>(&t) {
                Ok(ServerMsg::MlsLog { room, msgs, .. }) => {
                    for env in msgs {
                        match env.kind.as_str() {
                            // Genesis: it exists only to decide who started
                            // the room, and there is nothing to apply.
                            "commit" if env.blob.is_empty() => {}
                            "commit" => {
                                let _ = priya.lock().unwrap().process(&room, &env.blob);
                            }
                            "welcome" => {
                                if priya.lock().unwrap().join(&room, &env.blob).is_ok() {
                                    welcomed = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(ServerMsg::MemShards { shards: got, .. }) => shards.extend(got),
                _ => {}
            }
        }

        // The key came from the protocol. It was never sent, and the relay
        // never held it.
        if let Some(shard) = shards.iter().rev().find(|s| s.author_email == "ash@example.com") {
            let scope = {
                let mut p = shard.scope.splitn(3, '/');
                knoot::memory::Scope {
                    team: p.next().unwrap().into(),
                    repo: p.next().unwrap().into(),
                    area: p.next().unwrap_or("/").into(),
                }
            };
            let provider = knoot::mls::Mls::new(priya.clone());
            provider.bind(&scope.key(), &c.room);
            // Nothing derives the key first on purpose. A device that has only
            // ever *read* has never published, so if opening a shard needed a
            // prior `epoch()` call then a read-only member could see the whole
            // room and open none of it.
            let mut cache = knoot::memory::Cache::default();
            if cache.apply(&provider, &scope, shard.clone()) {
                opened = cache.heads().first().map(|h| h.fact.text.clone());
            }
        }
        if opened.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    assert!(welcomed, "ash's daemon must have added priya's laptop to the group");
    assert!(priya.lock().unwrap().in_room(&c.room), "and she is in it");
    assert_eq!(
        opened.as_deref(),
        Some(secret_sentence),
        "priya's laptop must be able to open a fact ash sealed, and read the actual sentence"
    );
}

/// The property phase 5 shipped without a test: after a removal, the room's
/// facts are re-sealed under the new epoch, and the departed laptop cannot
/// follow.
///
/// On a fixture of its own — its own relay, team and *two* subprocess daemons.
/// Everything else in this file shares one relay and one `general` room, which
/// is fine for asserting that something is absent and hopeless here: the
/// room's epoch moves whenever any other test's membership changes, and this
/// test is about what one specific epoch change does.
#[tokio::test]
async fn a_removal_rotates_the_rooms_key_and_rewraps_what_it_holds() {
    let secret = "krikkit-eleven-invoices-are-immutable";
    let db = tmp("rewrap-relay").join("relay.db");
    let admin = Admin::register_on(db.clone(), "Rotate", "ash@example.com").await;
    let repo_id = format!("rot-{}", uuid::Uuid::new_v4().simple());

    let (_, ash_key) = admin.add_member("ash-machine@example.com", "ash laptop").await;
    let (sam_member, sam_key) = admin.add_member("sam@example.com", "sam laptop").await;

    let ash = Daemon2::start(BIN, &admin.url, &repo_id, &ash_key, "rot-ash").await;
    let sam = Daemon2::start(BIN, &admin.url, &repo_id, &sam_key, "rot-sam").await;
    for d in [&ash, &sam] {
        d.hook(
            BIN,
            json!({
                "hook_event_name": "SessionStart", "session_id": "s1",
                "cwd": d.root.to_string_lossy()
            }),
        );
    }

    // Ash's machine writes a fact once its room's key has reached it.
    let mut wrote = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if ash.run(BIN, &["remember", "--name", "immutability", secret]).contains("remembered") {
            wrote = true;
            break;
        }
    }
    assert!(wrote, "ash's machine never got a key to seal with");

    // Sam's machine is added to the group and can read it. Until this holds,
    // the rest of the test is asserting nothing.
    let mut epoch_with_sam = 0i64;
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if sam.run(BIN, &["recall"]).contains(secret) {
            epoch_with_sam = shard_epoch(&db);
            break;
        }
    }
    assert!(
        epoch_with_sam > 0,
        "sam's laptop never joined the room, so nothing here is a test of rotation\n\
         sam: {}\nash: {}",
        sam.stderr(),
        ash.stderr(),
    );

    // Now he is taken out of the room. This is the moment the key must move.
    let general = admin.room_named("general").await;
    admin.remove_from_room(&general, &sam_member).await;

    let mut rotated = 0i64;
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let now = shard_epoch(&db);
        if now > epoch_with_sam {
            rotated = now;
            break;
        }
    }
    assert!(
        rotated > epoch_with_sam,
        "the shard was never re-sealed, so it is still readable under the epoch \
         sam's laptop holds (was {epoch_with_sam}, now {})",
        shard_epoch(&db)
    );

    // Ash can still read it. A rotation that lost the room's memory would be a
    // worse failure than not rotating at all.
    let mut still = false;
    for _ in 0..50 {
        if ash.run(BIN, &["recall"]).contains(secret) {
            still = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(still, "the room must keep what it knows across a rotation");

    // And the provenance is untouched: a rewrap is not a republish.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let author: String = conn
        .query_row("SELECT author_email FROM memory_shards LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        author, "ash-machine@example.com",
        "whoever removed him did not become the author"
    );
}

/// The epoch the room's shard is sealed under, straight out of the relay.
fn shard_epoch(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .ok()
        .and_then(|db| {
            db.query_row("SELECT MAX(epoch) FROM memory_shards", [], |r| r.get::<_, Option<i64>>(0))
                .ok()
                .flatten()
        })
        .unwrap_or(0)
}
