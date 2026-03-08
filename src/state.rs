use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::scan::{ChangeKind, ScannedSession};

pub struct StateStore {
    connection: Connection,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StoredSession {
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
}

impl StoredSession {
    pub fn classify(&self, current: &ScannedSession) -> ChangeKind {
        if self.sha256 == current.sha256 {
            return ChangeKind::Unchanged;
        }

        if self.device == current.device
            && self.inode == current.inode
            && current.size >= self.size
            && current.tail_matches(self.size, self.tail_len, &self.tail_sha256)
        {
            return ChangeKind::Appended;
        }

        ChangeKind::Rewritten
    }
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }

        let connection =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn begin_transaction(&mut self) -> Result<()> {
        self.connection
            .execute_batch("BEGIN IMMEDIATE TRANSACTION;")
            .context("failed to begin transaction")?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.connection
            .execute_batch("COMMIT;")
            .context("failed to commit transaction")?;
        Ok(())
    }

    pub fn load_all(&self) -> Result<BTreeMap<String, StoredSession>> {
        let mut statement = self.connection.prepare(
            "SELECT path_key, device, inode, size, mtime_ns, sha256, tail_sha256, tail_len, line_count, session_id, last_record_type
             FROM tracked_sessions",
        )?;

        let rows = statement.query_map([], |row| {
            Ok(StoredSession {
                path_key: row.get(0)?,
                device: row.get(1)?,
                inode: row.get(2)?,
                size: row.get(3)?,
                mtime_ns: row.get(4)?,
                sha256: row.get(5)?,
                tail_sha256: row.get(6)?,
                tail_len: row.get(7)?,
                line_count: row.get(8)?,
                session_id: row.get(9)?,
                last_record_type: row.get(10)?,
            })
        })?;

        let mut entries = BTreeMap::new();
        for row in rows {
            let entry = row?;
            entries.insert(entry.path_key.clone(), entry);
        }

        Ok(entries)
    }

    pub fn upsert(&self, session: &ScannedSession) -> Result<()> {
        self.connection.execute(
            "INSERT INTO tracked_sessions (
                path_key, device, inode, size, mtime_ns, sha256, tail_sha256, tail_len, line_count, session_id, last_record_type
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(path_key) DO UPDATE SET
                device = excluded.device,
                inode = excluded.inode,
                size = excluded.size,
                mtime_ns = excluded.mtime_ns,
                sha256 = excluded.sha256,
                tail_sha256 = excluded.tail_sha256,
                tail_len = excluded.tail_len,
                line_count = excluded.line_count,
                session_id = excluded.session_id,
                last_record_type = excluded.last_record_type",
            params![
                session.path_key,
                session.device,
                session.inode,
                session.size,
                session.mtime_ns,
                session.sha256,
                session.tail_sha256,
                session.tail_len,
                session.line_count,
                session.session_id,
                session.last_record_type,
            ],
        )?;

        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS tracked_sessions (
                path_key TEXT PRIMARY KEY,
                device INTEGER NOT NULL,
                inode INTEGER NOT NULL,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                tail_sha256 TEXT NOT NULL,
                tail_len INTEGER NOT NULL,
                line_count INTEGER NOT NULL,
                session_id TEXT,
                last_record_type TEXT
            );",
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::StoredSession;
    use crate::scan::{ChangeKind, ScannedSession, SessionScanner};

    #[test]
    fn detects_appended_file_for_resumed_session_path() -> Result<()> {
        let root = temp_dir("append");
        let session_path = root
            .join("2026")
            .join("02")
            .join("09")
            .join("rollout-2026-02-09T19-11-34-example.jsonl");
        write_session_file(
            &session_path,
            &[
                r#"{"timestamp":"2026-02-09T19:11:34Z","type":"session_meta","payload":{"id":"session-1"}}"#,
                r#"{"timestamp":"2026-02-09T19:12:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            ],
        )?;

        let initial = scan_one(&root)?;
        append_line(
            &session_path,
            r#"{"timestamp":"2026-03-08T00:00:00Z","type":"event_msg","payload":{"type":"context_compacted"}}"#,
        )?;
        let current = scan_one(&root)?;

        let stored = stored_session_from_scan(&initial);
        assert_eq!(stored.classify(&current), ChangeKind::Appended);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn detects_rewrite_when_existing_prefix_changes() -> Result<()> {
        let root = temp_dir("rewrite");
        let session_path = root.join("session.jsonl");
        write_session_file(
            &session_path,
            &[
                r#"{"timestamp":"2026-03-08T00:00:00Z","type":"session_meta","payload":{"id":"session-2"}}"#,
                r#"{"timestamp":"2026-03-08T00:01:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            ],
        )?;

        let initial = scan_one(&root)?;
        write_session_file(
            &session_path,
            &[
                r#"{"timestamp":"2026-03-08T00:00:00Z","type":"session_meta","payload":{"id":"session-2"}}"#,
                r#"{"timestamp":"2026-03-08T00:01:30Z","type":"event_msg","payload":{"type":"different_event"}}"#,
            ],
        )?;
        let current = scan_one(&root)?;

        let stored = stored_session_from_scan(&initial);
        assert_eq!(stored.classify(&current), ChangeKind::Rewritten);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn scan_one(root: &Path) -> Result<ScannedSession> {
        let mut sessions = SessionScanner::new(root.to_path_buf()).scan()?;
        Ok(sessions.remove(0))
    }

    fn stored_session_from_scan(scan: &ScannedSession) -> StoredSession {
        StoredSession {
            path_key: scan.path_key.clone(),
            device: scan.device,
            inode: scan.inode,
            size: scan.size,
            mtime_ns: scan.mtime_ns,
            sha256: scan.sha256.clone(),
            tail_sha256: scan.tail_sha256.clone(),
            tail_len: scan.tail_len,
            line_count: scan.line_count,
            session_id: scan.session_id.clone(),
            last_record_type: scan.last_record_type.clone(),
        }
    }

    fn write_session_file(path: &Path, lines: &[&str]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut content = lines.join("\n");
        content.push('\n');
        fs::write(path, content)?;
        Ok(())
    }

    fn append_line(path: &Path, line: &str) -> Result<()> {
        let mut file = OpenOptions::new().append(true).open(path)?;
        writeln!(file, "{line}")?;
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
