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
    /// Join a team on this machine with a device key, and print who it says
    /// you are: team, member, rooms and areas. Same store as `login`; this one
    /// asks the relay to confirm the key before saying it worked.
    Join {
        /// The device key, as the console showed it once (`knt_…`)
        key: String,
        /// The relay to join. Defaults to the one this repo is configured for.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Is coordination actually on? Checks binary, hooks, daemon, relay, token
    Status,
    /// Show active sessions and claims on this repo
    Who,
    /// Join the room as a person, from any editor.
    ///
    /// Claude Code sessions announce themselves through hooks. A teammate in
    /// VS Code, or an agent under another tool, does not — so knoot cannot see
    /// them and nor can anybody else's agent. This watches the working tree
    /// and registers what you touch, so you appear in `knoot who`, agents are
    /// told to stay out of files you are in, and you are told when a file you
    /// wanted is free.
    Present {
        /// What to call this session locally — it is how your mailbox is
        /// keyed and how your row is told apart from your agent's. The name
        /// teammates *see* comes from your device key, not from here.
        #[arg(long = "as")]
        name: Option<String>,
        /// What you are working on, so peers' agents know before they collide.
        #[arg(long)]
        doing: Option<String>,
        /// How often to look, in seconds.
        #[arg(long, default_value_t = 3)]
        interval: u64,
    },
    /// Live dashboard of sessions, claims, and collisions on this repo
    Watch,
    /// Message a peer session, or everyone. Agents use this to coordinate.
    Msg {
        /// Peer user name, or "all"
        to: String,
        /// Message text
        text: Vec<String>,
    },
    /// Add, list or remove the people on this team, and mint their keys.
    ///
    /// The console does this too, but only against a relay attached to
    /// Supabase — inviting by email is a cloud feature. On a self-hosted
    /// relay this is the way in.
    #[command(subcommand)]
    Member(MemberCmd),
    /// Show how this repo divides itself into areas, or import that division
    /// from CODEOWNERS.
    Areas {
        /// Read CODEOWNERS and write the areas it implies into .knoot.toml.
        /// Prints what it would do and changes nothing without --write.
        #[arg(long)]
        import_codeowners: bool,
        /// Actually write .knoot.toml.
        #[arg(long)]
        write: bool,
    },
    /// Publish a fact into this repo's shared memory: an interface decision,
    /// a convention, a gotcha. Teammates' agents are told about it on the turn
    /// they touch the code it names, without asking.
    Remember {
        /// The handle this fact is known by. Writing the same name again
        /// supersedes the earlier statement rather than standing beside it,
        /// which is how a contradiction becomes a chain.
        #[arg(long)]
        name: String,
        /// The paths this fact is about. What makes it more than a note: a
        /// write to one of these marks the fact possibly stale, and naming
        /// one surfaces the fact when a peer opens it.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Take the text from a file instead of the command line — through
        /// exactly the same refusals.
        #[arg(long)]
        from: Option<PathBuf>,
        /// The fact itself.
        text: Vec<String>,
    },
    /// Tell peers in this area what you are doing, at a depth the prompt
    /// cannot reach. Their next turn says so, without them asking. Lives as
    /// long as the session and is deleted when it ends.
    Plan {
        /// The files this plan is about.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Something you have settled, so a peer does not re-open it.
        /// Repeatable.
        #[arg(long = "decided")]
        decisions: Vec<String>,
        /// What you are doing.
        text: Vec<String>,
    },
    /// Cache something you had to work out — where a symbol lives, how the
    /// tests run, what a module does — so the next agent does not repeat it.
    /// Dropped automatically once the files it came from change.
    Cache {
        #[arg(long)]
        name: String,
        /// What it was derived from. This is what invalidates it.
        #[arg(long = "path")]
        paths: Vec<String>,
        text: Vec<String>,
    },
    /// What this repo's memory holds. A word or two filters it.
    Recall {
        query: Vec<String>,
    },
    /// Why is this file like this? The log, read back as one file's story:
    /// who claimed it and why, who was blocked, what they said to each other,
    /// who wrote it, and what the team has since decided about it.
    Why {
        /// The file to explain.
        path: String,
        /// How far back to look, in events.
        #[arg(long, default_value_t = 400)]
        limit: usize,
        #[arg(long)]
        relay: Option<String>,
    },
    /// Read and clear pending notifications for this user
    Inbox {
        /// User name (defaults to $KNOOT_USER, else $USER)
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
enum MemberCmd {
    /// Add a colleague and mint their first device key. Prints the key once —
    /// there is nowhere it can be read from again.
    Add {
        email: String,
        /// Let them manage members, rooms and other people's keys.
        #[arg(long)]
        admin: bool,
        /// What to call the machine the key is for.
        #[arg(long, default_value = "first machine")]
        label: String,
        /// Create the person without a key, for someone who will sign in to
        /// the console instead.
        #[arg(long)]
        no_key: bool,
        #[arg(long)]
        relay: Option<String>,
    },
    /// Who is on this team, and which machines they hold.
    Ls {
        #[arg(long)]
        relay: Option<String>,
    },
    /// Remove someone: their keys stop working, their memory goes, and
    /// nobody else is touched.
    Rm {
        email: String,
        #[arg(long)]
        relay: Option<String>,
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
        Cmd::Member(cmd) => member(cmd).await,
        Cmd::Areas { import_codeowners, write } => areas(import_codeowners, write),
        Cmd::Remember { name, paths, from, text } => remember(name, paths, from, text.join(" ")),
        Cmd::Plan { paths, decisions, text } => plan(paths, decisions, text.join(" ")),
        Cmd::Cache { name, paths, text } => cache(name, paths, text.join(" ")),
        Cmd::Recall { query } => recall(query.join(" ")),
        Cmd::Why { path, limit, relay } => why(path, limit, relay).await,
        Cmd::Daemon => daemon::run().await,
        Cmd::Hook => {
            hook::run();
            Ok(())
        }
        Cmd::Init { relay, repo } => init(relay, repo),
        Cmd::Login { relay, token } => login(relay, token),
        Cmd::Join { key, relay } => join(key, relay).await,
        Cmd::Status => status(),
        Cmd::Who => who(),
        Cmd::Present { name, doing, interval } => present(name, doing, interval).await,
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
    RepoConfig { relay: relay.clone(), repo: repo_id.clone(), hubs: Vec::new(), areas: Vec::new() }.save(&root)?;

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

/// Join a team on this machine.
///
/// `login` stores a token and believes it. That was honest when a token named
/// only a team and there was nothing to report back; with devices there is —
/// which person, which rooms, which areas — and a key that does not resolve is
/// worth finding out about now rather than as a silent 401 in the daemon's log
/// an hour later.
///
/// The credential is written either way. A relay that is merely unreachable
/// must not stop someone setting up their laptop on a train.
async fn join(key: String, relay: Option<String>) -> Result<()> {
    let relay = match relay {
        Some(r) => r,
        None => {
            let root = repo_root_here();
            RepoConfig::load(&root)
                .map(|c| c.relay)
                .context("no --relay given and this directory is not a knoot repo — run `knoot init` first, or pass --relay")?
        }
    };
    let origin = config::relay_origin(&relay);
    let mut creds = config::Credentials::load();
    creds.tokens.insert(origin.clone(), key.trim().to_string());
    creds.save()?;
    println!("key stored for {origin}");
    if relay.starts_with("ws://") && !origin.contains("127.0.0.1") && !origin.contains("localhost") {
        println!("warning: {origin} is not TLS. Use wss:// for anything outside this machine.");
    }

    let http = origin.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    let who: Value = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .get(format!("{http}/api/whoami"))
        .bearer_auth(key.trim())
        .send()
        .await
    {
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
            println!();
            println!("the relay refused that key. It is stored anyway, so nothing is lost —");
            println!("but coordination will be off until it is replaced. Ask whoever runs the");
            println!("console for a new device key, then run this again.");
            return Ok(());
        }
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            println!();
            println!("could not reach {http} to confirm the key ({}).", truncate_err(&e.to_string()));
            println!("The key is stored; run `knoot status` once the relay is reachable.");
            return Ok(());
        }
    };

    let me = &who["me"];
    println!();
    println!("team    {}", who["team"].as_str().unwrap_or("-"));
    println!("member  {}{}", me["email"].as_str().unwrap_or("-"),
        if me["unassigned"] == Value::Bool(true) { "  (unassigned — ask an admin to attach this key to you)" } else { "" });
    let rooms: Vec<&str> = who["rooms"].as_array().map(|a| a.iter().filter_map(|r| r.as_str()).collect()).unwrap_or_default();
    println!("rooms   {}", if rooms.is_empty() { "none".into() } else { rooms.join(", ") });
    let areas: Vec<String> = me["areas"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| {
                    let repo = x["repo"].as_str().unwrap_or("?");
                    let area = x["area"].as_str().unwrap_or("/");
                    if repo == "*" && area == "/" { "every repo".to_string() } else { format!("{repo}:{area}") }
                })
                .collect()
        })
        .unwrap_or_default();
    println!("areas   {}", if areas.is_empty() { "none — you are in no room, so nothing will coordinate".into() } else { areas.join(", ") });
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
    // Neither on nor broken: the first relay dial has not finished. Kept apart
    // from `problems` because there is nothing for the reader to fix.
    let mut still_dialing = false;
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
            // Which subtrees this repo coordinates by. Silent when there are
            // none, because one area is the normal shape and a line saying so
            // would be noise in every small team's status.
            if !c.areas.is_empty() {
                println!(
                    "[ok  ] areas     {}",
                    c.areas
                        .iter()
                        .map(|a| format!("{} ({})", a.name, a.paths.join(" ")))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
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
                // What memory this repo actually has. A quiet install and a
                // working one with nothing to say look identical from inside
                // an agent, which is the whole reason this command exists.
                if let Some(DResp::Memory { items, unreadable, provider, ready, identified }) =
                    root.as_ref().and_then(|r| {
                        hook::call_daemon(&DReq::Recall {
                            repo_root: r.to_string_lossy().to_string(),
                            query: String::new(),
                        })
                    })
                {
                    println!("[ok  ] memory    {} fact(s), provider {provider}", items.len());
                    if !identified {
                        // The failure that cost a whole lab run: "0 facts"
                        // reads as an empty room, and the truth was that
                        // nothing could ever be written.
                        println!(
                            "[WARN] memory    read-only: this key names no verified person, so \
                             nothing can be published"
                        );
                        problems.push(
                            "to publish memory, register a team and run `knoot join <key>`".into(),
                        );
                    } else if !ready {
                        println!(
                            "[WARN] memory    waiting for this room's key — nothing can be \
                             published here yet"
                        );
                    }
                    if unreadable > 0 {
                        println!(
                            "[WARN] memory    {unreadable} shard(s) unreadable — written under a \
                             key epoch this machine does not hold"
                        );
                    }
                }
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
                still_dialing = true;
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
    if problems.is_empty() && still_dialing {
        // Saying "coordination is on" here would be a guess, and saying it is
        // off would be a false alarm. The honest answer is that we do not know
        // yet, and when we will.
        println!("coordination is starting — the relay dial is still in flight.");
        println!("Run `knoot status` again in a moment; nothing here needs fixing.");
    } else if problems.is_empty() {
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

/// `knoot areas` — what this repo's subtrees are, and where they came from.
///
/// Importing is two steps on purpose. A CODEOWNERS file is written for review
/// routing, not for coordination, and the areas it implies are a proposal a
/// human should read before the whole team starts coordinating by them.
fn areas(import_codeowners: bool, write: bool) -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("not inside a knoot repo (no .knoot.toml found)"))?;
    let mut cfg = config::RepoConfig::load(&root)
        .ok_or_else(|| anyhow::anyhow!("could not read .knoot.toml"))?;

    if import_codeowners {
        let found = config::areas_from_codeowners(&root);
        if found.is_empty() {
            println!("no CODEOWNERS, or none of its patterns name a plain subtree");
            return Ok(());
        }
        for a in &found {
            println!("{:<20} {}", a.name, a.paths.join(", "));
        }
        if write {
            cfg.areas = found;
            cfg.save(&root)?;
            println!("\nwritten to {}", root.join(config::CONFIG_FILE).display());
            println!("commit it: every collaborator must divide the repo the same way");
        } else {
            println!("\nnothing written. Re-run with --write to put these in .knoot.toml");
        }
        return Ok(());
    }

    if cfg.areas.is_empty() {
        println!("no areas declared: the whole repo is one area, `/`");
        println!("declare them in .knoot.toml, or run: knoot areas --import-codeowners");
    } else {
        for a in &cfg.areas {
            println!("{:<20} {}", a.name, a.paths.join(", "));
        }
    }
    Ok(())
}

/// `knoot remember` — publish a fact.
///
/// A person types this; so does an agent, when it has decided something worth
/// a teammate knowing. Nothing is ever published that was not written on
/// purpose, in this shape: a free-text conclusion derived from a transcript is
/// an exfiltration path with no reviewer.
fn remember(name: String, paths: Vec<String>, from: Option<PathBuf>, text: String) -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("not inside a knoot repo (no .knoot.toml found)"))?;
    anyhow::ensure!(
        !text.trim().is_empty() || from.is_some(),
        "a fact needs some text, or --from <path>"
    );
    let req = DReq::Remember {
        repo_root: root.to_string_lossy().to_string(),
        session: session_key(),
        user: cli_user(),
        name: name.clone(),
        text,
        paths,
        from: from.map(|p| p.to_string_lossy().to_string()),
    };
    match hook::call_daemon(&req) {
        Some(DResp::Memory { .. }) => {
            println!("remembered: {name}");
            println!("your teammates' agents will be told when they touch what it names");
            Ok(())
        }
        // A refusal is the answer, not an error in the machinery, so it is
        // printed as what it is.
        Some(DResp::Err { msg }) => {
            println!("not published: {msg}");
            Ok(())
        }
        _ => {
            println!("knoot: daemon not reachable — nothing published");
            Ok(())
        }
    }
}

