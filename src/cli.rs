use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{
    ResolvedSyncConfig, default_codex_dir, default_config_path, load_sync_config, write_sync_config,
};
use crate::git_sync::{RepoSetupStatus, RepoSync, SyncOptions, prepare_repo};
use crate::scan::{ChangeKind, ScanWarning, ScannedSession, SessionScanner};
use crate::spool::{SpoolBatch, SpoolWriter};
use crate::state::{StateStore, StoredSession};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Sync Codex session logs into an append-only store"
)]
pub struct Cli {
    #[arg(long)]
    configure: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon(DaemonArgs),
    Inspect(InspectArgs),
    IngestOnce(IngestOnceArgs),
    SyncRepo(SyncRepoArgs),
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    #[arg(long, default_value_os_t = default_sessions_root())]
    root: PathBuf,

    #[arg(long, default_value_os_t = default_state_db())]
    state_db: PathBuf,
}

#[derive(Debug, Clone, Args)]
struct InspectArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value_t = 20)]
    limit: usize,

    #[arg(long)]
    write_state: bool,
}

#[derive(Debug, Clone, Args)]
struct IngestOnceArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value_os_t = default_spool_dir())]
    spool_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
struct SyncRepoArgs {
    #[arg(long, default_value_os_t = default_spool_dir())]
    spool_dir: PathBuf,

    #[arg(long)]
    repo: Option<PathBuf>,

    #[arg(long, default_value_os_t = default_config_path())]
    config: PathBuf,

    #[arg(long)]
    remote_url: Option<String>,

    #[arg(long, default_value = "origin")]
    remote: String,

    #[arg(long)]
    branch: Option<String>,

    #[arg(long)]
    no_push: bool,
}

#[derive(Debug, Clone, Args)]
struct DaemonArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value_os_t = default_spool_dir())]
    spool_dir: PathBuf,

    #[arg(long)]
    repo: Option<PathBuf>,

    #[arg(long, default_value_os_t = default_config_path())]
    config: PathBuf,

    #[arg(long)]
    remote_url: Option<String>,

    #[arg(long, default_value = "origin")]
    remote: String,

    #[arg(long)]
    branch: Option<String>,

    #[arg(long)]
    no_push: bool,

    #[arg(long, default_value_t = 10)]
    interval_secs: u64,

    #[arg(long)]
    max_iterations: Option<usize>,
}

pub fn run(cli: Cli) -> Result<()> {
    match (cli.configure, cli.command) {
        (true, None) => configure(),
        (true, Some(_)) => bail!("--configure cannot be combined with a subcommand"),
        (false, Some(Command::Daemon(args))) => daemon(args),
        (false, Some(Command::Inspect(args))) => inspect(args),
        (false, Some(Command::IngestOnce(args))) => ingest_once(args),
        (false, Some(Command::SyncRepo(args))) => sync_repo(args),
        (false, None) => bail!("no command provided; use --configure or a subcommand"),
    }
}

fn configure() -> Result<()> {
    let config_path = default_config_path();
    let existing = load_sync_config(&config_path)?;
    let config = prompt_sync_config(&config_path, existing.as_ref())?;
    let repo_status = prepare_repo(&config.repo_path, &config.remote_url, &config.branch)
        .with_context(|| {
            format!(
                "failed to prepare local repo {} from {}",
                config.repo_path.display(),
                config.remote_url
            )
        })?;
    write_sync_config(&config)?;

    println!("config: {}", config.path.display());
    println!("remote_url: {}", config.remote_url);
    println!("branch: {}", config.branch);
    println!("repo_path: {}", config.repo_path.display());
    println!(
        "repo_status: {}",
        match repo_status {
            RepoSetupStatus::ExistingRepo => "existing_repo_verified",
            RepoSetupStatus::Cloned => "cloned",
        }
    );

    match restart_user_service("codex-session-sync.service") {
        Ok(()) => println!("daemon_restart: ok"),
        Err(error) => {
            println!("daemon_restart: skipped");
            println!("daemon_restart_error: {error}");
        }
    }

    Ok(())
}

