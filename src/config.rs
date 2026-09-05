use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = ".knoot.toml";

/// What the config file was called when the project was called coord.
///
/// Read, never written. The reason it is still read at all is that this system
/// fails open: a repo whose config stopped being found does not error, it goes
/// quiet, and quiet is indistinguishable from working. Dropping the old name
/// would have turned a rename into silent, un-diagnosable loss of
/// coordination for anyone already enrolled. `knoot status` says when it is
/// reading one.
pub const LEGACY_CONFIG_FILE: &str = ".coord.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// WebSocket URL of the relay, e.g. ws://127.0.0.1:7420/ws
    pub relay: String,
    /// Stable repo identifier shared by all collaborators.
    pub repo: String,
    /// Paths every agent needs and nobody can hold for ten minutes without
    /// stalling the room: `package.json`, lockfiles, a shared type file, a
    /// route table. A hub gets a short lease and a queue instead of an owner.
    ///
    /// Declaring them is optional — a path claimed by three sessions inside
    /// half an hour is treated as a hub whether anyone named it or not — and
    /// the point of the list is to skip the first three collisions on a file
    /// everybody already knows is shared.
    ///
    /// Committed, like the rest of this file, so the whole team agrees; and
    /// omitted from what `save` writes when empty, so a repo enrolled before
    /// hubs existed does not grow a puzzling empty key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hubs: Vec<String>,
    /// The subtrees this repo is divided into. An area is the unit of *who
    /// can collide with whom*: a room grants `(repo, area)` pairs, and a
    /// session only hears about work in the areas it was granted.
    ///
    /// Declaring none is the normal case and means one area, `/`, holding the
    /// whole repo — which is exactly how every repo behaved before areas
    /// existed. Committed, like `hubs`, so the whole team divides the repo the
    /// same way; omitted from what `save` writes when empty, so a repo
    /// enrolled before areas existed does not grow a puzzling empty key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<AreaDef>,
}

/// A named subtree: the name a room grants, and the path prefixes it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaDef {
    pub name: String,
    pub paths: Vec<String>,
}

impl RepoConfig {
    pub fn load(repo_root: &Path) -> Option<Self> {
        let txt = std::fs::read_to_string(repo_root.join(CONFIG_FILE))
            .or_else(|_| std::fs::read_to_string(repo_root.join(LEGACY_CONFIG_FILE)))
            .ok()?;
        toml::from_str(&txt).ok()
    }

    /// Whether this repo is enrolled under the old file name, so a human can
    /// be told to migrate rather than left to find out by accident.
    pub fn is_legacy(repo_root: &Path) -> bool {
        !repo_root.join(CONFIG_FILE).is_file() && repo_root.join(LEGACY_CONFIG_FILE).is_file()
    }

    /// The area a repo-relative path belongs to, or `/` when no declaration
    /// claims it.
    ///
    /// A path belongs to the **most specific** area — `src/auth/` inside
    /// `src/` is the auth area — which is how CODEOWNERS resolves overlap, so
    /// nobody has to learn a second rule.
    pub fn area_for(&self, path: &str) -> String {
        area_of(&self.areas, path)
    }

