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
        RepoConfig { relay: cfg.relay, repo: "new-repo".into() }.save(&dir).unwrap();
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