fn inspect(args: InspectArgs) -> Result<()> {
    let previous = load_state_if_present(&args.common.state_db)?;
    let scanner = SessionScanner::new(args.common.root.clone());
    let report = scanner.scan()?;
    let warnings = report.warnings;
    log_scan_warnings(&warnings);
    let sessions = report.sessions;

    let mut counts = BTreeMap::<ChangeKind, usize>::new();
    let mut rows = Vec::with_capacity(sessions.len());

    if args.write_state {
        let mut state = StateStore::open(&args.common.state_db)?;
        state.begin_transaction()?;
        for session in &sessions {
            let change = classify(previous.get(&session.path_key), session);
            *counts.entry(change).or_insert(0) += 1;
            rows.push((change, session));
            state.upsert(session)?;
        }
        state.commit()?;
    } else {
        for session in &sessions {
            let change = classify(previous.get(&session.path_key), session);
            *counts.entry(change).or_insert(0) += 1;
            rows.push((change, session));
        }
    }

    println!("root: {}", args.common.root.display());
    println!("sessions: {}", sessions.len());
    println!("warnings: {}", warnings.len());
    println!("changes: {}", format_counts_line(&counts));

    for (change, session) in rows.into_iter().take(args.limit) {
        println!(
            "{change:<10} {:>6} lines  {:<36} {}",
            session.line_count,
            session.session_id.as_deref().unwrap_or("-"),
            session.path.display()
        );
    }

    Ok(())
}

fn ingest_once(args: IngestOnceArgs) -> Result<()> {
    let summary = run_ingest_once(&args.common.root, &args.common.state_db, &args.spool_dir)?;

    println!("root: {}", args.common.root.display());
    println!("state_db: {}", args.common.state_db.display());
    println!("spool_dir: {}", args.spool_dir.display());
    println!("sessions: {}", summary.sessions);
    println!("warnings: {}", summary.warnings);
    println!("batches_written: {}", summary.batches_written);
    println!("changes: {}", format_counts_line(&summary.counts));

    Ok(())
}

fn sync_repo(args: SyncRepoArgs) -> Result<()> {
    let Some(sync_config) =
        resolve_sync_config(&args.config, args.repo, args.remote_url, args.branch, true)?
    else {
        println!("config: {}", args.config.display());
        println!("skipped: true");
        println!("reason: missing_config");
        return Ok(());
    };

    let summary = run_sync_repo(
        &args.spool_dir,
        &sync_config,
        SyncOptions {
            remote: args.remote,
            branch: sync_config.branch.clone(),
            remote_url: sync_config.remote_url.clone(),
            push: !args.no_push,
        },
    )?;

    println!("config: {}", sync_config.path.display());
    println!("repo: {}", sync_config.repo_path.display());
    println!("spool_dir: {}", args.spool_dir.display());
    println!("pending_batches: {}", summary.pending_batches);
    println!("imported_files: {}", summary.imported_files);
    println!("created_commit: {}", summary.created_commit);
    println!("pushed: {}", summary.pushed);
    println!("skipped_due_to_lock: {}", summary.skipped_due_to_lock);

    Ok(())
}

fn daemon(args: DaemonArgs) -> Result<()> {
    let Some(sync_config) =
        resolve_sync_config(&args.config, args.repo, args.remote_url, args.branch, false)?
    else {
        tracing::info!(config = %args.config.display(), "sync config not present; exiting");
        return Ok(());
    };

    let mut iteration = 0usize;

    loop {
        iteration += 1;
        tracing::info!("starting daemon iteration {}", iteration);

        match run_ingest_once(&args.common.root, &args.common.state_db, &args.spool_dir) {
            Ok(ingest) => tracing::info!(
                iteration,
                sessions = ingest.sessions,
                batches_written = ingest.batches_written,
                changes = %format_counts_line(&ingest.counts),
                "ingest cycle complete"
            ),
            Err(error) => tracing::error!(iteration, error = %error, "ingest cycle failed"),
        }

        match run_sync_repo(
            &args.spool_dir,
            &sync_config,
            SyncOptions {
                remote: args.remote.clone(),
                branch: sync_config.branch.clone(),
                remote_url: sync_config.remote_url.clone(),
                push: !args.no_push,
            },
        ) {
            Ok(sync) => tracing::info!(
                iteration,
                pending_batches = sync.pending_batches,
                imported_files = sync.imported_files,
                created_commit = sync.created_commit,
                pushed = sync.pushed,
                skipped_due_to_lock = sync.skipped_due_to_lock,
                "sync cycle complete"
            ),
            Err(error) => tracing::error!(iteration, error = %error, "sync cycle failed"),
        }

        if args
            .max_iterations
            .is_some_and(|max_iterations| iteration >= max_iterations)
        {
            tracing::info!(iteration, "daemon loop complete");
            break;
        }

        thread::sleep(Duration::from_secs(args.interval_secs));
    }

    Ok(())
}

