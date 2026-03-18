use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session_file::{ParsedSessionFile, SessionLine};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub session_id: String,
    pub session_hash: String,
    pub message_hash: String,
    pub timestamp: String,
    pub timestamp_key: String,
    #[serde(default)]
    pub record_type: Option<String>,
    pub raw_jsonl: String,
    pub source_machine_id: String,
    pub source_path: String,
    #[serde(default)]
    pub source_line_number: Option<u64>,
}

pub struct MessageStore {
    repo_root: PathBuf,
}

pub struct UpsertSummary {
    pub messages_written: usize,
    pub touched_sessions: BTreeSet<String>,
}

impl MessageStore {
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }

    pub fn upsert_session_file(
        &self,
        machine_id: &str,
        file: &ParsedSessionFile,
    ) -> Result<UpsertSummary> {
        let mut touched_sessions = BTreeSet::new();
        let mut messages_written = 0usize;
        for (index, line) in file.lines.iter().enumerate() {
            if self.write_message(
                &file.session_id,
                &file.session_hash,
                machine_id,
                &file.path,
                index as u64,
                line,
            )? {
                touched_sessions.insert(file.session_hash.clone());
                messages_written += 1;
            }
        }
        Ok(UpsertSummary {
            messages_written,
            touched_sessions,
        })
    }

    pub fn write_message(
        &self,
        session_id: &str,
        session_hash: &str,
        machine_id: &str,
        source_path: &Path,
        source_line_number: u64,
        line: &SessionLine,
    ) -> Result<bool> {
        let target = self.message_path(session_hash, &line.timestamp_key, &line.message_hash);
        if target.exists() {
            return Ok(false);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let message = StoredMessage {
            session_id: session_id.to_string(),
            session_hash: session_hash.to_string(),
            message_hash: line.message_hash.clone(),
            timestamp: line.timestamp.clone(),
            timestamp_key: line.timestamp_key.clone(),
            record_type: Some(line.record_type.clone()),
            raw_jsonl: line.raw_jsonl.clone(),
            source_machine_id: machine_id.to_string(),
            source_path: source_path.to_string_lossy().into_owned(),
            source_line_number: Some(source_line_number),
        };
        let bytes = serde_json::to_vec_pretty(&message).context("failed to encode message")?;
        fs::write(&target, bytes).with_context(|| format!("failed to write {}", target.display()))?;
        Ok(true)
    }

    pub fn session_hashes(&self) -> Result<Vec<String>> {
        let base = self.repo_root.join("sessions");
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut hashes = Vec::new();
        for shard_a in fs::read_dir(&base).with_context(|| format!("failed to read {}", base.display()))? {
            let shard_a = shard_a?;
            if !shard_a.file_type()?.is_dir() {
                continue;
            }
            for shard_b in fs::read_dir(shard_a.path())? {
                let shard_b = shard_b?;
                if !shard_b.file_type()?.is_dir() {
                    continue;
                }
                for session_dir in fs::read_dir(shard_b.path())? {
                    let session_dir = session_dir?;
                    if session_dir.file_type()?.is_dir() {
                        hashes.push(session_dir.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
        hashes.sort();
        hashes.dedup();
        Ok(hashes)
    }

    pub fn load_session_messages(&self, session_hash: &str) -> Result<Vec<StoredMessage>> {
        let dir = self.session_dir(session_hash).join("messages");
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        load_messages_from_dir(&dir, &mut entries)?;

        entries.sort_by(|left, right| {
            left.timestamp_key
                .cmp(&right.timestamp_key)
                .then_with(|| message_type_rank(left).cmp(&message_type_rank(right)))
                .then_with(|| {
                    left.source_line_number
                        .unwrap_or(u64::MAX)
                        .cmp(&right.source_line_number.unwrap_or(u64::MAX))
                })
                .then_with(|| left.message_hash.cmp(&right.message_hash))
        });
        Ok(entries)
    }

    fn message_path(&self, session_hash: &str, timestamp_key: &str, message_hash: &str) -> PathBuf {
        self.session_dir(session_hash)
            .join("messages")
            .join(hour_shard(timestamp_key))
            .join(format!("{timestamp_key}-{message_hash}.json"))
    }

    fn session_dir(&self, session_hash: &str) -> PathBuf {
        let shard_a = &session_hash[..2];
        let shard_b = &session_hash[2..4];
        self.repo_root
            .join("sessions")
            .join(shard_a)
            .join(shard_b)
            .join(session_hash)
    }
}

fn load_messages_from_dir(dir: &Path, entries: &mut Vec<StoredMessage>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            load_messages_from_dir(&path, entries)?;
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let message: StoredMessage = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        entries.push(message);
    }
    Ok(())
}

fn hour_shard(timestamp_key: &str) -> &str {
    &timestamp_key[..10]
}

fn message_type_rank(message: &StoredMessage) -> u8 {
    let fallback_type = if message.record_type.is_none() {
        extract_type(&message.raw_jsonl)
    } else {
        None
    };
    match message
        .record_type
        .as_deref()
        .or(fallback_type.as_deref())
    {
        Some("session_meta") => 0,
        Some("event_msg") => 1,
        Some("response_item") => 2,
        Some("function_call") => 3,
        Some("function_call_output") => 4,
        Some(_) => 5,
        None => 6,
    }
}

fn extract_type(raw_jsonl: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw_jsonl).ok()?;
    value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::MessageStore;
    use crate::session_file::{ParsedSessionFile, SessionLine};

    #[test]
    fn stores_messages_with_hour_sharded_filename() -> Result<()> {
        let root = temp_dir("message-store");
        fs::create_dir_all(&root)?;
        let store = MessageStore::new(root.clone());
        let file = ParsedSessionFile {
            path: PathBuf::from("/tmp/session.jsonl"),
            session_id: "session-1".to_string(),
            session_hash: "abcd1234".to_string(),
            lines: vec![SessionLine {
                raw_jsonl: "{\"timestamp\":\"2026-03-18T21:04:05.123Z\"}".to_string(),
                message_hash: "hash-1".to_string(),
                timestamp: "2026-03-18T21:04:05.123Z".to_string(),
                timestamp_key: "20260318210405123".to_string(),
                record_type: "session_meta".to_string(),
            }],
        };

        let summary = store.upsert_session_file("machine-1", &file)?;
        assert_eq!(summary.messages_written, 1);
        let target = root
            .join("sessions")
            .join("ab")
            .join("cd")
            .join("abcd1234")
            .join("messages")
            .join("2026031821")
            .join("20260318210405123-hash-1.json");
        assert!(target.exists());

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
