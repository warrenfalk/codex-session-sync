use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ChangeKind {
    New,
    Appended,
    Rewritten,
    Unchanged,
}

impl Display for ChangeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::New => "new",
            Self::Appended => "appended",
            Self::Rewritten => "rewritten",
            Self::Unchanged => "unchanged",
        };
        f.write_str(label)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScannedSession {
    pub path: PathBuf,
    pub path_key: String,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub sha256: String,
    pub tail_sha256: String,
    pub tail_len: usize,
    pub line_count: usize,
    pub session_id: Option<String>,
    pub last_record_type: Option<String>,
    pub type_counts: BTreeMap<String, usize>,
    pub records: Vec<Value>,
    #[serde(skip_serializing)]
    bytes: Vec<u8>,
}

impl ScannedSession {
    pub fn tail_matches(
        &self,
        expected_size: u64,
        expected_tail_len: usize,
        expected_tail_sha256: &str,
    ) -> bool {
        if expected_size > self.size {
            return false;
        }

        if expected_tail_len == 0 {
            return true;
        }

        let end = usize::try_from(expected_size).ok();
        let Some(end) = end else {
            return false;
        };

        if end < expected_tail_len || end > self.bytes.len() {
            return false;
        }

        let start = end - expected_tail_len;
        sha256_hex(&self.bytes[start..end]) == expected_tail_sha256
    }
}

pub struct SessionScanner {
    root: PathBuf,
}

impl SessionScanner {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn scan(&self) -> Result<Vec<ScannedSession>> {
        let mut sessions = Vec::new();
        self.scan_dir(&self.root, &mut sessions)?;
        sessions.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(sessions)
    }

    fn scan_dir(&self, dir: &Path, sessions: &mut Vec<ScannedSession>) -> Result<()> {
        let entries = fs::read_dir(dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?;

        for entry in entries {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to read file type for {}", path.display()))?;
            if file_type.is_dir() {
                self.scan_dir(&path, sessions)?;
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                sessions.push(scan_session_file(&path)?);
            }
        }

        Ok(())
    }
}

fn scan_session_file(path: &Path) -> Result<ScannedSession> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let cursor = std::io::Cursor::new(&bytes);
    let reader = BufReader::new(cursor);

    let mut line_count = 0usize;
    let mut session_id = None;
    let mut last_record_type = None;
    let mut type_counts = BTreeMap::<String, usize>::new();
    let mut records = Vec::new();

    for (index, line_result) in reader.lines().enumerate() {
        let line = line_result
            .with_context(|| format!("failed to read line {} in {}", index + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "failed to parse JSON on line {} in {}",
                index + 1,
                path.display()
            )
        })?;

        let record_type = value
            .get("type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        if index == 0 {
            session_id = value
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        if let Some(record_type) = &record_type {
            *type_counts.entry(record_type.clone()).or_insert(0) += 1;
        }

        last_record_type = record_type;
        line_count += 1;
        records.push(value);
    }

    if line_count == 0 {
        bail!("session file {} is empty", path.display());
    }

    let tail_len = bytes.len().min(TAIL_BYTES);
    let tail_start = bytes.len() - tail_len;

    Ok(ScannedSession {
        path: path.to_path_buf(),
        path_key: path.to_string_lossy().into_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mtime_ns: metadata.mtime_nsec() + (metadata.mtime() * 1_000_000_000),
        sha256: sha256_hex(&bytes),
        tail_sha256: sha256_hex(&bytes[tail_start..]),
        tail_len,
        line_count,
        session_id,
        last_record_type,
        type_counts,
        records,
        bytes,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