fn build_batch(
    previous: Option<&StoredSession>,
    session: &ScannedSession,
    change: ChangeKind,
) -> Result<Option<SpoolBatch>> {
    let (start_line, records) = match change {
        ChangeKind::Unchanged => return Ok(None),
        ChangeKind::New | ChangeKind::Rewritten => (0usize, session.records.clone()),
        ChangeKind::Appended => {
            let previous_line_count = previous.map(|entry| entry.line_count).unwrap_or(0usize);
            let new_records = session
                .records
                .iter()
                .skip(previous_line_count)
                .cloned()
                .collect::<Vec<_>>();
            (previous_line_count, new_records)
        }
    };

    Ok(Some(SpoolBatch::from_session(
        session, change, start_line, records,
    )?))
}

fn classify(previous: Option<&StoredSession>, session: &ScannedSession) -> ChangeKind {
    previous
        .map(|entry| entry.classify(session))
        .unwrap_or(ChangeKind::New)
}

fn load_state_if_present(path: &Path) -> Result<BTreeMap<String, StoredSession>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    StateStore::open(path)?.load_all()
}

fn run_ingest_once(root: &Path, state_db: &Path, spool_dir: &Path) -> Result<IngestSummary> {
    let mut state = StateStore::open(state_db)?;
    let previous = state.load_all()?;
    let scanner = SessionScanner::new(root.to_path_buf());
    let report = scanner.scan()?;
    log_scan_warnings(&report.warnings);
    let sessions = report.sessions;
    let spool = SpoolWriter::new(spool_dir.to_path_buf())?;

    state.begin_transaction()?;

    let mut counts = BTreeMap::<ChangeKind, usize>::new();
    let mut batches_written = 0usize;

    for session in &sessions {
        let change = classify(previous.get(&session.path_key), session);
        *counts.entry(change).or_insert(0) += 1;

        if let Some(batch) = build_batch(previous.get(&session.path_key), session, change)? {
            spool.write_batch(&batch)?;
            batches_written += 1;
        }

        state.upsert(session)?;
    }

    state.commit()?;

    Ok(IngestSummary {
        sessions: sessions.len(),
        warnings: report.warnings.len(),
        batches_written,
        counts,
    })
}

fn run_sync_repo(
    spool_dir: &Path,
    sync_config: &ResolvedSyncConfig,
    options: SyncOptions,
) -> Result<SyncRunSummary> {
    let spool = SpoolWriter::new(spool_dir.to_path_buf())?;
    let pending = spool.load_pending_batches()?;

    if pending.is_empty() {
        return Ok(SyncRunSummary {
            pending_batches: 0,
            imported_files: 0,
            created_commit: false,
            pushed: false,
            skipped_due_to_lock: false,
        });
    }

    let repo = RepoSync::new(sync_config.repo_path.clone(), options)?;
    let summary = repo.import_batches(&pending)?;

    if !summary.skipped_due_to_lock {
        for batch in &pending {
            spool.mark_processed(batch)?;
        }
    }

    Ok(SyncRunSummary {
        pending_batches: pending.len(),
        imported_files: summary.imported_files,
        created_commit: summary.created_commit,
        pushed: summary.pushed,
        skipped_due_to_lock: summary.skipped_due_to_lock,
    })
}

fn default_sessions_root() -> PathBuf {
    default_codex_dir().join("sessions")
}

fn default_repo_path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_dir)
        .join("session-sync-repo")
}

