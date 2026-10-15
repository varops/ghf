# `git hf` reference

Schema `ghf/v1`. This document is part of the public API. Fields may be added; existing fields and error codes keep their meaning.

## Envelope

With `--json`, every command prints exactly one JSON object on stdout:

```json
{"schema": "ghf/v1", "command": "trace log", "ok": true, "data": { ... }}
{"schema": "ghf/v1", "command": "trace apply", "ok": false,
 "error": {"code": "E_APPLY_CONFLICT", "message": "..."}, "data": { ... }}
```

- **`command`** is the stable command name: `"checkpoint"`, `"trace restore"`, and so on.
- **`data`** is present on success. Errors may also carry it, with structured detail such as conflicts, the lock holder or pending counts.
- **No prompts.** With `--json` or a non-TTY, ghf never prompts. Missing input is an error (`E_USAGE`).
- **`--idempotency-key KEY`** works on any mutating command. Repeating it with the same arguments returns `{"replayed": true, "result": <first data>}` without acting again. Repeating it with different arguments returns `E_IDEMPOTENCY_CONFLICT`.

The `hook` command is the one exception: it answers in the harness's own format.

## Exit codes

| Exit | Class |
|---|---|
| 0 | Success |
| 1 | Internal error, or strict-mode capture failure |
| 2 | Usage or configuration |
| 3 | Refused, to protect work, a writer or a trace's state |
| 4 | Conflict |
| 5 | Server, network, trust or auth (local state is intact) |
| 6 | Not found |
| 64 | Not implemented |

## Error codes

| Code | Exit | Meaning |
|---|---|---|
| `E_INTERNAL` | 1 | Unexpected internal failure |
| `E_CAPTURE_FAILED` | 1 | Capture failed (strict mode surfaces this to the harness) |
| `E_USAGE` | 2 | Invalid arguments or missing input |
| `E_NOT_A_REPO` | 2 | Not inside a Git worktree |
| `E_NOT_INITIALIZED` | 2 | No `.ghf`; run `git hf init` |
| `E_CONFIG` | 2 | Invalid `.ghf` or user configuration |
| `E_AMBIGUOUS` | 2 | A reference prefix matches more than one item |
| `E_WORKTREE_DIRTY` | 3 | Uncaptured changes would be lost |
| `E_WRITER_LOCKED` | 3 | Another writer holds this worktree's trace; `data.writer`, `data.lease_expires_at` |
| `E_NO_OPEN_TRACE` | 3 | This worktree has no trace yet |
| `E_TRACE_STATE` | 3 | The trace's state does not allow this |
| `E_PATH_EXISTS` | 3 | Destination exists (`init` without `--force`, restore target) |
| `E_COMMIT_NOT_REACHABLE` | 3 | Commit is not reachable from the main ref |
| `E_APPLY_CONFLICT` | 4 | Apply conflicts; `data.conflicts[]` of `{path, reason}`, `data.applied` |
| `E_IDEMPOTENCY_CONFLICT` | 4 | Key reused with different arguments |
| `E_NOT_SYNCED` | 5 | Checkpoints not yet on the server |
| `E_UNTRUSTED_SERVER` | 5 | Server not trusted on this machine; run `git hf trust` |
| `E_AUTH` | 5 | Missing or rejected credentials |
| `E_SERVER` | 5 | Server unreachable or failed |
| `E_SERVER_MISMATCH` | 5 | Server identity changed since it was trusted |
| `E_NOT_FOUND` | 6 | No such checkpoint, trace or object |
| `E_OBJECT_MISSING` | 6 | Checkpoint content was pruned or never synced |
| `E_COMMIT_NOT_FOUND` | 6 | Commit does not exist locally |
| `E_NOT_IMPLEMENTED` | 64 | Reserved |

## References

| Form | Means |
|---|---|
| `@` | Latest checkpoint of this worktree's current trace |
| `@~N` | N checkpoints before `@` |
| `ck_…` or a unique prefix (at least 4 characters, `ck_` optional) | A checkpoint |
| `tr_…` or a unique prefix | A trace; where a checkpoint is expected, that trace's latest |

**Current trace:** the open trace of this worktree context, or its most recent trace if none is open.

**Unknown ids:** a reference this machine does not know triggers one pull from the server before failing with `E_NOT_FOUND`.

## Records

**Checkpoint** (`data.checkpoint`, and the items of `data.checkpoints`):

```json
{"id": "ck_…", "trace_id": "tr_…", "seq": 3, "parent": "ck_…", "context_id": "wc_…",
 "base_rev": "<HEAD sha>", "created_at": 1760540000000, "author": "claude-code:<session>",
 "root": "<manifest tree id>", "files": 2000, "bytes": 8192000,
 "message": "optional", "annotations": {"tests": "pass"},
 "changes": [{"path": "src/a.rs", "status": "modified"}, {"path": "b.rs", "status": "renamed", "from": "a.rs"}],
 "skipped": [{"path": "big.bin", "reason": "too_large", "size": 200000000}]}
```

