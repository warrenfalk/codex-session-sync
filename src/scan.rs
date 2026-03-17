use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, error::Category as JsonErrorCategory};
use sha2::{Digest, Sha256};

const TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Debug, Clone)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub message: String,
}

pub struct ScanReport {
    pub sessions: Vec<ScannedSession>,
    pub warnings: Vec<ScanWarning>,
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

    pub fn scan(&self) -> Result<ScanReport> {
        let mut sessions = Vec::new();
        let mut warnings = Vec::new();
        self.scan_dir(&self.root, &mut sessions, &mut warnings)?;
        sessions.sort_by(|left, right| left.path.cmp(&right.path));
        warnings.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(ScanReport { sessions, warnings })
    }

    fn scan_dir(
        &self,
        dir: &Path,
        sessions: &mut Vec<ScannedSession>,
        warnings: &mut Vec<ScanWarning>,
    ) -> Result<()> {
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
                self.scan_dir(&path, sessions, warnings)?;
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                match scan_session_file(&path) {
                    Ok(scan_result) => {
                        sessions.push(scan_result.session);
                        if let Some(message) = scan_result.warning {
                            warnings.push(ScanWarning {
                                path: path.clone(),
                                message,
                            });
                        }
                    }
                    Err(error) => warnings.push(ScanWarning {
                        path: path.clone(),
                        message: error.to_string(),
                    }),
                }
            }
        }

        Ok(())
    }
}

struct SessionScanResult {
    session: ScannedSession,
    warning: Option<String>,
}

fn scan_session_file(path: &Path) -> Result<SessionScanResult> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let raw_lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let file_ends_with_newline = bytes.last().copied() == Some(b'\n');

    let mut line_count = 0usize;
    let mut session_id = None;
    let mut last_record_type = None;
    let mut type_counts = BTreeMap::<String, usize>::new();
    let mut records = Vec::new();
    let mut warning = None;

    for (index, raw_line) in raw_lines.iter().enumerate() {
        if file_ends_with_newline && index + 1 == raw_lines.len() && raw_line.is_empty() {
            continue;
        }

        let line = std::str::from_utf8(raw_line).with_context(|| {
            format!("failed to decode line {} in {}", index + 1, path.display())
        })?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error)
                if should_ignore_incomplete_final_line(
                    index,
                    raw_lines.len(),
                    file_ends_with_newline,
                    &error,
                ) =>
            {
                warning = Some(format!(
                    "ignored incomplete final JSON record on line {} in {}",
                    index + 1,
                    path.display()
                ));
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to parse JSON on line {} in {}",
                        index + 1,
                        path.display()
                    )
                });
            }
        };

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

    Ok(SessionScanResult {
        session: ScannedSession {
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
        },
        warning,
    })
}

fn should_ignore_incomplete_final_line(
    index: usize,
    line_count: usize,
    file_ends_with_newline: bool,
    error: &serde_json::Error,
) -> bool {
    index + 1 == line_count && !file_ends_with_newline && error.classify() == JsonErrorCategory::Eof
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use serde_json::Value;

    use super::{SessionScanner, should_ignore_incomplete_final_line};

    #[test]
    fn ignores_incomplete_final_line_without_trailing_newline() -> Result<()> {
        let root = temp_dir("truncated-tail");
        let session_path = root.join("session.jsonl");
        write_bytes(
            &session_path,
            concat!(
                "{\"timestamp\":\"2026-03-08T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-1\"}}\n",
                "{\"timestamp\":\"2026-03-08T00:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
                "{\"timestamp\":\"2026-03-08T00:02:00Z\",\"type\":\"turn_context\",\"payload\":{\"user_instructions\":\"truncated"
            )
            .as_bytes(),
        )?;

        let report = SessionScanner::new(root.clone()).scan()?;

        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].line_count, 2);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.warnings[0].message,
            format!(
                "ignored incomplete final JSON record on line 3 in {}",
                session_path.display()
            )
        );

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn skips_file_with_invalid_non_final_line() -> Result<()> {
        let root = temp_dir("invalid-middle");
        let session_path = root.join("session.jsonl");
        write_bytes(
            &session_path,
            concat!(
                "{\"timestamp\":\"2026-03-08T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-2\"}}\n",
                "{\"timestamp\":\"2026-03-08T00:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"broken\"}\n",
                "{\"timestamp\":\"2026-03-08T00:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"after\"}}\n"
            )
            .as_bytes(),
        )?;

        let report = SessionScanner::new(root.clone()).scan()?;

        assert!(report.sessions.is_empty());
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].message.starts_with(&format!(
            "failed to parse JSON on line 2 in {}",
            session_path.display()
        )));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn only_ignores_eof_on_unterminated_final_line() {
        let error = serde_json::from_str::<Value>("{\"a\":").unwrap_err();
        assert!(should_ignore_incomplete_final_line(0, 1, false, &error));
        assert!(!should_ignore_incomplete_final_line(0, 1, true, &error));
        assert!(!should_ignore_incomplete_final_line(0, 2, false, &error));
    }

    fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
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
