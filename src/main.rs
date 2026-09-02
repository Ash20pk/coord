use coord::{config, daemon, hook, proto, relay, watch};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::RepoConfig;
use proto::{now_ms, DReq, DResp};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "coord", version, about = "Realtime coordination for coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay (sequenced event log + claim arbitration)
    Relay {
        #[arg(long, default_value = "127.0.0.1:7420")]
        listen: String,
        #[arg(long)]
        db: Option<PathBuf>,
        /// Host live agent terminals for this repo (the browser lab at /lab)
        #[arg(long)]
        lab_dir: Option<PathBuf>,
        /// Agent identities to spawn, comma separated
        #[arg(long, default_value = "ash,priya", value_delimiter = ',')]
        agents: Vec<String>,
        /// Program each terminal runs
        #[arg(long, default_value = "claude")]
        agent_program: String,
    },
    /// Run the local daemon (claim mirror + hot-path checks)
    Daemon,
    /// Hook shim invoked by Claude Code (reads hook JSON on stdin)
    Hook,
    /// Enable coord in the current repo: write .coord.toml and install hooks
    Init {
        #[arg(long, default_value = "ws://127.0.0.1:7420/ws")]
        relay: String,
        /// Repo identifier shared by all collaborators (default: derived from git remote or path)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show active sessions and claims on this repo
    Who,
    /// Live dashboard of sessions, claims, and collisions on this repo
    Watch,
    /// Message a peer session, or everyone. Agents use this to coordinate.
    Msg {
        /// Peer user name, or "all"
        to: String,
        /// Message text
        text: Vec<String>,
    },
    /// Read and clear pending notifications for this user
    Inbox {
        /// User name (defaults to $COORD_USER, else $USER)
        #[arg(long)]
        user: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Relay { listen, db, lab_dir, agents, agent_program } => {
            let db = db.unwrap_or_else(|| dirs::home_dir().unwrap().join(".coord/relay.db"));
            let lab = lab_dir.map(|dir| relay::LabOpts { dir, agents, program: agent_program });
            relay::run(listen, db, lab).await
        }
        Cmd::Daemon => daemon::run().await,
        Cmd::Hook => {
            hook::run();
            Ok(())
        }
        Cmd::Init { relay, repo } => init(relay, repo),
        Cmd::Who => who(),
        Cmd::Msg { to, text } => msg(to, text.join(" ")),
        Cmd::Inbox { user } => inbox(user),
        Cmd::Watch => {
            let root = config::find_repo_root(&std::env::current_dir()?)
                .context("no .coord.toml found — run `coord init` first")?;
            watch::run(root).await
        }
    }
}

fn repo_root_here() -> PathBuf {
    // Prefer git root; fall back to cwd.
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .unwrap_or_else(|| std::env::current_dir().unwrap())
}

fn derive_repo_id(root: &std::path::Path) -> String {
    let remote = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let basis = remote.unwrap_or_else(|| root.to_string_lossy().to_string());
    // Human-readable slug + short hash for uniqueness.
    let slug = basis
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git")
        .to_string();
    let mut h: u64 = 1469598103934665603;
    for b in basis.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{slug}-{h:08x}")
}