fn default_state_db() -> PathBuf {
    default_state_home()
        .join("codex-session-sync")
        .join("state.sqlite3")
}

fn default_spool_dir() -> PathBuf {
    default_state_home()
        .join("codex-session-sync")
        .join("spool")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn default_state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|path| path.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_sync_config(
    config_path: &Path,
    repo_override: Option<PathBuf>,
    remote_url_override: Option<String>,
    branch_override: Option<String>,
    require_config: bool,
) -> Result<Option<ResolvedSyncConfig>> {
    let file_config = load_sync_config(config_path)?;
    if file_config.is_none()
        && remote_url_override.is_none()
        && repo_override.is_none()
        && !require_config
    {
        return Ok(None);
    }

    let mut config = match file_config {
        Some(config) => config,
        None => {
            if require_config || remote_url_override.is_some() || repo_override.is_some() {
                ResolvedSyncConfig {
                    path: config_path.to_path_buf(),
                    remote_url: String::new(),
                    branch: "main".to_string(),
                    repo_path: default_codex_dir().join("session-sync-repo"),
                }
            } else {
                return Ok(None);
            }
        }
    };

    if let Some(repo_override) = repo_override {
        config.repo_path = repo_override;
    }
    if let Some(remote_url_override) = remote_url_override {
        config.remote_url = remote_url_override;
    }
    if let Some(branch_override) = branch_override {
        config.branch = branch_override;
    }

    if config.remote_url.is_empty() {
        anyhow::bail!(
            "no remote_url configured; expected it in {} or via --remote-url",
            config.path.display()
        );
    }

    Ok(Some(config))
}

fn prompt_sync_config(
    config_path: &Path,
    existing: Option<&ResolvedSyncConfig>,
) -> Result<ResolvedSyncConfig> {
    let remote_url = prompt_required(
        "Remote repository URL",
        existing.map(|config| config.remote_url.as_str()),
    )?;
    let branch = existing
        .map(|config| config.branch.clone())
        .unwrap_or_else(|| "main".to_string());
    let repo_path = existing
        .map(|config| config.repo_path.clone())
        .unwrap_or_else(|| default_repo_path_for_config(config_path));

    println!("branch: {}", branch);
    println!("repo_path: {}", repo_path.display());

    Ok(ResolvedSyncConfig {
        path: config_path.to_path_buf(),
        remote_url,
        branch,
        repo_path,
    })
}

fn prompt_required(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        match default {
            Some(default) => write!(stdout, "{label} [{default}]: ")?,
            None => write!(stdout, "{label}: ")?,
        }
        stdout.flush()?;

        line.clear();
        stdin.read_line(&mut line)?;
        let value = line.trim();

        if !value.is_empty() {
            return Ok(value.to_string());
        }
        if let Some(default) = default {
            return Ok(default.to_string());
        }
    }
}

fn restart_user_service(service: &str) -> Result<()> {
    let output = ProcessCommand::new("systemctl")
        .args(["--user", "restart", service])
        .output()
        .context("failed to run systemctl --user restart")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("systemctl exited with status {}", output.status)
    };
    bail!("{detail}")
}

fn format_counts(counts: &BTreeMap<ChangeKind, usize>) -> Vec<(ChangeKind, usize)> {
    [
        ChangeKind::New,
        ChangeKind::Appended,
        ChangeKind::Rewritten,
        ChangeKind::Unchanged,
    ]
    .into_iter()
    .map(|kind| (kind, counts.get(&kind).copied().unwrap_or(0)))
    .collect()
}

fn format_counts_line(counts: &BTreeMap<ChangeKind, usize>) -> String {
    format_counts(counts)
        .into_iter()
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn log_scan_warnings(warnings: &[ScanWarning]) {
    for warning in warnings {
        tracing::warn!(path = %warning.path.display(), error = warning.message, "skipping session file");
    }
}

struct IngestSummary {
    sessions: usize,
    warnings: usize,
    batches_written: usize,
    counts: BTreeMap<ChangeKind, usize>,
}

struct SyncRunSummary {
    pending_batches: usize,
    imported_files: usize,
    created_commit: bool,
    pushed: bool,
    skipped_due_to_lock: bool,
}
