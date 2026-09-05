//! Phase 2 of the multiplayer design: awareness, through the same hook
//! surfaces the product already owns.
//!
//! Every test here drives the real `knoot` binary with canned hook payloads
//! and asserts on what an agent would actually be told, because that is the
//! whole claim being made. A signal the daemon computes and nobody delivers is
//! worth nothing, and the exit criterion for this phase is about a transcript.
//!
//! One property per test, named for the thing that would be wrong.

mod common;
use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_knoot");

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
    assert!(out.status.success(), "hook must always exit 0, got {:?}", out.status);
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| serde_json::from_str(&s).expect("hook output must be valid JSON"))
}

/// The context or reason text an agent would see, whichever surface carried it.
fn told(out: &Option<Value>) -> String {
    let Some(v) = out else { return String::new() };
    let hs = &v["hookSpecificOutput"];
    [
        hs["additionalContext"].as_str(),
        hs["permissionDecisionReason"].as_str(),
        v["reason"].as_str(),
    ]
    .iter()
    .flatten()
    .cloned()
    .collect::<Vec<_>>()
    .join("\n")
}

fn denied(out: &Option<Value>) -> bool {
    out.as_ref()
        .map(|v| v["hookSpecificOutput"]["permissionDecision"] == "deny")
        .unwrap_or(false)
}

fn tool(root: &PathBuf, session: &str, event: &str, tool: &str, rel: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": session,
        "cwd": root.to_string_lossy(),
        "tool_name": tool,
        "tool_input": { "file_path": format!("{}/{}", root.to_string_lossy(), rel) }
    })
}

fn bash(root: &PathBuf, session: &str, event: &str, command: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": session,
        "cwd": root.to_string_lossy(),
        "tool_name": "Bash",
        "tool_input": { "command": command }
    })
}

fn prompt(root: &PathBuf, session: &str, text: &str) -> Value {
    json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "cwd": root.to_string_lossy(),
        "prompt": text
    })
}

async fn scenario(tag: &str) -> (PathBuf, PathBuf, String) {
    scenario_hubs(tag, &[]).await
}

async fn scenario_hubs(tag: &str, hubs: &[&str]) -> (PathBuf, PathBuf, String) {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp(tag);
    init_repo_with_hubs(&root, &url, &format!("aw-{tag}"), hubs);
    std::fs::create_dir_all(root.join("src")).unwrap();
    (sock, root, url)
}

/// A session announcing itself, which is what carries its user. Real sessions
/// always fire `SessionStart`; a session the daemon has only ever seen write
/// is attributed to the OS user, which is the documented fallback and not what
/// these tests are about.
fn joins(sock: &Path, root: &PathBuf, session: &str, user: &str) {
    hook_as(
        sock,
        json!({
            "hook_event_name": "SessionStart",
            "session_id": session,
            "cwd": root.to_string_lossy()
        }),
        user,
    );
}

/// One session writes a file, having claimed it and then let it go.
fn peer_writes(sock: &Path, root: &PathBuf, session: &str, user: &str, rel: &str) {
    joins(sock, root, session, user);
    std::fs::write(root.join(rel), "peer content\n").unwrap();
    hook_as(sock, tool(root, session, "PreToolUse", "Edit", rel), user);
    hook_as(sock, tool(root, session, "PostToolUse", "Edit", rel), user);
}

fn release(sock: &Path, root: &PathBuf, session: &str, user: &str) {
    hook_as(
        sock,
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": session,
            "cwd": root.to_string_lossy()
        }),
        user,
    );
}

// ------------------------------------------------------- 2.1 read snapshots