- **Times** are Unix milliseconds.
- **`changes[].status`** is one of `added`, `modified`, `deleted`, `renamed` or `mode`.
- **`changes`** compares the checkpoint with its parent. A trace's first checkpoint is compared with Git `HEAD` instead.
- **`skipped[].reason`** is one of `too_large`, `non_utf8_path`, `unreadable`, `submodule` or `special_file`.

**Trace:**

```json
{"id": "tr_…", "name": "optional", "state": "open|closed|finalized|pruned", "created_at": 0,
 "context_id": "wc_…", "forked_from": "ck_…", "closed_at": 0, "finalized_at": 0,
 "final_commit": "<sha>", "abandoned": false, "pruned_at": 0,
 "holds": [{"name": "pin", "expires_at": null, "created_at": 0}]}
```

`trace list` and `status` add a few fields to each trace:
- `checkpoints` and `unsynced`, both counts.
- `writer`.
- `local`, which is true if the trace belongs to this worktree.
- `retention`: `{eligible, eligible_at, warn, reason}`.

## Commands

### `init [--server URL] [--repository owner/name] [--strict] [--trust] [--force]`

Writes `.ghf` and sets up the local store under `<git-common-dir>/ghf/`.

- **`--repository`** defaults to the `origin` remote's `owner/name`.
- **`--trust`** trusts the server without prompting.
- **Without a server,** checkpoints stay local.

Data: `{path, created, config, context, trusted}`.

### `status`

Data:

```text
{repository_root, context, capture_mode, trace, latest_checkpoint, unsynced,
 last_sync_at, last_sync_error, capture_failures[], server, warnings[]}
```

`warnings` includes:
- An untrusted server or missing credentials.
- A failed sync.
- Recent capture failures.
- Traces close to the unfinalized cap.
- A context that was replaced because the directory came from another path or machine.

### `checkpoint [-m MSG] [--annotate K=V]... [--trace-name NAME] [--force] [--takeover]`

Captures the worktree.

- **Unchanged tree:** returns the latest checkpoint with `created: false`, unless you pass a message, an annotation or `--force`.
- **New trace:** opens one if this worktree has no open trace. The name comes from `--trace-name` or `$GHF_TRACE_NAME`.
- **Writer:** the checkpoint is written as `$GHF_WRITER`, or `manual:$USER` if that is unset.

Data: `{created, trace_created, checkpoint, trace}`.

### `sync`

Pushes traces, checkpoints and holds, then pulls metadata from other machines.

- **Content is lazy:** it is fetched on restore, diff or apply.
- **On failure:** `data.pending` holds the count still to push.

Data:

```text
{pushed_traces, pushed_checkpoints, pushed_objects, pushed_bytes, hold_ops,
 pulled_traces, pulled_checkpoints, pending}
```

### `trust [URL]` · `login [--server URL]`

- **`trust`** records the server's id in `~/.config/ghf/trust.toml`. Without this, nothing is uploaded.
- **`login`** reads a token from stdin into `~/.config/ghf/credentials.toml` (mode 0600). `$GHF_TOKEN` takes precedence over the saved token.

### `trace list [--state S] [--all]`

Data: `{traces[]}`. Pruned traces are hidden unless you pass `--all` or `--state pruned`.

### `trace log [TRACE] [--annotation K=V]... [-n N]`

Data: `{trace, checkpoints[]}`, oldest first.

### `trace diff A [B] [--patch]`

Compares A with B, or A with the working tree if B is omitted.

- **Data:** `{from, to, changes[], patch?}`. `--patch` includes a unified diff.
- **Human output:** always the patch.

### `trace restore ID [--worktree PATH]`

