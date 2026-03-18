use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

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

impl RepoSync {
    pub fn new(repo: PathBuf, options: SyncOptions) -> Result<Self> {
        ensure_repo_or_clone(&repo, &options.remote_url, &options.branch)?;
        Ok(Self { repo, options })
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo
    }

    pub fn try_run_locked<T, F>(&self, action: F) -> Result<Option<T>>
    where
        F: FnOnce(&Self) -> Result<T>,
    {
        let Some(_lock) = RepoLock::acquire(&self.repo)? else {
            return Ok(None);
        };
        ensure_clean_worktree(&self.repo)?;
        action(self).map(Some)
    }

    pub fn pull_remote(&self) -> Result<()> {
        if remote_exists(&self.repo, &self.options.remote)? {
            pull_rebase(&self.repo, &self.options.remote, &self.options.branch)?;
        }
        Ok(())
    }

    pub fn ensure_store_readme(&self) -> Result<bool> {
        if self.current_head()?.is_some() {
            return Ok(false);
        }

        let path = self.repo.join("README.md");
        if path.exists() {
            return Ok(false);
        }

        fs::write(&path, store_readme_contents())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(true)
    }

    pub fn changed_session_hashes_since(
        &self,
        previous_head: Option<&str>,
    ) -> Result<BTreeSet<String>> {
        let Some(previous_head) = previous_head else {
            return Ok(BTreeSet::new());
        };

        let current_head = self.current_head()?;
        if current_head.as_deref() == Some(previous_head) {
            return Ok(BTreeSet::new());
        }

        let output = git(
            &self.repo,
            ["diff", "--name-only", previous_head, "HEAD", "--", "sessions"],
        )?;

        let mut session_hashes = BTreeSet::new();
        for line in output.stdout.lines() {
            let mut parts = line.split('/');
            let Some(prefix) = parts.next() else {
                continue;
            };
            if prefix != "sessions" {
                continue;
            }
            let _shard_a = parts.next();
            let _shard_b = parts.next();
            let Some(session_hash) = parts.next() else {
                continue;
            };
            if session_hash.len() == 64 {
                session_hashes.insert(session_hash.to_string());
            }
        }

        Ok(session_hashes)
    }

    pub fn is_dirty(&self) -> Result<bool> {
        let output = git(
            &self.repo,
            [
                "status",
                "--porcelain",
                "--",
                ".",
                ":(exclude).codex-session-sync.lock",
            ],
        )?;
        Ok(!output.stdout.trim().is_empty())
    }

    pub fn commit_all(&self, message: &str) -> Result<bool> {
        if !self.is_dirty()? {
            return Ok(false);
        }
        git_add(&self.repo, ".")?;
        git_commit(&self.repo, message)?;
        Ok(true)
    }

    pub fn push_remote(&self) -> Result<bool> {
        if !self.options.push || !remote_exists(&self.repo, &self.options.remote)? {
            return Ok(false);
        }
        push_with_rebase_retry(&self.repo, &self.options.remote, &self.options.branch)?;
        Ok(true)
    }

    pub fn current_head(&self) -> Result<Option<String>> {
        let output = git_allow_failure(&self.repo, ["rev-parse", "HEAD"])?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(output.stdout.trim().to_string()))
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
            git(path, ["branch", "-M", branch])?;
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
            "sync repo {} has uncommitted changes; refusing to sync into a dirty worktree",
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
    git(
        repo,
        ["add", "--all", "--", pathspec, ":(exclude).codex-session-sync.lock"],
    )?;
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
    _file: File,
}

