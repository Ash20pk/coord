use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = ".coord.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// WebSocket URL of the relay, e.g. ws://127.0.0.1:7420/ws
    pub relay: String,
    /// Stable repo identifier shared by all collaborators.
    pub repo: String,
}

impl RepoConfig {
    pub fn load(repo_root: &Path) -> Option<Self> {
        let txt = std::fs::read_to_string(repo_root.join(CONFIG_FILE)).ok()?;
        toml::from_str(&txt).ok()
    }

    pub fn save(&self, repo_root: &Path) -> anyhow::Result<()> {
        std::fs::write(repo_root.join(CONFIG_FILE), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Walk up from `start` looking for .coord.toml.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(CONFIG_FILE).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Where a relay token lives. Deliberately **not** `.coord.toml`: that file is
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
    Some(dirs::home_dir()?.join(".coord").join(CREDENTIALS_FILE))
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
        credentials_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
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
    if let Ok(t) = std::env::var("COORD_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
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
}
