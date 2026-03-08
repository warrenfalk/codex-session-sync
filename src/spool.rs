use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::scan::{ChangeKind, ScannedSession};

pub struct SpoolWriter {
    pending_dir: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SpoolBatch {
    schema_version: u32,
    batch_id: String,
    ingested_at_unix_ms: u128,
    source_path: String,
    source_session_id: Option<String>,
    source_device: u64,
    source_inode: u64,
    source_size: u64,
    source_sha256: String,
    change_kind: ChangeKind,
    start_line: usize,
    end_line: usize,
    record_count: usize,
    records: Vec<Value>,
}

impl SpoolWriter {
    pub fn new(spool_root: PathBuf) -> Result<Self> {
        let pending_dir = spool_root.join("pending");
        fs::create_dir_all(&pending_dir)
            .with_context(|| format!("failed to create {}", pending_dir.display()))?;
        Ok(Self { pending_dir })
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
