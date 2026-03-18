# codex-session-sync

`codex-session-sync` is a Rust service for synchronizing Codex session JSONL files across machines through a Git repository.

The local Codex client writes JSONL session files under `~/.codex/sessions`. This project scans those files, stores individual messages as immutable objects in a separate Git repository, and projects repository contents back into the live local sessions tree. The local session tree remains the user-facing view. The Git repo is the shared message store.

The current implementation is CLI-first. It supports interactive first-time setup, one-shot inspection, one-shot sync, and a polling daemon loop.

## Quick Start

After installing the package and enabling the user service, the simplest setup path is:

```bash
codex-session-sync --configure
```

That flow will:

- ask for the remote Git repository URL
- keep the default branch as `main` unless your existing config already says otherwise
- use `~/.codex/session-sync-repo` as the local clone unless your existing config already says otherwise
- verify remote access by preparing the local repo, cloning it if needed
- support a brand-new empty remote repository with no branches yet
- write `~/.codex/sync.toml`
- try to restart `codex-session-sync.service`

After that, verify the service:

```bash
systemctl --user status codex-session-sync.service
journalctl --user -u codex-session-sync.service -f
```

## Sync Configuration

Sync configuration lives in:

```text
~/.codex/sync.toml
```

Current fields:

```toml
remote_url = "git@github.com:example/codex-session-sync.git"
branch = "main" # optional, defaults to "main"
repo_path = "/home/alice/.codex/session-sync-repo" # optional
```

Notes:

- `remote_url` is required for syncing.
- `branch` is optional and defaults to `main`.
- `repo_path` is optional.
- if `repo_path` is omitted, the local clone defaults to `~/.codex/session-sync-repo`
- if that local repo path does not exist yet, the sync code will automatically clone `remote_url` into it
- an empty remote repository is supported; the first successful sync will create the configured branch there
- when bootstrapping an empty remote, the first sync also creates a top-level `README.md` explaining that the repository is a Codex session data store

If you prefer to manage the file manually, create `~/.codex/sync.toml` yourself. If the user service was already installed before that file existed, it may have started once and then exited cleanly. After creating the config file, start or restart the user service:

```bash
systemctl --user restart codex-session-sync.service
```

If it was not running yet, `start` is also fine:

```bash
systemctl --user start codex-session-sync.service
```

To verify that it is working:

```bash
systemctl --user status codex-session-sync.service
journalctl --user -u codex-session-sync.service -f
```

Useful things to check on first run:

- the service stays active instead of exiting immediately
- `~/.codex/session-sync-repo` (or your configured `repo_path`) gets cloned if it did not already exist
- `~/.local/state/codex-session-sync/` or `$XDG_STATE_HOME/codex-session-sync/` starts receiving state files
- the journal shows successful sync cycles

## Design

The detailed design is in [DESIGN.md](/Users/warren/source/codex-session-sync/DESIGN.md).

At a high level:

1. Scan local live session files under `~/.codex/sessions`.
2. Store each JSONL message as an immutable object in the sync repo.
3. Pull remote changes and push local changes through Git.
4. Reproject repository messages back into local session files.

The repository is sharded by session hash and stores message objects named by sortable UTC timestamp plus message hash:

```text
sessions/<aa>/<bb>/<session_hash>/messages/<YYYYMMDDHHmmssfff>-<message_hash>.json
```

## Current Features

- JSONL session scanning and parsing
- Git-backed message object store
- Projection of remote sessions back into `~/.codex/sessions`
- Recovery shadows for late writes to replaced local files
- File-based local state under the user state directory
- Local same-checkout sync lock with `.codex-session-sync.lock`
- Polling daemon loop
- Tests for multi-clone convergence and shadow-based late-write recovery

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

Inspect the current local session tree:

```bash
cargo run -- inspect --limit 20
```

This reports live session files, shadow recovery files, and scan warnings.

### Sync Once

Run one full sync cycle:

```bash
cargo run -- sync-repo
```

This does all of the following:

1. pull the sync repo
2. scan live local session files
3. recover messages from shadow files
4. write missing message objects into the repo clone
5. commit and optionally push
6. reproject repository contents back into the local session tree

Useful flags:

```bash
cargo run -- sync-repo --state-dir ~/.local/state/codex-session-sync
cargo run -- sync-repo --repo /path/to/central-repo
cargo run -- sync-repo --no-push
```

`--no-push` still pulls remote changes and still reprojects the local session tree. It only disables the final push step.

### Daemon

Run the polling daemon loop:

```bash
cargo run -- daemon
```

Useful flags:

```bash
cargo run -- daemon --interval-secs 10
cargo run -- daemon --no-push
cargo run -- daemon --max-iterations 1
cargo run -- daemon --config ~/.codex/sync.toml
```

The daemon currently polls rather than using filesystem notifications.
If the config file does not exist, the daemon exits successfully without doing any work.

### Configure

Run interactive first-time setup:

```bash
cargo run -- --configure
```

Or, for an installed binary:

```bash
codex-session-sync --configure
```

This prompt currently asks for the remote repository URL and preserves the existing branch and repo path if you already have a config file. After collecting input, it verifies the repo, writes `~/.codex/sync.toml`, and attempts a `systemctl --user restart codex-session-sync.service`.

## Runtime Files

Runtime state lives under the user state directory:

- `$XDG_STATE_HOME/codex-session-sync/machine-id`
- `$XDG_STATE_HOME/codex-session-sync/last-projected-head`
- `$XDG_STATE_HOME/codex-session-sync/sessions/<aa>/<bb>/<session_hash>.toml`

If `XDG_STATE_HOME` is not set, this falls back to `~/.local/state`.

The sync repo itself gets a local coordination lock while a sync is in progress:

```text
.codex-session-sync.lock
```

This is a persistent lock file. The actual lock is held by the running process through an OS-backed file lock, so stale pathnames by themselves do not block future syncs.

If another process is already syncing the same checkout, the second process skips that sync cycle and retries later.

Projection may also create hidden shadow files next to live session files. Those are recovery artifacts for late writes against replaced inodes.

## Recommended Usage

Use a private repository as the central store. Do not point the tool at the live Codex session directory as a Git working tree.

A typical flow is:

1. Create a private remote repo.
2. Create `~/.codex/sync.toml` with its `remote_url`.
3. Optionally set `repo_path`; otherwise the tool will use `~/.codex/session-sync-repo`.
4. Run the daemon.
5. Let other machines run the same daemon with their own `~/.codex/sync.toml`.

Because the imported files are immutable message objects, concurrent clones mostly add different files instead of editing the same file.

## NixOS User Service

The flake now exports a NixOS module at `nixosModules.default`.

It is intended for `systemd.user.services`, not a system-wide daemon. The service runs while a user is logged in, which matches the expected Codex usage model.

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

- The user service is installed for all users.
- A user who does not have `~/.codex/sync.toml` will simply no-op and exit successfully.
- A user who does have `~/.codex/sync.toml` can be bootstrapped automatically because the local repo clone is created from `remote_url` if missing.
- The module passes explicit user-local paths for the config file, session root, and state directory.
- The package output wraps `git`, so the service does not depend on an external `git` being present in the user shell.

## Status

This is still evolving, but the core message-object sync path is now exercised by tests:

- same-checkout coordination via a local lock
- empty-remote bootstrap
- remote projection into a second machine's live sessions tree
- recovery of late writes through retained shadow files
