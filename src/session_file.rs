use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, error::Category as JsonErrorCategory};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

const TIMESTAMP_KEY_FORMAT: &[time::format_description::FormatItem<'_>] =
    format_description!("[year][month][day][hour][minute][second][subsecond digits:3]");
const SHADOW_MARKER: &str = ".sync-shadow-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionKind {
    Live,
    Shadow,
}

impl Display for SessionKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live => f.write_str("live"),
            Self::Shadow => f.write_str("shadow"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionLine {
    pub raw_jsonl: String,
    pub message_hash: String,
    pub timestamp: String,
    pub timestamp_key: String,
}

#[derive(Debug, Clone)]
pub struct ParsedSessionFile {
    pub path: PathBuf,
    pub session_id: String,
    pub session_hash: String,
    pub lines: Vec<SessionLine>,
}

#[derive(Debug, Clone)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct SessionScanReport {
    pub files: Vec<ParsedSessionFile>,
    pub warnings: Vec<ScanWarning>,
}

pub struct SessionFileScanner {
    root: PathBuf,
}

impl SessionFileScanner {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn scan_live(&self) -> Result<SessionScanReport> {
        self.scan_kind(SessionKind::Live)
    }

    pub fn scan_shadows(&self) -> Result<SessionScanReport> {
        self.scan_kind(SessionKind::Shadow)
    }

    fn scan_kind(&self, kind: SessionKind) -> Result<SessionScanReport> {
        let mut report = SessionScanReport::default();
        if !self.root.exists() {
            return Ok(report);
        }
        self.scan_dir(&self.root, kind, &mut report)?;
        report.files.sort_by(|left, right| left.path.cmp(&right.path));
        report.warnings.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(report)
    }

    fn scan_dir(&self, dir: &Path, kind: SessionKind, report: &mut SessionScanReport) -> Result<()> {
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
                self.scan_dir(&path, kind, report)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if !matches_kind(&path, kind) {
                continue;
            }

            match parse_session_file(&path, kind) {
                Ok(file) => report.files.push(file),
                Err(error) => report.warnings.push(ScanWarning {
                    path: path.clone(),
                    message: error.to_string(),
                }),
            }
        }

        Ok(())
    }
}

pub fn is_shadow_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.contains(SHADOW_MARKER))
}

pub fn shadow_path_for(target: &Path, nonce: &str) -> Result<PathBuf> {
    let parent = target
        .parent()
        .with_context(|| format!("missing parent for {}", target.display()))?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .with_context(|| format!("invalid file name for {}", target.display()))?;
    Ok(parent.join(format!(".{file_name}{SHADOW_MARKER}{nonce}")))
}

fn matches_kind(path: &Path, kind: SessionKind) -> bool {
    let is_shadow = is_shadow_path(path);
    match kind {
        SessionKind::Live => path.extension().and_then(|value| value.to_str()) == Some("jsonl") && !is_shadow,
        SessionKind::Shadow => is_shadow,
    }
}

fn parse_session_file(path: &Path, _kind: SessionKind) -> Result<ParsedSessionFile> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let raw_lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let file_ends_with_newline = bytes.last().copied() == Some(b'\n');

    let mut session_id = None;
    let mut lines = Vec::new();

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

        if session_id.is_none() {
            session_id = value
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .with_context(|| format!("missing top-level timestamp on line {} in {}", index + 1, path.display()))?;
        let timestamp_key = timestamp_key(timestamp)?;
        let message_hash = sha256_hex(raw_line);

        lines.push(SessionLine {
            raw_jsonl: line.to_string(),
            message_hash,
            timestamp: timestamp.to_string(),
            timestamp_key,
        });
    }

    let session_id =
        session_id.with_context(|| format!("missing session id in {}", path.display()))?;
    if lines.is_empty() {
        bail!("session file {} is empty", path.display());
    }

    Ok(ParsedSessionFile {
        path: path.to_path_buf(),
        session_hash: sha256_hex(session_id.as_bytes()),
        session_id,
        lines,
    })
}

fn timestamp_key(value: &str) -> Result<String> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .with_context(|| format!("failed to parse timestamp {value}"))?;
    let millis = (parsed.nanosecond() / 1_000_000) * 1_000_000;
    let normalized = parsed
        .replace_nanosecond(millis)
        .context("failed to normalize timestamp to milliseconds")?;
    normalized
        .format(TIMESTAMP_KEY_FORMAT)
        .context("failed to format timestamp key")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn should_ignore_incomplete_final_line(
    index: usize,
    total_lines: usize,
    file_ends_with_newline: bool,
    error: &serde_json::Error,
) -> bool {
    if file_ends_with_newline || index + 1 != total_lines {
        return false;
    }

    matches!(
        error.classify(),
        JsonErrorCategory::Eof | JsonErrorCategory::Syntax
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{SessionFileScanner, is_shadow_path, shadow_path_for};

    #[test]
    fn parses_live_and_shadow_files_separately() -> Result<()> {
        let root = temp_dir("scan-kinds");
        fs::create_dir_all(&root)?;
        let live_path = root.join("session.jsonl");
        let shadow_path = root.join(".session.jsonl.sync-shadow-1");
        let line = "{\"timestamp\":\"2026-03-18T21:04:05.123Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-1\"}}\n";
        fs::write(&live_path, line)?;
        fs::write(&shadow_path, line)?;

        let scanner = SessionFileScanner::new(root.clone());
        let live = scanner.scan_live()?;
        let shadows = scanner.scan_shadows()?;

        assert_eq!(live.files.len(), 1);
        assert_eq!(shadows.files.len(), 1);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn formats_shadow_path_next_to_target() -> Result<()> {
        let target = PathBuf::from("/tmp/a/session.jsonl");
        let shadow = shadow_path_for(&target, "nonce")?;
        assert!(is_shadow_path(&shadow));
        assert_eq!(shadow, PathBuf::from("/tmp/a/.session.jsonl.sync-shadow-nonce"));
        Ok(())
    }

    #[test]
    fn missing_root_scans_as_empty() -> Result<()> {
        let root = temp_dir("missing-root");
        let scanner = SessionFileScanner::new(root);
        let live = scanner.scan_live()?;
        let shadows = scanner.scan_shadows()?;
        assert!(live.files.is_empty());
        assert!(live.warnings.is_empty());
        assert!(shadows.files.is_empty());
        assert!(shadows.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn finds_session_id_when_session_meta_is_not_first_line() -> Result<()> {
        let root = temp_dir("session-id-later");
        fs::create_dir_all(&root)?;
        let path = root.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-18T21:04:05.123Z\",\"type\":\"response_item\",\"payload\":{\"kind\":\"message\"}}\n",
                "{\"timestamp\":\"2026-03-18T21:04:05.123Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-1\"}}\n"
            ),
        )?;

        let scanner = SessionFileScanner::new(root.clone());
        let live = scanner.scan_live()?;

        assert_eq!(live.files.len(), 1);
        assert_eq!(live.files[0].session_id, "session-1");

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
