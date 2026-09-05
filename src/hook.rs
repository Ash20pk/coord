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

    let tool = v["tool_name"].as_str().unwrap_or("");
    let req = match event {
        "PreToolUse" | "PostToolUse" if tool == "Bash" => {
            // Shell writes must be gated too, or the whole scheme is optional:
            // agents reach for sed/heredocs as readily as the Edit tool.
            if event == "PreToolUse" {
                let command = v["tool_input"]["command"].as_str().unwrap_or("").to_string();
                if command.is_empty() {
                    return;
                }
                DReq::BashPre { repo_root, session, command }
            } else {
                DReq::BashPost { repo_root, session }
            }
        }
        // A read is not a write and is never gated — but it is half of a
        // conflict. STORM's largest single result is that a write is stale
        // when what the agent *read* has changed, even where the file being
        // written is untouched, so the daemon needs to know what this session
        // has looked at.
        "PostToolUse" if matches!(tool, "Read" | "NotebookRead") => {
            let Some(path) = read_path_of(&v) else { return };
            DReq::FileRead { repo_root, session, path }
        }
        "PreToolUse" | "PostToolUse" => {
            let Some(path) = file_path_of(&v) else { return };
            if event == "PreToolUse" {
                // `Write` replaces a file whole; `Edit` changes part of one.
                // Creating a path a peer created a minute ago is a different
                // failure from editing it, and only the client knows which
                // tool is about to run.
                DReq::PreWrite { repo_root, session, path, creating: tool == "Write" }
            } else {
                DReq::PostWrite { repo_root, session, path }
            }
        }
        "SessionStart" => DReq::SessionStart {
            repo_root,
            session,
            user: session_user(),
            branch: git_branch(&cwd),
        },
        "UserPromptSubmit" => {
            let prompt = v["prompt"].as_str().unwrap_or("");
            if prompt.is_empty() || prompt.starts_with('/') {
                return;
            }
            let text: String = prompt.chars().take(160).collect();
            // Branch travels every turn, not just at SessionStart: a session
            // that checks out a new branch must claim under the new one.
            DReq::Intent { repo_root, session, text, user: session_user(), branch: git_branch(&cwd) }
        }
        "SessionEnd" => DReq::SessionEnd { repo_root, session },
        // The moment an agent tries to finish is the only reliable chance to
        // hand it something that arrived while it was working.
        "Stop" | "SubagentStop" => DReq::StopCheck {
            repo_root,
            user: session_user(),
            already_continued: v["stop_hook_active"].as_bool().unwrap_or(false),
        },
        _ => return,
    };

    let Some(resp) = call_daemon(&req) else { return };

    match (event, resp) {
        ("PreToolUse", DResp::Decision { allow: false, reason, notes }) => {
            // Advisory lines ride on the denial. It is the highest-attention
            // surface in the product: the agent is stopped, reading, and about
            // to re-plan, which is exactly when "the file you read has moved"
            // is worth something.
            let mut why = reason.unwrap_or_default();
            for n in &notes {
                why.push('\n');
                why.push_str(n);
            }
            let out = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": why,
                }
            });
            println!("{out}");
        }
        // Allowed, with something worth saying. Deliberately *not* a
        // `permissionDecision: allow`: that would auto-approve the write and
        // override whatever the human configured about confirming edits. The
        // note goes in as context and the tool call takes its normal course.
        ("PreToolUse", DResp::Decision { allow: true, notes, .. }) if !notes.is_empty() => {
            let out = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": notes.join("\n"),
                }
            });
            println!("{out}");
        }
        ("Stop", DResp::Mail { items }) | ("SubagentStop", DResp::Mail { items }) => {
            if items.is_empty() {
                return;
            }
            // `block` sends the agent back to work with these notes, which is
            // what makes a release notification arrive in real time rather
            // than whenever the human next types.
            let out = json!({
                "decision": "block",
                "reason": format!(
                    "{}\n\nAct on this if it affects your task; otherwise say so briefly and stop.",
                    items.join("\n")
                ),
            });
            println!("{out}");
        }
        ("PostToolUse", DResp::Mail { items }) => {
            if items.is_empty() {
                return;
            }
            let out = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": items.join("\n"),
                }
            });
            println!("{out}");
        }
        ("SessionStart", DResp::Peers { sessions, claims, writes, mail, notes, depended_on, memory, cached, context })
        | ("UserPromptSubmit", DResp::Peers { sessions, claims, writes, mail, notes, depended_on, memory, cached, context }) => {
            // Everything here arrives unasked. A capable model will run
            // `knoot who` and read its messages; a cheap one demonstrably
            // will not, even when told to — so nothing an agent needs to
            // coordinate may sit behind a command it has to think of.
            let mut ctx = String::new();

            // Mail first: it is the only part addressed to this agent.
            if !mail.is_empty() {
                ctx.push_str("knoot: messages for you\n");
                for m in &mail {
                    ctx.push_str(&format!("- {m}\n"));
                }
                ctx.push('\n');
            }

            // Then anything advisory the daemon worked out for this turn: a
            // peer on the same task, a file this session read that has since
            // moved or gone.
            if !notes.is_empty() {
                ctx.push_str("knoot: worth knowing before you start\n");
                for n in &notes {
                    ctx.push_str(&format!("- {n}\n"));
                }
                ctx.push('\n');
            }

            // What peers are *doing*, from the plans they published on
            // purpose. First of the memory sections, and above the peer list:
            // knowing somebody is mid-way through the design you were about to
            // choose changes the turn, where knowing they hold a file only
            // changes the order you do things in.
            if !context.is_empty() {
                ctx.push_str("knoot: what your peers are doing right now\n");
                for c in &context {
                    ctx.push_str(&format!("- {c}\n"));
                }
                ctx.push_str(
                    "Do not redo or design against these. If one overlaps your task, say so \
                     with knoot msg before you start.\n\n",
                );
            }

            // What the team has learned about the code this session is in.
            // After mail and advisories — those are about right now — and
            // before the peer writes, because a fact is the thing most likely
            // to change what the agent does rather than how it does it.
            if !memory.is_empty() {
                ctx.push_str("knoot: what this team already knows here\n");
                for m in &memory {
                    ctx.push_str(&format!("- {m}\n"));
                }
                ctx.push_str(
                    "These were written by your teammates. A line marked stale names who \
                     changed the file since; check it before you rely on it.\n\n",
                );
            }

            // Derived knowledge somebody already worked out. Anything whose
            // files have moved has been dropped upstream rather than flagged:
            // it was mechanical, it is now wrong, and it is cheap to redo.
            if !cached.is_empty() {
                ctx.push_str("knoot: already worked out here, so you need not\n");
                for c in &cached {
                    ctx.push_str(&format!("- {c}\n"));
                }
                ctx.push('\n');
            }

            if !writes.is_empty() {
                ctx.push_str("knoot: changed under you since your last turn\n");
                // Writes to files this session actually read come first and
                // are marked. "The ground moved" matters most where the agent
                // was standing, and until now the brief could not tell the
                // difference between a file it had reasoned about and one it
                // had never opened.
                let mut ordered: Vec<&crate::proto::PeerWrite> = writes.iter().collect();
                ordered.sort_by_key(|w| !depended_on.contains(&w.path));
                for w in ordered {
                    let mine = depended_on.contains(&w.path);
                    ctx.push_str(&format!(
                        "- {} wrote {}{}\n",
                        w.user,
                        w.path,
                        if mine { "  ← you read this one; re-read it before you rely on it" } else { "" }
                    ));
                }
                ctx.push_str(
                    "Re-read any of these you had already read; your notes on them may be stale.\n\n",
                );
            }

            if !sessions.is_empty() {
                ctx.push_str(&format!(
                    "knoot: {} other active session(s) on this repo right now:\n",
                    sessions.len()
                ));
                let mine = git_branch(&cwd);
                for s in &sessions {
                    let intent = if s.intent.is_empty() {
                        "(no stated intent yet)".into()
                    } else {
                        format!("\"{}\"", s.intent)
                    };
                    // A peer on another branch is not in the way; their work
                    // and ours meet at merge instead, which is worth knowing
                    // now and cannot be learned from git yet.
                    let elsewhere = !crate::proto::same_branch(&s.branch, &mine);
                    let held: Vec<&str> = claims
                        .iter()
                        .filter(|c| c.session == s.session)
                        .map(|c| c.path.as_str())
                        .collect();
                    // A person in an editor is a different kind of peer: they
                    // cannot be told to stop and re-plan, so the only useful
                    // instruction is to work somewhere else.
                    let human = crate::proto::is_human_session(&s.session);
                    ctx.push_str(&format!(
                        "- {}{} on branch {} — {}{}{}\n",
                        s.user,
                        if human { " (a person in an editor, not an agent)" } else { "" },
                        s.branch,
                        intent,
                        if held.is_empty() {
                            String::new()
                        } else {
                            format!(" [working in: {}]", held.join(", "))
                        },
                        if elsewhere && !held.is_empty() {
                            "  (different branch: you will not be blocked, but these files meet yours at merge)"
                        } else {
                            ""
                        },
                    ));
                }
                ctx.push_str(
                    "Avoid editing files they are working in; you will be blocked with details if \
                     you try, and told automatically when a file you were blocked on is released.\n",
                );
                if sessions.iter().any(|s| crate::proto::is_human_session(&s.session)) {
                    ctx.push_str(
                        "One of them is a person, not an agent. Do not ask them to release a \
                         file and do not wait on them — pick different work.\n",
                    );
                }
            }

            if ctx.is_empty() {
                return; // nothing to say; do not spend the agent's attention
            }

            // The one thing that still needs asking for. Kept last and kept
            // short: the rest of this context arrived on its own.
            ctx.push_str(
                "To reply or to tell peers you have finished something they are waiting on: \
                 knoot msg <user|all> \"text\"\n\
                 To tell them what you are about to do, so nobody duplicates it: \
                 knoot plan --path <file> \"what you are doing\"",
            );

            let out = json!({
                "hookSpecificOutput": { "hookEventName": event, "additionalContext": ctx }
            });
            println!("{out}");
        }
        _ => {}
    }
}

/// The path a read tool looked at. Separate from `file_path_of` because the
/// set of tools is different and a read must never be mistaken for a write.
fn read_path_of(v: &Value) -> Option<String> {
    let ti = &v["tool_input"];
    ti["file_path"]
        .as_str()
        .or_else(|| ti["notebook_path"].as_str())
        .map(str::to_string)
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

/// Who this session belongs to. KNOOT_USER lets several sessions on one
/// machine carry distinct identities (useful for testing, demos, and shared
/// boxes); otherwise fall back to the OS user.
fn session_user() -> String {
    crate::config::env_or_legacy("KNOOT_USER")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".into())
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
    call_daemon_at(&daemon::socket_path(), req)
}

/// Talk to a daemon at an explicit socket path. Returns None on any failure —
/// every caller treats None as "allow" (fail open).
pub fn call_daemon_at(sock: &std::path::Path, req: &DReq) -> Option<DResp> {
    let mut stream = UnixStream::connect(sock).ok()?;
    // Must exceed the daemon's own worst case (cold-start wait + claim timeout)
    // so we get its explicit fail-open verdict rather than timing out blind.
    stream.set_read_timeout(Some(Duration::from_millis(1_500))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).ok()?;
    serde_json::from_str(&resp).ok()
}
