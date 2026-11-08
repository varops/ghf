# Changelog

## Unreleased

### Fixed

- **Refused checkpoints survive `prune`.** Local prune dropped a trace the server had pruned even when it held checkpoints the server refused, so they were lost although they were promised to stay restorable. A trace with pending or refused checkpoints is now kept.
- **A refused hold no longer blocks sync.** Pinning or holding a trace that was pruned on the server, or that the server does not know, made every later `sync` fail at that hold. Such a hold is now dropped and reported in the new `refused_holds` field of `sync` and `status`. Holds on a trace this machine already knows is pruned are refused with `E_TRACE_STATE`.
- **No checkpoints into a closed trace.** A capture that read the open trace just before `trace close`, `trace finalize` or a pull closed it could still append its checkpoint to that trace and claim it as writer. A capture now re-checks the trace inside its transaction and opens a new trace if it is no longer open; in-place restore and apply do the same. `close` and `finalize` change the record and release the writer in one transaction, and a pulled trace that is no longer open loses its writer.
- **Changing server starts sync over.** After `init --server NEW --force`, or a changed repository, sync kept the old pull cursor (skipping the new server's history), never pushed checkpoints already synced to the old server, and kept using the old server's retention policy. Sync progress is now tied to the server URL and repository: when they change, the next sync pulls from the start and pushes every trace and every checkpoint whose content is on this machine. With no server configured, the cached server policy is ignored.
- **A losing finalize is reported.** When two machines finalized the same trace differently, the second one's sync silently took the first one's result later. Sync now adopts the server's merged record right away and lists the trace in the new `trace_conflicts` field.
- **No orphaned open traces.** When a worktree's context is replaced (the directory moved, or came from another machine), the old context's open trace is now closed. `prune` also closes the open trace of a worktree that was removed or moved away, and lists it in `local.closed_orphans`. This works for contexts created by this version or later.
- **Idempotency keys are reserved before the command runs.** Two runs with the same key at the same time both acted. Now the second gets the new error `E_IDEMPOTENCY_IN_PROGRESS` (exit 4). A reservation older than `GHF_IDEMPOTENCY_STALE` (default `10m`) is taken to be from a crashed run, and the key runs again. A failed run is recorded too: replaying its key returns the same error, with `data.replayed`, instead of acting again. The fingerprint no longer drops arguments that happen to equal the key, so `trace close K --idempotency-key K` and `trace close --idempotency-key K` no longer count as the same command. Existing stores gain the new column on first open.
- **Interrupted in-place restore and apply are reported.** An in-place restore or apply that stopped partway left a half-changed tree with no trace of what happened. ghf now writes `<git-dir>/ghf/operation.json` just before the tree changes and removes it once the result is recorded. While it exists, `status` reports the operation and the undo command in the new `interrupted` field, and captures are annotated `interrupted=restore` (or `apply`). Hooks are never blocked.
- **One sync at a time.** Foreground `sync`, `prune` and the pull behind an unknown reference could run alongside a background sync and interleave with it. They now take the same lock and wait for the run in progress.

### Server

- **Trace records are checked.** `PUT traces/{id}` refuses a record whose fields do not fit its state with `E_USAGE`. Every state change on client and server goes through the record's own `close`, `finalize` and `prune` transitions.
- **Clients cannot introduce a pruned trace.** A pushed `pruned` record for a trace the server did not know was stored as `closed` while keeping `pruned_at` and any finalize fields. It is now stored as `finalized` if it was finalized, else `closed`, without `pruned_at`.
- **Stale pending reports expire.** A client that stopped syncing kept its report of unsynced checkpoints forever and blocked pruning of those traces. Reports older than `--pending-max-age` (default `30d`, on `serve` and `prune`) are now ignored, and each ignored report is logged.

## 0.1.0-rc.3 — 2026-11-04

### Added

- **`ghf tail`.** Shows the latest checkpoints from every trace in the repository, across all worktrees on this machine plus anything a sync pulled in. `-f` keeps following and also prints traces opening, closing and being finalized. Lines show the local time, the author (one colour per author), the trace, the change counts, `tests=pass`/`fail` in green or red, and the message. Filter with `--trace`, `--author` and `--annotation`, list changed paths with `--stat`, or stream one envelope per line with `--json`. Colour turns off when the output is not a terminal or `NO_COLOR` is set.

### Changed

- **Local store.** A `checkpoint_arrivals` table, kept in step by triggers, records the order checkpoints reached the store. `tail -f` follows it, so it no longer misses a checkpoint stored right after pruning removed the newest one. Existing stores are filled in once, on first open.

## 0.1.0-rc.2 — 2026-10-19

### Added

- **`ghf` command.** The client also installs as `ghf`, the same program as `git hf`.
- **`ghf watch`.** Checkpoints writes that no hook sees, such as generators, background processes and editors, after they settle. It records on behalf of the trace's current writer, marked `captured_by=watch`, and never takes the trace.
- **Staged changes.** Each checkpoint records the Git index relative to its base revision, including partial staging. `trace restore --index` and `trace fork --index` restage it. That is the only time ghf writes to Git's index and object store, as `git add` would.
- **Windows.** The client and server build and pass the full test suite on Windows, now part of CI. Without symlink privileges, symlinks restore as plain files holding the target, as Git for Windows does.
- **Submodules.** Each submodule is recorded as the commit it has checked out. Restore moves a submodule to that commit only when it is checked out, clean and has the commit; otherwise it warns with the command to run.

### Changed

- Restoring the same checkpoint twice picks the next free default worktree name instead of failing.
- Docs use `ghf` throughout. `git hf` works identically.

### Server

- **Read-only tokens.** `ghf-server token create --read-only` creates a token that can read but not write. Uploads, trace and checkpoint pushes, holds, pending reports and prune refuse it with `E_AUTH`. `token list` shows each token's access. Existing tokens stay read-write.
- **Web viewer.** `/ui` shows what agents tried, without installing ghf: each repository's traces, a checkpoint timeline with annotations (`tests=pass`/`fail` highlighted) and fork links, diffs between checkpoints, and files at a checkpoint. Sign in by pasting a token once; it is kept in an `HttpOnly`, `SameSite=Strict` cookie. Pages follow the token's repository scope, use stable URLs, support light and dark themes, and offer no write actions. See [Viewing traces in a browser](docs/server.md#viewing-traces-in-a-browser).

### Performance

- **Faster edits.** A checkpoint after editing existing files reuses the last `git ls-files` listing, so it no longer starts git. Before reusing it, ghf checks the stamps (mtime, size, inode) on the listed directories, the index and the ignore files. A stamp less than 2 seconds old is never trusted, and the listing expires after 60 seconds.
- **Faster new files.** When the file set has changed, git runs in parallel with hashing the previously listed files.

Bench, on a 2,000-file repository:

| Checkpoint after | p50 | p95 |
|---|---|---|
| Editing existing files | 25–27 ms (was 42) | 28–34 ms (was 85) |
| Creating a file | 42 ms | 45–51 ms |

- **Benchmark.** It now ages its fixture like a real checkout and reports new-file checkpoints separately.
- **Opt out.** Set `GHF_NO_LISTING_CACHE=1` to always ask git.

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

- Hooks see only the agent's own tool calls. Writes by other processes are picked up at the next checkpoint. There is no file watcher yet.
- The Git index (staged state) and submodules are not captured.
- Linux and macOS only.
- Detecting a copied directory relies on its path and the machine id. A cloned VM that keeps both paths unchanged should delete `~/.config/ghf/machine-id`.
- About half of capture time is `git ls-files`, which grows with repository size.
