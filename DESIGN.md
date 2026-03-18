# codex-session-sync Design

## Status

This document describes the intended message-object design for `codex-session-sync`.
It is the target architecture. It is more authoritative than the current implementation, which still reflects an earlier batch-oriented design.

## Goals

- Keep `~/.codex/sessions` as the live session directory used by Codex.
- Sync sessions across machines through a Git repository.
- Avoid sync feedback loops when remote sessions are materialized back into the local sessions directory.
- Avoid SQLite. Local state should be stored as normal files.
- Make correctness depend on content-addressed objects, not on fragile offsets.
- Use offsets only as an optimization.

## Non-Goals

- Perfect preservation of intentionally duplicated identical JSONL lines.
- Destructive reconciliation of session history.
- Requiring Codex client changes.

## Core Model

The central repository stores individual JSONL messages as immutable objects.

Each object is identified by:

- `session_hash = sha256(session_id)`
- `message_hash = sha256(raw_jsonl_line_bytes)`

The raw JSONL line is the canonical payload. The system must hash the exact line bytes as observed on disk. It must not parse and re-serialize JSON before hashing.

The local `~/.codex/sessions` tree is a materialized view of the repository contents. This document refers to those reconstructed local files as the local projection.

## Duplicate Handling

If two JSONL lines have identical content, then they also have identical timestamps because the timestamp is part of the line content.

This design intentionally deduplicates such lines.

That means:

- if the same message is seen on multiple machines, it is stored once
- if an identical line is written twice adjacently in a session, it is collapsed to one stored message

This is an explicit design choice. Such duplication is treated as a bug in the source log rather than meaningful session data.

## Repository Layout

The repository is sharded by the first four hex digits of the session hash.

```text
sessions/
  ab/
    cd/
      <session_hash>/
        messages/
          <YYYYMMDDHHmmssfff>-<message_hash>.json
```

Each message file contains metadata plus the raw JSONL line. The exact on-disk encoding may be JSON. A message object must include at least:

- `session_id`
- `session_hash`
- `message_hash`
- `timestamp`
- `raw_jsonl`
- `source_machine_id`
- `source_path`

The repository path is determined only by `session_hash` and `message_hash`.

The filename must begin with the message timestamp in UTC using the format `YYYYMMDDHHmmssfff`.
This makes lexical filename order match chronological order and makes shell-based inspection easier.

Timestamp alone is not sufficient as a filename because multiple messages may share the same millisecond.
The `message_hash` suffix is therefore required.

`session_id` must still be stored inside the object for validation and debugging.

## Local State

Local state is stored as files, not in SQLite.

Recommended layout:

```text
$XDG_STATE_HOME/codex-session-sync/
  machine-id
  last-projected-head
  sessions/
    ab/
      cd/
        <session_hash>.toml
```

If `XDG_STATE_HOME` is not set, use `~/.local/state/codex-session-sync/`.

### machine-id

A stable per-machine identifier. This is written once and reused forever.

### last-projected-head

The last Git commit that has been fully materialized into `~/.codex/sessions`.

This is an optimization. It allows sync-down to project only sessions changed since the last applied commit.

### Per-session State

Each session state file should contain:

- `session_id`
- `session_hash`
- `local_path`
- `last_scan_offset`
- `last_scan_anchor_hash`
- optionally `last_known_size`
- optionally `last_known_mtime_ns`

The required fields are `local_path`, `last_scan_offset`, and `last_scan_anchor_hash`.

The design does not rely on active-session detection.
It instead relies on shadow snapshots during projection.

### Why Offset Alone Is Not Enough

Offset is only a fast-path optimization.

It is not sufficient by itself because sync-down may insert older remote messages before the current local tail. That rewrites the file and invalidates the old byte offset.

The anchor hash exists to detect that situation. If the anchor is no longer where the state expects it to be, the scanner must fall back to a full rescan of that file.

## Local Session Path

If a session already exists locally, its current path is preserved.

If a session is remote-only on this machine, it is materialized into a deterministic path:

```text
~/.codex/sessions/YYYY/MM/DD/<session_hash>.jsonl
```

Where `YYYY/MM/DD` is derived from the earliest message timestamp in that session.

Once chosen, that path is recorded in the per-session state file and reused.

## Sync Up

Sync-up means scanning local session files and writing missing message objects into the local Git clone.

For each local `.jsonl` session file:

1. Read the session id from the file.
2. Compute `session_hash = sha256(session_id)`.
3. Try the fast path:
   - resume from `last_scan_offset`
   - verify that `last_scan_anchor_hash` still matches the expected message near that point
4. If the anchor check fails, rescan the full file from the beginning.
5. For each JSONL line:
   - hash the raw line bytes to get `message_hash`
   - compute the repository object path from `session_hash` and `message_hash`
   - if the object already exists in the local repo clone, ignore it
   - otherwise create it
