use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::config::ResolvedSyncConfig;
use crate::file_state::{FileState, SessionState};
use crate::git_sync::{RepoSync, SyncOptions};
use crate::message_store::{MessageStore, StoredMessage};
use crate::session_file::{ParsedSessionFile, ScanWarning, SessionFileScanner, shadow_path_for};

#[derive(Debug, Default)]
pub struct SyncEngineSummary {
    pub live_sessions: usize,
    pub shadow_files: usize,
    pub messages_written: usize,
    pub projected_sessions: usize,
    pub warnings: usize,
    pub created_commit: bool,
    pub pushed: bool,
    pub skipped_due_to_lock: bool,
}

pub fn sync_once(
    root: &Path,
    state_root: &Path,
    sync_config: &ResolvedSyncConfig,
    options: SyncOptions,
) -> Result<SyncEngineSummary> {
    let state = FileState::new(state_root.to_path_buf())?;
    let machine_id = state.machine_id()?;
    let repo = RepoSync::new(sync_config.repo_path.clone(), options)?;

    let Some(summary) = repo.try_run_locked(|repo| {
        let previous_projected_head = state.projected_head()?;
        repo.pull_remote()?;
        repo.ensure_store_readme()?;
        let mut sessions_to_project = if previous_projected_head.is_some() {
            repo.changed_session_hashes_since(previous_projected_head.as_deref())?
        } else {
            BTreeSet::new()
        };
        let store = MessageStore::new(repo.repo_path().to_path_buf());
        let scanner = SessionFileScanner::new(root.to_path_buf());

        let live_report = scanner.scan_live()?;
        log_warnings(&live_report.warnings);
        let shadow_report = scanner.scan_shadows()?;
        log_warnings(&shadow_report.warnings);

        let mut summary = SyncEngineSummary {
            live_sessions: live_report.files.len(),
            shadow_files: shadow_report.files.len(),
            warnings: live_report.warnings.len() + shadow_report.warnings.len(),
            ..SyncEngineSummary::default()
        };

        for file in &live_report.files {
            ensure_live_session_state(&state, file)?;
            let upsert = store.upsert_session_file(&machine_id, file)?;
            summary.messages_written += upsert.messages_written;
            sessions_to_project.extend(upsert.touched_sessions);
        }

        for file in &shadow_report.files {
            let upsert = store.upsert_session_file(&machine_id, file)?;
            summary.messages_written += upsert.messages_written;
            sessions_to_project.extend(upsert.touched_sessions);
        }

        if previous_projected_head.is_none() {
            sessions_to_project.extend(store.session_hashes()?);
        }

        for session_hash in sessions_to_project {
            if project_session(root, &state, &store, &machine_id, &session_hash)? {
                summary.projected_sessions += 1;
            }
        }

        let created_commit = repo.commit_all("Sync Codex session messages")?;
        let pushed = if created_commit {
            repo.push_remote()?
        } else {
            false
        };
        summary.created_commit = created_commit;
        summary.pushed = pushed;

        if let Some(head) = repo.current_head()? {
            state.set_projected_head(&head)?;
        }

        Ok(summary)
    })?
    else {
        return Ok(SyncEngineSummary {
            skipped_due_to_lock: true,
            ..SyncEngineSummary::default()
        });
    };

    Ok(summary)
}

fn ensure_live_session_state(state: &FileState, file: &ParsedSessionFile) -> Result<()> {
    let metadata = fs::metadata(&file.path)
        .with_context(|| format!("failed to read metadata for {}", file.path.display()))?;
    let mut session = state
        .load_session(&file.session_hash)?
        .unwrap_or(SessionState {
            session_id: file.session_id.clone(),
            session_hash: file.session_hash.clone(),
            local_path: file.path.clone(),
            last_scan_offset: None,
            last_scan_anchor_hash: None,
            last_known_size: None,
            last_known_mtime_ns: None,
        });
    session.local_path = file.path.clone();
    session.session_id = file.session_id.clone();
    session.last_scan_anchor_hash = file.lines.last().map(|line| line.message_hash.clone());
    session.last_scan_offset = Some(metadata.len());
    session.last_known_size = Some(metadata.len());
    session.last_known_mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_nanos()).ok());
    state.save_session(&session)?;
    Ok(())
}