/// STORM's largest single result: a write is stale when what the agent *read*
/// has changed, even where the file being written is untouched. Advisory, and
/// it must stay advisory — the target is nobody's, and denying here would be a
/// false-positive machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_read_is_reported_before_the_write_and_does_not_deny_it() {
    let (sock, root, _url) = scenario("stale").await;
    std::fs::write(root.join("src/types.ts"), "original\n").unwrap();
    joins(&sock, &root, "sessA", "ash");

    // ash reads the shared type file and reasons about it.
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/types.ts"), "ash");
    // priya changes it underneath.
    peer_writes(&sock, &root, "sessB", "priya", "src/types.ts");
    release(&sock, &root, "sessB", "priya");

    // ash now writes a *different* file. Nothing is claimed, nothing collides
    // — and the write is still built on something that has moved.
    let out = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Edit", "src/auth.ts"), "ash");
    let said = told(&out);
    assert!(!denied(&out), "a stale read must never block a write: {said}");
    assert!(said.contains("src/types.ts"), "must name the file that moved: {said:?}");
    assert!(said.contains("priya"), "must name who moved it: {said:?}");
    assert!(
        out.as_ref().unwrap()["hookSpecificOutput"]["permissionDecision"].is_null(),
        "an advisory must not decide the permission — that would auto-approve the edit"
    );
}

/// The same fact, when the write *is* denied, rides on the denial: the agent
/// is stopped and reading, which is the highest-attention surface there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_read_on_a_held_path_rides_on_the_denial() {
    let (sock, root, _url) = scenario("staledeny").await;
    std::fs::write(root.join("src/types.ts"), "original\n").unwrap();
    joins(&sock, &root, "sessA", "ash");

    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/types.ts"), "ash");
    peer_writes(&sock, &root, "sessB", "priya", "src/types.ts");
    // priya keeps the claim this time.

    let out = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Edit", "src/types.ts"), "ash");
    let said = told(&out);
    assert!(denied(&out), "priya holds it, so this is still a deny: {said:?}");
    assert!(said.contains("claimed by priya"), "the denial must still say who holds it: {said:?}");
    assert!(said.contains("you read"), "and that our copy is behind: {said:?}");
}

/// Reporting is also acknowledging. Without this, every subsequent write in
/// the turn repeats the same news until the agent stops reading any of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_peer_write_is_reported_once_not_on_every_later_write() {
    let (sock, root, _url) = scenario("staleonce").await;
    std::fs::write(root.join("src/types.ts"), "original\n").unwrap();
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/types.ts"), "ash");
    peer_writes(&sock, &root, "sessB", "priya", "src/types.ts");
    release(&sock, &root, "sessB", "priya");

    let first = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Edit", "src/a.ts"), "ash");
    assert!(told(&first).contains("src/types.ts"), "the first write hears about it");
    let second = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Edit", "src/b.ts"), "ash");
    assert!(
        !told(&second).contains("src/types.ts"),
        "and the second does not: {:?}",
        told(&second)
    );
}

/// A brief that lists ten peer writes buries the one the agent actually built
/// on. The ones it read come first, and say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_writes_to_files_this_session_read_are_ranked_first_in_the_brief() {
    let (sock, root, _url) = scenario("ranked").await;
    for f in ["src/unread.ts", "src/depended.ts"] {
        std::fs::write(root.join(f), "x\n").unwrap();
    }
    // A first turn, so the daemon has a "since" to measure the next one from.
    hook_as(&sock, prompt(&root, "sessA", "start on auth"), "ash");
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/depended.ts"), "ash");

    peer_writes(&sock, &root, "sessB", "priya", "src/unread.ts");
    peer_writes(&sock, &root, "sessB", "priya", "src/depended.ts");
    release(&sock, &root, "sessB", "priya");

    let said = told(&hook_as(&sock, prompt(&root, "sessA", "carry on with auth"), "ash"));
    let dep = said.find("src/depended.ts").expect("the read file must be listed");
    let un = said.find("src/unread.ts").expect("the other write must be listed too");
    assert!(dep < un, "the file this session read must come first:\n{said}");
    assert!(said.contains("you read this one"), "and be marked as such:\n{said}");
}

// -------------------------------------------- 2.2 creation and deletion

/// 15.1% of real agent conflicts are add/add: two agents independently
/// creating the same new file. A claim on an existing path sees none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_creating_one_new_file_are_told_about_each_other() {
    let (sock, root, _url) = scenario("addadd").await;

    // priya creates src/retry.ts and finishes her turn.
    joins(&sock, &root, "sessB", "priya");
    hook_as(&sock, tool(&root, "sessB", "PreToolUse", "Write", "src/retry.ts"), "priya");
    std::fs::write(root.join("src/retry.ts"), "priya's retry\n").unwrap();
    hook_as(&sock, tool(&root, "sessB", "PostToolUse", "Write", "src/retry.ts"), "priya");
    release(&sock, &root, "sessB", "priya");

    // ash, who has never read it, is about to write the same path whole.
    let out = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Write", "src/retry.ts"), "ash");
    let said = told(&out);
    assert!(!denied(&out), "nobody holds it any more, so this is not a block: {said:?}");
    assert!(said.contains("already exists"), "must say the file is there: {said:?}");
    assert!(said.contains("priya"), "and who made it: {said:?}");
    assert!(said.contains("overwrite"), "and what writing it whole would do: {said:?}");
}

