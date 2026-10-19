//! Command grammar.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::hooks::{Harness, Scope};

#[derive(Debug, Parser)]
#[command(
    name = "git hf",
    bin_name = "git hf",
    version,
    about = "High-frequency source control for AI agents",
    after_help = "Every command accepts --json and then prints one versioned envelope:\n  {\"schema\":\"ghf/v1\",\"command\":...,\"ok\":true|false,\"data\":...,\"error\":{\"code\":\"E_...\",\"message\":...}}"
)]
pub struct Cli {
    /// Emit the versioned JSON envelope instead of human output. Never prompts.
    #[arg(long, global = true)]
    pub json: bool,

    /// Replay-safe key for mutating commands: a repeat with the same key
    /// returns the first result instead of acting again.
    #[arg(long, global = true, value_name = "KEY")]
    pub idempotency_key: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a .ghf file for this repository and set up the local store.
    Init {
        /// ghf server URL (https, or http for localhost).
        #[arg(long)]
        server: Option<String>,
        /// Repository identity on the server, `owner/name`. Defaults to the origin remote.
        #[arg(long)]
        repository: Option<String>,
        /// Fail tool calls when capture fails (for pipelines).
        #[arg(long)]
        strict: bool,
        /// Trust the server without prompting.
        #[arg(long)]
        trust: bool,
        /// Overwrite an existing .ghf.
        #[arg(long)]
        force: bool,
    },
    /// Show trace state, unsynced checkpoints, capture failures and warnings.
    Status,
    /// Capture the current worktree state.
    Checkpoint {
        /// Short intent line.
        #[arg(short, long)]
        message: Option<String>,
        /// Annotation such as `tests=pass`. Repeatable.
        #[arg(long = "annotate", value_name = "KEY=VALUE")]
        annotations: Vec<String>,
        /// Take over the trace from another writer.
        #[arg(long)]
        takeover: bool,
        /// Name for a new trace, if this checkpoint opens one ($GHF_TRACE_NAME).
        #[arg(long)]
        trace_name: Option<String>,
        /// Record a checkpoint even if nothing changed.
        #[arg(long)]
        force: bool,
    },
    /// Push pending checkpoints, pull remote traces, and report what is left.
    Sync {
        #[arg(long, hide = true)]
        background: bool,
    },
    /// Trust a ghf server for uploads from this machine.
    Trust {
        /// Server URL; defaults to the one in .ghf.
        url: Option<String>,
    },
    /// Save a server token (read from stdin) to the user credentials file.
    Login {
        #[arg(long)]
        server: Option<String>,
    },
    /// Inspect, restore and manage traces.
    #[command(subcommand)]
    Trace(TraceCommand),
    /// Prune eligible traces on the server and locally.
    Prune {
        #[arg(long)]
        dry_run: bool,
    },
    /// Install or remove automatic capture hooks for an agent harness.
    #[command(subcommand)]
    Hooks(HooksCommand),
    /// Checkpoint after file changes no hook sees (generators, editors, other processes).
    Watch {
        /// Quiet period after the last change before a checkpoint, e.g. 2s or 500ms.
        #[arg(long, default_value = "2s")]
        debounce: String,
        /// Longest continuous changes may delay a checkpoint.
        #[arg(long, default_value = "30s")]
        max_wait: String,
    },
    /// Hook entry point called by an agent harness (reads its payload on stdin).
    Hook { harness: Harness },
    /// Install or print the agent skill that teaches ghf workflows.
    #[command(subcommand)]
    Skill(SkillCommand),
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Write the skill to .claude/skills/ghf/SKILL.md (project) or ~/.claude/skills (user).
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Print the skill.
    Show,
}

