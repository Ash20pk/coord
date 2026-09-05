//! Codex speaks the hook surface natively — the same events and the same
//! output contract as Claude Code, but one editing tool, `apply_patch`, whose
//! payload is a whole patch, and no read tool at all.
//!
//! Every test drives the real `knoot` binary with payloads shaped exactly as
//! Codex's `codex-rs/hooks` crate serialises them (`turn_id`, `tool_name:
//! "apply_patch"`, `tool_input.command` holding the patch) and asserts on what
//! Codex would do with the answer. One property per test, named for the thing
//! that would be wrong.

mod common;
use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_knoot");

/// `knoot hook --agent codex`, as `knoot init` installs it.
fn hook_as(sock: &Path, payload: Value, user: &str) -> Option<Value> {
    hook_with(sock, payload, user, Some("codex"))
}

fn hook_with(sock: &Path, payload: Value, user: &str, agent: Option<&str>) -> Option<Value> {
    let mut cmd = Command::new(BIN);
    cmd.arg("hook");
    if let Some(a) = agent {
        cmd.arg("--agent").arg(a);
    }
    let mut child = cmd
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

fn denied(out: &Option<Value>) -> bool {
    out.as_ref().map(|v| v["hookSpecificOutput"]["permissionDecision"] == "deny").unwrap_or(false)
}

/// The fields Codex puts on every turn-scoped hook payload. `transcript_path`
/// is present because Codex sends it; knoot must never open it.
fn envelope(root: &PathBuf, session: &str, event: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": session,
        "turn_id": "turn-1",
        "cwd": root.to_string_lossy(),
        "transcript_path": "/nonexistent/transcript.jsonl",
        "model": "gpt-5-codex",
        "permission_mode": "default",
    })
}

/// An `apply_patch` call as Codex serialises it: the patch is the command.
fn patch(root: &PathBuf, session: &str, event: &str, body: &str) -> Value {
    let mut v = envelope(root, session, event);
    v["tool_name"] = json!("apply_patch");
    v["matcher_aliases"] = json!(["Write", "Edit"]);
    v["tool_use_id"] = json!("call_1");
    v["tool_input"] = json!({ "command": body });
    v
}

fn bash(root: &PathBuf, session: &str, event: &str, command: &str) -> Value {
    let mut v = envelope(root, session, event);
    v["tool_name"] = json!("Bash");
    v["tool_input"] = json!({ "command": command });
    v
}

fn prompt(sock: &Path, root: &PathBuf, session: &str, user: &str, text: &str) -> String {
    let mut v = envelope(root, session, "UserPromptSubmit");
    v["prompt"] = json!(text);
    told(&hook_as(sock, v, user))
}

fn joins(sock: &Path, root: &PathBuf, session: &str, user: &str) {
    let mut v = envelope(root, session, "SessionStart");
    v["source"] = json!("startup");
    hook_as(sock, v, user);
}

fn update(paths: &[&str]) -> String {
    let mut s = String::from("*** Begin Patch\n");
    for p in paths {
        s.push_str(&format!("*** Update File: {p}\n@@\n-a\n+b\n"));
    }
    s.push_str("*** End Patch\n");
    s
}

async fn scenario(tag: &str) -> (PathBuf, PathBuf, String) {
    let url = start_relay().await;
    let sock = start_daemon().await;
    let root = tmp(tag);
    init_repo(&root, &url, &format!("codex-{tag}"));
    (sock, root, url)
}

fn seed(root: &PathBuf, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "seed\n").unwrap();
}

// ------------------------------------------------------------- the edit tool

/// The claim behind everything else: a Codex edit is gated exactly as a
/// Claude Code edit is. Codex holds a file through a patch; Claude Code is
/// denied it with the same brief, holder and intent included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patch_claims_its_files_and_a_peer_is_denied_them() {
    let (sock, root, url) = scenario("gate").await;
    joins(&sock, &root, "cx-1", "priya");
    prompt(&sock, &root, "cx-1", "priya", "rewrite the auth module");

    let out = hook_as(&sock, patch(&root, "cx-1", "PreToolUse", &update(&["src/auth.rs"])), "priya");
    assert!(!denied(&out), "an unclaimed file is free: {out:?}");
    assert!(relay_holds_claim(&url, "codex-gate", "src/auth.rs").await, "the claim reached the relay");

    // A Claude Code session on the same repo and branch.
    let claude = json!({
        "hook_event_name": "PreToolUse", "session_id": "cc-1",
        "cwd": root.to_string_lossy(), "tool_name": "Edit",
        "tool_input": { "file_path": format!("{}/src/auth.rs", root.to_string_lossy()) }
    });
    let out = hook_with(&sock, claude, "ash", Some("claude"));
    assert!(denied(&out), "Claude Code must be blocked by a Codex claim: {out:?}");
    let why = told(&out);
    assert!(why.contains("priya"), "the brief names the holder:\n{why}");
    assert!(why.contains("rewrite the auth module"), "and their intent:\n{why}");
}

