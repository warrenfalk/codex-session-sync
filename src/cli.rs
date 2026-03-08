use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::scan::{ChangeKind, ScannedSession, SessionScanner};
use crate::spool::{SpoolBatch, SpoolWriter};
use crate::state::{StateStore, StoredSession};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Sync Codex session logs into an append-only store"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect(InspectArgs),
    IngestOnce(IngestOnceArgs),
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

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Inspect(args) => inspect(args),
        Command::IngestOnce(args) => ingest_once(args),
    }
}

fn inspect(args: InspectArgs) -> Result<()> {
    let previous = load_state_if_present(&args.common.state_db)?;
    let scanner = SessionScanner::new(args.common.root.clone());
    let sessions = scanner.scan()?;

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
    println!(
        "changes: {}",
        format_counts(&counts)
            .into_iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

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
    let mut state = StateStore::open(&args.common.state_db)?;
    let previous = state.load_all()?;
    let scanner = SessionScanner::new(args.common.root.clone());
    let sessions = scanner.scan()?;
    let spool = SpoolWriter::new(args.spool_dir.clone())?;

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

    println!("root: {}", args.common.root.display());
    println!("state_db: {}", args.common.state_db.display());
    println!("spool_dir: {}", args.spool_dir.display());
    println!("sessions: {}", sessions.len());
    println!("batches_written: {batches_written}");
    println!(
        "changes: {}",
        format_counts(&counts)
            .into_iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

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

fn default_sessions_root() -> PathBuf {
    home_dir()
        .map(|path| path.join(".codex").join("sessions"))
        .unwrap_or_else(|| PathBuf::from(".codex/sessions"))
}

fn default_state_db() -> PathBuf {
    PathBuf::from("var").join("state.sqlite3")
}

fn default_spool_dir() -> PathBuf {
    PathBuf::from("var").join("spool")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
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
