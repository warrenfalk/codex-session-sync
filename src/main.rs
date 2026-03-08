mod cli;
mod config;
mod git_sync;
mod scan;
mod spool;
mod state;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codex_session_sync=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    cli::run(cli::Cli::parse())
}
