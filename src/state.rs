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
