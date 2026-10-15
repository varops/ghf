//! `git-hf`: Git discovers this binary as the `git hf` subcommand.

mod capture;
mod cli;
mod cmd;
mod config;
mod error;
mod git;
mod hooks;
mod listing;
mod materialize;
mod refs;
mod remote;
mod scan;
mod store;
mod timing;
mod workspace;

use std::process;

use clap::Parser;
use clap::error::ErrorKind;
use ghf_proto::{ErrorCode, ExitCode, Response};
use serde_json::json;

use cli::{Cli, Command, HooksCommand, SkillCommand, TraceCommand};
use cmd::{Env, Out};
use error::{Failure, Result};

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let wants_json = std::env::args().any(|a| a == "--json");
            let is_info = matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            if wants_json && !is_info {
                let msg = err.render().to_string();
                emit_json(&Response::err("", ErrorCode::Usage, msg.trim(), None));
                process::exit(ExitCode::Usage as i32);
            }
            err.exit();
        }
    };

    // Hooks answer in the harness's format, not the ghf envelope.
    if let Command::Hook { harness } = cli.command {
        process::exit(hooks::run(harness));
    }

    let name = cli.command.name();
    let env = Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| ".".into()),
        json: cli.json,
    };
    let result = match (&cli.idempotency_key, cli.command.mutating()) {
        (Some(key), true) => idempotent(&env, key, &cli.command),
        _ => dispatch(&env, &cli.command).map(|o| (o, false)),
    };
    match result {
        Ok((out, _replayed)) => {
            if cli.json {
                emit_json(&Response::ok(name, out.data));
            } else if !out.human.is_empty() {
                println!("{}", out.human);
            }
        }
        Err(f) => {
            if cli.json {
                emit_json(&Response::err(name, f.code, &f.message, f.data));
            } else {
                eprintln!("git hf {name}: {} ({})", f.message, f.code);
            }
            process::exit(f.code.exit_code() as i32);
        }
    }
}

/// Run a mutating command at most once per key. A replay returns the
/// stored data; reusing a key with different arguments is an error.
fn idempotent(env: &Env, key: &str, command: &Command) -> Result<(Out, bool)> {
    let repo = git::Repo::discover(&env.cwd)?;
    let store = store::Store::open(&repo.common_dir)?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--json" && a != key && !a.starts_with("--idempotency-key"))
        .collect();
    let fingerprint = format!("{} {}", repo.root.display(), args.join("\u{1f}"));
    if let Some((fp, data)) = store.idempotent_response(key)? {
        if fp != fingerprint {
            fail_idem(key)?;
        }
        let data: serde_json::Value = serde_json::from_str(&data)?;
        return Ok((
            Out {
                human: format!("already done (idempotency key {key})"),
                data: json!({"replayed": true, "result": data}),
            },
            true,
        ));
    }
    let out = dispatch(env, command)?;
    store.save_idempotent(key, &fingerprint, &serde_json::to_string(&out.data)?)?;
    Ok((out, false))
}

fn fail_idem(key: &str) -> Result<()> {
    Err(Failure::new(
        ErrorCode::IdempotencyConflict,
        format!("idempotency key {key:?} was already used with different arguments"),
    ))
}

fn dispatch(env: &Env, command: &Command) -> Result<Out> {
    match command {
        Command::Init {
            server,
            repository,
            strict,
            trust,
            force,
        } => cmd::init(
            env,
            cmd::InitArgs {
                server: server.clone(),
                repository: repository.clone(),
                strict: *strict,
                trust: *trust,
                force: *force,
            },
        ),
        Command::Status => cmd::status(env),
        Command::Checkpoint {
            message,
            annotations,
            takeover,
            trace_name,
            force,
        } => cmd::checkpoint(
            env,
            cmd::CheckpointArgs {
                message: message.clone(),
                annotations: annotations.clone(),
                takeover: *takeover,
                trace_name: trace_name.clone(),
                force: *force,
            },
        ),
        Command::Sync { background } => cmd::sync(env, *background),
        Command::Trust { url } => cmd::trust(env, url.clone()),
        Command::Login { server } => cmd::login(env, server.clone()),
        Command::Prune { dry_run } => cmd::prune(env, *dry_run),
        Command::Hook { .. } => unreachable!("handled in main"),
        Command::Skill(SkillCommand::Show) => Ok(Out {
            human: hooks::SKILL.trim_end().to_owned(),
            data: json!({"skill": hooks::SKILL}),
        }),
        Command::Skill(SkillCommand::Install { scope }) => {
            let root = git::Repo::discover(&env.cwd).map(|r| r.root);
            let (path, changed) = hooks::install_skill(*scope, root.ok().as_deref())?;
            Ok(Out {
                human: format!(
                    "{} {}",
                    if changed { "wrote" } else { "up to date:" },
                    path.display()
                ),
                data: json!({"path": path, "changed": changed}),
            })
        }
        Command::Hooks(h) => {
            let (harness, scope, action) = match h {
                HooksCommand::Install { harness, scope } => (*harness, *scope, "install"),
                HooksCommand::Uninstall { harness, scope } => (*harness, *scope, "uninstall"),
                HooksCommand::Status { harness, scope } => (*harness, *scope, "status"),
            };
            let root = git::Repo::discover(&env.cwd)?.root;
            let (path, changed, installed) = hooks::manage(harness, scope, &root, action)?;
            Ok(Out {
                human: format!(
                    "{}: ghf hooks {} ({})",
                    path.display(),
                    if installed {
                        "installed"
                    } else {
                        "not installed"
                    },
                    if changed { "updated" } else { "unchanged" }
                ),
                data: json!({"path": path, "changed": changed, "installed": installed}),
            })
        }
        Command::Trace(t) => match t {
            TraceCommand::List { state, all } => cmd::trace_list(env, state.clone(), *all),
            TraceCommand::Log {
                trace,
                annotations,
                limit,
            } => cmd::trace_log(env, trace.clone(), annotations.clone(), *limit),
            TraceCommand::Diff { a, b, patch } => {
                cmd::trace_diff(env, a.clone(), b.clone(), *patch)
            }
            TraceCommand::Restore { checkpoint, target } => cmd::trace_restore(
                env,
                checkpoint.clone(),
                target.worktree.clone(),
                target.in_place,
                target.force,
            ),
            TraceCommand::Apply {
                range,
                write_conflicts,
            } => cmd::trace_apply(env, range.clone(), *write_conflicts),
            TraceCommand::Fork {
                checkpoint,
                worktree,
            } => cmd::trace_fork(env, checkpoint.clone(), worktree.clone()),
            TraceCommand::Close { trace } => cmd::trace_close(env, trace.clone()),
            TraceCommand::Finalize {
                trace,
                commit,
                abandoned,
            } => cmd::trace_finalize(env, trace.clone(), commit.clone(), *abandoned),
            TraceCommand::Pin { trace } => {
                cmd::trace_hold(env, trace.clone(), "pin".into(), None, false)
            }
            TraceCommand::Unpin { trace } => {
                cmd::trace_hold(env, trace.clone(), "pin".into(), None, true)
            }
            TraceCommand::Hold {
                trace,
                name,
                expires,
            } => cmd::trace_hold(env, trace.clone(), name.clone(), expires.clone(), false),
            TraceCommand::Release { trace, name } => {
                cmd::trace_hold(env, trace.clone(), name.clone(), None, true)
            }
        },
    }
}

fn emit_json(resp: &Response) {
    println!(
        "{}",
        serde_json::to_string(resp).expect("response serializes")
    );
}
