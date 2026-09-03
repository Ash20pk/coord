//! Layer 4: contract test against Claude Code's hook interface. Drives the
//! real `knoot` binary with canned hook payloads and asserts on exact stdout.
//! This is the test most likely to catch upstream hook-format drift.
//!
//! Note: these run on a multi_thread runtime — `hook()` blocks on a child
//! process, which would otherwise starve the in-process relay and daemon.

mod common;
use common::*;
use knoot::proto::{DReq, DResp};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_knoot");

/// Run `knoot hook` with a payload on stdin; returns parsed stdout (None if empty).
fn hook(sock: &Path, payload: Value) -> Option<Value> {
    hook_as(sock, payload, None)
}

/// As `hook`, but labelling the session with KNOOT_USER.
fn hook_as(sock: &Path, payload: Value, knoot_user: Option<&str>) -> Option<Value> {
    let mut cmd = Command::new(BIN);
    cmd.arg("hook").env("KNOOT_SOCK", sock).env("USER", "testuser");
    if let Some(u) = knoot_user {
        cmd.env("KNOOT_USER", u);
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
    let (sock, root, _url) = scenario_with_relay(tag).await;
    (sock, root)
}

async fn scenario_with_relay(tag: &str) -> (PathBuf, PathBuf, String) {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp(tag);
    init_repo(&root, &url, &format!("e2e-{tag}"));
    (sock, root, url)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allowed_edit_produces_no_output_and_records_a_claim() {
    let (sock, root, url) = scenario_with_relay("allow").await;
    let out = hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
    assert!(out.is_none(), "an allowed edit must print nothing, got {out:?}");
    // Without this the test would also pass with the daemon unreachable.
    assert!(
        relay_holds_claim(&url, "e2e-allow", "src/auth.ts").await,
        "silence must mean 'claimed', not 'coordination unreachable'"
    );
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
    assert!(reason.contains("knoot msg"), "brief must offer a way to coordinate: {reason}");
    assert!(reason.contains("released"), "brief must promise a release notification: {reason}");
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

/// Two clones of one repo on two branches. Same repo id (same origin), same
/// file, different working trees — which is not a collision, and blocking it
/// is the false positive that gets a tool switched off.
fn git_clone_dir(tag: &str, branch: &str, relay: &str, repo: &str) -> PathBuf {
    let root = tmp(tag);
    let run = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/response.js"), "// seed\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "seed"]);
    // -B, not -b: the initial branch may already be the one we want.
    run(&["checkout", "-q", "-B", branch]);
    init_repo(&root, relay, repo);
    root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_on_another_branch_does_not_block() {
    let url = start_relay().await;
    let sock = start_daemon().await;
    // One repo id, two checkouts: exactly what two teammates have.
    let a = git_clone_dir("branch-a", "main", &url, "e2e-branches");
    let b = git_clone_dir("branch-b", "feat/discounts", &url, "e2e-branches");

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": a.to_string_lossy()
    }), Some("ash"));
    hook_as(&sock, edit(&a, "sessA", "src/response.js", "PreToolUse"), Some("ash"));

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": b.to_string_lossy()
    }), Some("priya"));
    let out = hook_as(&sock, edit(&b, "sessB", "src/response.js", "PreToolUse"), Some("priya"));

    assert!(
        out.is_none(),
        "a different branch is not a collision — the write must be allowed: {out:?}"
    );
}

/// The same file on the *same* branch still blocks. Branch awareness must not
/// become a hole in the thing that works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_branch_still_blocks() {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let a = git_clone_dir("samebranch-a", "feat/x", &url, "e2e-samebranch");
    let b = git_clone_dir("samebranch-b", "feat/x", &url, "e2e-samebranch");

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": a.to_string_lossy()
    }), Some("ash"));
    hook_as(&sock, edit(&a, "sessA", "src/response.js", "PreToolUse"), Some("ash"));

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": b.to_string_lossy()
    }), Some("priya"));
    let out = hook_as(&sock, edit(&b, "sessB", "src/response.js", "PreToolUse"), Some("priya"))
        .expect("one branch, one file, two agents — this must still be denied");

    assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
}

