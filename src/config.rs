use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SyncConfigFile {
    pub remote_url: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub repo_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSyncConfig {
    pub path: PathBuf,
    pub remote_url: String,
    pub branch: String,
    pub repo_path: PathBuf,
}

pub fn load_sync_config(path: &Path) -> Result<Option<ResolvedSyncConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: SyncConfigFile =
        toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))?;
    let codex_dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_dir);
    let repo_path = parsed
        .repo_path
        .unwrap_or_else(|| codex_dir.join("session-sync-repo"));

    Ok(Some(ResolvedSyncConfig {
        path: path.to_path_buf(),
        remote_url: parsed.remote_url,
        branch: parsed.branch.unwrap_or_else(|| "main".to_string()),
        repo_path,
    }))
}

pub fn default_config_path() -> PathBuf {
    default_codex_dir().join("sync.toml")
}

pub fn default_codex_dir() -> PathBuf {
    home_dir()
        .map(|path| path.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::load_sync_config;

    #[test]
    fn defaults_repo_path_next_to_config_file() -> Result<()> {
        let root = temp_dir("config-default");
        fs::create_dir_all(&root)?;
        let config_path = root.join("sync.toml");
        fs::write(&config_path, "remote_url = \"ssh://example/repo.git\"\n")?;

        let config = load_sync_config(&config_path)?.expect("config should load");
        assert_eq!(config.branch, "main");
        assert_eq!(config.repo_path, root.join("session-sync-repo"));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("codex-session-sync-{label}-{suffix}"))
    }
}
