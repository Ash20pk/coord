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
