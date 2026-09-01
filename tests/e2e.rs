//! Layer 4: contract test against Claude Code's hook interface. Drives the
//! real `coord` binary with canned hook payloads and asserts on exact stdout.
//! This is the test most likely to catch upstream hook-format drift.
//!
//! Note: these run on a multi_thread runtime — `hook()` blocks on a child
//! process, which would otherwise starve the in-process relay and daemon.

mod common;
use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_coord");

/// Run `coord hook` with a payload on stdin; returns parsed stdout (None if empty).
fn hook(sock: &Path, payload: Value) -> Option<Value> {
    hook_as(sock, payload, None)
}

/// As `hook`, but labelling the session with COORD_USER.
fn hook_as(sock: &Path, payload: Value, coord_user: Option<&str>) -> Option<Value> {
    let mut cmd = Command::new(BIN);
    cmd.arg("hook").env("COORD_SOCK", sock).env("USER", "testuser");
    if let Some(u) = coord_user {
        cmd.env("COORD_USER", u);
    }
    run(cmd, payload)
}

fn run(mut cmd: Command, payload: Value) -> Option<Value> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hook must always exit 0, got {:?}", out.status);
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&s).expect("hook output must be valid JSON"))
    }
}

fn edit(root: &PathBuf, session: &str, rel: &str, event: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": session,
        "cwd": root.to_string_lossy(),
        "tool_name": "Edit",
        "tool_input": { "file_path": format!("{}/{}", root.to_string_lossy(), rel) }
    })
}