/// `knoot member` — the team's people, from a terminal.
///
/// Every call is an admin call against the relay's HTTP API with this
/// machine's stored key, so it works the same whether or not the relay has a
/// Supabase behind it.
async fn member(cmd: MemberCmd) -> Result<()> {
    let (relay, base, key) = match &cmd {
        MemberCmd::Add { relay, .. } | MemberCmd::Ls { relay, .. } | MemberCmd::Rm { relay, .. } => {
            admin_target(relay.clone())?
        }
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;

    match cmd {
        MemberCmd::Add { email, admin, label, no_key, .. } => {
            let mut body = serde_json::json!({
                "email": email,
                "role": if admin { "admin" } else { "member" },
            });
            if !no_key {
                body["label"] = serde_json::Value::String(label);
            }
            let r = http
                .post(format!("{base}/api/members"))
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await?;
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or_default();
            if !ok {
                println!("could not add {email}: {}", v["error"].as_str().unwrap_or("unknown"));
                return Ok(());
            }
            if v["existing"].as_bool() == Some(true) {
                println!("{email} is already on this team as {}", v["role"].as_str().unwrap_or("?"));
                println!("to give them another machine: knoot member add is not it — use the");
                println!("console's Agent keys, or `knoot join` on the machine itself");
                return Ok(());
            }
            println!("added {email} as {}", v["role"].as_str().unwrap_or("member"));
            if let Some(token) = v["token"].as_str() {
                println!();
                println!("their device key — shown once, and not recoverable:");
                println!();
                println!("  {token}");
                println!();
                // The ws:// URL, not the http:// one this command talks to:
                // credentials are keyed by relay origin, and a key stored
                // under `http://…` is a key the daemon never finds.
                println!("on their machine: knoot join {token} --relay {relay}");
                println!();
                println!("send it over something private. It speaks as them until revoked.");
            } else {
                println!("no key minted. They can sign in to the console, or you can mint one");
                println!("for them there.");
            }
            Ok(())
        }
        MemberCmd::Ls { .. } => {
            let v: Value = http
                .get(format!("{base}/api/team"))
                .bearer_auth(&key)
                .send()
                .await?
                .json()
                .await
                .unwrap_or_default();
            let members = v["members"].as_array().cloned().unwrap_or_default();
            if members.is_empty() {
                println!("no members — is this relay reachable, and is that key an admin's?");
                return Ok(());
            }
            let tokens = v["tokens"].as_array().cloned().unwrap_or_default();
            for m in &members {
                let id = m["id"].as_str().unwrap_or("");
                let machines: Vec<&str> = tokens
                    .iter()
                    .filter(|t| {
                        t["member_id"].as_str() == Some(id)
                            && t["revoked"].as_bool() != Some(true)
                    })
                    .filter_map(|t| t["label"].as_str())
                    .collect();
                println!(
                    "{:<32} {:<8} {}",
                    m["email"].as_str().unwrap_or("?"),
                    m["role"].as_str().unwrap_or("?"),
                    if m["unassigned"].as_bool() == Some(true) {
                        "(unassigned key — attach it to a person in the console)".to_string()
                    } else if machines.is_empty() {
                        "no machines".to_string()
                    } else {
                        machines.join(", ")
                    }
                );
            }
            Ok(())
        }
        MemberCmd::Rm { email, .. } => {
            // Resolved here rather than taking an id: nobody knows their
            // colleague's member id, and a destructive call that takes an
            // opaque string is a call somebody gets wrong.
            let v: Value = http
                .get(format!("{base}/api/team"))
                .bearer_auth(&key)
                .send()
                .await?
                .json()
                .await
                .unwrap_or_default();
            let target = v["members"]
                .as_array()
                .and_then(|ms| {
                    ms.iter().find(|m| {
                        m["email"].as_str().map(str::to_lowercase) == Some(email.to_lowercase())
                    })
                })
                .and_then(|m| m["id"].as_str().map(str::to_string));
            let Some(target) = target else {
                println!("{email} is not on this team");
                return Ok(());
            };
            let r = http
                .post(format!("{base}/api/members/{target}/remove"))
                .bearer_auth(&key)
                .json(&serde_json::json!({}))
                .send()
                .await?;
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or_default();
            if ok {
                println!("removed {email} — their keys no longer work");
                println!("everybody else's are untouched");
            } else {
                println!("could not remove {email}: {}", v["error"].as_str().unwrap_or("unknown"));
            }
            Ok(())
        }
    }
}

/// `knoot present` — a person in the room.
///
/// The one client that is not an agent. Claude Code announces itself through
/// hooks; every other editor is invisible, and the coordination pain a mixed
/// team feels is mostly about *people* — who is in what. So this registers a
/// human's touches the same way a hook registers an agent's, through exactly
/// the same daemon requests, and the presence list stops being a list of one
/// tool's sessions.
///
/// Changes come from `git status --porcelain`, not from mtimes: it respects
/// `.gitignore`, it is what the repo itself considers a change, and it costs
/// one process every few seconds.
async fn present(name: Option<String>, doing: Option<String>, interval: u64) -> Result<()> {
    let root = repo_root_here();
    let repo_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    anyhow::ensure!(
        RepoConfig::load(&root).is_some(),
        "not inside a knoot repo — run `knoot init` here first"
    );
    let user = name.unwrap_or_else(cli_user);
    // Stable for the life of the process, and marked as a person: a reader of
    // the log should be able to tell a human from an agent without guessing.
    let session =
        format!("{}{user}-{}", proto::HUMAN_SESSION_PREFIX, std::process::id());
    let rr = repo_root.to_string_lossy().to_string();

    let branch = git_branch_of(&repo_root);
    hook::call_daemon(&DReq::SessionStart {
        repo_root: rr.clone(),
        session: session.clone(),
        user: user.clone(),
        branch,
    });
    if let Some(text) = &doing {
        hook::call_daemon(&DReq::Intent {
            repo_root: rr.clone(),
            session: session.clone(),
            text: text.clone(),
            user: user.clone(),
            branch: git_branch_of(&repo_root),
        });
    }

    println!("you are in the room. Ctrl-C to leave.");
    println!("agents on this repo will be told a person is in the files you are editing —");
    println!("they cannot ask you to stop, so they are told to work elsewhere.");
    if doing.is_none() {
        println!("tip: --doing \"what you are up to\" is what stops somebody duplicating it.");
    }

    // Leaving properly matters: a session that vanishes holds its claims until
    // the lease expires, and ten minutes of a phantom human is ten minutes of
    // agents being blocked by nobody.
    let leaving = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let leaving = leaving.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            leaving.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    let mut seen: std::collections::HashSet<String> = Default::default();
    while !leaving.load(std::sync::atomic::Ordering::SeqCst) {
        for path in changed_files(&repo_root) {
            if !seen.insert(path.clone()) {
                continue; // already registered; the lease renews on activity
            }
            let abs = repo_root.join(&path).to_string_lossy().to_string();
            // The claim first, so peers are blocked with a brief naming you,
            // then the write, which is what "changed under you" reads. The
            // verdict is ignored on purpose: a person cannot be told to stop
            // by a tool, and pretending otherwise would make this unusable.
            hook::call_daemon(&DReq::PreWrite {
                repo_root: rr.clone(),
                session: session.clone(),
                path: abs.clone(),
                creating: false,
            });
            hook::call_daemon(&DReq::PostWrite {
                repo_root: rr.clone(),
                session: session.clone(),
                path: abs,
            });
            println!("  holding {path}");
        }
        // Anything addressed to this person, printed as it arrives. Without
        // this, being in the room is write-only.
        if let Some(DResp::Mail { items }) =
            hook::call_daemon(&DReq::Poll { repo_root: rr.clone(), user: user.clone() })
        {
            for m in items {
                println!("  {m}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
    }

    hook::call_daemon(&DReq::SessionEnd { repo_root: rr, session });
    println!("left the room; everything you held is free.");
    Ok(())
}

/// Files this working tree considers changed, repo-relative.
fn changed_files(repo_root: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "status", "--porcelain", "--untracked-files=all"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // `XY path` or `XY old -> new` for a rename; the new name is the
            // one somebody is working in.
            let rest = line.get(3..)?.trim();
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let path = path.trim_matches('"');
            (!path.is_empty() && !path.ends_with('/')).then(|| path.to_string())
        })
        .collect()
}

fn git_branch_of(repo_root: &std::path::Path) -> String {
    std::process::Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "-".into())
}

/// `knoot why <path>` — the log, read back.
///
/// Every event has been in SQLite since the first version and nothing ever
/// answered a question about the past; two dashboards render the live tail and
/// that is all. This is the flight recorder: one file, in order, in the words
/// of the people and agents who touched it.
async fn why(path: String, limit: usize, relay: Option<String>) -> Result<()> {
    let root = repo_root_here();
    let cfg = RepoConfig::load(&root)
        .context("not inside a knoot repo (no .knoot.toml found)")?;
    let relay = relay.unwrap_or_else(|| cfg.relay.clone());
    let origin = config::relay_origin(&relay);
    let base = origin.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);

    // Relative to the repo, however the caller spelled it.
    let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| root.join(&path));
    let rel = abs
        .strip_prefix(std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.trim_start_matches('/').to_string());

    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(format!("{base}/api/events"))
        .query(&[("repo", cfg.repo.as_str()), ("path", rel.as_str())])
        .query(&[("limit", limit)]);
    if let Some(tok) = config::token_for(&relay) {
        req = req.bearer_auth(tok);
    }
    let events: Vec<Value> = match req.send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        Ok(r) => {
            println!("the relay answered {} — is this key still good?", r.status());
            return Ok(());
        }
        Err(e) => {
            println!("could not reach {base} ({})", truncate_err(&e.to_string()));
            return Ok(());
        }
    };

    println!("{rel}");
    if events.is_empty() {
        println!("  nothing on the log about this file yet");
    }

    // Who each session is, learned as the story goes: an event names a
    // session, and a name is what a reader needs.
    let mut who: std::collections::HashMap<String, String> = Default::default();
    let mut lines = 0;
    for e in &events {
        let kind = e["type"].as_str().unwrap_or("");
        let ts = e["ts"].as_u64().unwrap_or(0);
        let sess = e["session"].as_str().unwrap_or("");
        if let Some(u) = e["user"].as_str() {
            if !u.is_empty() && !sess.is_empty() {
                who.insert(sess.to_string(), u.to_string());
            }
        }
        let name = |s: &str| who.get(s).cloned().unwrap_or_else(|| s.to_string());
        let when = when_ago(ts);
        let line = match kind {
            "claim_acquired" => {
                let intent = e["intent"].as_str().unwrap_or("");
                Some(format!(
                    "{when}  {} took it{}",
                    name(sess),
                    if intent.is_empty() { String::new() } else { format!(" — \"{intent}\"") }
                ))
            }
            "claim_denied" => Some(format!(
                "{when}  {} was blocked; {} held it",
                name(sess),
                e["holder_user"].as_str().unwrap_or("someone")
            )),
            "claim_released" => Some(format!("{when}  {} let it go", name(sess))),
            "file_written" => Some(format!("{when}  {} wrote it", name(sess))),
            "path_removed" => Some(format!(
                "{when}  {} {} it",
                name(sess),
                if e["moved"].as_bool() == Some(true) { "moved" } else { "deleted" }
            )),
            "ungated_write" => Some(format!(
                "{when}  {} wrote it while {} held it (not stopped, only seen)",
                name(sess),
                e["holder_user"].as_str().unwrap_or("someone")
            )),
            "cross_branch_overlap" => Some(format!(
                "{when}  {} touched it on {}, {} on {} — these meet at merge",
                name(sess),
                e["branch"].as_str().unwrap_or("?"),
                e["peer_user"].as_str().unwrap_or("?"),
                e["peer_branch"].as_str().unwrap_or("?")
            )),
            "stale_read" => Some(format!(
                "{when}  {} was working from a stale read of it ({} had changed it)",
                name(sess),
                e["peer_user"].as_str().unwrap_or("someone")
            )),
            "create_collision" => Some(format!(
                "{when}  {} and {} both created it",
                name(sess),
                e["peer_user"].as_str().unwrap_or("someone")
            )),
            "path_freed" => Some(format!(
                "{when}  freed by {}",
                e["by_user"].as_str().unwrap_or("someone")
            )),
            "message" => Some(format!(
                "{when}  {} said: \"{}\"",
                e["from_user"].as_str().unwrap_or("someone"),
                e["text"].as_str().unwrap_or("")
            )),
            "intent_declared" => {
                let text = e["text"].as_str().unwrap_or("");
                (!text.is_empty()).then(|| format!("{when}  {} set out to: {text}", name(sess)))
            }
            // Presence on its own is noise in a file's story.
            _ => None,
        };
        if let Some(l) = line {
            println!("  {l}");
            lines += 1;
        }
    }
    if lines == 0 && !events.is_empty() {
        println!("  only presence on the log — nobody has claimed or written it");
    }

    // And what the team has decided about it since, which is the other half of
    // "why is this like this".
    if let Some(DResp::Memory { items, .. }) = hook::call_daemon(&DReq::Recall {
        repo_root: root.to_string_lossy().to_string(),
        query: rel.clone(),
    }) {
        let about: Vec<&String> = items.iter().filter(|i| i.contains(&rel)).collect();
        if !about.is_empty() {
            println!();
            println!("what the team knows about it:");
            for i in about {
                for l in i.lines() {
                    println!("  {l}");
                }
            }
        }
    }
    Ok(())
}

