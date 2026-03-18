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
          <message_hash>.json
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
   - load all message objects for that session
   - sort them by `timestamp`
   - use `message_hash` as the deterministic tie-breaker when timestamps are equal
   - write a `.tmp` file containing `raw_jsonl` lines separated by `\n`
   - rename the `.tmp` file atomically into the final `local_path`
4. Update `last-projected-head`.

The projection file must end with a trailing newline.

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

## Failure Model

The system must prefer duplicate work over data loss.

Safe fallback behavior:

- if per-session state is missing, do a full rescan
- if the offset anchor does not match, do a full rescan
- if `last-projected-head` is missing, do a full projection
- if a message object already exists, treat that as success

## Concurrency Model

Different machines may upload messages for the same session concurrently.

This is safe because:

- objects are immutable
- objects are addressed by content
- sync-down reconstructs a session from the full set of objects in the repository

The only repository conflicts that matter are normal Git synchronization conflicts. Those are handled at the repository level, not the session-object level.

## Important Assumptions

- Every JSONL line contains a usable timestamp.
- Sorting by timestamp, then by message hash, is sufficient to reconstruct a stable session order.
- Collapsing identical lines is acceptable.

If any of those assumptions turn out to be false in real Codex data, this design will need revision.