/// An `Edit` is not the add/add case: the agent has the file open and is
/// changing part of it, and "this already exists" is not news.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editing_an_existing_file_is_not_reported_as_a_creation_collision() {
    let (sock, root, _url) = scenario("addedit").await;
    joins(&sock, &root, "sessB", "priya");
    hook_as(&sock, tool(&root, "sessB", "PreToolUse", "Write", "src/retry.ts"), "priya");
    std::fs::write(root.join("src/retry.ts"), "priya's retry\n").unwrap();
    hook_as(&sock, tool(&root, "sessB", "PostToolUse", "Write", "src/retry.ts"), "priya");
    release(&sock, &root, "sessB", "priya");

    let said = told(&hook_as(
        &sock,
        tool(&root, "sessA", "PreToolUse", "Edit", "src/retry.ts"),
        "ash",
    ));
    assert!(!said.contains("already exists"), "an edit is not a creation: {said:?}");
}

/// 26.8% of real agent conflicts are modify/delete. A claim cannot express
/// "the file you were standing on has gone", so it is broadcast instead — and
/// only to the sessions it is news for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_reaches_every_session_that_read_the_path() {
    let (sock, root, _url) = scenario("deleted").await;
    std::fs::write(root.join("src/legacy.ts"), "old\n").unwrap();

    // ash reads it and is mid-task.
    hook_as(&sock, prompt(&root, "sessA", "wire up the legacy adapter"), "ash");
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/legacy.ts"), "ash");
    // A session that never touched it, as a control.
    hook_as(&sock, prompt(&root, "sessC", "work on billing"), "sam");

    // priya deletes it by shell.
    joins(&sock, &root, "sessB", "priya");
    let cmd = format!("rm {}/src/legacy.ts", root.to_string_lossy());
    hook_as(&sock, bash(&root, "sessB", "PreToolUse", &cmd), "priya");
    std::fs::remove_file(root.join("src/legacy.ts")).unwrap();
    hook_as(&sock, bash(&root, "sessB", "PostToolUse", &cmd), "priya");
    // The note travels via the relay, like every other delivered event.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let ash = told(&hook_as(&sock, prompt(&root, "sessA", "continue"), "ash"));
    assert!(ash.contains("src/legacy.ts"), "the reader must be told: {ash:?}");
    assert!(ash.contains("deleted"), "and told what happened: {ash:?}");

    let sam = told(&hook_as(&sock, prompt(&root, "sessC", "continue"), "sam"));
    assert!(
        !sam.contains("src/legacy.ts"),
        "a session that never touched it must not be told: {sam:?}"
    );
}

/// A `rm` that removed nothing must announce nothing. Reporting a deletion
/// that did not happen is worse than missing one: it invites an agent to
/// recreate a file that is still there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_that_did_not_happen_is_not_announced() {
    let (sock, root, _url) = scenario("nodelete").await;
    std::fs::write(root.join("src/legacy.ts"), "old\n").unwrap();
    hook_as(&sock, prompt(&root, "sessA", "read the adapter"), "ash");
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/legacy.ts"), "ash");

    joins(&sock, &root, "sessB", "priya");
    let cmd = format!("rm {}/src/legacy.ts", root.to_string_lossy());
    hook_as(&sock, bash(&root, "sessB", "PreToolUse", &cmd), "priya");
    // The command "failed": the file is still there.
    hook_as(&sock, bash(&root, "sessB", "PostToolUse", &cmd), "priya");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let ash = told(&hook_as(&sock, prompt(&root, "sessA", "continue"), "ash"));
    assert!(!ash.contains("deleted"), "nothing was deleted: {ash:?}");
}