    pub fn save(&self, repo_root: &Path) -> anyhow::Result<()> {
        std::fs::write(repo_root.join(CONFIG_FILE), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Walk up from `start` looking for .knoot.toml.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(CONFIG_FILE).is_file() || dir.join(LEGACY_CONFIG_FILE).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The area a repo-relative path belongs to under `areas`, or `/` when no
/// declaration claims it.
///
/// A path belongs to the **most specific** area — `src/auth/` inside `src/` is
/// the auth area — which is how CODEOWNERS resolves overlap, so nobody has to
/// learn a second rule. Free-standing rather than a method because the relay
/// holds the declarations without holding the repo they came from.
pub fn area_of(areas: &[AreaDef], path: &str) -> String {
    let path = path.trim_start_matches('/');
    let mut best: Option<(usize, &str)> = None;
    for a in areas {
        for p in &a.paths {
            let p = p.trim_matches('/');
            if p.is_empty() {
                continue;
            }
            let hit = path == p || path.starts_with(&format!("{p}/"));
            if hit && best.is_none_or(|(len, _)| p.len() > len) {
                best = Some((p.len(), &a.name));
            }
        }
    }
    best.map(|(_, n)| n.to_string()).unwrap_or_else(|| ROOT_AREA.to_string())
}

/// The area every path falls into when nothing more specific claims it, and
/// the only area a repo has until someone declares others.
pub const ROOT_AREA: &str = "/";

/// Areas read out of a repo's `CODEOWNERS`.
///
/// Google's answer to fifty thousand engineers on one repo is an OWNERS file
/// per subtree; a large org has already done this work, and re-entering it by
/// hand is how a declaration goes stale. One area per distinct owner set —
/// owners are what CODEOWNERS is *about*, so the areas it yields are the
/// subtrees that already have different people behind them — named after the
/// first owner with its `@` and any org prefix stripped.
///
/// Patterns that are not a plain subtree (`*`, `*.rs`, a glob in the middle)
/// are skipped rather than guessed at: an area that quietly means something
/// other than what CODEOWNERS says is worse than one that is missing.
pub fn areas_from_codeowners(repo_root: &Path) -> Vec<AreaDef> {
    let txt = ["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"]
        .iter()
        .find_map(|p| std::fs::read_to_string(repo_root.join(p)).ok());
    let Some(txt) = txt else { return Vec::new() };

    let mut order: Vec<String> = Vec::new();
    let mut by_owner: std::collections::HashMap<String, AreaDef> = std::collections::HashMap::new();
    for line in txt.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        let (Some(pattern), owners) = (parts.next(), parts.collect::<Vec<_>>()) else { continue };
        if owners.is_empty() {
            continue;
        }
        let Some(prefix) = subtree_of(pattern) else { continue };
        let key = owners.join(" ");
        let name = area_name(owners[0]);
        let e = by_owner.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            AreaDef { name, paths: Vec::new() }
        });
        if !e.paths.contains(&prefix) {
            e.paths.push(prefix);
        }
    }
    order.into_iter().filter_map(|k| by_owner.remove(&k)).collect()
}

/// A CODEOWNERS pattern as a plain subtree prefix, or `None` when it is a
/// glob, a file extension rule, or the catch-all — none of which name a
/// subtree, and the catch-all is `/` already.
fn subtree_of(pattern: &str) -> Option<String> {
    let p = pattern.trim_end_matches("/*").trim_end_matches("/**").trim_matches('/');
    if p.is_empty() || p == "*" || p.contains(['*', '?', '[', '!']) {
        return None;
    }
    Some(p.to_string())
}

/// `@acme/payments-team` -> `payments-team`. The org prefix is the same on
/// every line, so keeping it would make every area name start with noise.
fn area_name(owner: &str) -> String {
    let o = owner.trim_start_matches('@');
    o.rsplit('/').next().unwrap_or(o).to_string()
}

/// Where a relay token lives. Deliberately **not** `.knoot.toml`: that file is
/// committed so a teammate who clones is enrolled with no setup, and a shared
/// secret must never ride along with it. One file per user, keyed by relay
/// origin, mode 0600.
pub const CREDENTIALS_FILE: &str = "credentials.toml";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// relay origin (scheme://host:port) -> token
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
}

fn credentials_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".knoot").join(CREDENTIALS_FILE))
}

/// Where tokens lived under the old name. Read on load, never written, for
/// the same reason as `LEGACY_CONFIG_FILE`: a token that stopped being found
/// means a relay that refuses us, which fails open and says nothing.
fn legacy_credentials_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".coord").join(CREDENTIALS_FILE))
}

/// An environment variable under its current name, or the name it had when
/// the project was called coord.
///
/// The stakes differ per variable but point the same way. `KNOOT_RELAY_TOKEN`
/// missing does not stop a relay — it starts an **open** one, so a rename
/// that dropped the old name would have quietly unauthenticated a hosted
/// relay. `KNOOT_USER` missing does not stop a session, it just attributes
/// its work to the OS user. Both are silent, which is the argument for
/// reading both names.
pub fn env_or_legacy(name: &str) -> Option<String> {
    let legacy = name.replacen("KNOOT_", "COORD_", 1);
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var(&legacy).ok().filter(|v| !v.is_empty()))
}

