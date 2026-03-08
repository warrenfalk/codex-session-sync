use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::spool::StoredBatch;

pub struct RepoSync {
    repo: PathBuf,
    options: SyncOptions,
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub remote: String,
    pub branch: String,
    pub push: bool,
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub imported_files: usize,
    pub created_commit: bool,
    pub pushed: bool,
}

impl RepoSync {
    pub fn new(repo: PathBuf, options: SyncOptions) -> Result<Self> {
        ensure_repo(&repo)?;
        Ok(Self { repo, options })
    }

    pub fn import_batches(&self, batches: &[StoredBatch]) -> Result<SyncSummary> {
        ensure_clean_worktree(&self.repo)?;

        if self.options.push && remote_exists(&self.repo, &self.options.remote)? {
            pull_rebase(&self.repo, &self.options.remote, &self.options.branch)?;
        }

        let mut imported_files = 0usize;
        for batch in batches {
            if self.import_batch(batch)? {
                imported_files += 1;
            }
        }

        let created_commit = if imported_files > 0 {
            git_add(&self.repo, ".")?;
            git_commit(
                &self.repo,
                &format!("Import {} spool batch(es)", imported_files),
            )?;
            true
        } else {
            false
        };

        let pushed = if self.options.push && remote_exists(&self.repo, &self.options.remote)? {
            if created_commit {
                push_with_rebase_retry(&self.repo, &self.options.remote, &self.options.branch)?;
            }
            created_commit
        } else {
            false
        };

        Ok(SyncSummary {
            imported_files,
            created_commit,
            pushed,
        })
    }

    fn import_batch(&self, batch: &StoredBatch) -> Result<bool> {
        let target = target_path(&self.repo, batch);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let bytes = serde_json::to_vec_pretty(&batch.batch).context("failed to encode batch")?;

        if target.exists() {
            let existing = fs::read(&target)
                .with_context(|| format!("failed to read {}", target.display()))?;
            if existing == bytes {
                return Ok(false);
            }
            bail!(
                "target already exists with different contents: {}",
                target.display()
            );
        }

        fs::write(&target, bytes)
            .with_context(|| format!("failed to write {}", target.display()))?;
        Ok(true)
    }
}

fn ensure_repo(path: &Path) -> Result<()> {
    if !path.join(".git").exists() {
        bail!("{} is not a git repository", path.display());
    }
    Ok(())
}

fn ensure_clean_worktree(repo: &Path) -> Result<()> {
    let output = git(repo, ["status", "--porcelain"])?;
    if !output.stdout.trim().is_empty() {
        bail!(
            "sync repo {} has uncommitted changes; refusing to import into a dirty worktree",
            repo.display()
        );
    }
    Ok(())
}

fn remote_exists(repo: &Path, remote: &str) -> Result<bool> {
    let output = git_allow_failure(repo, ["remote", "get-url", remote])?;
    Ok(output.status.success())
}

fn pull_rebase(repo: &Path, remote: &str, branch: &str) -> Result<()> {
    let ls_remote = git_allow_failure(repo, ["ls-remote", "--heads", remote, branch])?;
    if !ls_remote.status.success() || ls_remote.stdout.trim().is_empty() {
        return Ok(());
    }

    git(repo, ["pull", "--rebase", remote, branch])?;
    Ok(())
}

fn push_with_rebase_retry(repo: &Path, remote: &str, branch: &str) -> Result<()> {
    for attempt in 0..3 {
        let push = git_allow_failure(repo, ["push", remote, &format!("HEAD:{branch}")])?;
        if push.status.success() {
            return Ok(());
        }

        if attempt == 2 {
            bail!("git push failed after retrying:\n{}", push.stderr);
        }

        pull_rebase(repo, remote, branch)?;
    }

    bail!("unreachable push retry loop")
}

fn git_add(repo: &Path, pathspec: &str) -> Result<()> {
    git(repo, ["add", pathspec])?;
    Ok(())
}

fn git_commit(repo: &Path, message: &str) -> Result<()> {
    git(
        repo,
        [
            "-c",
            "user.name=codex-session-sync",
            "-c",
            "user.email=codex-session-sync@local",
            "commit",
            "-m",
            message,
        ],
    )?;
    Ok(())
}

fn target_path(repo: &Path, batch: &StoredBatch) -> PathBuf {
    let session_component = batch
        .batch
        .source_session_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("unknown-{}", short_hash(&batch.batch.source_path)));

    repo.join("sessions")
        .join(session_component)
        .join("batches")
        .join(format!("{}.json", batch.batch.batch_id))
}

fn short_hash(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

fn git<I, S>(repo: &Path, args: I) -> Result<GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_allow_failure(repo, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!(
            "git command failed in {}:\n{}",
            repo.display(),
            output.stderr
        );
    }
}

fn git_allow_failure<I, S>(repo: &Path, args: I) -> Result<GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", repo.display()))?;
    Ok(GitOutput::from(output))
}

struct GitOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl From<Output> for GitOutput {
    fn from(output: Output) -> Self {
        Self {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use serde_json::json;

    use super::{RepoSync, SyncOptions};
    use crate::scan::ChangeKind;
    use crate::spool::{SpoolBatch, StoredBatch};

    #[test]
    fn imports_batch_into_session_layout_and_commits() -> Result<()> {
        let repo_dir = temp_dir("repo");
        fs::create_dir_all(&repo_dir)?;
        git_init(&repo_dir)?;

        let batch = StoredBatch {
            path: repo_dir.join("pending-batch.json"),
            batch: SpoolBatch {
                schema_version: 1,
                batch_id: "batch-1".to_string(),
                ingested_at_unix_ms: 1,
                source_path: "/tmp/session.jsonl".to_string(),
                source_session_id: Some("session-1".to_string()),
                source_device: 1,
                source_inode: 2,
                source_size: 3,
                source_sha256: "abc".to_string(),
                change_kind: ChangeKind::Appended,
                start_line: 10,
                end_line: 11,
                record_count: 1,
                records: vec![json!({"type": "event_msg"})],
            },
        };

        let sync = RepoSync::new(
            repo_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                push: false,
            },
        )?;
        let summary = sync.import_batches(&[batch])?;

        assert_eq!(summary.imported_files, 1);
        assert!(summary.created_commit);
        assert!(!summary.pushed);
        assert!(
            repo_dir
                .join("sessions")
                .join("session-1")
                .join("batches")
                .join("batch-1.json")
                .exists()
        );

        fs::remove_dir_all(repo_dir)?;
        Ok(())
    }

    fn git_init(path: &Path) -> Result<()> {
        let status = std::process::Command::new("git")
            .current_dir(path)
            .arg("init")
            .status()?;
        anyhow::ensure!(status.success(), "git init failed");
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