#[derive(Debug, Subcommand)]
pub enum TraceCommand {
    /// List traces with state.
    List {
        /// Only traces in this state: open, closed, finalized, pruned.
        #[arg(long)]
        state: Option<String>,
        /// Include pruned traces.
        #[arg(long)]
        all: bool,
    },
    /// List checkpoints in a trace (default: this worktree's current trace).
    Log {
        trace: Option<String>,
        /// Only checkpoints with this annotation, e.g. tests=pass. Repeatable.
        #[arg(long = "annotation", value_name = "KEY=VALUE")]
        annotations: Vec<String>,
        /// Show only the last N checkpoints.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Diff two checkpoints, or a checkpoint and the working tree.
    Diff {
        a: String,
        b: Option<String>,
        /// Include the unified patch in --json output.
        #[arg(long)]
        patch: bool,
    },
    /// Reconstruct a checkpoint, by default into a new worktree.
    Restore {
        checkpoint: String,
        #[command(flatten)]
        target: RestoreTarget,
    },
    /// Three-way apply `A..B` (or one checkpoint's own changes) onto the working tree.
    Apply {
        #[arg(value_name = "A..B")]
        range: String,
        /// Leave conflict markers instead of refusing.
        #[arg(long)]
        write_conflicts: bool,
    },
    /// Start a new trace from a checkpoint in a new worktree.
    Fork {
        checkpoint: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
        /// Also restage the checkpoint's staged changes.
        #[arg(long)]
        index: bool,
    },
    /// Mark a trace closed (default: this worktree's open trace).
    Close { trace: Option<String> },
    /// Finalize a trace against a landed commit, or as abandoned.
    Finalize {
        trace: Option<String>,
        #[arg(
            long,
            conflicts_with = "abandoned",
            required_unless_present = "abandoned"
        )]
        commit: Option<String>,
        #[arg(long)]
        abandoned: bool,
    },
    /// Place a manual hold (named "pin") that stops pruning.
    Pin { trace: String },
    /// Release the manual "pin" hold.
    Unpin { trace: String },
    /// Place a named hold, optionally expiring.
    Hold {
        trace: String,
        #[arg(long)]
        name: String,
        /// Expire after this long, e.g. 7d or 12h.
        #[arg(long)]
        expires: Option<String>,
    },
    /// Release a named hold.
    Release {
        trace: String,
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct RestoreTarget {
    /// Directory for the new worktree (default: next to the repository).
    #[arg(long, conflicts_with = "in_place")]
    pub worktree: Option<PathBuf>,
    /// Restore into the current worktree; refused if uncaptured work exists.
    #[arg(long)]
    pub in_place: bool,
    /// With --in-place: checkpoint the current state first, then restore.
    #[arg(long, requires = "in_place")]
    pub force: bool,
    /// Also restage the checkpoint's staged changes (writes them into the
    /// Git index and object store, as `git add` would).
    #[arg(long)]
    pub index: bool,
}

#[derive(Debug, Subcommand)]
pub enum HooksCommand {
    /// Add ghf hooks to the harness settings.
    Install {
        harness: Harness,
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Remove ghf hooks from the harness settings.
    Uninstall {
        harness: Harness,
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Report whether ghf hooks are installed.
    Status {
        harness: Harness,
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
}

impl Command {
    /// Stable command name used in the JSON envelope.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Status => "status",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Sync { .. } => "sync",
            Self::Trust { .. } => "trust",
            Self::Login { .. } => "login",
            Self::Prune { .. } => "prune",
            Self::Hook { .. } => "hook",
            Self::Watch { .. } => "watch",
            Self::Skill(SkillCommand::Install { .. }) => "skill install",
            Self::Skill(SkillCommand::Show) => "skill show",
            Self::Hooks(h) => match h {
                HooksCommand::Install { .. } => "hooks install",
                HooksCommand::Uninstall { .. } => "hooks uninstall",
                HooksCommand::Status { .. } => "hooks status",
            },
            Self::Trace(t) => match t {
                TraceCommand::List { .. } => "trace list",
                TraceCommand::Log { .. } => "trace log",
                TraceCommand::Diff { .. } => "trace diff",
                TraceCommand::Restore { .. } => "trace restore",
                TraceCommand::Apply { .. } => "trace apply",
                TraceCommand::Fork { .. } => "trace fork",
                TraceCommand::Close { .. } => "trace close",
                TraceCommand::Finalize { .. } => "trace finalize",
                TraceCommand::Pin { .. } => "trace pin",
                TraceCommand::Unpin { .. } => "trace unpin",
                TraceCommand::Hold { .. } => "trace hold",
                TraceCommand::Release { .. } => "trace release",
            },
        }
    }

    /// Commands that change state and honor --idempotency-key.
    pub fn mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status
                | Self::Hook { .. }
                | Self::Watch { .. }
                | Self::Skill(SkillCommand::Show)
                | Self::Hooks(HooksCommand::Status { .. })
                | Self::Trace(
                    TraceCommand::List { .. }
                        | TraceCommand::Log { .. }
                        | TraceCommand::Diff { .. }
                )
        )
    }
}