/// A timestamp as "3m ago", padded so the column lines up.
fn when_ago(ts: u64) -> String {
    if ts == 0 {
        return format!("{:>8}", "?");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(ts);
    let s = now.saturating_sub(ts) / 1000;
    let out = if s < 90 {
        format!("{s}s ago")
    } else if s < 5400 {
        format!("{}m ago", s / 60)
    } else if s < 172_800 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    };
    format!("{out:>8}")
}

/// The relay to administer and the key to do it with.
fn admin_target(relay: Option<String>) -> Result<(String, String, String)> {
    let relay = match relay {
        Some(r) => r,
        None => RepoConfig::load(&repo_root_here())
            .map(|c| c.relay)
            .context(
                "no --relay given and this directory is not a knoot repo — \
                 run this inside one, or pass --relay",
            )?,
    };
    let origin = config::relay_origin(&relay);
    let key = config::token_for(&relay).context(
        "no key stored for this relay — run `knoot join <key>` or `knoot login` first",
    )?;
    let base = origin.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    Ok((relay, base, key))
}

/// `knoot plan` — publish this session's context.
///
/// The session id comes from the environment because Claude Code exposes it
/// to the commands it runs; without one this is still useful — the plan is
/// keyed by the user instead, and a second call from the same person replaces
/// the first, which is what a person at a terminal would expect anyway.
fn plan(paths: Vec<String>, decisions: Vec<String>, text: String) -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("not inside a knoot repo (no .knoot.toml found)"))?;
    anyhow::ensure!(!text.trim().is_empty(), "say what you are doing");
    let req = DReq::Plan {
        repo_root: root.to_string_lossy().to_string(),
        session: session_key(),
        user: cli_user(),
        text,
        paths,
        decisions,
    };
    match hook::call_daemon(&req) {
        Some(DResp::Ok) => {
            println!("noted — peers in this area will see it on their next turn");
            Ok(())
        }
        Some(DResp::Err { msg }) => {
            println!("not published: {msg}");
            Ok(())
        }
        _ => {
            println!("knoot: daemon not reachable — nothing published");
            Ok(())
        }
    }
}