fn project_session(
    root: &Path,
    state: &FileState,
    store: &MessageStore,
    machine_id: &str,
    session_hash: &str,
) -> Result<bool> {
    let mut messages = store.load_session_messages(session_hash)?;
    if messages.is_empty() {
        return Ok(false);
    }

    let local_path = resolve_local_path(root, state, &messages[0], session_hash)?;
    ensure_projection_target_safe(root, &local_path)?;
    let desired = render_messages(&messages);
    let current = fs::read(&local_path).ok();
    if current.as_deref() == Some(desired.as_slice()) {
        return Ok(false);
    }

    let mut shadow = None;
    if local_path.exists() {
        let shadow_path = shadow_path_for(&local_path, &nonce())?;
        fs::hard_link(&local_path, &shadow_path).with_context(|| {
            format!(
                "failed to create shadow {} from {}",
                shadow_path.display(),
                local_path.display()
            )
        })?;
        if let Ok(shadow_file) = parse_shadow_once(&shadow_path) {
            let upsert = store.upsert_session_file(machine_id, &shadow_file)?;
            if upsert.messages_written > 0 {
                messages = store.load_session_messages(session_hash)?;
            }
        }
        shadow = Some(shadow_path);
    }

    write_projection(&local_path, &render_messages(&messages), shadow.as_deref())?;
    Ok(true)
}

fn resolve_local_path(
    root: &Path,
    state: &FileState,
    first_message: &StoredMessage,
    session_hash: &str,
) -> Result<PathBuf> {
    if let Some(existing) = state.load_session(session_hash)? {
        return Ok(existing.local_path);
    }

    let parsed = OffsetDateTime::parse(&first_message.timestamp, &Rfc3339)
        .with_context(|| format!("failed to parse timestamp {}", first_message.timestamp))?;
    let year = parsed.year();
    let month = parsed.month() as u8;
    let day = parsed.day();
    let local_path = root
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{day:02}"))
        .join(format!("{session_hash}.jsonl"));
    let state_file = SessionState {
        session_id: first_message.session_id.clone(),
        session_hash: session_hash.to_string(),
        local_path: local_path.clone(),
        last_scan_offset: None,
        last_scan_anchor_hash: None,
        last_known_size: None,
        last_known_mtime_ns: None,
    };
    state.save_session(&state_file)?;
    Ok(local_path)
}

fn render_messages(messages: &[StoredMessage]) -> Vec<u8> {
    let mut rendered = messages
        .iter()
        .map(|message| message.raw_jsonl.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    rendered.push(b'\n');
    rendered
}

fn write_projection(path: &Path, bytes: &[u8], shadow: Option<&Path>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("jsonl.sync-tmp-{}", nonce()));
    {
        let mut file =
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to fsync {}", tmp.display()))?;
    }
    ensure_projection_target_unchanged(path, shadow)?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn ensure_projection_target_safe(root: &Path, path: &Path) -> Result<()> {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        bail!("refusing to project into non-jsonl path {}", path.display());
    }
    if crate::session_file::is_shadow_path(path) {
        bail!("refusing to project into shadow path {}", path.display());
    }

    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("refusing to project outside {}: {}", root.display(), path.display()))?;
    for component in relative.components() {
        use std::path::Component;
        if !matches!(component, Component::Normal(_)) {
            bail!("refusing to project into unsafe path {}", path.display());
        }
    }

    ensure_no_symlink_components(root, path)?;

    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("refusing to overwrite non-regular file {}", path.display());
        }
    }

    Ok(())
}