/// The output contract, exactly. Codex's parser rejects a
/// `hookSpecificOutput` without a `hookEventName` and a deny without a
/// non-empty reason, so both are asserted on the bytes, not on intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denial_is_shaped_the_way_codex_parses_it() {
    let (sock, root, _) = scenario("shape").await;
    hook_as(&sock, patch(&root, "cx-1", "PreToolUse", &update(&["src/a.rs"])), "priya");
    let out = hook_as(&sock, patch(&root, "cx-2", "PreToolUse", &update(&["src/a.rs"])), "sam").unwrap();
    let hs = &out["hookSpecificOutput"];
    assert_eq!(hs["hookEventName"], "PreToolUse");
    assert_eq!(hs["permissionDecision"], "deny");
    assert!(
        hs["permissionDecisionReason"].as_str().is_some_and(|r| !r.trim().is_empty()),
        "Codex refuses a deny with no reason: {out}"
    );
    // Never `allow`: Codex treats it as unsupported without `updatedInput`,
    // and it would override the human's approval settings.
    assert_ne!(hs["permissionDecision"], "allow");
}

/// One patch, several files, checked as a unit. If the third file is held
/// by a peer the whole patch is denied — and the first two are *not* left
/// claimed by a session that is not going to write them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patch_denied_on_one_file_claims_none_of_the_others() {
    let (sock, root, url) = scenario("unit").await;
    // Sam holds c.rs.
    hook_as(&sock, patch(&root, "cx-sam", "PreToolUse", &update(&["src/c.rs"])), "sam");

    let out = hook_as(
        &sock,
        patch(&root, "cx-priya", "PreToolUse", &update(&["src/a.rs", "src/b.rs", "src/c.rs"])),
        "priya",
    );
    assert!(denied(&out), "one held file denies the patch: {out:?}");
    assert!(told(&out).contains("src/c.rs"), "and names the file that did it:\n{}", told(&out));

    assert!(!relay_holds_claim(&url, "codex-unit", "src/a.rs").await, "a.rs must not be left claimed");
    assert!(!relay_holds_claim(&url, "codex-unit", "src/b.rs").await, "b.rs must not be left claimed");
}

/// A patch that adds a file is a *creation*, and two agents creating the
/// same new file is the collision a claim on an existing path cannot see.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creating_a_file_a_peer_just_created_is_reported() {
    let (sock, root, _) = scenario("create").await;
    joins(&sock, &root, "cx-1", "priya");
    joins(&sock, &root, "cx-2", "sam");
    let add = "*** Begin Patch\n*** Add File: src/new.rs\n+fn main() {}\n*** End Patch\n";
    hook_as(&sock, patch(&root, "cx-1", "PreToolUse", add), "priya");
    seed(&root, "src/new.rs");
    hook_as(&sock, patch(&root, "cx-1", "PostToolUse", add), "priya");
    // Priya's session ends, so nothing is *held*; the collision is the point.
    hook_as(&sock, envelope(&root, "cx-1", "SessionEnd"), "priya");

    let out = hook_as(&sock, patch(&root, "cx-2", "PreToolUse", add), "sam");
    assert!(!denied(&out), "a creation collision is advisory, never a block: {out:?}");
    let said = told(&out);
    assert!(said.contains("src/new.rs") && said.contains("priya"), "who created it first:\n{said}");
}

/// A patch that deletes a file tells everyone who had read it. Announced
/// after the fact and only once the path is really gone — a patch that
/// failed deleted nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_reaches_a_session_that_read_the_path() {
    let (sock, root, _) = scenario("delete").await;
    seed(&root, "src/gone.rs");
    joins(&sock, &root, "cx-reader", "ash");
    // Ash read it through the shell, the only way Codex reads.
    hook_as(&sock, bash(&root, "cx-reader", "PreToolUse", "cat src/gone.rs"), "ash");
    hook_as(&sock, bash(&root, "cx-reader", "PostToolUse", "cat src/gone.rs"), "ash");

    joins(&sock, &root, "cx-priya", "priya");
    let del = "*** Begin Patch\n*** Delete File: src/gone.rs\n*** End Patch\n";
    hook_as(&sock, patch(&root, "cx-priya", "PreToolUse", del), "priya");
    // Announced only once it has actually happened.
    hook_as(&sock, patch(&root, "cx-priya", "PostToolUse", del), "priya");
    let early = prompt(&sock, &root, "cx-reader", "ash", "carry on");
    assert!(!early.contains("gone.rs") || !early.contains("deleted"), "not gone yet:\n{early}");

    std::fs::remove_file(root.join("src/gone.rs")).unwrap();
    hook_as(&sock, patch(&root, "cx-priya", "PreToolUse", del), "priya");
    hook_as(&sock, patch(&root, "cx-priya", "PostToolUse", del), "priya");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let brief = prompt(&sock, &root, "cx-reader", "ash", "carry on");
    assert!(brief.contains("src/gone.rs"), "the reader is told:\n{brief}");
}

