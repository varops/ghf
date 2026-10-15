# ghf

**High-frequency source control for AI agents. Git-compatible by design.**

ghf (Git High Frequency) saves the state of a worktree every time an agent finishes an edit. Each save is a **checkpoint**. The checkpoints for one unit of work form a **trace**. Any agent on any authorized machine can list, diff, restore, apply or fork a trace.

ghf sits next to Git, not inside it. It writes nothing into commits, messages, refs, notes or the object database. Accepted work still lands as ordinary Git commits and pull requests, and plain Git users never need to know ghf exists.

- **Git** holds accepted history.
- **ghf** holds recoverable working history, with its own retention, permissions and sync.

**Status: 0.1.0-rc.1.** The full MVP is implemented: capture, sync, restore and apply, and the trace lifecycle.

## What it does for a fleet

| Use case | How |
|---|---|
| Roll back to the last green state | `git hf trace log --annotation tests=pass`, then `git hf trace restore <id> --in-place` |
| Hand work to another machine | `git hf sync` here; `git hf trace restore <id>` there |
| Try two approaches from one point | `git hf trace fork <id>` once per attempt |
| Show a human what was tried | `git hf trace log` and `git hf trace diff <a> <b>` |
| Survive a crashed worker | A checkpoint is acknowledged only after it is durable locally; sync catches up later |

## Install

Requirements:

- Git 2.31 or later.
- Linux or macOS.
- Rust 1.89 or later, if you build from source.

```bash
cargo install --git https://github.com/varops/ghf git-hf ghf-server
```

You can also download a binary from [Releases](https://github.com/varops/ghf/releases). Put `git-hf` on your `PATH`, and Git runs it as `git hf`.

## Quick start

**1. Run a server.** It is one binary plus a data directory. See [docs/server.md](docs/server.md) for TLS, retention and backups.

```bash
ghf-server --data-dir /var/lib/ghf token create --name ci --repos 'acme/*'
ghf-server --data-dir /var/lib/ghf serve --listen 0.0.0.0:7878 --tls-cert cert.pem --tls-key key.pem
```

**2. Enable a repository.** `.ghf` holds no secrets, so you can commit it.

```bash
git hf init --server https://ghf.example.com --repository acme/project
git add .ghf && git commit -m "Enable ghf"
```

**3. Set up each machine.** This is done once per machine.

```bash
git hf trust                       # approve the server for uploads
echo "$GHF_TOKEN" | git hf login   # or export GHF_TOKEN
git hf hooks install claude-code   # automatic capture (also: cursor)
git hf skill install               # teach the agent the workflow
```

**4. Work.** Hooks take a checkpoint after every agent edit. You can also take one yourself:

```bash
git hf checkpoint -m "parser passes" --annotate tests=pass
git hf status
git hf trace log
git hf trace diff @~2 @
git hf trace restore @~2                      # into a new worktree
git hf trace restore @~2 --in-place           # refused if anything is uncaptured
git hf trace apply ck_01j9..ck_01ja           # three-way merge onto this worktree
git hf trace fork @                           # new worktree, new trace
git hf trace close
git hf trace finalize --commit "$(git rev-parse HEAD)"
```

## What capture covers

Capture runs after each file-editing or shell tool call in a supported harness (Claude Code, Cursor), or whenever `git hf checkpoint` runs.

**Captured:**
- Tracked files and untracked files that are not ignored.
- Deletes and renames.
- The executable bit.
- Symlinks, stored as links.

**Not captured:**
- Paths matched by `.gitignore` or `.ghfignore`. Use `.ghfignore` for anything that must never leave the machine.
- Files over `capture.max_file_size` (default 100 MB). These are recorded as skipped.
- Submodules.
- The Git index (staged state).

**Not seen:** hooks fire on the agent's tool calls. Anything else that writes files is invisible until the next checkpoint: a background process, a generator run outside a tool call, or a human in an editor. Because every checkpoint is a full worktree state, the next checkpoint picks those changes up.

## Safety rules

- **Restore defaults to a new worktree.** An in-place restore runs only if the working tree exactly matches the latest checkpoint. `--force` checkpoints first, so even a forced restore can be undone.
- **Apply is all or nothing.** It is a three-way merge that stops on any conflict and leaves the tree unchanged.
- **Restores and applies add history.** Each one records a new checkpoint.
- **One writer per worktree.** Parallel agents use separate Git worktrees. A second writer gets `E_WRITER_LOCKED`.
- **Pruning never touches Git.** It skips held traces, traces with unsynced checkpoints, and the base checkpoints of retained forks.

## Automation contract

Agents are the primary users, so the CLI is an API. Every command takes `--json` and prints one versioned envelope:

```json
{"schema":"ghf/v1","command":"trace apply","ok":false,
 "error":{"code":"E_APPLY_CONFLICT","message":"1 conflicting path(s); the working tree is unchanged"},
 "data":{"applied":false,"conflicts":[{"path":"src/lib.rs","reason":"content changed on both sides"}]}}
```

- **Error codes** are stable and never change meaning.
- **Exit codes** map to error classes.
- **No prompts:** with `--json` or a non-TTY, ghf never prompts.
- **Idempotency:** mutating commands accept `--idempotency-key`.

See [docs/cli.md](docs/cli.md) for every command, its data and every error code.

## Performance

Measured on a 2,000-file repository with 200 edits, release build, Apple Silicon laptop:

| Metric | Result | PRD target |
|---|---|---|
| Checkpoint latency, one edit | p50 41 ms, p95 45 ms | p95 < 50 ms |
| Sync of 200 checkpoints (1,721 objects, 0.6 MB) | 1.7 s | p95 < 5 s per sync |
| Cross-machine restore | 0.6 s | p95 < 10 s |

About half of capture time is `git ls-files`. Reproduce with:

```bash
cargo test --release -p git-hf --test bench -- --ignored --nocapture
```

To see where time goes in one capture, set `GHF_TIMING=1`.

## Layout

| Path | Crate | What it is |
|---|---|---|
| [`extension/`](extension) | `git-hf` | Client binary that Git runs as `git hf` |
| [`server/`](server) | `ghf-server` | Canonical store for checkpoints and traces |
| [`proto/`](proto) | `ghf-proto` | Shared object format, records, API types and error codes |
| [`skills/ghf/`](skills/ghf) | | Agent skill; `git hf skill install` writes it |
| [`docs/`](docs) | | Concept, PRDs, CLI reference and server guide |

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests plus the PRD §14 acceptance scenarios
```

The acceptance suite in [`extension/tests/acceptance.rs`](extension/tests/acceptance.rs) runs every PRD scenario. It uses real Git repositories, a real `ghf-server` and simulated machines.