6. Update the per-session state file with the newest usable offset and anchor.

Correctness does not depend on the offset. If the fast path is invalid, a full rescan is always safe because duplicate objects are ignored.

## Sync Down

Sync-down means projecting repository messages back into `~/.codex/sessions`.

1. Pull the repository.
2. Determine which sessions changed since `last-projected-head`.
3. For each changed session:
   - create a hidden hard-link shadow of the current `local_path` in the same directory, for example:
     - `.<basename>.sync-shadow-<nonce>`
   - load all message objects for that session
   - load any valid local-only messages recoverable from the shadow file that are not yet in the repository
   - build the union of:
     - repository messages for the session
     - recoverable local-only shadow messages
   - sort that union by `timestamp`
   - use `message_hash` as the deterministic tie-breaker when timestamps are equal
   - write a `.tmp` file containing `raw_jsonl` lines separated by `\n`
   - `fsync` the `.tmp` file
   - rename the `.tmp` file atomically into the final `local_path`
   - keep the shadow file until all messages recoverable from it are present in the repository and reflected in projection
4. Update `last-projected-head`.

The projection file must end with a trailing newline.

The shadow file is the recovery mechanism for late writes by an uncooperative local Codex process.
If Codex still holds an open handle to the old inode after projection, those writes land in the old inode, which remains reachable through the shadow path.

Projection safety invariants:

- only project into paths under the configured `~/.codex/sessions` root
- only project into regular `.jsonl` files
- never project into a shadow path
- refuse to project through symlinks or other special filesystem objects
- after creating a shadow for an existing target, verify that the target path still refers to that same file before atomically replacing it

## Shadow Retention And Garbage Collection

Shadow files must be collected conservatively.

The system should not delete a shadow immediately after scanning it once, because a local Codex process may still hold an open handle to the old inode and continue writing to it after projection.

The intended garbage-collection policy is:

1. A shadow only becomes a collection candidate after a successful sync cycle has:
   - scanned the shadow
   - uploaded any missing messages recoverable from it into the repository
   - reprojected the affected session
2. The shadow must then remain unchanged for at least one later successful sync cycle.
   Unchanged means:
   - same size
   - same modification time
   - ideally the same content hash
3. The shadow must also be older than the grace period.

The default grace period should be one week.

That default is intentionally long because leaving a Codex CLI session open for days is realistic, and the old process may continue writing through an inherited file descriptor long after projection replaced the path.

The system should therefore treat a shadow as free to collect only when it is:

- fully reconciled into the repository
- unchanged on a later successful sync cycle
- older than one week

The implementation should not rely on detecting whether another process still has the old inode open.
Portable, reliable open-handle detection is not available in the general case, and OS-specific checks such as `lsof` would still be race-prone.

The design therefore prefers time-based and stability-based retention over handle inspection.

## Feedback Loop Avoidance

Feedback loop avoidance does not rely on hiding projected files from the scanner.

It relies on content-addressed deduplication:

- sync-down writes raw JSONL lines into `~/.codex/sessions`
- sync-up may later scan those files again
- scanned lines produce the same `message_hash`
- the corresponding message object already exists in the local repo clone
- therefore nothing new is uploaded

This is why correctness depends on object existence and not on offsets.

Offsets only make the common case faster.

Shadow files are not live session files. They are recovery artifacts.
Normal session scanning should ignore shadow files by name pattern and process them only through dedicated recovery logic.

## Failure Model

The system must prefer duplicate work over data loss.

Safe fallback behavior:

- if per-session state is missing, do a full rescan
- if the offset anchor does not match, do a full rescan
- if `last-projected-head` is missing, do a full projection
- if a message object already exists, treat that as success
- if a projection race is suspected, keep the shadow file and retry projection later
- if the local session file is malformed after an uncooperative rewrite, salvage valid lines, ignore broken tails, and reproject from repository messages plus recoverable shadow messages

## Concurrency Model

Different machines may upload messages for the same session concurrently.

This is safe because:

- objects are immutable
- objects are addressed by content
- sync-down reconstructs a session from the full set of objects in the repository

The only repository conflicts that matter are normal Git synchronization conflicts. Those are handled at the repository level, not the session-object level.

Local projection conflicts are handled by:

- preserving the old inode through a shadow hard link
- rebuilding the canonical local file from the repository plus recoverable local-only shadow messages

## Important Assumptions

- Every JSONL line contains a usable timestamp.
- Sorting by timestamp, then by message hash, is sufficient to reconstruct a stable session order.
- Collapsing identical lines is acceptable.
- Shadow hard links are created on the same filesystem as the live session file.

If any of those assumptions turn out to be false in real Codex data, this design will need revision.