// ------------------------------------------------------------ reads via shell

/// Codex has no Read tool. A write is stale when what the agent read has
/// changed, so the reads it does through the shell have to count — and this
/// is the mechanism that makes them count for Claude Code in auto mode too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_read_through_the_shell_is_a_read_for_staleness() {
    let (sock, root, _) = scenario("shellread").await;
    seed(&root, "src/types.rs");
    seed(&root, "src/user.rs");
    joins(&sock, &root, "cx-priya", "priya");
    joins(&sock, &root, "cx-sam", "sam");

    // Priya reads types.rs with sed -n, as an agent without Read does.
    hook_as(&sock, bash(&root, "cx-priya", "PreToolUse", "sed -n '1,40p' src/types.rs"), "priya");
    hook_as(&sock, bash(&root, "cx-priya", "PostToolUse", "sed -n '1,40p' src/types.rs"), "priya");

    // Sam changes it.
    hook_as(&sock, patch(&root, "cx-sam", "PreToolUse", &update(&["src/types.rs"])), "sam");
    std::fs::write(root.join("src/types.rs"), "changed\n").unwrap();
    hook_as(&sock, patch(&root, "cx-sam", "PostToolUse", &update(&["src/types.rs"])), "sam");
    hook_as(&sock, envelope(&root, "cx-sam", "SessionEnd"), "sam");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Priya writes a *different* file. The write is allowed and the stale
    // read is on the brief.
    let out = hook_as(&sock, patch(&root, "cx-priya", "PreToolUse", &update(&["src/user.rs"])), "priya");
    assert!(!denied(&out), "a stale read never denies: {out:?}");
    let said = told(&out);
    assert!(said.contains("src/types.rs") && said.contains("sam"), "the stale read, and who moved it:\n{said}");
}

/// Reads are conservative: a number or a pattern is not a file, and nothing
/// is recorded for a path that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shell_read_of_nothing_records_nothing() {
    let (sock, root, _) = scenario("noread").await;
    joins(&sock, &root, "cx-priya", "priya");
    for cmd in ["grep -n TODO", "head -n 40", "cat src/does-not-exist.rs"] {
        let out = hook_as(&sock, bash(&root, "cx-priya", "PreToolUse", cmd), "priya");
        assert!(out.is_none(), "{cmd} must be silent: {out:?}");
    }
}

// ----------------------------------------------------- the rest of the surface

/// Memory reaches a Codex session unasked, as it does a Claude Code one. The
/// whole point of building this natively rather than over MCP: a fact is on
/// the brief the turn the agent needs it, not behind a tool it must call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_and_a_peers_plan_reach_a_codex_turn_unasked() {
    let (sock, root, _) = scenario("brief").await;
    joins(&sock, &root, "cc-ash", "ash");
    joins(&sock, &root, "cx-priya", "priya");
    // Ash, on Claude Code, declares intent and holds a file.
    hook_with(
        &sock,
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "cc-ash",
                "cwd": root.to_string_lossy(), "prompt": "rewriting the billing rounding" }),
        "ash",
        Some("claude"),
    );
    hook_with(
        &sock,
        json!({ "hook_event_name": "PreToolUse", "session_id": "cc-ash", "cwd": root.to_string_lossy(),
                "tool_name": "Edit", "tool_input": { "file_path": format!("{}/src/billing.rs", root.to_string_lossy()) } }),
        "ash",
        Some("claude"),
    );

    let brief = prompt(&sock, &root, "cx-priya", "priya", "add invoice totals");
    assert!(brief.contains("ash"), "who is here:\n{brief}");
    assert!(brief.contains("src/billing.rs"), "and what they hold:\n{brief}");
    assert!(brief.contains("billing rounding"), "and what they are doing:\n{brief}");
}

