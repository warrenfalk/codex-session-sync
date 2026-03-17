use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize)]
struct PersistedSyncConfig<'a> {
    remote_url: &'a str,
    branch: &'a str,
    repo_path: &'a Path,
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

pub fn write_sync_config(config: &ResolvedSyncConfig) -> Result<()> {
    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let serialized = toml::to_string_pretty(&PersistedSyncConfig {
        remote_url: &config.remote_url,
        branch: &config.branch,
        repo_path: &config.repo_path,
    })
    .context("failed to serialize sync config")?;

    fs::write(&config.path, serialized)
        .with_context(|| format!("failed to write {}", config.path.display()))?;
    Ok(())
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

    use super::{ResolvedSyncConfig, load_sync_config, write_sync_config};

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

    #[test]
    fn writes_round_trip_config() -> Result<()> {
        let root = temp_dir("config-write");
        fs::create_dir_all(&root)?;
        let config_path = root.join("sync.toml");
        let expected = ResolvedSyncConfig {
            path: config_path.clone(),
            remote_url: "git@github.com:example/repo.git".to_string(),
            branch: "main".to_string(),
            repo_path: root.join("session-sync-repo"),
        };

        write_sync_config(&expected)?;

        let actual = load_sync_config(&config_path)?.expect("config should load");
        assert_eq!(actual.remote_url, expected.remote_url);
        assert_eq!(actual.branch, expected.branch);
        assert_eq!(actual.repo_path, expected.repo_path);

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
