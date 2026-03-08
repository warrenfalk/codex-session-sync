# codex-session-sync

`codex-session-sync` is a Rust service for copying local Codex session logs into an append-only Git-backed store.

The local Codex client writes JSONL session files under `~/.codex/sessions`. This project watches those files by scanning them, detects whether each file is new, appended, or rewritten, and writes normalized batch files into a local spool. Those spool batches can then be imported into a separate Git repository that acts as the centralized store.

The current implementation is CLI-first. It supports one-shot inspection and ingestion, one-shot sync into a Git repo, and a polling daemon loop.

## Design

The sync flow has four stages:

1. Scan local Codex session files under `~/.codex/sessions`.
2. Compare each file to prior state stored in SQLite.
3. Write new or changed records into an append-only local spool.
4. Import spool batches into a separate Git repo as immutable files, then commit and optionally push.

The central repo layout is intentionally append-only:

```text
sessions/<session-id>/batches/<batch-id>.json
```

This avoids mutable per-session files and makes concurrent multi-clone sync much easier to merge.

## Current Features

- JSONL session scanning and parsing
- Append vs rewrite detection
- SQLite-backed local file state
- Append-only local spool
- Git-backed batch import
- Local same-checkout sync lock with `.codex-session-sync.lock`
- Polling daemon loop
- Tests for append detection, rewrite detection, local lock behavior, and multi-clone Git convergence

## Development Setup

Enter the flake dev shell:

```bash
nix develop
```

The shell provides:

- minimal Rust toolchain
- `cargo-nextest`
- `git`
- `pkg-config`
- `sqlite`

The shell also keeps Cargo and Rustup state inside the repo:

```text
.cargo-home/
.rustup-home/
```

## Build And Test

```bash
cargo check
cargo test
```

## Commands

### Inspect

Inspect the current local Codex session tree without changing state:

```bash
cargo run -- inspect --limit 20
```

Inspect and update the local SQLite state snapshot:

```bash
cargo run -- inspect --write-state
```

### Ingest Once

Scan local sessions, detect changes, and write batches into the local spool:

```bash
cargo run -- ingest-once
```

By default this uses:

- session root: `~/.codex/sessions`
- state DB: `$XDG_STATE_HOME/codex-session-sync/state.sqlite3`
- spool dir: `$XDG_STATE_HOME/codex-session-sync/spool`

If `XDG_STATE_HOME` is not set, the fallback is:

- `~/.local/state/codex-session-sync/state.sqlite3`
- `~/.local/state/codex-session-sync/spool`

### Sync Once

Import pending spool batches into a separate Git repo and commit them there:

```bash
cargo run -- sync-repo --repo /path/to/central-repo
```

If you want to skip pushing and only create a local commit in that repo:

```bash
cargo run -- sync-repo --repo /path/to/central-repo --no-push
```

The sync target must already be a Git repository. If the repo has an `origin` remote and you do not pass `--no-push`, the tool will:

1. `pull --rebase`
2. import immutable batch files
3. commit
4. push
5. retry with rebase if the push races with another clone

### Daemon

Run the polling daemon loop:

```bash
cargo run -- daemon --repo /path/to/central-repo
```

Useful flags:

```bash
cargo run -- daemon --repo /path/to/central-repo --interval-secs 10
cargo run -- daemon --repo /path/to/central-repo --no-push
cargo run -- daemon --repo /path/to/central-repo --max-iterations 1
```

The daemon currently polls rather than using filesystem notifications.

## Runtime Files

Runtime state lives under the user state directory:

- `$XDG_STATE_HOME/codex-session-sync/state.sqlite3`
- `$XDG_STATE_HOME/codex-session-sync/spool/pending/`
- `$XDG_STATE_HOME/codex-session-sync/spool/processed/`

If `XDG_STATE_HOME` is not set, this falls back to `~/.local/state`.

The sync repo itself gets a local coordination lock while a sync is in progress:

```text
.codex-session-sync.lock/
```

If another process is already syncing the same checkout, the second process skips that sync cycle and retries later.

## Recommended Usage

Use a dedicated clone of a private repository as the central store. Do not point the tool at the live Codex session directory as a Git working tree.

A typical flow is:

1. Create a private remote repo.
2. Clone it to a local path that this tool will use as the sync target.
3. Run the daemon against that local clone.
4. Let other machines run the same daemon against their own clones of the same remote.

Because the imported files are immutable batch files, concurrent clones mostly add different files instead of editing the same file.

## NixOS User Service

The flake now exports a NixOS module at `nixosModules.default`.

It is intended for `systemd.user.services`, not a system-wide daemon. The service runs while the target user is logged in, which matches the expected Codex usage model.

Example NixOS configuration:

```nix
{
  inputs.codex-session-sync.url = "path:/path/to/codex-session-sync";

  outputs = { self, nixpkgs, codex-session-sync, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        codex-session-sync.nixosModules.default
        ({ ... }: {
          services.codex-session-sync = {
            enable = true;
            user = "warren";
            repoPath = "%h/.local/share/codex-session-sync/repo";
            remote = "origin";
            branch = "main";
            intervalSeconds = 10;
            push = true;
          };
        })
      ];
    };
  };
}
```

Important notes:

- `user` should be set explicitly. The module uses `ConditionUser=` so the unit only runs for that user's systemd user manager.
- `repoPath` must point to an existing writable Git clone of the central repository.
- The module passes explicit user-local paths for the session root, state DB, and spool directory.
- The package output wraps `git`, so the service does not depend on an external `git` being present in the user shell.

## Status

This is an early implementation, but the core sync model is already exercised by tests:

- same-checkout coordination via a local lock
- local append and rewrite detection
- concurrent multi-clone convergence against a shared bare remote

The biggest remaining work is operational polish rather than basic protocol shape.
