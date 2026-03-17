use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::spool::StoredBatch;

pub struct RepoSync {
    repo: PathBuf,
    options: SyncOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoSetupStatus {
    ExistingRepo,
    Cloned,
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub remote: String,
    pub branch: String,
    pub remote_url: String,
    pub push: bool,
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub imported_files: usize,
    pub created_commit: bool,
    pub pushed: bool,
    pub skipped_due_to_lock: bool,
}

impl RepoSync {
    pub fn new(repo: PathBuf, options: SyncOptions) -> Result<Self> {
        ensure_repo_or_clone(&repo, &options.remote_url, &options.branch)?;
        Ok(Self { repo, options })
    }

    pub fn import_batches(&self, batches: &[StoredBatch]) -> Result<SyncSummary> {
        self.import_batches_with_after_pull(batches, || Ok(()))
    }

    fn import_batches_with_after_pull<F>(
        &self,
        batches: &[StoredBatch],
        after_pull: F,
    ) -> Result<SyncSummary>
    where
        F: FnOnce() -> Result<()>,
    {
        let Some(_lock) = RepoLock::acquire(&self.repo)? else {
            return Ok(SyncSummary {
                skipped_due_to_lock: true,
                ..SyncSummary::default()
            });
        };

        ensure_clean_worktree(&self.repo)?;

        if self.options.push && remote_exists(&self.repo, &self.options.remote)? {
            pull_rebase(&self.repo, &self.options.remote, &self.options.branch)?;
        }

        after_pull()?;

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
            skipped_due_to_lock: false,
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

pub fn prepare_repo(path: &Path, remote_url: &str, branch: &str) -> Result<RepoSetupStatus> {
    if path.join(".git").exists() {
        git(path, ["ls-remote", "--heads", remote_url, branch])?;
        return Ok(RepoSetupStatus::ExistingRepo);
    }

    ensure_repo_or_clone(path, remote_url, branch)
}

fn ensure_repo_or_clone(path: &Path, remote_url: &str, branch: &str) -> Result<RepoSetupStatus> {
    if path.join(".git").exists() {
        return Ok(RepoSetupStatus::ExistingRepo);
    }

    if path.exists() {
        let mut entries =
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?;
        if entries.next().is_some() {
            bail!(
                "{} exists but is not a git repository and is not empty",
                path.display()
            );
        }
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let clone_parent = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let clone_target = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid repo path {}", path.display()))?;

    match detect_remote_branch_state(&clone_parent, remote_url, branch)? {
        RemoteBranchState::BranchExists => {
            git(
                &clone_parent,
                ["clone", "--branch", branch, remote_url, clone_target],
            )?;
        }
        RemoteBranchState::EmptyRemote => {
            git(&clone_parent, ["clone", remote_url, clone_target])?;
            git(&path.to_path_buf(), ["branch", "-M", branch])?;
        }
        RemoteBranchState::MissingBranch => {
            bail!(
                "remote {} does not have branch {} and is not empty",
                remote_url,
                branch
            );
        }
    }

    Ok(RepoSetupStatus::Cloned)
}

enum RemoteBranchState {
    BranchExists,
    EmptyRemote,
    MissingBranch,
}

fn detect_remote_branch_state(
    repo: &Path,
    remote_url: &str,
    branch: &str,
) -> Result<RemoteBranchState> {
    let branch_output = git(repo, ["ls-remote", "--heads", remote_url, branch])?;
    if !branch_output.stdout.trim().is_empty() {
        return Ok(RemoteBranchState::BranchExists);
    }

    let heads_output = git(repo, ["ls-remote", "--heads", remote_url])?;
    if heads_output.stdout.trim().is_empty() {
        Ok(RemoteBranchState::EmptyRemote)
    } else {
        Ok(RemoteBranchState::MissingBranch)
    }
}

fn ensure_clean_worktree(repo: &Path) -> Result<()> {
    let output = git(
        repo,
        [
            "status",
            "--porcelain",
            "--",
            ".",
            ":(exclude).codex-session-sync.lock",
        ],
    )?;
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
        .args([
            "-c",
            "user.name=codex-session-sync",
            "-c",
            "user.email=codex-session-sync@local",
        ])
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

struct RepoLock {
    path: PathBuf,
}

impl RepoLock {
    fn acquire(repo: &Path) -> Result<Option<Self>> {
        let path = repo.join(".codex-session-sync.lock");
        match fs::create_dir(&path) {
            Ok(()) => {
                write_lock_metadata(&path)?;
                Ok(Some(Self { path }))
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to create lock directory {}", path.display())),
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            tracing::warn!(error = %error, path = %self.path.display(), "failed to remove repo lock");
        }
    }
}

fn write_lock_metadata(lock_dir: &Path) -> Result<()> {
    let metadata_path = lock_dir.join("owner.json");
    let hostname = env::var("HOSTNAME").ok();
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "hostname": hostname,
    });
    fs::write(&metadata_path, serde_json::to_vec_pretty(&payload)?)
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use serde_json::json;

    use super::{RepoSetupStatus, RepoSync, SyncOptions, SyncSummary, prepare_repo};
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
                remote_url: repo_dir.display().to_string(),
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

    #[test]
    fn skips_sync_when_repo_lock_is_already_held() -> Result<()> {
        let repo_dir = temp_dir("repo-locked");
        fs::create_dir_all(&repo_dir)?;
        git_init(&repo_dir)?;
        fs::create_dir(repo_dir.join(".codex-session-sync.lock"))?;

        let batch = StoredBatch {
            path: repo_dir.join("pending-batch.json"),
            batch: SpoolBatch {
                schema_version: 1,
                batch_id: "batch-locked".to_string(),
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
                remote_url: repo_dir.display().to_string(),
                push: false,
            },
        )?;
        let summary = sync.import_batches(&[batch])?;

        assert!(summary.skipped_due_to_lock);
        assert_eq!(summary.imported_files, 0);
        assert!(
            !repo_dir
                .join("sessions")
                .join("session-1")
                .join("batches")
                .join("batch-locked.json")
                .exists()
        );

        fs::remove_dir_all(repo_dir)?;
        Ok(())
    }

    #[test]
    fn concurrent_clones_converge_for_same_session() -> Result<()> {
        let remote_dir = temp_dir("remote");
        let seed_dir = temp_dir("seed");
        let clone_a = temp_dir("clone-a");
        let clone_b = temp_dir("clone-b");
        let verify_dir = temp_dir("verify");

        fs::create_dir_all(&remote_dir)?;
        fs::create_dir_all(&seed_dir)?;
        git_init_bare(&remote_dir)?;
        git_init(&seed_dir)?;
        git(
            seed_dir.as_path(),
            ["commit", "--allow-empty", "-m", "Initial commit"],
        )?;
        git(seed_dir.as_path(), ["branch", "-M", "main"])?;
        git(
            seed_dir.as_path(),
            ["remote", "add", "origin", remote_dir.to_str().unwrap()],
        )?;
        git(seed_dir.as_path(), ["push", "-u", "origin", "main"])?;

        git_clone(&remote_dir, &clone_a)?;
        git_clone(&remote_dir, &clone_b)?;

        let barrier = Arc::new(Barrier::new(2));
        let batch_a = test_batch("batch-a", "session-shared", json!({"writer": "A"}));
        let batch_b = test_batch("batch-b", "session-shared", json!({"writer": "B"}));
        let remote_dir_a = remote_dir.clone();
        let remote_dir_b = remote_dir.clone();

        let barrier_a = Arc::clone(&barrier);
        let barrier_b = Arc::clone(&barrier);
        let thread_a = thread::spawn(move || -> Result<SyncSummary> {
            let sync = RepoSync::new(
                clone_a,
                SyncOptions {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                    remote_url: remote_dir_a.display().to_string(),
                    push: true,
                },
            )?;
            sync.import_batches_with_after_pull(&[batch_a], || {
                barrier_a.wait();
                Ok(())
            })
        });
        let thread_b = thread::spawn(move || -> Result<SyncSummary> {
            let sync = RepoSync::new(
                clone_b,
                SyncOptions {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                    remote_url: remote_dir_b.display().to_string(),
                    push: true,
                },
            )?;
            sync.import_batches_with_after_pull(&[batch_b], || {
                barrier_b.wait();
                Ok(())
            })
        });

        let summary_a = thread_a.join().expect("thread A panicked")?;
        let summary_b = thread_b.join().expect("thread B panicked")?;

        assert_eq!(summary_a.imported_files, 1);
        assert_eq!(summary_b.imported_files, 1);
        assert!(summary_a.created_commit);
        assert!(summary_b.created_commit);
        assert!(summary_a.pushed);
        assert!(summary_b.pushed);

        git_clone(&remote_dir, &verify_dir)?;
        assert!(
            verify_dir
                .join("sessions")
                .join("session-shared")
                .join("batches")
                .join("batch-a.json")
                .exists()
        );
        assert!(
            verify_dir
                .join("sessions")
                .join("session-shared")
                .join("batches")
                .join("batch-b.json")
                .exists()
        );

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(seed_dir)?;
        fs::remove_dir_all(verify_dir)?;
        Ok(())
    }

    fn git_init(path: &Path) -> Result<()> {
        git(path, ["init"])?;
        Ok(())
    }

    fn git_init_bare(path: &Path) -> Result<()> {
        git(path, ["init", "--bare"])?;
        Ok(())
    }

    fn git_clone(remote: &Path, target: &Path) -> Result<()> {
        let target_parent = target.parent().expect("clone target must have parent");
        fs::create_dir_all(target_parent)?;
        git(
            target_parent,
            [
                "clone",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                target.file_name().unwrap().to_str().unwrap(),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn bootstraps_missing_repo_by_cloning_remote() -> Result<()> {
        let remote_dir = temp_dir("bootstrap-remote");
        let seed_dir = temp_dir("bootstrap-seed");
        let target_dir = temp_dir("bootstrap-target");

        fs::create_dir_all(&remote_dir)?;
        fs::create_dir_all(&seed_dir)?;
        git_init_bare(&remote_dir)?;
        git_init(&seed_dir)?;
        git(
            seed_dir.as_path(),
            ["commit", "--allow-empty", "-m", "Initial commit"],
        )?;
        git(seed_dir.as_path(), ["branch", "-M", "main"])?;
        git(
            seed_dir.as_path(),
            ["remote", "add", "origin", remote_dir.to_str().unwrap()],
        )?;
        git(seed_dir.as_path(), ["push", "-u", "origin", "main"])?;
        fs::remove_dir_all(&target_dir).ok();

        let _sync = RepoSync::new(
            target_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote_dir.display().to_string(),
                push: false,
            },
        )?;

        assert!(target_dir.join(".git").exists());

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(seed_dir)?;
        fs::remove_dir_all(target_dir)?;
        Ok(())
    }

    #[test]
    fn bootstraps_empty_remote_and_pushes_first_branch() -> Result<()> {
        let remote_dir = temp_dir("empty-remote");
        let target_dir = temp_dir("empty-target");
        let verify_dir = temp_dir("empty-verify");

        fs::create_dir_all(&remote_dir)?;
        git_init_bare(&remote_dir)?;
        fs::remove_dir_all(&target_dir).ok();

        let batch = test_batch("batch-empty", "session-empty", json!({"writer": "A"}));
        let sync = RepoSync::new(
            target_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote_dir.display().to_string(),
                push: true,
            },
        )?;
        let summary = sync.import_batches(&[batch])?;

        assert_eq!(summary.imported_files, 1);
        assert!(summary.created_commit);
        assert!(summary.pushed);

        git_clone(&remote_dir, &verify_dir)?;
        assert!(
            verify_dir
                .join("sessions")
                .join("session-empty")
                .join("batches")
                .join("batch-empty.json")
                .exists()
        );

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(target_dir)?;
        fs::remove_dir_all(verify_dir)?;
        Ok(())
    }

    #[test]
    fn prepare_repo_verifies_existing_repo_access() -> Result<()> {
        let remote_dir = temp_dir("prepare-existing-remote");
        let seed_dir = temp_dir("prepare-existing-seed");

        fs::create_dir_all(&remote_dir)?;
        fs::create_dir_all(&seed_dir)?;
        git_init_bare(&remote_dir)?;
        git_init(&seed_dir)?;
        git(
            seed_dir.as_path(),
            ["commit", "--allow-empty", "-m", "Initial commit"],
        )?;
        git(seed_dir.as_path(), ["branch", "-M", "main"])?;
        git(
            seed_dir.as_path(),
            ["remote", "add", "origin", remote_dir.to_str().unwrap()],
        )?;
        git(seed_dir.as_path(), ["push", "-u", "origin", "main"])?;

        let status = prepare_repo(&seed_dir, remote_dir.to_str().unwrap(), "main")?;
        assert_eq!(status, RepoSetupStatus::ExistingRepo);

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(seed_dir)?;
        Ok(())
    }

    fn git<I, S>(path: &Path, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let status = std::process::Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.name=codex-session-sync-tests",
                "-c",
                "user.email=codex-session-sync-tests@local",
            ])
            .args(args)
            .status()?;
        anyhow::ensure!(status.success(), "git command failed");
        Ok(())
    }

    fn test_batch(batch_id: &str, session_id: &str, payload: serde_json::Value) -> StoredBatch {
        StoredBatch {
            path: PathBuf::from(format!("{batch_id}.json")),
            batch: SpoolBatch {
                schema_version: 1,
                batch_id: batch_id.to_string(),
                ingested_at_unix_ms: 1,
                source_path: format!("/tmp/{session_id}.jsonl"),
                source_session_id: Some(session_id.to_string()),
                source_device: 1,
                source_inode: 2,
                source_size: 3,
                source_sha256: "abc".to_string(),
                change_kind: ChangeKind::Appended,
                start_line: 10,
                end_line: 11,
                record_count: 1,
                records: vec![payload],
            },
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("codex-session-sync-{label}-{suffix}"))
    }
}