/// Allowed, but not silent: the write is told it will meet another branch's
/// work at merge, while re-planning still costs one turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_branch_write_is_warned_about() {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let a = git_clone_dir("warn-a", "main", &url, "e2e-warn");
    let b = git_clone_dir("warn-b", "feat/discounts", &url, "e2e-warn");

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": a.to_string_lossy()
    }), Some("ash"));
    hook_as(&sock, edit(&a, "sessA", "src/response.js", "PreToolUse"), Some("ash"));

    hook_as(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": b.to_string_lossy()
    }), Some("priya"));
    hook_as(&sock, edit(&b, "sessB", "src/response.js", "PreToolUse"), Some("priya"));
    let out = hook_as(&sock, edit(&b, "sessB", "src/response.js", "PostToolUse"), Some("priya"))
        .expect("a cross-branch write must say so");

    let ctx = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.contains("ash"), "must name the peer: {ctx}");
    assert!(ctx.contains("main"), "must name their branch: {ctx}");
    assert!(ctx.contains("merge"), "must say where it lands: {ctx}");
    assert!(!ctx.contains("blocked\n"), "must not read as a block: {ctx}");
}

/// Gap 1: a peer's write must reach the next turn on its own. A cheap model
/// will not run `knoot who`, so anything it needs to coordinate cannot sit
/// behind a command it has to think of.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_is_told_what_changed_under_it() {
    let (sock, root) = scenario("pushwrites").await;
    let rs = root.to_string_lossy().to_string();

    for (s, u) in [("sessA", "ash"), ("sessB", "priya")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }
    // A takes its first turn, which sets the bookmark the next one measures from.
    hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": rs, "prompt": "read the billing module"
    }), Some("ash"));

    // priya writes a file while ash is mid-task.
    hook_as(&sock, edit(&root, "sessB", "src/billing.ts", "PreToolUse"), Some("priya"));
    hook_as(&sock, edit(&root, "sessB", "src/billing.ts", "PostToolUse"), Some("priya"));

    let out = hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": rs, "prompt": "now compute the total"
    }), Some("ash"))
    .expect("the next turn must be told the ground moved");

    let ctx = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.contains("changed under you"), "must flag the change: {ctx}");
    assert!(ctx.contains("priya"), "must name the author: {ctx}");
    assert!(ctx.contains("src/billing.ts"), "must name the path: {ctx}");
    assert!(ctx.contains("stale"), "must say why it matters: {ctx}");
}

/// The same write must not be re-reported every turn: a bookmark advances.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_is_reported_once_not_every_turn() {
    let (sock, root) = scenario("pushonce").await;
    let rs = root.to_string_lossy().to_string();

    for (s, u) in [("sessA", "ash"), ("sessB", "priya")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }
    hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA", "cwd": rs, "prompt": "first"
    }), Some("ash"));
    hook_as(&sock, edit(&root, "sessB", "src/billing.ts", "PreToolUse"), Some("priya"));
    hook_as(&sock, edit(&root, "sessB", "src/billing.ts", "PostToolUse"), Some("priya"));

    let second = hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA", "cwd": rs, "prompt": "second"
    }), Some("ash")).expect("the change lands on this turn");
    assert!(second["hookSpecificOutput"]["additionalContext"]
        .as_str().unwrap().contains("changed under you"));

    let third = hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA", "cwd": rs, "prompt": "third"
    }), Some("ash"));
    let ctx = third
        .as_ref()
        .and_then(|v| v["hookSpecificOutput"]["additionalContext"].as_str())
        .unwrap_or("");
    assert!(
        !ctx.contains("changed under you"),
        "a write already reported must not repeat: {ctx}"
    );
}

/// Mail used to wait for the Stop hook. It should be in front of the agent at
/// the start of the turn instead, when it can still change the plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_arrive_at_the_start_of_a_turn() {
    let (sock, root) = scenario("pushmail").await;
    let rs = root.to_string_lossy().to_string();

    for (s, u) in [("sessA", "ash"), ("sessB", "priya")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }
    let mut cmd = Command::new(BIN);
    cmd.args(["msg", "ash", "discount() returns a number, not an array"])
        .current_dir(&root)
        .env("KNOOT_SOCK", &sock)
        .env("KNOOT_USER", "priya")
        .env("USER", "testuser");
    assert!(cmd.status().unwrap().success(), "knoot msg must succeed");

    let out = hook_as(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": rs, "prompt": "wire up the endpoint"
    }), Some("ash"))
    .expect("a waiting message must reach the turn that starts after it");

    let ctx = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.contains("messages for you"), "must be labelled as mail: {ctx}");
    assert!(ctx.contains("returns a number"), "must carry the text: {ctx}");
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
async fn read_only_tools_are_never_gated() {
    let (sock, root) = scenario("readonly").await;
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse")); // A claims it

    for tool in ["Read", "Grep", "Glob", "WebFetch"] {
        let out = hook(&sock, json!({
            "hook_event_name": "PreToolUse", "session_id": "sessB",
            "cwd": root.to_string_lossy(), "tool_name": tool,
            "tool_input": { "file_path": format!("{}/src/auth.ts", root.to_string_lossy()) }
        }));
        assert!(out.is_none(), "{tool} must never be blocked, got {out:?}");
    }
}

