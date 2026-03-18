use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub session_hash: String,
    pub local_path: PathBuf,
    #[serde(default)]
    pub last_scan_offset: Option<u64>,
    #[serde(default)]
    pub last_scan_anchor_hash: Option<String>,
    #[serde(default)]
    pub last_known_size: Option<u64>,
    #[serde(default)]
    pub last_known_mtime_ns: Option<i64>,
}

pub struct FileState {
    root: PathBuf,
}

impl FileState {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("sessions"))
            .with_context(|| format!("failed to create {}", root.join("sessions").display()))?;
        Ok(Self { root })
    }

    pub fn machine_id(&self) -> Result<String> {
        let path = self.root.join("machine-id");
        if path.exists() {
            return fs::read_to_string(&path)
                .map(|value| value.trim().to_string())
                .with_context(|| format!("failed to read {}", path.display()));
        }

        let seed = format!(
            "{}:{}:{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("clock before epoch")?
                .as_nanos(),
            std::env::var("HOSTNAME").unwrap_or_default()
        );
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        let machine_id = hex::encode(hasher.finalize())[..32].to_string();
        fs::write(&path, format!("{machine_id}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(machine_id)
    }

    pub fn projected_head(&self) -> Result<Option<String>> {
        let path = self.root.join("last-projected-head");
        if !path.exists() {
            return Ok(None);
        }

        let value = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(value.trim().to_string()))
    }

    pub fn set_projected_head(&self, head: &str) -> Result<()> {
        let path = self.root.join("last-projected-head");
        fs::write(&path, format!("{head}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn load_session(&self, session_hash: &str) -> Result<Option<SessionState>> {
        let path = self.session_state_path(session_hash);
        if !path.exists() {
            return Ok(None);
        }

        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let state: SessionState = toml::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(state))
    }

    pub fn save_session(&self, state: &SessionState) -> Result<()> {
        let path = self.session_state_path(&state.session_hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = toml::to_string_pretty(state).context("failed to encode session state")?;
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    fn session_state_path(&self, session_hash: &str) -> PathBuf {
        let shard_a = &session_hash[..2];
        let shard_b = &session_hash[2..4];
        self.root
            .join("sessions")
            .join(shard_a)
            .join(shard_b)
            .join(format!("{session_hash}.toml"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{FileState, SessionState};

    #[test]
    fn persists_machine_id_and_session_state() -> Result<()> {
        let root = temp_dir("file-state");
        let state = FileState::new(root.clone())?;
        let machine_a = state.machine_id()?;
        let machine_b = state.machine_id()?;
        assert_eq!(machine_a, machine_b);

        let session = SessionState {
            session_id: "session-1".to_string(),
            session_hash: "abcd1234".to_string(),
            local_path: PathBuf::from("/tmp/session.jsonl"),
            last_scan_offset: Some(42),
            last_scan_anchor_hash: Some("hash".to_string()),
            last_known_size: Some(100),
            last_known_mtime_ns: Some(123),
        };
        state.save_session(&session)?;
        let loaded = state.load_session("abcd1234")?.expect("session state");
        assert_eq!(loaded.local_path, session.local_path);

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
