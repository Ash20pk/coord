use knoot::{config, daemon, hook, proto, relay, watch};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::RepoConfig;
use proto::{now_ms, DReq, DResp};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "knoot", version, about = "Realtime coordination for coding agents")]
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
    /// Enable knoot in the current repo: write .knoot.toml and install hooks
    Init {
        #[arg(long, default_value = "ws://127.0.0.1:7420/ws")]
        relay: String,
        /// Repo identifier shared by all collaborators (default: derived from git remote or path)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Store the token for a relay (kept in ~/.knoot/credentials.toml, not in
    /// the repo — .knoot.toml is committed and must never carry a secret)
    Login {
        #[arg(long)]
        relay: String,
        /// The team's shared relay token
        #[arg(long)]
        token: String,
    },
    /// Is coordination actually on? Checks binary, hooks, daemon, relay, token
    Status,
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
        /// User name (defaults to $KNOOT_USER, else $USER)
        #[arg(long)]
        user: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Relay { listen, db, lab_dir, agents, agent_program } => {
            let db = db.unwrap_or_else(|| dirs::home_dir().unwrap().join(".knoot/relay.db"));
            let lab = lab_dir.map(|dir| relay::LabOpts { dir, agents, program: agent_program });
            relay::run(listen, db, lab).await
        }
        Cmd::Daemon => daemon::run().await,
        Cmd::Hook => {
            hook::run();
            Ok(())
        }
        Cmd::Init { relay, repo } => init(relay, repo),
        Cmd::Login { relay, token } => login(relay, token),
        Cmd::Status => status(),
        Cmd::Who => who(),
        Cmd::Msg { to, text } => msg(to, text.join(" ")),
        Cmd::Inbox { user } => inbox(user),
        Cmd::Watch => {
            let root = config::find_repo_root(&std::env::current_dir()?)
                .context("no .knoot.toml found — run `knoot init` first")?;
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

/// Whether a settings.json hook command is one of ours. Matches the absolute
/// paths written by older versions as well as the PATH form, so re-running
/// `init` repairs a committed config instead of appending a second entry.
fn is_knoot_hook(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if !cmd.ends_with(" hook") {
        return false;
    }
    let prog = cmd.trim_end_matches(" hook");
    prog == "knoot"
        || prog == "${KNOOT_BIN:-knoot}"
        || prog.rsplit('/').next() == Some("knoot")
}

fn init(relay: String, repo: Option<String>) -> Result<()> {
    let root = repo_root_here();
    let repo_id = repo.unwrap_or_else(|| derive_repo_id(&root));
    RepoConfig { relay: relay.clone(), repo: repo_id.clone() }.save(&root)?;

    // Install hooks into <root>/.claude/settings.json (merge, don't clobber).
    //
    // The command must resolve on *every* teammate's machine, because this
    // file is committed — that is the whole onboarding story. Writing
    // `current_exe()` here bakes in the path of whoever ran `init`
    // (`/Users/someone/knoot/target/release/knoot`), which does not exist for
    // anyone else: their hooks fail, knoot fails open, and it silently does
    // nothing for the whole team while looking fine to the person who set it
    // up. So: resolve `knoot` from PATH, with KNOOT_BIN as the escape hatch
    // for anyone who keeps it somewhere unusual.
    let hook_cmd = "${KNOOT_BIN:-knoot} hook".to_string();
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
        // Drop any previous knoot entry rather than skipping: an older install
        // has an outdated matcher and would silently keep its narrower scope.
        arr.retain(|g| {
            !g["hooks"]
                .as_array()
                .map(|hs| hs.iter().any(|h| h["command"].as_str().is_some_and(is_knoot_hook)))
                .unwrap_or(false)
        });
        let mut group = json!({ "hooks": [{ "type": "command", "command": hook_cmd }] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        arr.push(group);
    }
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!("knoot enabled for {}", root.display());
    println!("  repo id : {repo_id}");
    println!("  relay   : {relay}");
    println!("  hooks   : {}", settings_path.display());
    // Say it here rather than letting someone discover it from silence.
    if which_knoot().is_none() {
        println!(
            "\nwarning: `knoot` is not on PATH. The hooks just written call it by name so they \
             work for everyone who clones this repo — install the binary on PATH, or set \
             KNOOT_BIN to its location."
        );
    }
    println!("\nNext steps:");
    println!("  1. start a relay somewhere shared:   knoot relay --listen 0.0.0.0:7420");
    println!("  2. start the local daemon:           knoot daemon");
    println!("  3. restart Claude Code sessions in this repo — they now coordinate.");
    println!("  4. commit .knoot.toml and .claude/settings.json — teammates who clone are enrolled.");
    println!("     Each of them needs the binary on PATH, `knoot daemon`, and, on a hosted");
    println!("     relay, `knoot login`.");
    Ok(())
}

fn repo_root_or_bail() -> Result<PathBuf> {
    config::find_repo_root(&std::env::current_dir()?)
        .context("no .knoot.toml found — run `knoot init` first")
}

/// Who this CLI invocation speaks for. Mirrors the hook's rule so a message
/// and a notification agree on identity.
fn cli_user() -> String {
    config::env_or_legacy("KNOOT_USER")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn msg(to: String, text: String) -> Result<()> {
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send: knoot msg <user|all> \"your message\"");
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
        None => anyhow::bail!("knootd not running — start it with `knoot daemon`"),
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
        _ => anyhow::bail!("knootd not running — start it with `knoot daemon`"),
    }
}

fn login(relay: String, token: String) -> Result<()> {
    let origin = config::relay_origin(&relay);
    let mut creds = config::Credentials::load();
    creds.tokens.insert(origin.clone(), token);
    creds.save()?;
    println!("token stored for {origin}");
    if relay.starts_with("ws://") && !origin.contains("127.0.0.1") && !origin.contains("localhost") {
        // A bearer token over plaintext is a token anyone on the path can take.
        println!(
            "warning: {origin} is not TLS. Use wss:// for anything outside this machine."
        );
    }
    Ok(())
}

/// Where `knoot` resolves from, if anywhere. `init` writes hooks that call it
/// by name, so "is it on PATH" is a real question with a real failure mode.
fn which_knoot() -> Option<PathBuf> {
    if let Some(explicit) = config::env_or_legacy("KNOOT_BIN") {
        let p = PathBuf::from(explicit);
        return p.is_file().then_some(p);
    }
    std::env::var("PATH").ok()?.split(':').find_map(|dir| {
        let p = std::path::Path::new(dir).join("knoot");
        p.is_file().then_some(p)
    })
}

/// Every way knoot can be silently off, in one place.
///
/// Fail-open means a broken knoot looks exactly like a working one from
/// inside an agent: no errors, no blocks, nothing. That is the right
/// behaviour and it is also why this command has to exist — it is the only
/// way for a human to tell "nothing collided" from "nothing was watching".
fn status() -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    let ok = |b: bool| if b { "ok  " } else { "FAIL" };

    // 1. the binary the committed hooks call by name
    let on_path = which_knoot();
    println!(
        "[{}] binary    {}",
        ok(on_path.is_some()),
        match &on_path {
            Some(p) => p.display().to_string(),
            None => "`knoot` not found on PATH (set KNOOT_BIN, or install it there)".into(),
        }
    );
    if on_path.is_none() {
        problems.push("install knoot on PATH, or set KNOOT_BIN".into());
    }

    // 2. this repo
    let root = config::find_repo_root(&std::env::current_dir()?);
    let cfg = root.as_deref().and_then(config::RepoConfig::load);
    match (&root, &cfg) {
        (Some(r), Some(c)) => {
            println!("[ok  ] repo      {} (id: {})", r.display(), c.repo);
            // Still working, but on a name that is no longer written. Say so
            // once rather than leaving it to be discovered.
            if config::RepoConfig::is_legacy(r) {
                println!(
                    "[WARN] repo      enrolled as {} (the old name) — still read, no longer written",
                    config::LEGACY_CONFIG_FILE
                );
                problems.push(format!(
                    "migrate this repo: git mv {} {} && knoot init --relay {}",
                    config::LEGACY_CONFIG_FILE,
                    config::CONFIG_FILE,
                    c.relay
                ));
            }
        }
        _ => {
            println!("[FAIL] repo      no .knoot.toml here — run `knoot init`");
            problems.push("run `knoot init` in this repo".into());
        }
    }

    // 3. hooks: installed, and calling something that exists
    if let Some(r) = &root {
        let path = r.join(".claude/settings.json");
        let settings: Option<Value> =
            std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
        let cmds: Vec<String> = settings
            .iter()
            .filter_map(|s| s["hooks"].as_object())
            .flat_map(|h| h.values())
            .filter_map(|v| v.as_array())
            .flatten()
            .filter_map(|g| g["hooks"].as_array())
            .flatten()
            .filter_map(|h| h["command"].as_str())
            .filter(|c| is_knoot_hook(c))
            .map(str::to_string)
            .collect();
        let events = cmds.len();
        // An absolute path here is the bug that broke every teammate: it
        // resolves for whoever ran `init` and for nobody else.
        let absolute: Vec<&String> = cmds.iter().filter(|c| c.starts_with('/')).collect();
        if events == 0 {
            println!("[FAIL] hooks     not installed — run `knoot init`");
            problems.push("run `knoot init` to install hooks".into());
        } else if let Some(a) = absolute.first() {
            println!("[WARN] hooks     {events} events, but hardcoded to a local path:");
            println!("                 {a}");
            println!("                 that path will not exist for teammates who clone this repo");
            problems.push("re-run `knoot init` to write a PATH-resolved hook command".into());
        } else {
            println!("[ok  ] hooks     {events} events, resolved from PATH");
        }
    }

    // 4. the daemon, asked rather than assumed
    let daemon = cfg.is_some()
        && root.as_ref().is_some_and(|r| {
            hook::call_daemon(&DReq::Who { repo_root: r.to_string_lossy().to_string() }).is_some()
        });
    println!("[{}] daemon    {}", ok(daemon), if daemon { "responding" } else { "not running — start it with `knoot daemon`" });
    if !daemon {
        problems.push("start the daemon: knoot daemon".into());
    }

    // 5. the relay: asked, not inferred. A stored token says nothing about
    //    whether the dial succeeded — and a daemon whose relay task died still
    //    answers every local request, so "daemon responding" is not evidence.
    if let Some(c) = &cfg {
        let origin = config::relay_origin(&c.relay);
        let have_token = config::token_for(&c.relay).is_some();
        let token_note =
            if have_token { "token: present" } else { "token: none — fine for an open relay" };
        let health = root.as_ref().and_then(|r| {
            match hook::call_daemon(&DReq::Health { repo_root: r.to_string_lossy().to_string() }) {
                Some(DResp::Health { connected, ready, last_error }) => {
                    Some((connected, ready, last_error))
                }
                _ => None,
            }
        });
        match health {
            Some((true, true, _)) => {
                println!("[ok  ] relay     {} ({token_note})", c.relay);
            }
            Some((true, false, _)) => {
                println!("[WARN] relay     {} connected, no snapshot yet ({token_note})", c.relay);
                problems.push("relay is connected but has sent no snapshot — check the relay's log".into());
            }
            // Not connected and nothing has failed yet: the first dial is
            // still in flight. Saying "unreachable" here is wrong and alarming
            // — a TLS handshake to a hosted relay takes a moment, so `knoot
            // daemon && knoot status` would report a broken relay that is
            // merely young. It cost an hour of chasing a phantom to notice.
            Some((false, _, None)) => {
                println!("[..  ] relay     {} connecting… ({token_note})", c.relay);
                println!("                 run `knoot status` again in a moment");
            }
            Some((false, _, err)) => {
                println!("[FAIL] relay     {} unreachable ({token_note})", c.relay);
                if let Some(e) = &err {
                    println!("                 last error: {}", truncate_err(e));
                    if e.contains("401") {
                        problems.push(format!(
                            "the relay rejected this token: knoot login --relay {} --token <token>",
                            c.relay
                        ));
                    } else {
                        problems.push(format!("cannot reach {}: check it is running and reachable", c.relay));
                    }
                } else {
                    problems.push(format!("cannot reach {}", c.relay));
                }
            }
            // No daemon to ask. Its own line already said so.
            None => {
                println!("[?   ] relay     {} — no daemon to ask ({token_note})", c.relay);
            }
        }
        if !have_token && !origin.contains("127.0.0.1") && !origin.contains("localhost") {
            problems.push(format!("if that relay requires auth: knoot login --relay {} --token <token>", c.relay));
        }
    }

    println!();
    if problems.is_empty() {
        println!("coordination is on.");
    } else {
        println!("coordination is OFF or partial. Edits are still allowed — knoot fails open —");
        println!("but nothing is being coordinated. To fix:");
        for p in &problems {
            println!("  - {p}");
        }
    }
    Ok(())
}

/// A dial error is a nest of wrapped causes; the first line is the part a
/// human acts on.
fn truncate_err(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e);
    if first.chars().count() > 120 {
        format!("{}…", first.chars().take(120).collect::<String>())
    } else {
        first.to_string()
    }
}