// ------------------------------------------------------------ 2.3 hubs

/// A widely-shared file held for a whole turn is every other agent's critical
/// path — the failure both STORM and Co-Coder name. A hub is leased short and
/// queued rather than owned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hub_lease_is_short_and_the_queue_is_reported() {
    let (sock, root, url) = scenario_hubs("hub", &["package.json"]).await;
    std::fs::write(root.join("package.json"), "{}\n").unwrap();

    joins(&sock, &root, "sessA", "ash");
    joins(&sock, &root, "sessB", "priya");
    hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Edit", "package.json"), "ash");
    let claims = relay_claims(&url, "aw-hub").await;
    let hub = claims
        .iter()
        .find(|c| c.path == "package.json")
        .expect("the hub must still be claimed like anything else");
    let left = hub.lease_until.saturating_sub(knoot::proto::now_ms());
    assert!(
        left <= knoot::proto::HUB_LEASE_MS && left + 30_000 > knoot::proto::HUB_LEASE_MS,
        "a declared hub must get the short lease, got {left}ms"
    );

    let out = hook_as(&sock, tool(&root, "sessB", "PreToolUse", "Edit", "package.json"), "priya");
    let said = told(&out);
    assert!(denied(&out), "it is still held, so this is still a deny: {said:?}");
    assert!(said.contains("hub"), "the denial must say it is a hub: {said:?}");
    assert!(said.contains("Lease expires in ~2m"), "and how short the lease is: {said:?}");
    assert!(
        said.contains("you are next"),
        "first in line must be told so, not told about zero people: {said:?}"
    );

    // A third session arrives behind priya, and is told how many are ahead.
    joins(&sock, &root, "sessC", "sam");
    let third =
        told(&hook_as(&sock, tool(&root, "sessC", "PreToolUse", "Edit", "package.json"), "sam"));
    assert!(third.contains("ahead of you"), "a real queue must be counted: {third:?}");
}

/// A hub nobody thought to declare is still a hub. Three sessions in one file
/// inside half an hour is not a coincidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_three_sessions_claim_becomes_a_hub_without_being_declared() {
    let (sock, root, url) = scenario("hubauto").await;
    std::fs::write(root.join("src/routes.ts"), "x\n").unwrap();

    // Three sessions take it in turn. Nothing is declared anywhere.
    for (i, (session, user)) in
        [("s1", "ash"), ("s2", "priya"), ("s3", "sam")].iter().enumerate()
    {
        joins(&sock, &root, session, user);
        hook_as(&sock, tool(&root, session, "PreToolUse", "Edit", "src/routes.ts"), user);
        hook_as(&sock, tool(&root, session, "PostToolUse", "Edit", "src/routes.ts"), user);
        release(&sock, &root, session, user);
        assert!(i < 3);
    }

    // The fourth claim is on a file the relay now knows is shared.
    hook_as(&sock, tool(&root, "s4", "PreToolUse", "Edit", "src/routes.ts"), "dev");
    let claims = relay_claims(&url, "aw-hubauto").await;
    let held = claims.iter().find(|c| c.path == "src/routes.ts").expect("claimed");
    let left = held.lease_until.saturating_sub(knoot::proto::now_ms());
    assert!(
        left <= knoot::proto::HUB_LEASE_MS,
        "three claimants in the window makes a hub, got a {left}ms lease"
    );
}

// -------------------------------------------------------- 2.4 task claims

/// Duplicate work was 78% of the waste grite measured, and it is duplicate
/// *tasks* — which no file claim can see. knoot already has an intent
/// sentence every turn, so comparing them is the cheap version of a task
/// tracker, for the case where the whole point is that nobody set one up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_declaring_one_task_are_told_about_each_other() {
    let (sock, root, _url) = scenario("duptask").await;
    hook_as(&sock, prompt(&root, "sessA", "add retry with backoff to the http client"), "ash");
    let said = told(&hook_as(
        &sock,
        prompt(&root, "sessB", "please add retry and backoff to our HTTP client"),
        "priya",
    ));
    assert!(said.contains("very like this"), "the second session must be warned: {said:?}");
    assert!(said.contains("ash"), "and told who to ask: {said:?}");
}