Creates a new Git worktree (`git worktree add --detach` at the checkpoint's base revision) and writes the checkpoint's files. It also creates:
- A new worktree context.
- A new trace forked from `ID`, whose first checkpoint records the restore.

**Default path:** `../<repo>-ghf-<suffix>`.

**Verification:** the result is checked to be byte-identical to the checkpoint.

Data: `{worktree, base_rev, context, trace, checkpoint, restored_from, files_written, warnings[]}`.

### `trace restore ID --in-place [--force]`

Runs only if the working tree matches the latest checkpoint of this worktree's open trace. Otherwise it fails with `E_WORKTREE_DIRTY`.

- **`--force`** checkpoints first; that checkpoint's id is in `pre_restore_checkpoint`.
- **Untouched:** ignored files are never touched.
- **History:** the result is recorded as a new checkpoint.

Data: `{restored_from, checkpoint, pre_restore_checkpoint, files_written, files_deleted}`.

### `trace apply A..B | A.. | ID [--write-conflicts]`

Three-way merges the changes between two checkpoints onto the working tree. Text uses `git merge-file`.

**Ranges:**
- `A..` means A to the latest checkpoint of A's trace.
- `ID` alone means the parent of ID to ID.

**On conflict:** it stops with `E_APPLY_CONFLICT` and leaves the tree unchanged. With `--write-conflicts`, it writes the merge with conflict markers instead.

**History:** pending work is checkpointed before the apply, and the result is checkpointed after.

Data:

```text
{applied, from, to, checkpoint, pre_apply_checkpoint, files_written, files_deleted, conflicts[]}
```

### `trace fork ID [--worktree PATH]`

The same as `trace restore ID`, named for intent.

### `trace close [TRACE]` · `trace finalize [TRACE] --commit SHA | --abandoned`

- **`close`** ends writing to the trace.
- **`finalize --commit`** checks that the commit exists and is reachable from the main ref. The main ref is `[git] main` in `.ghf`; otherwise the first of `origin/HEAD`, `origin/main`, `origin/master`, `main`, `master` that exists.
- **Retention** starts at finalization.

Data: `{trace, changed}`.

### `trace pin|unpin TRACE` · `trace hold TRACE --name N [--expires 7d]` · `trace release TRACE --name N`

Holds stop pruning. `pin` is the hold named `pin`. Data: `{trace}`.

### `prune [--dry-run]`

Asks the server to prune this repository, then drops local copies of pruned traces and collects unreferenced local objects.

- **Without a server,** it applies the `.ghf` `[retention]` policy locally.
- **Unsynced checkpoints** are never dropped.

Data: `{dry_run, server, local}`.

### `hooks install|uninstall|status HARNESS [--scope project|local|user]`

`HARNESS` is `claude-code` or `cursor`. Data: `{path, changed, installed}`.

| Harness | Settings file | Events |
|---|---|---|
| Claude Code | `.claude/settings.json` (`local`: `settings.local.json`; `user`: `~/.claude/settings.json`) | `PostToolUse` matching `Write\|Edit\|MultiEdit\|NotebookEdit\|Bash` |
| Cursor | `.cursor/hooks.json` (`user`: `~/.cursor/hooks.json`) | `afterFileEdit`, `afterShellExecution` |

### `hook claude-code|cursor`

The entry point the hooks call. It reads the harness payload on stdin and checkpoints the repository the edit happened in. The writer is `claude-code:<session_id>` or `cursor:<conversation_id>`. Outside a ghf-enabled repository it does nothing.

| Outcome | Claude Code | Cursor |
|---|---|---|
| Success | No output, exit 0 | Exit 0 |
| Best-effort failure | `{"systemMessage": …}`, exit 0 | Exit 0 |
| Strict failure | `{"continue": false, "stopReason": "… E_CAPTURE_FAILED"}`, exit 0 | Exit 1 |

Every failure is recorded for `status`. If the local store itself is broken, it goes to `<git-dir>/ghf/failures.log`.

### `skill install [--scope project|user]` · `skill show`

Writes the agent skill to `.claude/skills/ghf/SKILL.md`, or to `~/.claude/skills` with `--scope user`.

## Configuration: `.ghf`

```toml
version = 1

[server]
url = "https://ghf.example.com"   # https, or http only for localhost
repository = "acme/project"

[capture]
mode = "best-effort"      # or "strict"
sync = "background"       # or "manual"
max_file_size = "100MB"
writer_ttl = "30m"        # an idle writer's lock expires after this

[git]
main = "origin/main"      # finalize checks reachability from here

[retention]               # used only when no server policy is known
after_finalize = "30d"
unfinalized_cap = "90d"
```

`.ghfignore` sits at the repository root and uses gitignore syntax. Matching paths are never captured, uploaded, restored or deleted.

## Environment

| Variable | Effect |
|---|---|
| `GHF_TOKEN` | Server token (overrides the credentials file) |
| `GHF_WRITER` | Writer identity for manual commands and hooks |
| `GHF_TRACE_NAME` | Name for traces this process opens |
| `GHF_STRICT` | `1` forces strict mode, `0` forces best-effort |
| `GHF_SYNC` | `manual` disables background sync |
| `GHF_CONFIG_DIR` | User config directory (default `$XDG_CONFIG_HOME/ghf` or `~/.config/ghf`) |
| `GHF_ALLOW_INSECURE` | `1` allows plain `http://` to non-local hosts |
| `GHF_GC_GRACE` | Minimum object age before local GC deletes it (default `1h`) |
| `GHF_TIMING` | Print capture phase timings to stderr |

## Local state

Everything ghf keeps on a machine lives outside Git history:

- `<git-common-dir>/ghf/ghf.db` holds the checkpoints, traces, content, stat cache and sync state. It is shared by all worktrees of the repository.
- `<git-dir>/ghf/context.json` holds this worktree's context.
- `<git-dir>/ghf/capture.lock` serializes captures in one worktree.
- `~/.config/ghf/` holds the machine id, trusted servers and credentials.