/// Mail arrives at `Stop` as a `decision: block`, which Codex turns into a
/// new turn with the reason as its prompt — the only way to reach an agent
/// that is about to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_reaches_a_codex_session_when_it_tries_to_stop() {
    let (sock, root, _) = scenario("stop").await;
    joins(&sock, &root, "cx-priya", "priya");
    let sent = Command::new(BIN)
        .args(["msg", "priya", "auth.rs is yours now"])
        .current_dir(&root)
        .env("KNOOT_SOCK", &sock)
        .env("KNOOT_USER", "ash")
        .output()
        .unwrap();
    assert!(sent.status.success());

    let mut stop = envelope(&root, "cx-priya", "Stop");
    stop["stop_hook_active"] = json!(false);
    let out = hook_as(&sock, stop, "priya").expect("mail must interrupt the stop");
    assert_eq!(out["decision"], "block");
    assert!(out["reason"].as_str().unwrap().contains("auth.rs is yours now"));
}

/// The agent is inferred when nobody says. A payload carrying `turn_id` and
/// `apply_patch` is Codex whatever the command line said or did not say.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_codex_payload_is_recognised_without_the_flag() {
    let (sock, root, url) = scenario("infer").await;
    let out = hook_with(&sock, patch(&root, "cx-1", "PreToolUse", &update(&["src/x.rs"])), "priya", None);
    assert!(!denied(&out));
    assert!(relay_holds_claim(&url, "codex-infer", "src/x.rs").await, "the patch was understood as a write");
}

/// Codex sends `transcript_path` and `tool_response`. knoot does not open
/// either: a payload pointing at a transcript that does not exist behaves
/// identically to one that does, and a `tool_response` full of file content
/// changes nothing about what is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_transcript_and_tool_response_are_never_read() {
    let (sock, root, url) = scenario("privacy").await;
    let mut v = patch(&root, "cx-1", "PostToolUse", &update(&["src/y.rs"]));
    v["transcript_path"] = json!("/definitely/not/here.jsonl");
    v["tool_response"] = json!({ "output": "SECRET_TOKEN=abc123 and the whole file body" });
    hook_as(&sock, patch(&root, "cx-1", "PreToolUse", &update(&["src/y.rs"])), "priya");
    let out = hook_as(&sock, v, "priya");
    assert!(out.is_none(), "a post-write with nothing to say is silent: {out:?}");
    assert!(relay_holds_claim(&url, "codex-privacy", "src/y.rs").await);
    // What reached the relay names the path and nothing else.
    let base = url.replacen("ws://", "http://", 1);
    let base = base.trim_end_matches("/ws");
    let (_, v) = http("GET", &format!("{base}/api/events?repo=codex-privacy"), None, None).await;
    let log = v.to_string();
    assert!(log.contains("src/y.rs"), "the path is on the log:\n{log}");
    assert!(!log.contains("SECRET_TOKEN"), "tool output must never reach the relay:\n{log}");
    assert!(!log.contains("+b"), "a patch hunk must never reach the relay:\n{log}");
}

// ---------------------------------------------------------------- knoot init

/// `knoot init` enrols both agents by default, into the file each reads, in
/// the shape each parses — and `knoot status` reports them separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_writes_a_hooks_file_for_each_agent() {
    let root = tmp("init-both");
    let out = Command::new(BIN)
        .args(["init", "--relay", "ws://127.0.0.1:1/ws"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let codex: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".codex/hooks.json")).unwrap()).unwrap();
    let pre = &codex["hooks"]["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash|apply_patch", "Codex's canonical tool names");
    assert_eq!(pre["hooks"][0]["command"], "${KNOOT_BIN:-knoot} hook --agent codex");
    for ev in ["PostToolUse", "SessionStart", "UserPromptSubmit", "SessionEnd", "Stop"] {
        assert!(codex["hooks"][ev].is_array(), "codex missing {ev}");
    }

    let claude: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "${KNOOT_BIN:-knoot} hook --agent claude");

    // Re-running replaces rather than duplicates.
    Command::new(BIN).args(["init", "--relay", "ws://127.0.0.1:1/ws"]).current_dir(&root).output().unwrap();
    let again: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(again["hooks"]["PreToolUse"].as_array().unwrap().len(), 1, "init must be idempotent");

    let status = Command::new(BIN).arg("status").current_dir(&root).output().unwrap();
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(s.contains("hooks     claude") && s.contains("hooks     codex"), "one line per agent:\n{s}");
}

/// `--agent codex` alone leaves Claude Code's settings untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_for_one_agent_touches_only_that_agents_file() {
    let root = tmp("init-one");
    let out = Command::new(BIN)
        .args(["init", "--relay", "ws://127.0.0.1:1/ws", "--agent", "codex"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(root.join(".codex/hooks.json").is_file());
    assert!(!root.join(".claude/settings.json").exists());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("/hooks"), "init tells the user Codex needs the hooks trusted once:\n{s}");
}