async fn scenario(tag: &str) -> (PathBuf, PathBuf) {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp(tag);
    init_repo(&root, &url, &format!("e2e-{tag}"));
    (sock, root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allowed_edit_produces_no_output() {
    let (sock, root) = scenario("allow").await;
    let out = hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
    assert!(out.is_none(), "an allowed edit must print nothing, got {out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_edit_emits_a_deny_decision_with_a_usable_brief() {
    let (sock, root) = scenario("deny").await;

    // Session A: start, declare intent, claim the file.
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "prompt": "refactor the auth session handling"
    }));
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));

    // Session B collides.
    let out = hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse"))
        .expect("a conflicting edit must produce output");

    let hso = &out["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "PreToolUse");
    assert_eq!(hso["permissionDecision"], "deny", "must deny, not ask");

    let reason = hso["permissionDecisionReason"].as_str().unwrap();
    // The brief has to carry everything the model needs to re-plan.
    assert!(reason.contains("src/auth.ts"), "brief must name the file: {reason}");
    assert!(reason.contains("testuser"), "brief must name the holder: {reason}");
    assert!(
        reason.contains("refactor the auth session handling"),
        "brief must carry the holder's intent: {reason}"
    );
    assert!(reason.contains("re-plan"), "brief must tell the model what to do: {reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_injects_peer_presence_context() {
    let (sock, root) = scenario("presence").await;

    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "prompt": "refactor auth"
    }));
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));

    let out = hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": root.to_string_lossy()
    }))
    .expect("a joining session must be told about peers");

    let hso = &out["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "SessionStart");
    let ctx = hso["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("testuser"), "context must name the peer: {ctx}");
    assert!(ctx.contains("refactor auth"), "context must carry peer intent: {ctx}");
    assert!(ctx.contains("src/auth.ts"), "context must list held paths: {ctx}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_session_gets_no_presence_noise() {
    let (sock, root) = scenario("solo").await;
    let out = hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "only", "cwd": root.to_string_lossy()
    }));
    assert!(out.is_none(), "a lone session must not be told about itself: {out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_write_tools_are_never_gated() {
    let (sock, root) = scenario("readonly").await;
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse")); // A claims it

    for tool in ["Read", "Grep", "Glob", "Bash", "WebFetch"] {
        let out = hook(&sock, json!({
            "hook_event_name": "PreToolUse", "session_id": "sessB",
            "cwd": root.to_string_lossy(), "tool_name": tool,
            "tool_input": { "file_path": format!("{}/src/auth.ts", root.to_string_lossy()) }
        }));
        assert!(out.is_none(), "{tool} must never be blocked, got {out:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_end_releases_the_claim_for_the_next_session() {
    let (sock, root) = scenario("release").await;
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
    assert!(
        hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse")).is_some(),
        "B should be blocked while A holds"
    );

    hook(&sock, json!({
        "hook_event_name": "SessionEnd", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert!(
        hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse")).is_none(),
        "B must be free to edit once A ends"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slash_commands_are_not_recorded_as_intent() {
    let (sock, root) = scenario("slash").await;
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "prompt": "/clear"
    }));
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));

    let out = hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse")).unwrap();
    let reason = out["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
    assert!(!reason.contains("/clear"), "slash commands must not leak as intent: {reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_and_partial_payloads_never_break_the_agent() {
    let (sock, root) = scenario("garbage").await;
    let cases = vec![
        json!({}),
        json!({ "hook_event_name": "PreToolUse" }),
        json!({ "hook_event_name": "Nonsense", "session_id": "x", "cwd": root.to_string_lossy() }),
        json!({ "hook_event_name": "PreToolUse", "session_id": "x",
                "cwd": root.to_string_lossy(), "tool_name": "Edit", "tool_input": {} }),
        json!({ "hook_event_name": "PreToolUse", "session_id": "x", "cwd": "/nonexistent/path",
                "tool_name": "Edit", "tool_input": { "file_path": "/nonexistent/path/a.ts" } }),
    ];
    for c in cases {
        let out = hook(&sock, c.clone());
        assert!(out.is_none(), "payload {c} must be ignored silently, got {out:?}");
    }

    // Outright invalid JSON on stdin.
    let mut child = Command::new(BIN)
        .arg("hook")
        .env("COORD_SOCK", &sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"not json").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "invalid JSON must still exit 0");
    assert!(out.stdout.is_empty(), "invalid JSON must produce no output");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_level_claim_blocks_nested_edits_but_not_siblings() {
    let (sock, root) = scenario("nested").await;
    // A claims a specific file; B's edit to the same file is blocked, sibling is fine.
    hook(&sock, edit(&root, "sessA", "src/auth/session.ts", "PreToolUse"));
    assert!(
        hook(&sock, edit(&root, "sessB", "src/auth/session.ts", "PreToolUse")).is_some(),
        "same file must be blocked"
    );
    assert!(
        hook(&sock, edit(&root, "sessB", "src/auth2/session.ts", "PreToolUse")).is_none(),
        "src/auth2 must not be captured by an src/auth claim"
    );
    assert!(
        hook(&sock, edit(&root, "sessB", "src/auth/other.ts", "PreToolUse")).is_none(),
        "a file-level claim must not block siblings in the same directory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_latency_stays_in_the_interactive_budget() {
    let (sock, root) = scenario("latency").await;
    hook(&sock, edit(&root, "sessA", "src/warm.ts", "PreToolUse")); // warm the path

    const N: u32 = 20;
    let start = std::time::Instant::now();
    for i in 0..N {
        hook(&sock, edit(&root, "sessA", &format!("src/f{i}.ts"), "PreToolUse"));
    }
    let per_call = start.elapsed() / N;
    // Includes process spawn + socket + relay round-trip. Generous ceiling so
    // this fails only on a real regression (e.g. a sync round-trip added).
    assert!(
        per_call < std::time::Duration::from_millis(60),
        "hook latency regressed to {per_call:?} per call"
    );
    eprintln!("hook latency: {per_call:?} per call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coord_user_labels_distinct_sessions_on_one_machine() {
    // Two sessions on the same box (same $USER) must still be able to carry
    // distinct identities — this is how a single dev tests, demos, or runs
    // several named agents at once.
    let (sock, root) = scenario("labels").await;

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }), Some("ash"));
    hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "prompt": "refactor auth"
    }), Some("ash"));
    hook_as(&sock, edit(&root, "sessA", "src/auth.js", "PreToolUse"), Some("ash"));

    let out = hook_as(&sock, edit(&root, "sessB", "src/auth.js", "PreToolUse"), Some("priya"))
        .expect("collision must be reported");
    let reason = out["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
    assert!(reason.contains("ash"), "brief must name the holder's label: {reason}");
    assert!(!reason.contains("testuser"), "COORD_USER must win over $USER: {reason}");
}