/// `knoot cache` — publish derived knowledge.
fn cache(name: String, paths: Vec<String>, text: String) -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("not inside a knoot repo (no .knoot.toml found)"))?;
    anyhow::ensure!(!text.trim().is_empty(), "a cache entry needs some content");
    let req = DReq::Cache {
        repo_root: root.to_string_lossy().to_string(),
        session: session_key(),
        user: cli_user(),
        name: name.clone(),
        text,
        paths,
    };
    match hook::call_daemon(&req) {
        Some(DResp::Memory { .. }) => {
            println!("cached: {name}");
            Ok(())
        }
        Some(DResp::Err { msg }) => {
            println!("not published: {msg}");
            Ok(())
        }
        _ => {
            println!("knoot: daemon not reachable — nothing published");
            Ok(())
        }
    }
}

/// The session this CLI call belongs to, or the user when there is no session
/// id to be had. Either way it is a stable key, which is all a supersession
/// chain needs.
fn session_key() -> String {
    std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(cli_user)
}

/// `knoot recall` — what the repo knows. For people and for capable models;
/// nothing depends on it, because the same facts reach an agent unasked.
fn recall(query: String) -> Result<()> {
    let root = config::find_repo_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("not inside a knoot repo (no .knoot.toml found)"))?;
    let req = DReq::Recall { repo_root: root.to_string_lossy().to_string(), query };
    match hook::call_daemon(&req) {
        Some(DResp::Memory { items, unreadable, .. }) => {
            if items.is_empty() {
                println!("nothing remembered here yet");
                println!("write one: knoot remember --name <handle> --path <file> \"...\"");
            }
            for i in &items {
                println!("{i}\n");
            }
            if unreadable > 0 {
                println!(
                    "{unreadable} shard(s) could not be opened — written under a key epoch \
                     this machine does not hold"
                );
            }
            Ok(())
        }
        Some(DResp::Err { msg }) => {
            println!("knoot: {msg}");
            Ok(())
        }
        _ => {
            println!("knoot: daemon not reachable");
            Ok(())
        }
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
                // Which kind of peer this is. An agent can be asked to move;
                // a person cannot, and the reader needs to know which.
                let kind = if proto::is_human_session(&s.session) { "person" } else { "agent" };
                println!("{:<12} {:<8} {:<16} {:<44} {}s ago", s.user, kind, s.branch, intent, ago);
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
mod presence_tests {
    use super::*;

    fn repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("knoot-present-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(d.join("src")).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .output();
        }
        d
    }

    /// What a person is editing, as the repo itself sees it. `git status`
    /// rather than mtimes: it already knows about `.gitignore`, and a build
    /// directory is not somebody's work in progress.
    #[test]
    fn changed_files_are_what_the_repo_calls_changed() {
        let d = repo("changed");
        std::fs::write(d.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(d.join("src/a.rs"), "fn a() {}\n").unwrap();
        let _ = std::process::Command::new("git").arg("-C").arg(&d).args(["add", "-A"]).output();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(["commit", "-qm", "init"])
            .output();

        // Nothing touched: nothing held.
        assert!(changed_files(&d).is_empty(), "a clean tree is nobody working");

        std::fs::write(d.join("src/a.rs"), "fn a() { todo!() }\n").unwrap();
        std::fs::write(d.join("src/b.rs"), "fn b() {}\n").unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("target/junk.o"), "x").unwrap();
        std::fs::write(d.join("build.log"), "noise").unwrap();

        let mut got = changed_files(&d);
        got.sort();
        assert_eq!(
            got,
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            "an edit and a new file, and nothing the repo ignores"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// A path with a space in it arrives quoted from git, and a claim on
    /// `"src/two words.rs"` would be a claim on a file that does not exist.
    #[test]
    fn a_path_with_a_space_is_unquoted() {
        let d = repo("space");
        std::fs::write(d.join("src/two words.rs"), "x").unwrap();
        assert_eq!(changed_files(&d), vec!["src/two words.rs".to_string()]);
        std::fs::remove_dir_all(&d).ok();
    }

    /// A person and an agent are told apart by the session id, everywhere.
    #[test]
    fn a_human_session_is_recognisable_as_one() {
        assert!(proto::is_human_session("human-priya-4821"));
        assert!(!proto::is_human_session("f47ac10b-58cc-4372-a567-0e02b2c3d479"));
        assert!(!proto::is_human_session(""), "an unnamed session is not a person");
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