impl RepoLock {
    fn acquire(repo: &Path) -> Result<Option<Self>> {
        let path = repo.join(".codex-session-sync.lock");
        let mut file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open lock file {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                write_lock_metadata(&mut file)?;
                Ok(Some(Self { path, _file: file }))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to lock {}", path.display())),
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if let Err(error) = self._file.unlock() {
            tracing::warn!(error = %error, path = %self.path.display(), "failed to unlock repo lock");
        }
    }
}

fn write_lock_metadata(file: &mut File) -> Result<()> {
    let hostname = env::var("HOSTNAME").ok();
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "hostname": hostname,
    });
    let bytes = serde_json::to_vec_pretty(&payload)?;
    file.set_len(0).context("failed to clear lock file")?;
    file.seek(SeekFrom::Start(0))
        .context("failed to rewind lock file")?;
    file.write_all(&bytes)
        .context("failed to write lock metadata")?;
    file.write_all(b"\n")
        .context("failed to terminate lock metadata")?;
    file.sync_all().context("failed to sync lock metadata")?;
    Ok(())
}

fn store_readme_contents() -> &'static str {
    "# Codex Session Sync Store\n\nThis repository stores synchronized Codex session message objects for `codex-session-sync`.\nIt is a data store, not a normal source repository.\n\nThe `sessions/` tree contains immutable message objects keyed by session hash and message content.\n"
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{RepoSetupStatus, RepoSync, SyncOptions, prepare_repo};

    #[test]
    fn skips_sync_when_repo_lock_is_already_held() -> Result<()> {
        let repo_dir = temp_dir("repo-locked");
        fs::create_dir_all(&repo_dir)?;
        git_init(&repo_dir)?;
        let held_lock = super::RepoLock::acquire(&repo_dir)?.expect("lock should be acquired");

        let sync = RepoSync::new(
            repo_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: repo_dir.display().to_string(),
                push: false,
            },
        )?;
        let ran = sync.try_run_locked(|_| Ok(()))?;

        assert!(ran.is_none());
        drop(held_lock);

        let ran = sync.try_run_locked(|_| Ok("ok"))?;
        assert_eq!(ran, Some("ok"));

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
        let remote_dir_a = remote_dir.clone();
        let remote_dir_b = remote_dir.clone();

        let barrier_a = Arc::clone(&barrier);
        let barrier_b = Arc::clone(&barrier);
        let thread_a = thread::spawn(move || -> Result<()> {
            let sync = RepoSync::new(
                clone_a.clone(),
                SyncOptions {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                    remote_url: remote_dir_a.display().to_string(),
                    push: true,
                },
            )?;
            sync.try_run_locked(|repo| {
                repo.pull_remote()?;
                write_message_object(repo.repo_path(), SHARED_SESSION_HASH, "20260318210000000-a")?;
                barrier_a.wait();
                repo.commit_all("writer A")?;
                repo.push_remote()?;
                Ok(())
            })?;
            Ok(())
        });
        let thread_b = thread::spawn(move || -> Result<()> {
            let sync = RepoSync::new(
                clone_b.clone(),
                SyncOptions {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                    remote_url: remote_dir_b.display().to_string(),
                    push: true,
                },
            )?;
            sync.try_run_locked(|repo| {
                repo.pull_remote()?;
                write_message_object(repo.repo_path(), SHARED_SESSION_HASH, "20260318210001000-b")?;
                barrier_b.wait();
                repo.commit_all("writer B")?;
                repo.push_remote()?;
                Ok(())
            })?;
            Ok(())
        });

        thread_a.join().expect("thread A panicked")?;
        thread_b.join().expect("thread B panicked")?;

        git_clone(&remote_dir, &verify_dir)?;
        let session_dir = session_dir(&verify_dir, SHARED_SESSION_HASH).join("messages");
        let files = collect_file_names(&session_dir)?;
        assert!(files.contains(&"20260318210000000-a.json".to_string()));
        assert!(files.contains(&"20260318210001000-b.json".to_string()));

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(seed_dir)?;
        fs::remove_dir_all(verify_dir)?;
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

        let sync = RepoSync::new(
            target_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote_dir.display().to_string(),
                push: true,
            },
        )?;
        sync.ensure_store_readme()?;
        write_message_object(&target_dir, SHARED_SESSION_HASH, "20260318210000000-initial")?;
        let created = sync.commit_all("Initial sync")?;
        let pushed = sync.push_remote()?;

        assert!(created);
        assert!(pushed);

        git_clone(&remote_dir, &verify_dir)?;
        let session_dir = session_dir(&verify_dir, SHARED_SESSION_HASH).join("messages");
        let files = collect_file_names(&session_dir)?;
        assert!(files.contains(&"20260318210000000-initial.json".to_string()));
        let readme = fs::read_to_string(verify_dir.join("README.md"))?;
        assert!(readme.contains("Codex Session Sync Store"));

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(target_dir)?;
        fs::remove_dir_all(verify_dir)?;
        Ok(())
    }

    #[test]
    fn pulls_remote_changes_even_without_local_messages() -> Result<()> {
        let remote_dir = temp_dir("pull-remote");
        let seed_dir = temp_dir("pull-seed");
        let clone_dir = temp_dir("pull-clone");
        let writer_dir = temp_dir("pull-writer");

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

        git_clone(&remote_dir, &clone_dir)?;
        git_clone(&remote_dir, &writer_dir)?;
        write_message_object(&writer_dir, SHARED_SESSION_HASH, "20260318210000000-remote")?;
        git(writer_dir.as_path(), ["add", "."])?;
        git(writer_dir.as_path(), ["commit", "-m", "Add remote message"])?;
        git(writer_dir.as_path(), ["push", "origin", "main"])?;

        let sync = RepoSync::new(
            clone_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote_dir.display().to_string(),
                push: false,
            },
        )?;
        sync.pull_remote()?;

        let session_dir = session_dir(&clone_dir, SHARED_SESSION_HASH).join("messages");
        let files = collect_file_names(&session_dir)?;
        assert!(files.contains(&"20260318210000000-remote.json".to_string()));

        fs::remove_dir_all(remote_dir)?;
        fs::remove_dir_all(seed_dir)?;
        fs::remove_dir_all(clone_dir)?;
        fs::remove_dir_all(writer_dir)?;
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

    #[test]
    fn reports_changed_session_hashes_since_previous_head() -> Result<()> {
        let repo_dir = temp_dir("changed-session-repo");
        fs::create_dir_all(&repo_dir)?;
        git_init(&repo_dir)?;

        let sync = RepoSync::new(
            repo_dir.clone(),
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: repo_dir.display().to_string(),
                push: false,
            },
        )?;

        git(repo_dir.as_path(), ["commit", "--allow-empty", "-m", "Initial commit"])?;
        let previous_head = sync.current_head()?.expect("initial head");
        write_message_object(&repo_dir, SHARED_SESSION_HASH, "20260318210000000-local")?;
        sync.commit_all("Add message")?;

        let changed = sync.changed_session_hashes_since(Some(&previous_head))?;
        assert_eq!(
            changed,
            BTreeSet::from([SHARED_SESSION_HASH.to_string()])
        );

        fs::remove_dir_all(repo_dir)?;
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

    fn write_message_object(repo: &Path, session_hash: &str, stem: &str) -> Result<()> {
        let path = session_dir(repo, session_hash)
            .join("messages")
            .join(format!("{stem}.json"));
        let parent = path.parent().expect("message path parent");
        fs::create_dir_all(parent)?;
        fs::write(&path, "{}\n")?;
        Ok(())
    }

    fn session_dir(repo: &Path, session_hash: &str) -> PathBuf {
        repo.join("sessions")
            .join(&session_hash[..2])
            .join(&session_hash[2..4])
            .join(session_hash)
    }

    fn collect_file_names(dir: &Path) -> Result<BTreeSet<String>> {
        let mut files = BTreeSet::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(files)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("codex-session-sync-{label}-{suffix}"))
    }

    const SHARED_SESSION_HASH: &str =
        "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
}
