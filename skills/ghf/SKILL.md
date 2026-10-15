---
name: ghf
description: Work safely in repositories that use ghf (Git High Frequency) checkpoints. Use when a repository has a `.ghf` file, when you need to roll back to an earlier working state, hand work to another agent or machine, fork an experiment, or show what was tried. Covers automatic capture, the manual `git hf checkpoint` fallback, restore vs apply vs fork, closing and finalizing traces, and the gaps capture does not cover.
---

# ghf

> Status: stub. Sections are outlined from PRD §9.11 and get filled in as the CLI lands.

ghf records the state of your worktree after each edit as a **checkpoint**. Checkpoints for one unit of work form a **trace**. Traces never merge; accepted work still lands as ordinary Git commits.

## Detect ghf

TODO: a `.ghf` file at the repo root; `git hf status --json`.

## One worktree per agent

TODO: one active writer per worktree. Parallel agents use separate Git worktrees. `E_WRITER_LOCKED` means another writer owns this one.

## Capture

TODO: automatic capture through harness hooks after file-editing tool calls; `git hf checkpoint [-m "intent"] [--annotate tests=pass]` as the fallback.

## What capture does not see

Say this plainly to the user when it matters:

- Hooks see tool calls. They do **not** see files written by shell scripts, code generators, or a human in an editor. Run `git hf checkpoint` after those.
- The Git index (staged state) is **not** captured. Checkpoints hold the working tree only.

## Restore, apply, fork

TODO:

- `git hf trace restore <id>`: defaults to a new worktree. Prefer this.
- `--in-place`: only when the tree matches the latest checkpoint; `E_WORKTREE_DIRTY` otherwise.
- `git hf trace apply <a>..<b>`: three-way apply; stops on `E_APPLY_CONFLICT` with the tree unchanged.
- `git hf trace fork <id>`: start an experiment in a new worktree and trace.

## Close and finalize

TODO: `git hf trace close`, then `git hf trace finalize --commit <sha>` or `--abandoned`.

## Commit normally

TODO: make ordinary Git commits. Never add ghf IDs, trailers or notes to commits.

## Automation contract

TODO: always pass `--json`; branch on `error.code`, never on message text.