/// Was a known gap: agents reach for sed/heredocs as readily as the Edit tool,
/// and auto mode prefers Bash outright. Closed by parsing write targets out of
/// the command at PreToolUse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_write_to_a_claimed_file_is_blocked() {
    let (sock, root) = scenario("bashgap").await;
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse")); // A claims it

    let target = format!("{}/src/auth.ts", root.to_string_lossy());
    for cmd in [
        format!("sed -i '' 's/a/b/' {target}"),
        format!("echo x >> {target}"),
        format!("cat > {target} <<'EOF'\nhi\nEOF"),
        format!("echo x | tee {target}"),
        format!("cp /tmp/whatever {target}"),
        format!("rm {target}"),
        // relative path, and a path with .. in it, must resolve the same
        "sed -i '' 's/a/b/' src/auth.ts".to_string(),
        "echo x > src/../src/auth.ts".to_string(),
    ] {
        let out = hook(&sock, json!({
            "hook_event_name": "PreToolUse", "session_id": "sessB",
            "cwd": root.to_string_lossy(), "tool_name": "Bash",
            "tool_input": { "command": cmd }
        }));
        assert!(out.is_some(), "Bash write must be gated like Edit: {cmd}");
    }
}

/// The real thing the misnamed failure test claimed to cover: kill the relay
/// process, bring it back, and the daemon must reconnect and enforce again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_restart_is_survived_by_the_daemon() {
    use std::net::TcpListener;
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let url = format!("ws://{addr}/ws");
    let db = tmp("restart-db").join("relay.db");

    let spawn_relay = || {
        Command::new(BIN)
            .args(["relay", "--listen", &addr, "--db", &db.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let wait_up = || {
        for _ in 0..100 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("relay did not come up on {addr}");
    };

    let mut relay = spawn_relay();
    wait_up();
    let sock = start_daemon().await;
    let root = tmp("restart");
    init_repo(&root, &url, "restart-repo");

    // Before: coordination works.
    assert!(hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse")).is_none());
    assert!(hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse")).is_some());

    // Kill the relay. The daemon must fail open, not hang or crash.
    relay.kill().unwrap();
    relay.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        hook(&sock, edit(&root, "sessB", "src/other.ts", "PreToolUse")).is_none(),
        "relay down must fail open"
    );

    // Bring it back (fresh state — leases are the safety net, not persistence).
    let mut relay = spawn_relay();
    wait_up();
    // Daemon reconnects with 3s backoff; give it room.
    let mut enforced = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
        if hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse")).is_some() {
            enforced = true;
            break;
        }
    }
    relay.kill().ok();
    assert!(enforced, "daemon must reconnect after relay restart and enforce again");
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
        .env("KNOOT_SOCK", &sock)
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
    assert!(!reason.contains("testuser"), "KNOOT_USER must win over $USER: {reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_bash_is_never_blocked_and_claims_nothing() {
    let (sock, root, url) = scenario_with_relay("bashread").await;
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse")); // A holds it

    for cmd in [
        "cat src/auth.ts",
        "grep -n token src/auth.ts",
        "ls -la src",
        "git status --porcelain",
        "sed 's/a/b/' src/auth.ts",
        "echo 'writing > src/auth.ts is what I would do'",
        "cat src/auth.ts > /dev/null",
    ] {
        let out = hook(&sock, json!({
            "hook_event_name": "PreToolUse", "session_id": "sessB",
            "cwd": root.to_string_lossy(), "tool_name": "Bash",
            "tool_input": { "command": cmd }
        }));
        assert!(out.is_none(), "read-only command must not be blocked: {cmd} -> {out:?}");
    }
    // And none of them should have taken a claim for sessB.
    assert!(
        relay_holds_claim(&url, "e2e-bashread", "src/auth.ts").await,
        "sessA's claim should be the only one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bash_write_outside_any_claim_is_allowed_and_then_claimed() {
    let (sock, root, url) = scenario_with_relay("bashclaim").await;
    let target = format!("{}/src/fresh.ts", root.to_string_lossy());
    let out = hook(&sock, json!({
        "hook_event_name": "PreToolUse", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "tool_name": "Bash",
        "tool_input": { "command": format!("echo hi > {target}") }
    }));
    assert!(out.is_none(), "an unclaimed path must be writable");
    assert!(
        relay_holds_claim(&url, "e2e-bashclaim", "src/fresh.ts").await,
        "a shell write must take the claim, so peers are blocked next"
    );
    // A peer must now be blocked on that same path.
    let peer = hook(&sock, json!({
        "hook_event_name": "PreToolUse", "session_id": "sessB",
        "cwd": root.to_string_lossy(), "tool_name": "Bash",
        "tool_input": { "command": format!("echo bye >> {target}") }
    }));
    assert!(peer.is_some(), "peer must be blocked from the newly claimed path");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpredictable_bash_write_is_detected_after_the_fact() {
    // Interpreters can write anything, so the parser cannot gate them. The
    // working-tree diff must at least record what happened.
    let (sock, root, url) = scenario_with_relay("audit").await;
    std::process::Command::new("git").args(["-C", &root.to_string_lossy(), "init", "-q"])
        .status().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/auth.ts"), "original\n").unwrap();
    std::process::Command::new("git").args(["-C", &root.to_string_lossy(), "add", "-A"])
        .status().unwrap();
    std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "-c", "user.email=t@t", "-c", "user.name=t",
               "commit", "-qm", "seed"])
        .status().unwrap();

    let ungated = watch_ungated(&url, "e2e-audit");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // sessA claims the file through the normal path.
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
    assert!(relay_holds_claim(&url, "e2e-audit", "src/auth.ts").await);

    // sessB writes it via an interpreter the parser cannot read.
    let script = format!(
        "python3 -c \"open('{}/src/auth.ts','w').write('clobbered')\"",
        root.to_string_lossy()
    );
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": root.to_string_lossy()
    }));
    let pre = hook(&sock, json!({
        "hook_event_name": "PreToolUse", "session_id": "sessB",
        "cwd": root.to_string_lossy(), "tool_name": "Bash",
        "tool_input": { "command": script.clone() }
    }));
    assert!(pre.is_none(), "the parser cannot gate an interpreter, so it must allow");

    std::process::Command::new("bash").args(["-lc", &script]).status().unwrap();

    hook(&sock, json!({
        "hook_event_name": "PostToolUse", "session_id": "sessB",
        "cwd": root.to_string_lossy(), "tool_name": "Bash",
        "tool_input": { "command": script }
    }));

    // The collision must appear in the log as ungated — observed, not prevented.
    let mut found = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if ungated.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            found = true;
            break;
        }
    }
    assert!(found, "an ungated write over a peer's claim must be recorded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_is_refreshed_on_every_prompt_not_just_at_startup() {
    let (sock, root) = scenario("refresh").await;

    // sessB starts alone: no peers to report.
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessB", "cwd": root.to_string_lossy()
    }));

    // sessA appears afterwards and takes a file.
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA", "cwd": root.to_string_lossy()
    }));
    hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessA",
        "cwd": root.to_string_lossy(), "prompt": "refactor the auth module"
    }));
    hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));

    // sessB's next prompt must learn about sessA — it started before sessA did,
    // so SessionStart context alone would leave it permanently blind.
    let out = hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "sessB",
        "cwd": root.to_string_lossy(), "prompt": "now add refreshSession"
    }))
    .expect("a prompt with peers active must carry their context");

    let hso = &out["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "UserPromptSubmit");
    let ctx = hso["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("refactor the auth module"), "must carry peer intent: {ctx}");
    assert!(ctx.contains("src/auth.ts"), "must list what the peer holds: {ctx}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_with_no_peers_stays_quiet() {
    let (sock, root) = scenario("quiet").await;
    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "solo", "cwd": root.to_string_lossy()
    }));
    let out = hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "solo",
        "cwd": root.to_string_lossy(), "prompt": "do the thing"
    }));
    assert!(out.is_none(), "no peers means no injected context: {out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_concurrent_write_is_not_blamed_on_the_audited_session() {
    // The working tree is shared, so a peer's edit lands inside our audit
    // window too. Observed live: ci-bot was reported as writing over sam's
    // claim on a file it never touched.
    let (sock, root, url) = scenario_with_relay("blame").await;
    let rs = root.to_string_lossy().to_string();
    std::process::Command::new("git").args(["-C", &rs, "init", "-q"]).status().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/billing.js"), "original\n").unwrap();
    std::fs::write(root.join("src/api.js"), "original\n").unwrap();
    std::process::Command::new("git").args(["-C", &rs, "add", "-A"]).status().unwrap();
    std::process::Command::new("git")
        .args(["-C", &rs, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "seed"])
        .status().unwrap();

    for s in ["sam", "cibot"] {
        hook(&sock, json!({
            "hook_event_name": "SessionStart", "session_id": s, "cwd": rs
        }));
    }
    // sam claims billing.js through the Edit path.
    hook(&sock, edit(&root, "sam", "src/billing.js", "PreToolUse"));
    assert!(relay_holds_claim(&url, "e2e-blame", "src/billing.js").await);

    let ungated = watch_ungated(&url, "e2e-blame");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // cibot starts an opaque command (audited) touching only api.js...
    let script = format!("python3 -c \"open('{rs}/src/api.js','a').write('x')\"");
    hook(&sock, json!({
        "hook_event_name": "PreToolUse", "session_id": "cibot",
        "cwd": rs, "tool_name": "Bash", "tool_input": { "command": script.clone() }
    }));
    // ...while sam writes billing.js in the same window, and reports it.
    std::fs::write(root.join("src/billing.js"), "sam's change\n").unwrap();
    hook(&sock, edit(&root, "sam", "src/billing.js", "PostToolUse"));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    std::process::Command::new("bash").args(["-lc", &script]).status().unwrap();
    hook(&sock, json!({
        "hook_event_name": "PostToolUse", "session_id": "cibot",
        "cwd": rs, "tool_name": "Bash", "tool_input": { "command": script }
    }));

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(
        ungated.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a peer's own reported write must not be attributed to the audited session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ungated_write_is_still_caught_while_the_holder_writes_continuously() {
    // Observed live and missed entirely: the holder was editing the file every
    // few seconds, so "a peer wrote this in my window" was always true and the
    // audit skipped every one of the intruder's writes. Naming the file in the
    // command is what distinguishes our write from theirs.
    let (sock, root, url) = scenario_with_relay("masked").await;
    let rs = root.to_string_lossy().to_string();
    std::process::Command::new("git").args(["-C", &rs, "init", "-q"]).status().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/auth.js"), "original\n").unwrap();
    std::process::Command::new("git").args(["-C", &rs, "add", "-A"]).status().unwrap();
    std::process::Command::new("git")
        .args(["-C", &rs, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "seed"])
        .status().unwrap();

    for s in ["holder", "intruder"] {
        hook(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }));
    }
    hook(&sock, edit(&root, "holder", "src/auth.js", "PreToolUse"));

    let ungated = watch_ungated(&url, "e2e-masked");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The intruder runs an opaque command naming the file...
    let script = format!("python3 -c \"open('{rs}/src/auth.js','a').write('intruder\\n')\"");
    hook(&sock, json!({
        "hook_event_name": "PreToolUse", "session_id": "intruder",
        "cwd": rs, "tool_name": "Bash", "tool_input": { "command": script.clone() }
    }));
    // ...while the holder keeps writing the very same file throughout.
    std::fs::write(root.join("src/auth.js"), "holder edit 1\n").unwrap();
    hook(&sock, edit(&root, "holder", "src/auth.js", "PostToolUse"));
    std::process::Command::new("bash").args(["-lc", &script]).status().unwrap();
    std::fs::write(root.join("src/auth.js"), "holder edit 2\n").unwrap();
    hook(&sock, edit(&root, "holder", "src/auth.js", "PostToolUse"));

    hook(&sock, json!({
        "hook_event_name": "PostToolUse", "session_id": "intruder",
        "cwd": rs, "tool_name": "Bash", "tool_input": { "command": script }
    }));

    let mut found = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if ungated.load(std::sync::atomic::Ordering::SeqCst) > 0 { found = true; break; }
    }
    assert!(found, "a busy holder must not mask an intruder's ungated write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_session_is_told_when_the_path_is_released() {
    // The gap that made this not really multiplayer: a blocked session waited
    // on a lease it could not observe, and nobody told it when work finished.
    let (sock, root) = scenario("notify").await;
    let rs = root.to_string_lossy().to_string();

    for s in ["holder", "waiter"] {
        hook(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }));
    }
    hook(&sock, json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "holder",
        "cwd": rs, "prompt": "refactor the auth module"
    }));
    hook(&sock, edit(&root, "holder", "src/auth.ts", "PreToolUse"));

    // waiter is blocked, which registers its interest.
    assert!(
        hook(&sock, edit(&root, "waiter", "src/auth.ts", "PreToolUse")).is_some(),
        "waiter should be blocked"
    );
    // Nothing pending yet: being blocked is not itself news.
    assert!(hook(&sock, json!({
        "hook_event_name": "Stop", "session_id": "waiter", "cwd": rs, "stop_hook_active": false
    })).is_none());

    // The holder finishes.
    hook(&sock, json!({ "hook_event_name": "SessionEnd", "session_id": "holder", "cwd": rs }));

    // The waiter must be woken with the news, via the Stop hook.
    let mut got = None;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(out) = hook(&sock, json!({
            "hook_event_name": "Stop", "session_id": "waiter", "cwd": rs, "stop_hook_active": false
        })) {
            got = Some(out);
            break;
        }
    }
    let out = got.expect("waiter must be notified that the path was released");
    assert_eq!(out["decision"], "block", "the notification must send it back to work");
    let reason = out["reason"].as_str().unwrap();
    assert!(reason.contains("src/auth.ts"), "must name the freed path: {reason}");
    assert!(reason.contains("free"), "must say it is available: {reason}");
    assert!(
        reason.contains("refactor the auth module"),
        "must carry what the holder was doing: {reason}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_can_message_each_other() {
    let (sock, root) = scenario("msg").await;
    let rs = root.to_string_lossy().to_string();
    for (s, u) in [("s-ash", "ash"), ("s-priya", "priya")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }

    let sent = ask_daemon(&sock, DReq::Msg {
        repo_root: rs.clone(),
        from_user: "ash".into(),
        to: Some("priya".into()),
        text: "I'm done with auth.js, it's yours".into(),
    });
    assert!(matches!(sent, Some(DResp::Ok)), "send should succeed: {sent:?}");

    // priya learns of it when finishing a turn; ash does not hear its own note.
    let mut delivered = None;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(out) = hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s-priya", "cwd": rs, "stop_hook_active": false
        }), Some("priya")) {
            delivered = Some(out);
            break;
        }
    }
    let out = delivered.expect("priya must receive the message");
    let reason = out["reason"].as_str().unwrap();
    assert!(reason.contains("from ash"), "must name the sender: {reason}");
    assert!(reason.contains("it's yours"), "must carry the text: {reason}");

    assert!(
        hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s-ash", "cwd": rs, "stop_hook_active": false
        }), Some("ash")).is_none(),
        "a sender must not be notified of its own message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_cannot_trap_a_session_in_a_loop() {
    let (sock, root) = scenario("loop").await;
    let rs = root.to_string_lossy().to_string();
    for (s, u) in [("s-a", "ash"), ("s-b", "bee")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }

    // A peer that will not stop talking.
    for i in 0..8 {
        ask_daemon(&sock, DReq::Msg {
            repo_root: rs.clone(), from_user: "ash".into(),
            to: Some("bee".into()), text: format!("note {i}"),
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Simulate repeated continuations: after a few, stop must be allowed.
    let mut blocked = 0;
    for _ in 0..6 {
        let out = hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s-b", "cwd": rs, "stop_hook_active": true
        }), Some("bee"));
        if out.is_some() { blocked += 1; } else { break; }
    }
    assert!(blocked <= 3, "a chatty peer must not keep a session spinning (blocked {blocked}x)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_identity_comes_from_the_caller_not_the_os_user() {
    // Observed live: 28 messages between four agents all showed "from ash",
    // because Claude Code exposes no session id to the commands it runs and
    // the daemon fell back to the OS user.
    let (sock, root) = scenario("identity").await;
    let rs = root.to_string_lossy().to_string();
    for (s, u) in [("s-sam", "sam"), ("s-priya", "priya")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }

    ask_daemon(&sock, DReq::Msg {
        repo_root: rs.clone(),
        from_user: "sam".into(),
        to: Some("priya".into()),
        text: "billing.js is ready".into(),
    });

    let mut got = None;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(out) = hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s-priya", "cwd": rs, "stop_hook_active": false
        }), Some("priya")) { got = Some(out); break; }
    }
    let reason = got.expect("priya must get it")["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("from sam"), "sender must be sam, not the OS user: {reason}");
    assert!(!reason.contains("testuser"), "must not fall back to $USER: {reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_reaches_every_peer_but_not_the_sender() {
    let (sock, root) = scenario("broadcast2").await;
    let rs = root.to_string_lossy().to_string();
    for (s, u) in [("s-a", "ash"), ("s-b", "bee"), ("s-c", "cat")] {
        hook_as(&sock, json!({ "hook_event_name": "SessionStart", "session_id": s, "cwd": rs }), Some(u));
    }
    ask_daemon(&sock, DReq::Msg {
        repo_root: rs.clone(), from_user: "ash".into(), to: None,
        text: "goal is green, stop editing".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    for u in ["bee", "cat"] {
        let out = hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s", "cwd": rs, "stop_hook_active": false
        }), Some(u));
        let reason = out.unwrap_or_else(|| panic!("{u} must receive the broadcast"))["reason"]
            .as_str().unwrap().to_string();
        assert!(reason.contains("to everyone"), "{u} should see it was a broadcast: {reason}");
    }
    assert!(
        hook_as(&sock, json!({
            "hook_event_name": "Stop", "session_id": "s-a", "cwd": rs, "stop_hook_active": false
        }), Some("ash")).is_none(),
        "the sender must not receive its own broadcast"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_idle_past_the_old_prune_window_keeps_its_identity() {
    // The run that exposed this had a 41-minute gap between startup and the
    // first prompt; every later claim was then attributed to the OS user.
    let mut v = knoot::proto::View::default();
    v.apply(&knoot::proto::Event::SessionStarted {
        session: "s1".into(), user: "sam".into(), branch: "main".into(),
        ts: knoot::proto::now_ms() - 45 * 60 * 1000,
    });
    v.prune();
    assert_eq!(
        v.sessions.get("s1").map(|s| s.user.as_str()),
        Some("sam"),
        "45 minutes idle at a prompt is alive, not stale"
    );
}

/// A granted claim must be in this daemon's own mirror by the time the hook
/// returns — not whenever the relay's broadcast gets back to us.
///
/// The Bash gate consults only the local mirror, so anything the mirror has
/// not caught up on is a file a peer session can `sed -i` freely. The window
/// is milliseconds, which is exactly long enough: the previous version of this
/// passed on macOS and failed on Linux CI, having been written as a race
/// rather than as an assertion. Against `start_granting_relay` the broadcast
/// never arrives at all, so the assertion holds on every machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_granted_claim_is_in_the_local_mirror_before_the_hook_returns() {
    let url = start_granting_relay().await;
    let sock = start_daemon().await;
    let root = tmp("mirror-lag");
    init_repo(&root, &url, "e2e-mirror-lag");

    hook(&sock, json!({
        "hook_event_name": "SessionStart", "session_id": "sessA",
        "cwd": root.to_string_lossy()
    }));
    // sessA takes the file through the arbitrated path.
    let out = hook(&sock, edit(&root, "sessA", "src/auth.ts", "PreToolUse"));
    assert!(out.is_none(), "the relay granted it, so it must be allowed: {out:?}");

    // With no broadcast coming, the only way sessB can be stopped is if the
    // grant was recorded locally the moment it was won.
    let target = format!("{}/src/auth.ts", root.to_string_lossy());
    for cmd in [format!("echo x >> {target}"), "sed -i '' 's/a/b/' src/auth.ts".to_string()] {
        let denial = hook(&sock, json!({
            "hook_event_name": "PreToolUse", "session_id": "sessB",
            "cwd": root.to_string_lossy(), "tool_name": "Bash",
            "tool_input": { "command": cmd }
        }));
        let denial = denial.unwrap_or_else(|| {
            panic!("a peer's Bash write to a claimed file must be blocked: {cmd}")
        });
        assert_eq!(
            denial["hookSpecificOutput"]["permissionDecision"], "deny",
            "must be a denial, not merely output: {denial}"
        );
    }

    // And the same must hold for the Edit path.
    let edit_denial = hook(&sock, edit(&root, "sessB", "src/auth.ts", "PreToolUse"));
    assert!(edit_denial.is_some(), "a peer's Edit of a claimed file must be blocked too");
}