fn ensure_no_symlink_components(root: &Path, path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
    }
    ensure_not_symlink(&current)?;

    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("refusing to project outside {}: {}", root.display(), path.display()))?;
    for component in relative.components() {
        current.push(component.as_os_str());
        if current.exists() {
            ensure_not_symlink(&current)?;
        }
    }
    Ok(())
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    #[cfg(unix)]
    if metadata.file_type().is_symlink() || metadata.file_type().is_socket() || metadata.file_type().is_fifo()
    {
        bail!("refusing to project through special file {}", path.display());
    }
    #[cfg(not(unix))]
    if metadata.file_type().is_symlink() {
        bail!("refusing to project through symlink {}", path.display());
    }
    Ok(())
}

fn ensure_projection_target_unchanged(path: &Path, shadow: Option<&Path>) -> Result<()> {
    match shadow {
        Some(shadow) => ensure_same_file_identity(path, shadow),
        None => {
            if path.exists() {
                bail!(
                    "projection target {} appeared during write; refusing to replace it",
                    path.display()
                );
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn ensure_same_file_identity(path: &Path, shadow: &Path) -> Result<()> {
    let target = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let shadow_meta = fs::metadata(shadow)
        .with_context(|| format!("failed to inspect {}", shadow.display()))?;
    if target.dev() != shadow_meta.dev() || target.ino() != shadow_meta.ino() {
        bail!(
            "projection target {} changed after shadow creation; refusing to replace it",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file_identity(path: &Path, shadow: &Path) -> Result<()> {
    let target = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let shadow_meta = fs::metadata(shadow)
        .with_context(|| format!("failed to inspect {}", shadow.display()))?;
    if target.len() != shadow_meta.len() {
        bail!(
            "projection target {} changed after shadow creation; refusing to replace it",
            path.display()
        );
    }
    Ok(())
}

fn parse_shadow_once(path: &Path) -> Result<ParsedSessionFile> {
    let scanner = SessionFileScanner::new(
        path.parent()
            .map(PathBuf::from)
            .context("shadow file missing parent directory")?,
    );
    let report = scanner.scan_shadows()?;
    report
        .files
        .into_iter()
        .find(|file| file.path == path)
        .with_context(|| format!("failed to parse shadow {}", path.display()))
}

fn log_warnings(warnings: &[ScanWarning]) {
    for warning in warnings {
        tracing::warn!(path = %warning.path.display(), warning = %warning.message, "session scan warning");
    }
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::sync_once;
    use crate::config::ResolvedSyncConfig;
    use crate::file_state::{FileState, SessionState};
    use crate::git_sync::SyncOptions;

    #[test]
    fn syncs_remote_session_into_second_machine() -> Result<()> {
        let sandbox = temp_dir("sync-two-machines");
        let remote = sandbox.join("remote.git");
        let repo_a = sandbox.join("repo-a");
        let repo_b = sandbox.join("repo-b");
        let root_a = sandbox.join("sessions-a");
        let root_b = sandbox.join("sessions-b");
        let state_a = sandbox.join("state-a");
        let state_b = sandbox.join("state-b");

        fs::create_dir_all(&root_a)?;
        fs::create_dir_all(&root_b)?;
        git_init_bare(&remote)?;
        write_session_file(
            &root_a.join("2026/03/18/session-a.jsonl"),
            "session-1",
            &[
                "2026-03-18T21:00:00.000Z",
                "2026-03-18T21:00:01.000Z",
            ],
        )?;

        let config_a = sync_config(&repo_a, &remote);
        let config_b = sync_config(&repo_b, &remote);

        sync_once(
            &root_a,
            &state_a,
            &config_a,
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote.display().to_string(),
                push: true,
            },
        )?;
        sync_once(
            &root_b,
            &state_b,
            &config_b,
            SyncOptions {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                remote_url: remote.display().to_string(),
                push: true,
            },
        )?;

        let projected = find_first_jsonl(&root_b)?;
        let contents = fs::read_to_string(&projected)?;
        assert!(contents.contains("2026-03-18T21:00:00.000Z"));
        assert!(contents.contains("2026-03-18T21:00:01.000Z"));

        fs::remove_dir_all(sandbox)?;
        Ok(())
    }

    #[test]
    fn recovers_late_write_from_shadow_file() -> Result<()> {
        let sandbox = temp_dir("shadow-recovery");
        let remote = sandbox.join("remote.git");
        let repo_a = sandbox.join("repo-a");
        let repo_b = sandbox.join("repo-b");
        let root_a = sandbox.join("sessions-a");
        let root_b = sandbox.join("sessions-b");
        let state_a = sandbox.join("state-a");
        let state_b = sandbox.join("state-b");

        fs::create_dir_all(&root_a)?;
        fs::create_dir_all(&root_b)?;
        git_init_bare(&remote)?;
        let path_a = root_a.join("2026/03/18/session-a.jsonl");
        write_session_file(&path_a, "session-1", &["2026-03-18T21:00:00.000Z"])?;

        let config_a = sync_config(&repo_a, &remote);
        let config_b = sync_config(&repo_b, &remote);

        let options = SyncOptions {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            remote_url: remote.display().to_string(),
            push: true,
        };

        sync_once(&root_a, &state_a, &config_a, options.clone())?;
        sync_once(&root_b, &state_b, &config_b, options.clone())?;

        let projected = find_first_jsonl(&root_b)?;
        let mut old_handle = File::options().append(true).open(&projected)?;

        append_session_line(&path_a, "event", "2026-03-18T21:00:01.000Z")?;
        sync_once(&root_a, &state_a, &config_a, options.clone())?;
        sync_once(&root_b, &state_b, &config_b, options.clone())?;

        writeln!(
            old_handle,
            "{{\"timestamp\":\"2026-03-18T21:00:02.000Z\",\"type\":\"event\",\"payload\":{{\"value\":\"late\"}}}}"
        )?;
        old_handle.flush()?;

        sync_once(&root_b, &state_b, &config_b, options.clone())?;
        sync_once(&root_a, &state_a, &config_a, options)?;

        let updated = fs::read_to_string(find_first_jsonl(&root_b)?)?;
        assert!(updated.contains("2026-03-18T21:00:00.000Z"));
        assert!(updated.contains("2026-03-18T21:00:01.000Z"));
        assert!(updated.contains("2026-03-18T21:00:02.000Z"));

        fs::remove_dir_all(sandbox)?;
        Ok(())
    }

    #[test]
    fn refuses_to_project_outside_sessions_root() -> Result<()> {
        let sandbox = temp_dir("outside-root");
        let remote = sandbox.join("remote.git");
        let repo_a = sandbox.join("repo-a");
        let repo_b = sandbox.join("repo-b");
        let root_a = sandbox.join("sessions-a");
        let root_b = sandbox.join("sessions-b");
        let state_a = sandbox.join("state-a");
        let state_b = sandbox.join("state-b");
        let escaped = sandbox.join("escaped.jsonl");

        fs::create_dir_all(&root_a)?;
        fs::create_dir_all(&root_b)?;
        git_init_bare(&remote)?;
        write_session_file(
            &root_a.join("2026/03/18/session-a.jsonl"),
            "session-1",
            &["2026-03-18T21:00:00.000Z"],
        )?;

        let config_a = sync_config(&repo_a, &remote);
        let config_b = sync_config(&repo_b, &remote);
        let options = SyncOptions {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            remote_url: remote.display().to_string(),
            push: true,
        };

        sync_once(&root_a, &state_a, &config_a, options.clone())?;

        let state = FileState::new(state_b.clone())?;
        state.save_session(&SessionState {
            session_id: "session-1".to_string(),
            session_hash: sha256_hex("session-1"),
            local_path: escaped.clone(),
            last_scan_offset: None,
            last_scan_anchor_hash: None,
            last_known_size: None,
            last_known_mtime_ns: None,
        })?;

        let error = sync_once(&root_b, &state_b, &config_b, options).expect_err("sync should fail");
        assert!(error
            .to_string()
            .contains("refusing to project outside"));
        assert!(!escaped.exists());

        fs::remove_dir_all(sandbox)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_overwrite_symlink_target() -> Result<()> {
        use std::os::unix::fs::symlink;

        let sandbox = temp_dir("symlink-target");
        let remote = sandbox.join("remote.git");
        let repo_a = sandbox.join("repo-a");
        let repo_b = sandbox.join("repo-b");
        let root_a = sandbox.join("sessions-a");
        let root_b = sandbox.join("sessions-b");
        let state_a = sandbox.join("state-a");
        let state_b = sandbox.join("state-b");
        let real_target = sandbox.join("real-target.jsonl");

        fs::create_dir_all(&root_a)?;
        fs::create_dir_all(root_b.join("2026/03/18"))?;
        git_init_bare(&remote)?;
        write_session_file(
            &root_a.join("2026/03/18/session-a.jsonl"),
            "session-1",
            &["2026-03-18T21:00:00.000Z"],
        )?;

        let config_a = sync_config(&repo_a, &remote);
        let config_b = sync_config(&repo_b, &remote);
        let options = SyncOptions {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            remote_url: remote.display().to_string(),
            push: true,
        };

        sync_once(&root_a, &state_a, &config_a, options.clone())?;

        fs::write(&real_target, "do not touch\n")?;
        let symlink_path = root_b.join(format!("2026/03/18/{}.jsonl", sha256_hex("session-1")));
        symlink(&real_target, &symlink_path)?;

        let error = sync_once(&root_b, &state_b, &config_b, options).expect_err("sync should fail");
        assert!(error
            .to_string()
            .contains("refusing to project through special file"));
        assert_eq!(fs::read_to_string(&real_target)?, "do not touch\n");

        fs::remove_dir_all(sandbox)?;
        Ok(())
    }

    fn sync_config(repo: &Path, remote: &Path) -> ResolvedSyncConfig {
        ResolvedSyncConfig {
            path: repo.join("sync.toml"),
            remote_url: remote.display().to_string(),
            branch: "main".to_string(),
            repo_path: repo.to_path_buf(),
        }
    }

    fn write_session_file(path: &Path, session_id: &str, timestamps: &[&str]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut lines = Vec::new();
        for (index, timestamp) in timestamps.iter().enumerate() {
            if index == 0 {
                lines.push(format!(
                    "{{\"timestamp\":\"{timestamp}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\"}}}}"
                ));
            } else {
                lines.push(format!(
                    "{{\"timestamp\":\"{timestamp}\",\"type\":\"event\",\"payload\":{{\"index\":{index}}}}}"
                ));
            }
        }
        fs::write(path, lines.join("\n") + "\n")?;
        Ok(())
    }

    fn append_session_line(path: &Path, kind: &str, timestamp: &str) -> Result<()> {
        let mut file = File::options().append(true).open(path)?;
        writeln!(
            file,
            "{{\"timestamp\":\"{timestamp}\",\"type\":\"{kind}\",\"payload\":{{\"kind\":\"{kind}\"}}}}"
        )?;
        Ok(())
    }

    fn git_init_bare(path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        git(path, ["init", "--bare"])
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

    fn find_first_jsonl(root: &Path) -> Result<PathBuf> {
        for year in fs::read_dir(root)? {
            let year = year?;
            if !year.file_type()?.is_dir() {
                continue;
            }
            for month in fs::read_dir(year.path())? {
                let month = month?;
                if !month.file_type()?.is_dir() {
                    continue;
                }
                for day in fs::read_dir(month.path())? {
                    let day = day?;
                    if !day.file_type()?.is_dir() {
                        continue;
                    }
                    for file in fs::read_dir(day.path())? {
                        let file = file?;
                        if file.path().extension().and_then(|value| value.to_str()) == Some("jsonl") {
                            return Ok(file.path());
                        }
                    }
                }
            }
        }
        anyhow::bail!("no jsonl file found in {}", root.display())
    }

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("codex-session-sync-{label}-{suffix}"))
    }

    fn sha256_hex(value: &str) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(value.as_bytes());
        hex::encode(hasher.finalize())
    }
}
