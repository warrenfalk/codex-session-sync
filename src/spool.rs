use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::scan::{ChangeKind, ScannedSession};

pub struct SpoolWriter {
    pending_dir: PathBuf,
    processed_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub ingested_at_unix_ms: u128,
    pub source_path: String,
    pub source_session_id: Option<String>,
    pub source_device: u64,
    pub source_inode: u64,
    pub source_size: u64,
    pub source_sha256: String,
    pub change_kind: ChangeKind,
    pub start_line: usize,
    pub end_line: usize,
    pub record_count: usize,
    pub records: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct StoredBatch {
    pub path: PathBuf,
    pub batch: SpoolBatch,
}

impl SpoolWriter {
    pub fn new(spool_root: PathBuf) -> Result<Self> {
        let pending_dir = spool_root.join("pending");
        let processed_dir = spool_root.join("processed");
        fs::create_dir_all(&pending_dir)
            .with_context(|| format!("failed to create {}", pending_dir.display()))?;
        fs::create_dir_all(&processed_dir)
            .with_context(|| format!("failed to create {}", processed_dir.display()))?;
        Ok(Self {
            pending_dir,
            processed_dir,
        })
    }

    pub fn write_batch(&self, batch: &SpoolBatch) -> Result<PathBuf> {
        let target = self.pending_dir.join(format!("{}.json", batch.batch_id));
        let tmp = self.pending_dir.join(format!("{}.tmp", batch.batch_id));
        let bytes = serde_json::to_vec_pretty(batch).context("failed to serialize spool batch")?;
        fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("failed to move {} to {}", tmp.display(), target.display()))?;
        Ok(target)
    }

    pub fn load_pending_batches(&self) -> Result<Vec<StoredBatch>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.pending_dir)
            .with_context(|| format!("failed to read {}", self.pending_dir.display()))?
        {
            let entry = entry.with_context(|| {
                format!("failed to read entry in {}", self.pending_dir.display())
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }

            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let batch: SpoolBatch = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            entries.push(StoredBatch { path, batch });
        }

        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    pub fn mark_processed(&self, batch: &StoredBatch) -> Result<PathBuf> {
        let file_name = batch
            .path
            .file_name()
            .with_context(|| format!("missing file name for {}", batch.path.display()))?;
        let target = self.processed_dir.join(file_name);
        fs::rename(&batch.path, &target).with_context(|| {
            format!(
                "failed to move {} to {}",
                batch.path.display(),
                target.display()
            )
        })?;
        Ok(target)
    }
}

impl SpoolBatch {
    pub fn from_session(
        session: &ScannedSession,
        change_kind: ChangeKind,
        start_line: usize,
        records: Vec<Value>,
    ) -> Result<Self> {
        let ingested_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis();
        let end_line = start_line + records.len();
        let mut batch_hasher = Sha256::new();
        batch_hasher.update(session.path_key.as_bytes());
        batch_hasher.update(session.sha256.as_bytes());
        batch_hasher.update(format!("{change_kind}:{start_line}:{end_line}").as_bytes());
        let batch_id = format!(
            "{ingested_at_unix_ms}-{}",
            hex::encode(batch_hasher.finalize())
        );

        Ok(Self {
            schema_version: 1,
            batch_id,
            ingested_at_unix_ms,
            source_path: session.path_key.clone(),
            source_session_id: session.session_id.clone(),
            source_device: session.device,
            source_inode: session.inode,
            source_size: session.size,
            source_sha256: session.sha256.clone(),
            change_kind,
            start_line,
            end_line,
            record_count: records.len(),
            records,
        })
    }
}
