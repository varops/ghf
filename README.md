# ghf

**High-frequency source control for AI agents. Git-compatible by design.**

ghf (Git High Frequency) saves the state of a worktree every time an agent finishes an edit. These saves are **checkpoints**. Checkpoints for one unit of work form a **trace**. Any agent on any authorized machine can list, diff, restore, apply or fork a trace.

ghf sits next to Git, not inside it. It writes nothing into commits, messages, refs, notes or the object database. Accepted work still lands as ordinary Git commits and pull requests, and plain Git users never need to know ghf exists.

- **Git** holds accepted history.
- **ghf** holds recoverable working history, with its own retention, permissions and sync.

## Status

Pre-M1. The repository is scaffolding only: the command grammar exists and every command returns `E_NOT_IMPLEMENTED`.

## Layout

| Path | Crate | What it is |
|---|---|---|
| [`extension/`](extension) | `git-hf` | Client binary. Git runs it as `git hf`. |
| [`server/`](server) | `ghf-server` | Canonical store for checkpoints and traces. One binary plus a data directory. |
| [`proto/`](proto) | `ghf-proto` | Shared types: JSON envelope, error codes, exit codes. |
| [`skills/ghf/`](skills/ghf) | | Agent skill that teaches the checkpoint and recovery workflow. |
| [`docs/`](docs) | | Concept and PRDs. |

## Build

Requires Rust 1.85 or later.

```bash
cargo build
cargo test
```

Put `target/debug` on your `PATH` and Git finds the extension:

```bash
git hf --help
```

## CLI

Illustrative and not final.

```bash
git hf init
git hf status
git hf checkpoint [-m "short intent"] [--annotate tests=pass]
git hf sync

git hf trace list
git hf trace log [<trace>]
git hf trace diff <a> [<b>]

git hf trace restore <id> [--worktree <path>]
git hf trace restore <id> --in-place [--force]
git hf trace apply <a>..<b> [--write-conflicts]
git hf trace fork <id> [--worktree <path>]

git hf trace close [<trace>]
git hf trace finalize --commit <sha> | --abandoned
git hf trace pin <trace>
git hf prune [--dry-run]
```

### Automation contract

Agents are the primary users, so the CLI is an API. Every command takes `--json` and prints one versioned envelope:

```json
{"schema":"ghf/v1","command":"trace log","ok":false,
 "error":{"code":"E_NOT_IMPLEMENTED","message":"`trace log` is not implemented yet"}}
```

Error codes are stable and never change meaning. Exit codes map to error classes:

| Exit | Class | Codes |
|---|---|---|
| 0 | Success | |
| 1 | Internal | `E_INTERNAL` |
| 2 | Usage | `E_USAGE`, `E_NOT_A_REPO` |
| 3 | Refused to protect work | `E_WORKTREE_DIRTY`, `E_WRITER_LOCKED` |
| 4 | Conflict | `E_APPLY_CONFLICT` |
| 5 | Sync | `E_NOT_SYNCED` |
| 64 | Not implemented | `E_NOT_IMPLEMENTED` |

With `--json` or no TTY, ghf never prompts. Missing input is an error, never a hang.

## Docs

