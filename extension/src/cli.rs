//! Command grammar. Mirrors the illustrative CLI in PRD §10; not final.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "git hf",
    bin_name = "git hf",
    version,
    about = "High-frequency source control for AI agents"
)]
pub struct Cli {
    /// Emit the versioned JSON envelope instead of human output. Never prompts.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a .ghf file for this repository.
    Init {
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        repository: Option<String>,
    },
    /// Show capture failures, unsynced checkpoints and trace state.
    Status,
    /// Capture the current worktree state.
    Checkpoint {
        /// Short intent line.
        #[arg(short, long)]
        message: Option<String>,
        /// Annotation such as `tests=pass`. Repeatable.
        #[arg(long = "annotate", value_name = "KEY=VALUE")]
        annotations: Vec<String>,
    },
    /// Push pending checkpoints to the server and report what is left.
    Sync,
    /// Inspect, restore and manage traces.
    #[command(subcommand)]
    Trace(TraceCommand),
    /// Prune eligible trace payloads.
    Prune {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TraceCommand {
    /// List traces with state.
    List,
    /// List checkpoints in a trace.
    Log { trace: Option<String> },
    /// Diff two checkpoints, or a checkpoint and the working tree.
    Diff { a: String, b: Option<String> },
    /// Reconstruct a checkpoint, by default into a new worktree.
    Restore {
        checkpoint: String,
        #[command(flatten)]
        target: RestoreTarget,
    },
    /// Three-way apply the changes in `<a>..<b>` onto the working tree.
    Apply {
        #[arg(value_name = "A..B")]
        range: String,
        #[arg(long)]
        write_conflicts: bool,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Start a new trace from a checkpoint in a new worktree.
    Fork {
        checkpoint: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
    /// Mark a trace closed.
    Close { trace: Option<String> },
    /// Finalize a trace against a landed commit, or as abandoned.
    Finalize {
        #[arg(
            long,
            conflicts_with = "abandoned",
            required_unless_present = "abandoned"
        )]
        commit: Option<String>,
        #[arg(long)]
        abandoned: bool,
    },
    /// Place a manual hold on a trace.
    Pin { trace: String },
}

#[derive(Debug, Args)]
pub struct RestoreTarget {
    #[arg(long, conflicts_with = "in_place")]
    pub worktree: Option<PathBuf>,
    /// Restore into the current worktree; refused if uncaptured work exists.
    #[arg(long)]
    pub in_place: bool,
    /// With --in-place: checkpoint the current state first, then restore.
    #[arg(long, requires = "in_place")]
    pub force: bool,
}

impl Command {
    /// Stable command name used in the JSON envelope.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Status => "status",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Sync => "sync",
            Self::Prune { .. } => "prune",
            Self::Trace(t) => match t {
                TraceCommand::List => "trace list",
                TraceCommand::Log { .. } => "trace log",
                TraceCommand::Diff { .. } => "trace diff",
                TraceCommand::Restore { .. } => "trace restore",
                TraceCommand::Apply { .. } => "trace apply",
                TraceCommand::Fork { .. } => "trace fork",
                TraceCommand::Close { .. } => "trace close",
                TraceCommand::Finalize { .. } => "trace finalize",
                TraceCommand::Pin { .. } => "trace pin",
            },
        }
    }
}