fn init(relay: String, repo: Option<String>) -> Result<()> {
    let root = repo_root_here();
    let repo_id = repo.unwrap_or_else(|| derive_repo_id(&root));
    RepoConfig { relay: relay.clone(), repo: repo_id.clone() }.save(&root)?;

    // Install hooks into <root>/.claude/settings.json (merge, don't clobber).
    let exe = std::env::current_exe()?.to_string_lossy().to_string();
    let hook_cmd = format!("{exe} hook");
    let settings_path = root.join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap())?;
    let mut settings: Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));

    let hooks = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));

    let entries: &[(&str, Option<&str>)] = &[
        ("PreToolUse", Some("Write|Edit|MultiEdit|NotebookEdit|Bash")),
        ("PostToolUse", Some("Write|Edit|MultiEdit|NotebookEdit|Bash")),
        ("SessionStart", None),
        ("UserPromptSubmit", None),
        ("SessionEnd", None),
        ("Stop", None),
    ];
    for (event, matcher) in entries {
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        let arr = arr.as_array_mut().context("hook entry not an array")?;
        // Drop any previous coord entry rather than skipping: an older install
        // has an outdated matcher and would silently keep its narrower scope.
        arr.retain(|g| {
            !g["hooks"]
                .as_array()
                .map(|hs| hs.iter().any(|h| h["command"].as_str().is_some_and(|c| c.ends_with(" hook"))))
                .unwrap_or(false)
        });
        let mut group = json!({ "hooks": [{ "type": "command", "command": hook_cmd }] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        arr.push(group);
    }
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!("coord enabled for {}", root.display());
    println!("  repo id : {repo_id}");
    println!("  relay   : {relay}");
    println!("  hooks   : {}", settings_path.display());
    println!("\nNext steps:");
    println!("  1. start a relay somewhere shared:   coord relay --listen 0.0.0.0:7420");
    println!("  2. start the local daemon:           coord daemon");
    println!("  3. restart Claude Code sessions in this repo — they now coordinate.");
    Ok(())
}

fn repo_root_or_bail() -> Result<PathBuf> {
    config::find_repo_root(&std::env::current_dir()?)
        .context("no .coord.toml found — run `coord init` first")
}

/// Who this CLI invocation speaks for. Mirrors the hook's rule so a message
/// and a notification agree on identity.
fn cli_user() -> String {
    std::env::var("COORD_USER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn msg(to: String, text: String) -> Result<()> {
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send: coord msg <user|all> \"your message\"");
    }
    let root = repo_root_or_bail()?;
    let to = if to.eq_ignore_ascii_case("all") { None } else { Some(to) };
    let req = DReq::Msg {
        repo_root: root.to_string_lossy().to_string(),
        from_user: cli_user(),
        to: to.clone(),
        text,
    };
    match hook::call_daemon(&req) {
        Some(DResp::Err { msg }) => anyhow::bail!(msg),
        Some(_) => {
            println!("sent to {}", to.unwrap_or_else(|| "everyone".into()));
            Ok(())
        }
        None => anyhow::bail!("coordd not running — start it with `coord daemon`"),
    }
}

fn inbox(user: Option<String>) -> Result<()> {
    let root = repo_root_or_bail()?;
    let req = DReq::Poll {
        repo_root: root.to_string_lossy().to_string(),
        user: user.unwrap_or_else(cli_user),
    };
    match hook::call_daemon(&req) {
        Some(DResp::Mail { items }) if items.is_empty() => {
            println!("no new messages");
            Ok(())
        }
        Some(DResp::Mail { items }) => {
            for i in items {
                println!("{i}");
            }
            Ok(())
        }
        Some(DResp::Err { msg }) => anyhow::bail!(msg),
        _ => anyhow::bail!("coordd not running — start it with `coord daemon`"),
    }
}

fn who() -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .context("no .coord.toml found — run `coord init` first")?;
    let req = DReq::Who { repo_root: root.to_string_lossy().to_string() };
    let resp = hook::call_daemon(&req).context("coordd not running — start it with `coord daemon`")?;
    match resp {
        DResp::Peers { sessions, claims } => {
            if sessions.is_empty() {
                println!("no active sessions on this repo");
                return Ok(());
            }
            for s in sessions {
                let ago = (now_ms().saturating_sub(s.last_seen)) / 1000;
                let intent = if s.intent.is_empty() { "-".into() } else { format!("\"{}\"", s.intent) };
                println!("{:<12} {:<16} {:<50} {}s ago", s.user, s.branch, intent, ago);
                for c in claims.iter().filter(|c| c.session == s.session) {
                    let mins = c.lease_until.saturating_sub(now_ms()) / 60_000;
                    println!("             └─ {} (lease {}m)", c.path, mins);
                }
            }
            Ok(())
        }
        DResp::Err { msg } => anyhow::bail!(msg),
        _ => anyhow::bail!("unexpected response"),
    }
}