/// And two people doing genuinely different things hear nothing, or the
/// warning stops meaning anything within a day.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_on_different_tasks_are_left_alone() {
    let (sock, root, _url) = scenario("nodup").await;
    hook_as(&sock, prompt(&root, "sessA", "add retry with backoff to the http client"), "ash");
    let said = told(&hook_as(
        &sock,
        prompt(&root, "sessB", "fix the rounding in the invoice tax calculation"),
        "priya",
    ));
    assert!(!said.contains("very like this"), "unrelated work must not be flagged: {said:?}");
}

// ------------------------------------------------------- failing open

/// Every one of these signals is new machinery on the hot path, and none of
/// it may ever be the reason an agent cannot write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn none_of_the_new_awareness_can_block_a_write() {
    let (sock, root, _url) = scenario("failopen").await;
    std::fs::write(root.join("src/types.ts"), "x\n").unwrap();
    hook_as(&sock, prompt(&root, "sessA", "add retry to the http client"), "ash");
    hook_as(&sock, tool(&root, "sessA", "PostToolUse", "Read", "src/types.ts"), "ash");
    peer_writes(&sock, &root, "sessB", "priya", "src/types.ts");
    release(&sock, &root, "sessB", "priya");
    // A stale read, a duplicate intent and a creation collision at once.
    hook_as(&sock, prompt(&root, "sessB", "add retry to the http client too"), "priya");
    std::fs::write(root.join("src/retry.ts"), "priya's\n").unwrap();
    peer_writes(&sock, &root, "sessB", "priya", "src/retry.ts");
    release(&sock, &root, "sessB", "priya");

    let out = hook_as(&sock, tool(&root, "sessA", "PreToolUse", "Write", "src/retry.ts"), "ash");
    assert!(!denied(&out), "awareness advises; it never denies: {:?}", told(&out));
}

// ---------------------------------------------------- people, not only agents

/// Gap 4: a teammate in another editor did not exist to knoot, so nobody's
/// agent could be told to stay out of their way.
///
/// `knoot present` registers a person's touches through the same daemon
/// requests a hook uses, so the only thing that has to be true is that a
/// person is *described* as one — an agent can be asked to move and a person
/// cannot, and a brief that blurs the two gives useless advice.
// Multi-threaded, like every other test here: `hook_as` blocks the thread on a
// subprocess that needs the in-process daemon to answer, and a current-thread
// runtime cannot do both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_in_an_editor_is_named_as_one_in_an_agents_brief() {
    let (sock, root, _url) = scenario("person").await;
    std::fs::write(root.join("src/response.js"), "export function respond() {}\n").unwrap();

    // A person, as `knoot present` announces one.
    let human = format!("{}priya-4821", knoot::proto::HUMAN_SESSION_PREFIX);
    joins(&sock, &root, &human, "priya");
    hook_as(&sock, prompt(&root, &human, "rewriting the error shape by hand"), "priya");
    hook_as(&sock, tool(&root, &human, "PreToolUse", "Edit", "src/response.js"), "priya");

    // An agent's next turn.
    joins(&sock, &root, "s-agent", "sam");
    let brief = told(&hook_as(&sock, prompt(&root, "s-agent", "tidy the response helper"), "sam"));

    assert!(brief.contains("a person in an editor"), "the peer list must say which:\n{brief}");
    assert!(
        brief.contains("Do not ask them to release a file"),
        "and the advice must differ from the advice about an agent:\n{brief}"
    );
    assert!(brief.contains("src/response.js"), "with what they are in:\n{brief}");
}

/// And an agent is still an agent: the extra advice must not appear when
/// every peer is one, or it becomes noise on every turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_room_of_agents_is_not_told_anybody_is_a_person() {
    let (sock, root, _url) = scenario("agentsonly").await;
    joins(&sock, &root, "s-one", "priya");
    hook_as(&sock, prompt(&root, "s-one", "adding retries"), "priya");
    joins(&sock, &root, "s-two", "sam");
    let brief = told(&hook_as(&sock, prompt(&root, "s-two", "tidy up"), "sam"));
    assert!(brief.contains("priya"), "the peer is there:\n{brief}");
    assert!(!brief.contains("a person in an editor"), "{brief}");
    assert!(!brief.contains("Do not ask them to release"), "{brief}");
}
