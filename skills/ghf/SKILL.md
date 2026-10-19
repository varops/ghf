---
name: ghf
description: Work safely in Git repositories that use ghf (git hf) checkpoints. Use when a repository has a `.ghf` file, when you need to roll back to an earlier working state (for example the last state where tests passed), continue another agent's or machine's work, try an alternative approach from an earlier point, see what was tried, or move changes between worktrees. Covers automatic capture, the `ghf checkpoint` fallback, restore vs apply vs fork, closing and finalizing traces, and what capture does not see.
---

# ghf: checkpoints for agent work

ghf saves the state of your worktree after each edit. Each save is a **checkpoint**. The checkpoints for one unit of work form a **trace**. You can list, diff, restore, apply or fork any checkpoint, on this machine or another one.

ghf never writes into Git history. Finished work still lands as ordinary Git commits.

## 1. Detect ghf

A repository uses ghf if it has a `.ghf` file at its root. Check its state with:

```bash
ghf status --json
```

Always pass `--json` and act on `error.code`, never on message text. ghf never prompts when `--json` is set.

## 2. One worktree per agent

Only one writer may use a worktree's trace at a time. If two agents edit the same worktree, the second one gets `E_WRITER_LOCKED`.

For parallel work, give each agent its own Git worktree:

```bash
git worktree add ../task-b
```

Each worktree gets its own worktree context and its own trace.

## 3. Capture

**Automatic.** If the harness hooks are installed, every file edit and shell command creates a checkpoint. You do nothing. Install the hooks with:

```bash
ghf hooks install claude-code   # or: cursor
```

**Manual.** Call this yourself after edits the hooks cannot see, and to mark meaningful states:

```bash
ghf checkpoint -m "short intent" --annotate tests=pass
```

After running tests, mark the result with `--annotate tests=pass` or `--annotate tests=fail`. A pass annotation makes "last green" easy to find later. If nothing changed, a checkpoint with a message or annotation is still recorded.

## 4. What capture does not see

Tell the user plainly when either of these matters:

- **Out-of-band writes.** Hooks fire on the agent's tool calls. They do not see files written by a background process, a code generator running outside a tool call, or a human in an editor. Run `ghf checkpoint` after those, or keep `ghf watch` running beside you to checkpoint them automatically.
- **Staged changes come back only on request.** The Git index is captured, but restores leave it alone unless you pass `--index`.
- **Submodules are recorded by commit.** Uncommitted changes inside a submodule are not captured.

Also never captured:

- Paths matched by `.gitignore` or `.ghfignore`.
- Files over the size limit. These are listed in the checkpoint's `skipped`.

## 5. Find a checkpoint

```bash
ghf trace log --json                          # this worktree's trace, newest last
ghf trace log --annotation tests=pass --json  # only green states
ghf trace list --json                         # all traces, every machine (after sync)
ghf trace diff @~3 @                          # compare two checkpoints
ghf trace diff @~1                            # a checkpoint vs the working tree
```

Ways to name a checkpoint:

- `@` is the latest checkpoint of this worktree's trace, and `@~N` is N before it.
- A full id like `ck_…`, or a unique prefix of one.
- A trace id like `tr_…`, which means that trace's latest checkpoint.

## 6. Restore, apply, fork

Pick the least destructive option.

| Goal | Command | Safety |
|---|---|---|
| Look at or continue from an old state | `ghf trace restore <id>` | Creates a new worktree. The current one is untouched. **Prefer this.** |
| Roll this worktree back (for example to last green) | `ghf trace restore <id> --in-place` | Refused with `E_WORKTREE_DIRTY` if anything is uncaptured. Checkpoint first, then retry. |
| Roll back even with uncaptured edits | `ghf trace restore <id> --in-place --force` | Checkpoints the current state first, so it can be undone. |
| Bring changes from another trace into this one | `ghf trace apply <a>..<b>` | Three-way merge. Stops on conflicts with `E_APPLY_CONFLICT` and leaves the tree unchanged. |
| Bring in one checkpoint's own changes | `ghf trace apply <id>` | Same as `<parent>..<id>`. |
| Try a different approach from a point | `ghf trace fork <id>` | New worktree, new trace. |

Every restore and apply is recorded as a new checkpoint, so it can be undone.

When `apply` reports conflicts, `data.conflicts` lists the paths and why. You have two options:

- Resolve them by hand.
- Re-run with `--write-conflicts`, which leaves standard conflict markers. Fix the markers, then run `ghf checkpoint`.

### Roll back to last green

```bash
ghf trace log --annotation tests=pass --json   # take the last id
ghf checkpoint                                 # capture anything pending
ghf trace restore <id> --in-place
```

## 7. Hand off across machines

1. **On the first machine,** run `ghf sync`. Sync also runs in the background after each checkpoint.
2. **On the second machine,** run `ghf trust`, then `ghf sync`, then `ghf trace restore <id>`.

The restore always creates a new worktree with a new context. It never reuses the original one.

## 8. Finish

1. Commit normally with `git add` and `git commit`. **Never** put ghf ids, trailers or notes in commit messages.
2. Close the trace when the work is done: `ghf trace close`.
3. After the commit lands on the main branch, finalize the trace with `ghf trace finalize --commit <sha>`. If the work was dropped, use `ghf trace finalize --abandoned` instead.
4. To keep a trace from being pruned, use `ghf trace pin <trace>`. Release it with `unpin`.

## 9. Error codes to handle

| Code | Exit | Meaning | Do |
|---|---|---|---|
| `E_WRITER_LOCKED` | 3 | Another writer owns this worktree's trace | Use your own worktree. Use `--takeover` only if the owner is gone. |
| `E_WORKTREE_DIRTY` | 3 | Uncaptured changes would be lost | `ghf checkpoint`, then retry, or restore into a new worktree |
| `E_APPLY_CONFLICT` | 4 | Merge conflicts; the tree is unchanged | Resolve, or use `--write-conflicts` |
| `E_NOT_FOUND` | 6 | Unknown checkpoint or trace | `ghf sync`, then retry |
| `E_OBJECT_MISSING` | 6 | Content was pruned or never synced | Choose another checkpoint |
| `E_NOT_SYNCED`, `E_SERVER` | 5 | Server unreachable | Work continues locally; sync later |
| `E_UNTRUSTED_SERVER` | 5 | Server not trusted on this machine | Ask the user to run `ghf trust` |
| `E_CAPTURE_FAILED` | 1 | Strict-mode capture failed | Stop and report to the user |

The full reference is in `docs/cli.md` in the ghf repository.
