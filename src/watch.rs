//! `coord watch` — a live view of who is working where. Connects to the relay
//! as a read-only client, mirrors the event log, and redraws a dashboard.

use crate::config::RepoConfig;
use crate::proto::*;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMsg;

const FEED_MAX: usize = 200;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const VIOLET: &str = "\x1b[35m";
const R: &str = "\x1b[0m";

struct State {
    view: View,
    feed: VecDeque<String>,
    connected: bool,
    denials: u64,
    writes: u64,
    ungated: u64,
}

pub async fn run(repo_root: std::path::PathBuf) -> Result<()> {
    let cfg = RepoConfig::load(&repo_root).context("no .coord.toml — run `coord init` first")?;
    let st = Arc::new(Mutex::new(State {
        view: View::default(),
        feed: VecDeque::new(),
        connected: false,
        denials: 0,
        writes: 0,
        ungated: 0,
    }));

    let s2 = st.clone();
    let cfg2 = cfg.clone();
    tokio::spawn(async move { stream(cfg2, s2).await });

    // Redraw on a fixed tick so lease countdowns stay live.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tick.tick().await;
        let mut s = st.lock().unwrap();
        s.view.prune();
        print!("{}", render(&s, &cfg));
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

async fn stream(cfg: RepoConfig, st: Arc<Mutex<State>>) {
    loop {
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&cfg.relay).await {
            let hello = ClientMsg::Hello { repo: cfg.repo.clone(), daemon: "watch".into() };
            if ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.is_ok() {
                st.lock().unwrap().connected = true;
                while let Some(Ok(WsMsg::Text(t))) = ws.next().await {
                    let Ok(sm) = serde_json::from_str::<ServerMsg>(&t) else { continue };
                    let mut s = st.lock().unwrap();
                    match sm {
                        ServerMsg::Welcome { claims, sessions, .. } => {
                            s.view.claims = claims;
                            s.view.sessions =
                                sessions.into_iter().map(|x| (x.session.clone(), x)).collect();
                        }
                        ServerMsg::Event { event, .. } => {
                            if let Some(line) = describe(&event) {
                                s.feed.push_back(line);
                                if s.feed.len() > FEED_MAX {
                                    s.feed.pop_front();
                                }
                            }
                            match event {
                                Event::ClaimDenied { .. } => s.denials += 1,
                                Event::UngatedWrite { .. } => s.ungated += 1,
                                Event::FileWritten { .. } => s.writes += 1,
                                _ => {}
                            }
                            s.view.apply(&event);
                        }
                        ServerMsg::ClaimResp { .. } => {}
                    }
                }
            }
        }
        st.lock().unwrap().connected = false;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn clock(ts: Ts) -> String {
    let secs = (ts / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn describe(e: &Event) -> Option<String> {
    Some(match e {
        Event::SessionStarted { user, branch, ts, .. } => {
            format!("{DIM}{}{R}  {CYAN}{:<9}{R} joined  {DIM}branch {}{R}", clock(*ts), user, branch)
        }
        Event::IntentDeclared { text, ts, session } => format!(
            "{DIM}{}{R}  {CYAN}{:<9}{R} intent  {}",
            clock(*ts),
            short(session),
            truncate(text, 60)
        ),
        Event::ClaimAcquired { user, path, .. } => {
            format!("{DIM}{}{R}  {CYAN}{:<9}{R} {GREEN}claim {R}  {}", clock(now_ms()), user, path)
        }
        Event::PathFreed { path, by_user, ts, .. } => format!(
            "{DIM}{}{R}  {CYAN}{:<9}{R} {GREEN}FREED  {R} {} {DIM}(waiters notified){R}",
            clock(*ts),
            by_user,
            path
        ),
        Event::Message { from_user, to, text, ts, .. } => format!(
            "{DIM}{}{R}  {CYAN}{:<9}{R} {VIOLET}MSG    {R} {}{}",
            clock(*ts),
            from_user,
            to.as_ref().map(|t| format!("→{t}: ")).unwrap_or_else(|| "→all: ".into()),
            truncate(text, 50)
        ),
        Event::UngatedWrite { user, path, holder_user, ts, .. } => format!(
            "{DIM}{}{R}  {CYAN}{:<9}{R} {RED}UNGATED{R} {} {DIM}(wrote over {}){R}",
            clock(*ts),
            user,
            path,
            holder_user
        ),
        Event::ClaimDenied { user, path, holder_user, ts, .. } => format!(
            "{DIM}{}{R}  {CYAN}{:<9}{R} {RED}BLOCKED{R} {} {DIM}(held by {}){R}",
            clock(*ts),
            user,
            path,
            holder_user
        ),
        Event::FileWritten { path, ts, .. } => {
            format!("{DIM}{}  wrote   {}{R}", clock(*ts), path)
        }
        Event::SessionEnded { ts, .. } => format!("{DIM}{}  session ended{R}", clock(*ts)),
        Event::ClaimReleased { path, ts, .. } => {
            format!("{DIM}{}  released {}{R}", clock(*ts), path)
        }
    })
}

fn truncate(s: &str, n: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n - 1).collect::<String>())
    }
}

fn render(s: &State, cfg: &RepoConfig) -> String {
    let (cols, rows) = term_size();
    let mut out = String::from("\x1b[H\x1b[2J"); // home + clear

    let dot = if s.connected { format!("{GREEN}●{R}") } else { format!("{RED}●{R}") };
    let live = s.view.sessions.len();
    out.push_str(&format!(
        "{BOLD}coord{R} {}  {DIM}{}{R}   {BOLD}{}{R} session(s)   {BOLD}{}{R} claim(s)   \
         writes {BOLD}{}{R}   blocked {BOLD}{}{}{R}   ungated {BOLD}{}{}{R}\n",
        dot,
        cfg.repo,
        live,
        s.view.claims.len(),
        s.writes,
        if s.denials > 0 { RED } else { "" },
        s.denials,
        if s.ungated > 0 { RED } else { "" },
        s.ungated
    ));
    out.push_str(&format!("{DIM}{}{R}\n", "─".repeat(cols)));

    if s.view.sessions.is_empty() {
        out.push_str(&format!("{DIM}  no active sessions — start Claude Code in this repo{R}\n"));
    } else {
        out.push_str(&format!(
            "{DIM}{:<10} {:<9} {:<34} {}{R}\n",
            "USER", "SESSION", "INTENT", "HOLDS"
        ));
        let mut sessions: Vec<_> = s.view.sessions.values().collect();
        sessions.sort_by(|a, b| a.user.cmp(&b.user).then(a.session.cmp(&b.session)));
        for si in sessions {
            let held: Vec<String> = s
                .view
                .claims
                .iter()
                .filter(|c| c.session == si.session)
                .map(|c| {
                    let m = c.lease_until.saturating_sub(now_ms()) / 60_000;
                    format!("{} {DIM}({}m){R}", c.path, m)
                })
                .collect();
            let holds = if held.is_empty() {
                format!("{DIM}—{R}")
            } else {
                format!("{YELLOW}{}{R}", held.join(", "))
            };
            let intent = if si.intent.is_empty() {
                format!("{DIM}(none yet){R}")
            } else {
                truncate(&si.intent, 34)
            };
            out.push_str(&format!(
                "{CYAN}{:<10}{R} {DIM}{:<9}{R} {:<34} {}\n",
                truncate(&si.user, 10),
                short(&si.session),
                intent,
                holds
            ));
        }
    }

    out.push_str(&format!("{DIM}{}{R}\n", "─".repeat(cols)));
    // Fill whatever rows remain with the tail of the event feed. Counting the
    // lines actually emitted keeps the header pinned when the pane is short.
    let used = out.lines().count();
    let space = rows.saturating_sub(used + 1);
    for line in s.feed.iter().rev().take(space).collect::<Vec<_>>().into_iter().rev() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn term_size() -> (usize, usize) {
    // Query the tty directly. COLUMNS/LINES are inherited from whatever
    // launched us and are routinely wrong inside a tmux pane.
    match terminal_size::terminal_size() {
        Some((terminal_size::Width(w), terminal_size::Height(h))) => (w as usize, h as usize),
        None => (100, 24),
    }
}
