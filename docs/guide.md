# ghf user guide

This guide walks you through setting up ghf and using it day to day. For every flag and every JSON field, see the [CLI reference](cli.md). To run the server, see the [server guide](server.md).

- [1. What ghf does](#1-what-ghf-does)
- [2. Install](#2-install)
- [3. Set up a server](#3-set-up-a-server)
- [4. Enable a repository](#4-enable-a-repository)
- [5. Set up each machine](#5-set-up-each-machine)
- [6. Everyday use](#6-everyday-use)
- [7. Recipes](#7-recipes)
- [8. Finishing work](#8-finishing-work)
- [9. Using ghf from pipelines and agents](#9-using-ghf-from-pipelines-and-agents)
- [10. What ghf does not capture](#10-what-ghf-does-not-capture)
- [11. Troubleshooting](#11-troubleshooting)
- [12. Removing ghf](#12-removing-ghf)

## 1. What ghf does

ghf saves the state of your worktree every time an agent finishes an edit. Each save is a **checkpoint**: a complete copy of every captured file at that moment. The checkpoints for one unit of work form a **trace**.

You can do four things with a checkpoint:

- **Diff** it against another checkpoint, or against the working tree.
- **Restore** it into a new worktree, or roll the current worktree back to it.
- **Apply** the changes between two checkpoints onto your current work.
- **Fork** it, starting a new trace from that point.

Checkpoints sync to a ghf server, so another machine can restore them too.

ghf never writes into Git history. Your commits, branches, refs and history are exactly what they would be without it. Finished work still lands as normal commits and pull requests, and teammates who don't use ghf see nothing different.

| Term | Meaning |
|---|---|
| Checkpoint | One saved worktree state, id `ck_…` |
| Trace | The checkpoints of one unit of work, id `tr_…` |
| Worktree context | ghf's identity for one directory on one machine, id `wc_…` |
| Writer | The agent or person adding checkpoints to a trace |
| Hold | A named lock that stops a trace from being pruned |

## 2. Install

Download the archive for your platform from [Releases](https://github.com/varops/ghf/releases). Each archive holds the client as `ghf` and `git-hf` (the same program under two names) plus `ghf-server`. Put them on your `PATH`:

```bash
tar -xzf ghf-v0.1.0-rc.2-aarch64-apple-darwin.tar.gz
sudo cp ghf-v0.1.0-rc.2-aarch64-apple-darwin/{ghf,git-hf,ghf-server} /usr/local/bin/
```

Or build from source with Rust 1.89 or later:

```bash
cargo install --git https://github.com/varops/ghf git-hf ghf-server
```

Run it as `ghf`, or as `git hf`: Git finds `git-hf` on your `PATH`. Every example in this guide works either way. Check the install:

```bash
ghf --version
```

You need Git 2.31 or later, on Linux, macOS or Windows. On Windows, Git has no executable bit, and symlinks need Developer Mode (or administrator rights); without it, ghf writes a plain file holding the link target, as Git for Windows does.

## 3. Set up a server

You can skip this step to try ghf on one machine; checkpoints then stay local. You need a server to share checkpoints between machines.

Create a token, then start the server:

```bash
ghf-server --data-dir /var/lib/ghf token create --name fleet --repos 'acme/*'
ghf-server --data-dir /var/lib/ghf serve --listen 0.0.0.0:7878 \
  --tls-cert /etc/ghf/cert.pem --tls-key /etc/ghf/key.pem
```

`token create` prints the token once. Store it in your secret manager.

**TLS is required.** Clients refuse plain `http://` for any host other than localhost. Either pass a certificate and key as above, or run the server behind a reverse proxy that terminates TLS.

See the [server guide](server.md) for retention, backups and token scopes.

## 4. Enable a repository

Do this once per repository:

```bash
cd ~/src/project
ghf init --server https://ghf.example.com --repository acme/project
git add .ghf
git commit -m "Enable ghf"
```

`.ghf` is an ordinary file that holds no secrets, so it is safe to commit. Plain Git users can ignore it.

If you leave out `--repository`, ghf derives `owner/name` from the `origin` remote.

To keep paths from ever being captured or uploaded, list them in a `.ghfignore` file at the repository root. It uses the same syntax as `.gitignore`:

```gitignore
.env
secrets/
*.pem
```

## 5. Set up each machine

On every machine that runs agents, inside a clone of the repository:

```bash
ghf trust                       # approve the server for uploads from this machine
echo "$GHF_TOKEN" | ghf login   # or export GHF_TOKEN in the agent's environment
ghf hooks install claude-code   # automatic capture (or: cursor)
ghf skill install               # teach the agent how to use ghf
ghf status
```

What each command does:

- **`trust`** records the server's identity. If that identity ever changes, ghf stops uploading and tells you so.
- **`login`** saves the token to `~/.config/ghf/credentials.toml`, readable only by you.
- **`hooks install`** adds a post-edit hook to `.claude/settings.json`, or `.cursor/hooks.json` for Cursor.
  - Commit that file to enable capture for everyone who uses the repo.
  - Use `--scope local` for `.claude/settings.local.json` or `--scope user` for your home directory.
- **`skill install`** writes `.claude/skills/ghf/SKILL.md`.

`ghf status` should show the server as trusted and report no warnings.

## 6. Everyday use

With hooks installed, there is nothing to do: every file edit and shell command in the agent takes a checkpoint. If nothing changed, no checkpoint is recorded.

Take one yourself after work the hooks can't see, or to mark a meaningful state:

```bash
ghf checkpoint -m "parser handles nested quotes" --annotate tests=pass
```

Look around:

```bash
ghf status              # this worktree's trace, sync state, warnings
ghf trace log           # its checkpoints, newest last
ghf trace list          # every trace, including other machines' after a sync
ghf trace diff @~3 @    # what changed over the last three checkpoints
ghf trace diff @        # the working tree against the latest checkpoint
```

Ways to name a checkpoint:

- `@` is the latest checkpoint of this worktree's trace, and `@~N` is N before it.
- A full id like `ck_01m5054ddt4h527131xs986n09`, or a unique prefix such as `01m5054d`.
- A trace id like `tr_…`, which means that trace's latest checkpoint.

Sync runs in the background after each checkpoint. Run `ghf sync` to push and pull now, and to see what is still pending.

## 7. Recipes

### Roll back to the last state where tests passed

```bash
ghf trace log --annotation tests=pass   # pick the last one
ghf checkpoint                          # capture anything pending first
ghf trace restore <id> --in-place
```

If anything in the working tree is not captured, the in-place restore refuses with `E_WORKTREE_DIRTY` and changes nothing. You have two ways forward:

- Run `ghf checkpoint` and retry.
- Use `--force`, which takes that checkpoint for you before restoring.

Either way, the state you left is one restore away.

### Look at an old state without touching your work

```bash
ghf trace restore @~5
```

This creates a new Git worktree next to the repository, named `../<repo>-ghf-<suffix>`, and prints its path. Your current worktree is untouched. Add `--worktree <path>` to choose the location.

### Continue another machine's work

On the first machine, make sure everything is uploaded:

```bash
ghf sync
```

On the second machine:

```bash
ghf sync
ghf trace list
ghf trace restore <checkpoint-or-trace-id>
```

The restored worktree gets its own new context and trace, linked to the checkpoint it came from. Uncommitted changes come across too, because checkpoints hold the working tree, not commits.

### Try two approaches from the same point

```bash
ghf trace fork <id> --worktree ../attempt-a
ghf trace fork <id> --worktree ../attempt-b
```

Run one agent in each worktree. When one wins, finalize the losing trace as abandoned (see [8. Finishing work](#8-finishing-work)).

### Bring changes from one trace into another

```bash
ghf trace apply <from>..<to>   # the changes between two checkpoints
ghf trace apply <id>           # one checkpoint's own changes
```

Apply runs a three-way merge onto your working tree.

- **On conflict,** it stops and changes nothing, listing the conflicts with a reason for each.
- **To resolve by hand,** add `--write-conflicts` to leave standard conflict markers. Fix them, then run `ghf checkpoint`.

### Capture changes no hook sees

```bash
ghf watch
```

Run it beside the agent, for example in another terminal or as a background service. After each burst of file changes settles (2 seconds by default, `--debounce` to change), it takes a checkpoint. That catches generators, background processes and edits in your editor.

The watcher works on behalf of whoever is writing the trace. Its checkpoints carry the agent's name, are marked `captured_by=watch`, and never take the trace away from the agent.

### Keep what you staged

Every checkpoint records the Git index too: what was staged, including partial staging. Restores leave your index alone unless you ask:

```bash
ghf trace restore <id> --index              # new worktree with the same staged changes
ghf trace restore <id> --in-place --index   # roll back files and the index
```

`--index` writes the staged content into Git's object store and index, exactly as `git add` would. That is the only time ghf writes to Git.

### Submodules

A checkpoint records which commit each submodule has checked out. On restore, a submodule that is checked out, clean and has that commit locally is moved to it. Otherwise ghf leaves it alone and tells you what to run, for example `git submodule update --init lib`.

### Show a reviewer what was tried

```bash
ghf trace log <trace>
ghf trace diff <a> <b>
```

### Viewing traces in a browser

Reviewers without ghf installed can use the server's web viewer. Create a read-only token for them:

```bash
ghf-server --data-dir /var/lib/ghf token create --name review --repos 'acme/project' --read-only
```

They open `https://ghf.example.com/ui`, paste the token once, and can browse every trace in the repositories it covers: the timeline of checkpoints with their messages and `tests=pass`/`fail` annotations, diffs between any two checkpoints, and files as they were at each one. Page URLs are stable, so you can paste a link to a trace or diff into a review. The viewer never changes anything. See the [server guide](server.md#viewing-traces-in-a-browser) for details.

## 8. Finishing work

Commit and push the way you always do. Then tell ghf the work is done:

```bash
ghf trace close                                      # stop writing to this trace
ghf trace finalize --commit "$(git rev-parse HEAD)"  # after the commit is on main
```

`finalize --commit` checks that the commit is reachable from the main branch. That is `origin/HEAD`, `origin/main` and so on, or `[git] main` in `.ghf`.

If you drop the work instead, run `ghf trace finalize --abandoned`.

**Pruning.**
- A finalized trace is deleted from the server and the local store 30 days after finalization.
- A closed trace that is never finalized is deleted after 90 days, and `status` warns a week before.
- Both periods are set per repository on the server.

**Keeping a trace longer:**

```bash
ghf trace pin <trace>                          # until you unpin it
ghf trace hold <trace> --name audit --expires 14d
```

Pruning runs on the server every hour, or on demand with `ghf prune`. It never touches Git.

## 9. Using ghf from pipelines and agents

Every command accepts `--json` and then prints exactly one object:

```json
{"schema":"ghf/v1","command":"checkpoint","ok":true,"data":{"created":true,"checkpoint":{"id":"ck_…"}}}
```

Branch on `error.code`, never on message text. Exit codes group the errors: 3 means ghf refused to protect work, 4 a conflict, 5 a server or network problem, 6 not found. See the [CLI reference](cli.md#error-codes) for the full table.

**Settings for pipelines:**

| Setting | Effect |
|---|---|
| `ghf init --strict`, or `GHF_STRICT=1` | A failed capture stops the agent instead of warning. Use this in CI-like pipelines. |
| `GHF_WRITER=<id>` | Names the writer. Hooks already use the agent's session id. |
| `GHF_TRACE_NAME=<ticket>` | Names the trace a run opens, for example after the ticket. |
| `GHF_SYNC=manual` | Sync only when you run `ghf sync`. |
| `--idempotency-key <key>` | A retried command does not act twice. |

**Parallel agents.** Give each one its own Git worktree with `git worktree add ../task-b`. Two agents in one worktree conflict: the second gets `E_WRITER_LOCKED`.

## 10. What ghf does not capture

- **Out-of-band writes, unless you watch.** Hooks fire on the agent's own tool calls. Files written by a background process, a code generator or a person in an editor are picked up at the next checkpoint. Run `ghf watch` to checkpoint them as they happen (see [7. Recipes](#capture-changes-no-hook-sees)).
- **Ignored, oversized and special files.**
  - Anything matched by `.gitignore`, `.git/info/exclude`, your global excludes or `.ghfignore`.
  - Files over 100 MB. The limit is `capture.max_file_size`, and skipped files are listed in the checkpoint.
- **Inside submodules.** A submodule is recorded as the commit it has checked out; uncommitted changes inside it are not captured.
- **New files in ignored-only directories.** A new file in a directory that so far held only ignored files can take up to a minute to appear in checkpoints.

Restore and apply never overwrite a file ghf did not capture. If one is in the way, they refuse and list it.

## 11. Troubleshooting

Start with `ghf status`. It lists recent capture failures, unsynced checkpoints and any server problem.

| You see | Cause | Do |
|---|---|---|
| `E_NOT_INITIALIZED` | No `.ghf` in this repository | `ghf init`, or pull the commit that adds it |
| `E_UNTRUSTED_SERVER` | This machine hasn't approved the server | `ghf trust` |
| `E_AUTH` | Missing or rejected token | `echo "$TOKEN" \| ghf login`, or set `GHF_TOKEN` |
| `E_SERVER_MISMATCH` | The server's identity changed | Confirm with the server's operator, then `ghf trust` |
| `E_SERVER` | Server unreachable | Keep working; checkpoints are safe locally. `ghf sync` later |
| `E_WRITER_LOCKED` | Another agent owns this worktree's trace | Use a separate worktree. If the owner is gone, wait 30 minutes or pass `--takeover` |
| `E_WORKTREE_DIRTY` | A restore would lose uncaptured work | `ghf checkpoint` first, restore into a new worktree, or use `--force` |
| `E_APPLY_CONFLICT` | Both sides changed the same lines or files | Resolve by hand or use `--write-conflicts` |
| `E_NOT_FOUND` | Unknown checkpoint id | `ghf sync` to pull other machines' traces |
| `E_OBJECT_MISSING` | The checkpoint was pruned, or never left its machine | Pick another checkpoint, or sync the machine that made it |
| `E_COMMIT_NOT_REACHABLE` | The commit isn't on the main branch yet | Merge first, `git fetch`, then finalize |
| "refused by the server" in `status` | The trace was pruned on the server | Those checkpoints remain restorable on this machine only |

To see where capture time goes, run a checkpoint with `GHF_TIMING=1`.

If the hooks seem silent, check two things:

- `ghf hooks status claude-code` shows whether the hooks are installed.
- `git-hf` must be on the agent's `PATH`: the hooks call `git hf hook …`.

## 12. Removing ghf

```bash
ghf hooks uninstall claude-code
git rm .ghf
rm -rf "$(git rev-parse --git-common-dir)/ghf"   # local checkpoints for this repository
```

Your Git history is unaffected; ghf never wrote to it. To remove trust and credentials, delete `~/.config/ghf/`.