/// The origin part of a relay URL, used as the credential key so one token
/// serves every repo on that relay.
pub fn relay_origin(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("ws", url),
    };
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

impl Credentials {
    pub fn load() -> Self {
        let read = |p: Option<PathBuf>| {
            p.and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| toml::from_str::<Self>(&t).ok())
        };
        // The new file wins; tokens only in the old one still work, so a
        // rename cannot quietly log a whole team out of its relay.
        match (read(credentials_path()), read(legacy_credentials_path())) {
            (Some(new), Some(old)) => {
                let mut merged = old;
                merged.tokens.extend(new.tokens);
                merged
            }
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = credentials_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        // A shared secret readable by every process on a shared box is not a
        // secret; best-effort because some filesystems will not honour it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// The token to present to `relay_url`, if any. The environment wins so CI and
/// containers need no file on disk.
pub fn token_for(relay_url: &str) -> Option<String> {
    if let Some(t) = env_or_legacy("KNOOT_TOKEN") {
        return Some(t);
    }
    Credentials::load().tokens.get(&relay_origin(relay_url)).cloned()
}

#[cfg(test)]
mod tests {
    fn areas() -> Vec<AreaDef> {
        vec![
            AreaDef { name: "core".into(), paths: vec!["src".into()] },
            AreaDef { name: "auth".into(), paths: vec!["src/auth".into(), "docs/auth".into()] },
        ]
    }

    #[test]
    fn a_path_belongs_to_the_most_specific_area() {
        let a = areas();
        assert_eq!(area_of(&a, "src/auth/token.rs"), "auth", "not the enclosing `src`");
        assert_eq!(area_of(&a, "src/http/client.rs"), "core");
        assert_eq!(area_of(&a, "docs/auth/README.md"), "auth", "one area, two prefixes");
        assert_eq!(area_of(&a, "README.md"), ROOT_AREA, "unclaimed paths fall to `/`");
        assert_eq!(area_of(&a, "srcfoo/x.rs"), ROOT_AREA, "a prefix is a subtree, not a substring");
        assert_eq!(area_of(&a, "src"), "core", "the prefix itself belongs to its area");
    }

    #[test]
    fn a_repo_that_declares_no_areas_is_one_area() {
        let cfg = RepoConfig {
            relay: "ws://x".into(),
            repo: "r".into(),
            hubs: Vec::new(),
            areas: Vec::new(),
        };
        assert_eq!(cfg.area_for("anything/at/all"), ROOT_AREA);
    }

    #[test]
    fn codeowners_becomes_one_area_per_owner_set() {
        let dir = std::env::temp_dir().join(format!("knoot-co-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".github")).unwrap();
        std::fs::write(
            dir.join(".github/CODEOWNERS"),
            "# review routing\n\
             *            @acme/everyone\n\
             /src/auth/   @acme/auth-team\n\
             /docs/auth   @acme/auth-team\n\
             /src/billing @acme/payments @acme/auth-team\n\
             *.md         @acme/docs\n",
        )
        .unwrap();

        let got = areas_from_codeowners(&dir);
        assert_eq!(
            got,
            vec![
                AreaDef {
                    name: "auth-team".into(),
                    paths: vec!["src/auth".into(), "docs/auth".into()],
                },
                AreaDef { name: "payments".into(), paths: vec!["src/billing".into()] },
            ],
            "one area per distinct owner set; the catch-all and the extension rule \
             name no subtree and are skipped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_repo_with_no_codeowners_imports_nothing() {
        let dir = std::env::temp_dir().join(format!("knoot-co-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(areas_from_codeowners(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn areas_survive_a_round_trip_through_the_config_file() {
        let dir = std::env::temp_dir().join(format!("knoot-ar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg =
            RepoConfig { relay: "ws://x".into(), repo: "r".into(), hubs: Vec::new(), areas: areas() };
        cfg.save(&dir).unwrap();
        assert_eq!(RepoConfig::load(&dir).unwrap().areas, areas());

        // And a repo enrolled before areas existed does not grow the key.
        let bare =
            RepoConfig { relay: "ws://x".into(), repo: "r".into(), hubs: Vec::new(), areas: Vec::new() };
        bare.save(&dir).unwrap();
        let txt = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(!txt.contains("areas"), "no puzzling empty key: {txt}");
        std::fs::remove_dir_all(&dir).ok();
    }

    use super::*;

    /// One token per relay, not per repo: a team on one relay logs in once.
    #[test]
    fn origin_is_scheme_and_host_only() {
        assert_eq!(relay_origin("ws://127.0.0.1:7420/ws"), "ws://127.0.0.1:7420");
        assert_eq!(relay_origin("wss://relay.example.com/ws"), "wss://relay.example.com");
        assert_eq!(relay_origin("wss://relay.example.com:443/ws?x=1"), "wss://relay.example.com:443");
    }

    /// Two relays on the same host but different ports are different relays,
    /// or a dev token would be sent to production.
    #[test]
    fn port_is_part_of_the_origin() {
        assert_ne!(relay_origin("ws://host:1/ws"), relay_origin("ws://host:2/ws"));
    }

    #[test]
    fn a_missing_scheme_does_not_panic() {
        assert_eq!(relay_origin("127.0.0.1:7420/ws"), "ws://127.0.0.1:7420");
    }

    /// The rename must not be able to un-enrol a repo. This system fails open,
    /// so a config file that stopped being found does not raise anything — the
    /// repo simply stops coordinating and looks exactly like a quiet one.
    #[test]
    fn a_repo_enrolled_under_the_old_name_still_loads() {
        let dir = std::env::temp_dir().join(format!("knoot-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(LEGACY_CONFIG_FILE),
            "relay = \"ws://127.0.0.1:7420/ws\"\nrepo = \"old-repo\"\n",
        )
        .unwrap();

        let cfg = RepoConfig::load(&dir).expect("the old file must still be read");
        assert_eq!(cfg.repo, "old-repo");
        assert_eq!(find_repo_root(&dir).as_deref(), Some(dir.as_path()));
        assert!(RepoConfig::is_legacy(&dir), "and it must be reported as legacy");

        // Once migrated, the new file wins and nothing is flagged.
        RepoConfig { relay: cfg.relay, repo: "new-repo".into(), hubs: Vec::new(), areas: Vec::new() }.save(&dir).unwrap();
        assert_eq!(RepoConfig::load(&dir).unwrap().repo, "new-repo");
        assert!(!RepoConfig::is_legacy(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `KNOOT_RELAY_TOKEN` is the dangerous one: unset, a relay does not fail
    /// to start, it starts *open*. A rename that dropped the old name would
    /// have silently unauthenticated a hosted relay.
    #[test]
    fn the_old_environment_variable_names_still_work() {
        // Nothing set.
        std::env::remove_var("KNOOT_RELAY_TOKEN");
        std::env::remove_var("COORD_RELAY_TOKEN");
        assert_eq!(env_or_legacy("KNOOT_RELAY_TOKEN"), None);

        // Only the old name.
        std::env::set_var("COORD_RELAY_TOKEN", "old");
        assert_eq!(env_or_legacy("KNOOT_RELAY_TOKEN").as_deref(), Some("old"));

        // Both: the current name wins.
        std::env::set_var("KNOOT_RELAY_TOKEN", "new");
        assert_eq!(env_or_legacy("KNOOT_RELAY_TOKEN").as_deref(), Some("new"));

        // An empty value is not a value, under either name.
        std::env::set_var("KNOOT_RELAY_TOKEN", "");
        assert_eq!(env_or_legacy("KNOOT_RELAY_TOKEN").as_deref(), Some("old"));
        std::env::set_var("COORD_RELAY_TOKEN", "");
        assert_eq!(env_or_legacy("KNOOT_RELAY_TOKEN"), None);

        std::env::remove_var("KNOOT_RELAY_TOKEN");
        std::env::remove_var("COORD_RELAY_TOKEN");
    }
}
