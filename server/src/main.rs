//! `ghf-server`: one binary plus a data directory (PRD §9.12).
//!
//! HTTP stack, auth and storage arrive in M2.

use std::path::PathBuf;
use std::process;

use clap::Parser;
use ghf_proto::{ErrorCode, ExitCode, SCHEMA_VERSION};

#[derive(Debug, Parser)]
#[command(
    name = "ghf-server",
    version,
    about = "ghf checkpoint and trace server"
)]
struct Cli {
    /// Directory for manifests, blobs and trace metadata.
    #[arg(long, env = "GHF_DATA_DIR")]
    data_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, env = "GHF_LISTEN", default_value = "127.0.0.1:7878")]
    listen: String,
}

fn main() {
    let cli = Cli::parse();
    eprintln!(
        "ghf-server {} ({SCHEMA_VERSION}): serving {} on {} is not implemented yet ({})",
        env!("CARGO_PKG_VERSION"),
        cli.data_dir.display(),
        cli.listen,
        ErrorCode::NotImplemented,
    );
    process::exit(ExitCode::NotImplemented as i32);
}
