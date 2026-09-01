//! The shim Claude Code invokes via hooks. Reads the hook payload on stdin,
//! consults the local daemon over the unix socket, and answers in the hook
//! output format. Every failure path exits 0 with no output: fail open.

use crate::config;
use crate::daemon;
use crate::proto::{DReq, DResp};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn run() {
    // Any panic or error must never block the agent.
    let _ = std::panic::catch_unwind(inner);
}

fn inner() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(&input) else { return };
    let event = v["hook_event_name"].as_str().unwrap_or("");
    let session = v["session_id"].as_str().unwrap_or("").to_string();
    let cwd = v["cwd"].as_str().unwrap_or(".").to_string();

    let Some(root) = config::find_repo_root(std::path::Path::new(&cwd)) else { return };
    let repo_root = root.to_string_lossy().to_string();

    let req = match event {
        "PreToolUse" | "PostToolUse" => {
            let Some(path) = file_path_of(&v) else { return };
            if event == "PreToolUse" {
                DReq::PreWrite { repo_root, session, path }
            } else {
                DReq::PostWrite { repo_root, session, path }
            }
        }
        "SessionStart" => DReq::SessionStart {
            repo_root,
            session,
            user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
            branch: git_branch(&cwd),
        },
        "UserPromptSubmit" => {
            let prompt = v["prompt"].as_str().unwrap_or("");
            if prompt.is_empty() || prompt.starts_with('/') {
                return;
            }
            let text: String = prompt.chars().take(160).collect();
            DReq::Intent { repo_root, session, text }
        }
        "SessionEnd" => DReq::SessionEnd { repo_root, session },
        _ => return,
    };

    let Some(resp) = call_daemon(&req) else { return };

    match (event, resp) {
        ("PreToolUse", DResp::Decision { allow: false, reason }) => {
            let out = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason.unwrap_or_default(),
                }
            });
            println!("{out}");
        }
        ("SessionStart", DResp::Peers { sessions, claims }) => {
            if sessions.is_empty() {
                return;
            }
            let mut ctx = format!(
                "coord: {} other active session(s) on this repo right now:\n",
                sessions.len()
            );
            for s in &sessions {
                let intent = if s.intent.is_empty() { "(no stated intent yet)".into() } else { format!("\"{}\"", s.intent) };
                let held: Vec<&str> = claims
                    .iter()
                    .filter(|c| c.session == s.session)
                    .map(|c| c.path.as_str())
                    .collect();
                ctx.push_str(&format!(
                    "- {} on branch {} — {}{}\n",
                    s.user,
                    s.branch,
                    intent,
                    if held.is_empty() { String::new() } else { format!(" [working in: {}]", held.join(", ")) },
                ));
            }
            ctx.push_str("Avoid editing files they are working in; you will be blocked with details if you try.");
            let out = json!({
                "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": ctx }
            });
            println!("{out}");
        }
        _ => {}
    }
}

fn file_path_of(v: &Value) -> Option<String> {
    let tool = v["tool_name"].as_str()?;
    if !matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    let ti = &v["tool_input"];
    ti["file_path"]
        .as_str()
        .or_else(|| ti["notebook_path"].as_str())
        .map(str::to_string)
}

fn git_branch(cwd: &str) -> String {
    std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "-".into())
}

pub fn call_daemon(req: &DReq) -> Option<DResp> {
    let sock = daemon::socket_path();
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(900))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).ok()?;
    serde_json::from_str(&resp).ok()
}