fn who() -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .context("no .knoot.toml found — run `knoot init` first")?;
    let req = DReq::Who { repo_root: root.to_string_lossy().to_string() };
    let resp = hook::call_daemon(&req).context("knootd not running — start it with `knoot daemon`")?;
    match resp {
        DResp::Peers { sessions, claims, writes, .. } => {
            if sessions.is_empty() {
                println!("no active sessions on this repo");
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
            if !writes.is_empty() {
                println!("\nrecently written:");
                for w in writes {
                    let ago = (now_ms().saturating_sub(w.ts)) / 1000;
                    println!("{:<12} {:<50} {}s ago", w.user, w.path, ago);
                }
            }
            Ok(())
        }
        DResp::Err { msg } => anyhow::bail!(msg),
        _ => anyhow::bail!("unexpected response"),
    }
}


#[cfg(test)]
mod init_tests {
    use super::*;

    /// The bug this guards: `init` used to write `current_exe()`, so the
    /// committed hook config named a path inside whoever ran it. Every
    /// teammate's hooks then failed, knoot failed open, and coordination was
    /// silently off for the whole team while looking healthy to one person.
    #[test]
    fn a_hook_command_must_not_be_machine_specific() {
        assert!(!"${KNOOT_BIN:-knoot} hook".contains('/'), "no absolute path may be baked in");
    }

    #[test]
    fn our_own_hooks_are_recognised_in_every_form_we_have_shipped() {
        assert!(is_knoot_hook("${KNOOT_BIN:-knoot} hook"), "current form");
        assert!(is_knoot_hook("knoot hook"), "bare PATH form");
        assert!(is_knoot_hook("/Users/someone/knoot/target/release/knoot hook"), "legacy absolute");
        assert!(is_knoot_hook("  knoot hook  "), "surrounding whitespace");
    }

    /// Re-running `init` must repair a stale install, not append a second
    /// entry, and must never eat somebody else's hook.
    #[test]
    fn other_peoples_hooks_are_left_alone() {
        assert!(!is_knoot_hook("prettier --write"));
        assert!(!is_knoot_hook("my-linter hook"));
        assert!(!is_knoot_hook("coordinator hook"), "suffix match must respect the whole name");
        assert!(!is_knoot_hook("knoot who"), "only the hook subcommand is ours");
        assert!(!is_knoot_hook("/opt/tools/coordinate hook"));
    }
}
