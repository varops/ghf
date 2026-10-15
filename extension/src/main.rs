//! `git-hf`: Git discovers this binary as the `git hf` subcommand.

mod cli;

use std::process;

use clap::Parser;
use clap::error::ErrorKind;
use ghf_proto::{ErrorCode, ExitCode, Response};

use cli::{Cli, Command};

/// A command failure carrying a stable error code.
struct Failure {
    code: ErrorCode,
    message: String,
}

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
                emit_json(&Response::err("", ErrorCode::Usage, msg.trim()));
                process::exit(ExitCode::Usage as i32);
            }
            err.exit();
        }
    };

    let name = cli.command.name();
    match run(&cli.command) {
        Ok(data) => {
            if cli.json {
                emit_json(&Response::ok(name, data));
            }
        }
        Err(f) => {
            if cli.json {
                emit_json(&Response::err(name, f.code, &f.message));
            } else {
                eprintln!("git hf {name}: {} ({})", f.message, f.code);
            }
            process::exit(f.code.exit_code() as i32);
        }
    }
}

fn run(command: &Command) -> Result<serde_json::Value, Failure> {
    Err(Failure {
        code: ErrorCode::NotImplemented,
        message: format!("`{}` is not implemented yet", command.name()),
    })
}

fn emit_json(resp: &Response) {
    println!(
        "{}",
        serde_json::to_string(resp).expect("response serializes")
    );
}
