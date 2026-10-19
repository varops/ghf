# ghf

**High-frequency source control for AI agents. Git-compatible by design.**

> [!IMPORTANT]
> ghf is an early preview, currently being tested in Cloop and Automaze Works. Anything can change before v1 is officially released, including commands, the `--json` schema, the server API and the storage format.

ghf (Git High Frequency) saves the state of a worktree every time an agent finishes an edit. Each save is a **checkpoint**. The checkpoints for one unit of work form a **trace**. Any agent on any authorized machine can list, diff, restore, apply or fork a trace.

ghf sits next to Git, not inside it. It writes nothing into commits, messages, refs or notes, and touches Git's index and object store only when you ask it to re-stage a checkpoint's staged changes (`--index`), exactly as `git add` would. Accepted work still lands as ordinary Git commits and pull requests, and plain Git users never need to know ghf exists.

- **Git** holds accepted history.
- **ghf** holds recoverable working history, with its own retention, permissions and sync.

**Status: 0.1.0-rc.2.** The full MVP is implemented: capture, sync, restore and apply, and the trace lifecycle.

## What it does for a fleet

| Use case | How |
|---|---|
| Roll back to the last green state | `ghf trace log --annotation tests=pass`, then `ghf trace restore <id> --in-place` |
| Hand work to another machine | `ghf sync` here; `ghf trace restore <id>` there |
| Try two approaches from one point | `ghf trace fork <id>` once per attempt |
| Show a human what was tried | `ghf trace log` and `ghf trace diff <a> <b>` |
| Survive a crashed worker | A checkpoint is acknowledged only after it is durable locally; sync catches up later |

New here? Start with the [user guide](docs/guide.md).

## Install

Requirements:

- Git 2.31 or later.
- Linux, macOS or Windows.
- Rust 1.89 or later, if you build from source.

```bash
cargo install --git https://github.com/varops/ghf git-hf ghf-server
```

You can also download the binaries from [Releases](https://github.com/varops/ghf/releases). The client installs under two names that run the same program: `ghf`, and `git-hf`, which Git runs as `git hf`. Use whichever you prefer.

## Quick start

**1. Run a server.** It is one binary plus a data directory. See [docs/server.md](docs/server.md) for TLS, retention and backups.

```bash
ghf-server --data-dir /var/lib/ghf token create --name ci --repos 'acme/*'
ghf-server --data-dir /var/lib/ghf serve --listen 0.0.0.0:7878 --tls-cert cert.pem --tls-key key.pem
```

**2. Enable a repository.** `.ghf` holds no secrets, so you can commit it.

```bash
ghf init --server https://ghf.example.com --repository acme/project
git add .ghf && git commit -m "Enable ghf"
```

**3. Set up each machine.** This is done once per machine.

```bash
ghf trust                       # approve the server for uploads
echo "$GHF_TOKEN" | ghf login   # or export GHF_TOKEN
ghf hooks install claude-code   # automatic capture (also: cursor)
ghf skill install               # teach the agent the workflow
```

**4. Work.** Hooks take a checkpoint after every agent edit. You can also take one yourself:

```bash
ghf checkpoint -m "parser passes" --annotate tests=pass
ghf status
ghf trace log
ghf trace diff @~2 @
ghf trace restore @~2                      # into a new worktree
ghf trace restore @~2 --in-place           # refused if anything is uncaptured
ghf trace apply ck_01j9..ck_01ja           # three-way merge onto this worktree
ghf trace fork @                           # new worktree, new trace
ghf trace close
ghf trace finalize --commit "$(git rev-parse HEAD)"
```

## What capture covers

Capture runs after each file-editing or shell tool call in a supported harness (Claude Code, Cursor), or whenever `ghf checkpoint` runs.

**Captured:**
- Tracked files and untracked files that are not ignored.
- Deletes and renames.
- The executable bit.
- Symlinks, stored as links.
- Staged changes (the Git index). Restore re-stages them only with `--index`.
- Submodules, as the commit each one has checked out.

**Not captured:**
- Paths matched by `.gitignore` or `.ghfignore`. Use `.ghfignore` for anything that must never leave the machine.
- Files over `capture.max_file_size` (default 100 MB). These are recorded as skipped.
- Uncommitted changes inside a submodule.

**Not seen by hooks:** hooks fire on the agent's tool calls. A background process, a generator run outside a tool call, or a human in an editor writes files no hook sees. Run `ghf watch` beside the agent to checkpoint those changes as they happen. Without it, the next checkpoint still picks them up, because every checkpoint is a full worktree state.

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

Measured on a 2,000-file repository, release build, Apple Silicon laptop:

| Metric | Result | Target |
|---|---|---|
| Checkpoint after editing 1–3 existing files (200 edits) | p50 25–27 ms, p95 28–34 ms | p95 < 50 ms |
| Checkpoint after creating a file | p50 42 ms, p95 45–51 ms | p95 < 50 ms (borderline) |
| Sync of 250 checkpoints (about 2,000 objects, 0.7 MB) | 2.3 s | p95 < 5 s per sync |
| Cross-machine restore | 0.6 s | p95 < 10 s |

**Why creating a file is slower.** When files change but none are added, removed or renamed, ghf reuses its last `git ls-files` listing. It knows nothing was added or removed because the timestamps on the listed directories haven't changed. Creating a file changes that set, so ghf asks git again. It runs git in parallel with hashing, but on this machine starting git alone takes about 13 ms.

Reproduce with:

```bash
cargo test --release -p git-hf --test bench -- --ignored --nocapture
```

To see where time goes in one capture, set `GHF_TIMING=1`.

## Layout

| Path | Crate | What it is |
|---|---|---|
| [`extension/`](extension) | `git-hf` | Client: the `ghf` and `git-hf` binaries (Git runs the latter as `git hf`) |
| [`server/`](server) | `ghf-server` | Canonical store for checkpoints and traces |
| [`proto/`](proto) | `ghf-proto` | Shared object format, records, API types and error codes |
| [`skills/ghf/`](skills/ghf) | | Agent skill; `ghf skill install` writes it |
| [`docs/`](docs) | | [User guide](docs/guide.md), [CLI reference](docs/cli.md), [server guide](docs/server.md) |

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests plus the end-to-end acceptance scenarios
```

The acceptance suite in [`extension/tests/acceptance.rs`](extension/tests/acceptance.rs) runs every acceptance scenario. It uses real Git repositories, a real `ghf-server` and simulated machines.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
