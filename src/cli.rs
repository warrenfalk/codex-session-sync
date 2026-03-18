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
use crate::git_sync::{RepoSetupStatus, SyncOptions, prepare_repo};
use crate::session_file::SessionFileScanner;
use crate::sync_engine::sync_once;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Sync Codex session logs into a Git-backed message store"
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
    SyncRepo(SyncRepoArgs),
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    #[arg(long, default_value_os_t = default_sessions_root())]
    root: PathBuf,

    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
struct InspectArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Debug, Clone, Args)]
struct SyncRepoArgs {
    #[command(flatten)]
    common: CommonArgs,

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
    sync: SyncRepoArgs,

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
    let scanner = SessionFileScanner::new(args.common.root.clone());
    let live = scanner.scan_live()?;
    let shadows = scanner.scan_shadows()?;

    println!("root: {}", args.common.root.display());
    println!("state_dir: {}", args.common.state_dir.display());
    println!("live_sessions: {}", live.files.len());
    println!("shadow_files: {}", shadows.files.len());
    println!("warnings: {}", live.warnings.len() + shadows.warnings.len());

    for file in live.files.into_iter().take(args.limit) {
        println!(
            "live   {:<36} {}",
            file.session_id,
            file.path.display()
        );
    }
    for file in shadows.files.into_iter().take(args.limit) {
        println!(
            "shadow {:<36} {}",
            file.session_id,
            file.path.display()
        );
    }

    Ok(())
}

fn sync_repo(args: SyncRepoArgs) -> Result<()> {
    let Some(sync_config) = resolve_sync_config(
        &args.config,
        args.repo,
        args.remote_url,
        args.branch,
        true,
    )? else {
        println!("config: {}", args.config.display());
        println!("skipped: true");
        println!("reason: missing_config");
        return Ok(());
    };

    let summary = sync_once(
        &args.common.root,
        &args.common.state_dir,
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
    println!("root: {}", args.common.root.display());
    println!("state_dir: {}", args.common.state_dir.display());
    println!("live_sessions: {}", summary.live_sessions);
    println!("shadow_files: {}", summary.shadow_files);
    println!("messages_written: {}", summary.messages_written);
    println!("projected_sessions: {}", summary.projected_sessions);
    println!("warnings: {}", summary.warnings);
    println!("created_commit: {}", summary.created_commit);
    println!("pushed: {}", summary.pushed);
    println!("skipped_due_to_lock: {}", summary.skipped_due_to_lock);

    Ok(())
}

fn daemon(args: DaemonArgs) -> Result<()> {
    let Some(sync_config) = resolve_sync_config(
        &args.sync.config,
        args.sync.repo.clone(),
        args.sync.remote_url.clone(),
        args.sync.branch.clone(),
        false,
    )? else {
        tracing::info!(config = %args.sync.config.display(), "sync config not present; exiting");
        return Ok(());
    };

    let mut iteration = 0usize;
    loop {
        iteration += 1;
        tracing::info!(iteration, "starting daemon iteration");

        match sync_once(
            &args.sync.common.root,
            &args.sync.common.state_dir,
            &sync_config,
            SyncOptions {
                remote: args.sync.remote.clone(),
                branch: sync_config.branch.clone(),
                remote_url: sync_config.remote_url.clone(),
                push: !args.sync.no_push,
            },
        ) {
            Ok(summary) => tracing::info!(
                iteration,
                live_sessions = summary.live_sessions,
                shadow_files = summary.shadow_files,
                messages_written = summary.messages_written,
                projected_sessions = summary.projected_sessions,
                warnings = summary.warnings,
                created_commit = summary.created_commit,
                pushed = summary.pushed,
                skipped_due_to_lock = summary.skipped_due_to_lock,
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

fn default_sessions_root() -> PathBuf {
    default_codex_dir().join("sessions")
}

fn default_state_dir() -> PathBuf {
    default_state_home().join("codex-session-sync")
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
        bail!(
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

fn default_repo_path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_dir)
        .join("session-sync-repo")
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
