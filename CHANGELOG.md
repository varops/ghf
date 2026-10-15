# Changelog

## 0.1.0-rc.1 — 2026-10-15

This is the first release candidate. It implements the full ghf MVP: capture, sync, restore and apply, and the trace lifecycle.

### Client (`git hf`)

- **Capture.**
  - **Automatic:** hooks for Claude Code (`PostToolUse`) and Cursor (`afterFileEdit`, `afterShellExecution`). `git hf checkpoint` is the manual fallback.
  - **Durability:** each checkpoint is a complete, independently restorable worktree state, acknowledged only after a single atomic local commit.
  - **Failure modes:** best-effort by default; strict mode for pipelines.
- **What a checkpoint records.**
  - Tracked and untracked files, deletes, renames, the executable bit and symlinks.
  - `.gitignore` and `.ghfignore` are respected, and oversized files are recorded as skipped.
- **Worktrees and writers.**
  - Each worktree gets its own context and trace.
  - One writer per trace, enforced with a lease, so a second writer gets `E_WRITER_LOCKED`.
  - A directory copied from another path or machine gets a new context automatically.
- **Inspect:** `status`, `trace list`, `trace log` (filter by annotation, for example `tests=pass`) and `trace diff`. Checkpoint references: `@`, `@~N`, id prefixes.
- **Restore, apply, fork.**
  - Restore goes into a new worktree by default and is verified byte-identical.
  - In-place restore is refused if anything is uncaptured; `--force` checkpoints first.
  - Three-way `apply` is all or nothing, with an optional `--write-conflicts`.
  - Every restore and apply is recorded as a checkpoint.
- **Lifecycle:** `close`; `finalize --commit` (checked for reachability from the main ref) or `--abandoned`; `pin`/`unpin` and named holds; `prune` with local garbage collection.
- **Sync.**
  - Background sync after each checkpoint, plus explicit `git hf sync`.
  - Batched, idempotent uploads; metadata pulled from other machines; content fetched lazily on demand.
- **Trust and credentials.** A server must be trusted (and its id pinned) before any upload. Tokens live in `GHF_TOKEN` or `~/.config/ghf/credentials.toml`, never in `.ghf`.
- **Automation contract.**
  - `--json` envelope `ghf/v1`, stable error codes and exit codes, `--idempotency-key`, and no prompts in automation.
  - The full reference is in `docs/cli.md`.
- **Agent skill:** `git hf skill install`.
- **Safety.**
  - Manifests with unsafe entry names (`..`, `/`, `.git`, duplicates) are rejected by both client and server, so a restore can never write outside its worktree.
  - Restore and apply refuse, before changing anything, when an ignored, skipped or untracked file is in the way.
  - Local garbage collection marks and sweeps in one transaction.
  - Racily-clean files are never cached.
  - Pending reports protect the traces that unsynced forks were made from.

### Server (`ghf-server`)

- **Packaging:** one binary plus a data directory (SQLite), with optional built-in TLS.
- **Auth:** tokens scoped per repository: `owner/name`, `owner/*` or `*`.
- **Integrity:** content-addressed objects (BLAKE3 + zstd), verified on upload. A checkpoint is accepted only when its whole manifest closure is present.
- **Retention.**
  - Finalized traces after 30 days; closed traces that are never finalized after 90 days.
  - Both are configurable per repository.
  - Pruning skips held traces, traces with unsynced checkpoints on any client, and the base checkpoints of retained forks.
  - Hourly prune and garbage collection.

### Measured

On a 2,000-file repository, release build:

| Metric | Result |
|---|---|
| Checkpoint latency, p50 | 41–42 ms |
| Checkpoint latency, p95 | 45 ms for single-file edits; 85 ms for the bench's multi-file pattern (target 50 ms) |
| Sync of 200 checkpoints | 1.7 s |
| Cross-machine restore | 0.6 s |

### Known limitations

- Hooks see only the agent's own tool calls. Writes by other processes are picked up at the next checkpoint. There is no file watcher yet (PRD P2).
- The Git index (staged state) and submodules are not captured.
- Linux and macOS only.
- Detecting a copied directory relies on its path and the machine id. A cloned VM that keeps both paths unchanged should delete `~/.config/ghf/machine-id`.
- About half of capture time is `git ls-files`, which grows with repository size.
