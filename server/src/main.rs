//! `ghf-server`: canonical store for ghf checkpoints and traces.
//! One binary plus a data directory (PRD §9.12).

mod http;
mod prune;
mod store;
#[cfg(test)]
mod tests;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use ghf_proto::Policy;
use ghf_proto::retention::parse_duration_ms;

use store::Store;

#[derive(Debug, Parser)]
#[command(
    name = "ghf-server",
    version,
    about = "ghf checkpoint and trace server"
)]
struct Cli {
    /// Directory for metadata, objects and the server id.
    #[arg(long, env = "GHF_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP API.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "GHF_LISTEN", default_value = "127.0.0.1:7878")]
        listen: SocketAddr,
        /// PEM certificate chain; enables HTTPS together with --tls-key.
        #[arg(long, env = "GHF_TLS_CERT", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// PEM private key.
        #[arg(long, env = "GHF_TLS_KEY", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Default retention after finalization.
        #[arg(long, default_value = "30d")]
        after_finalize: String,
        /// Default cap for closed traces that are never finalized.
        #[arg(long, default_value = "90d")]
        unfinalized_cap: String,
        /// How often to prune all repositories; `0s` disables.
        #[arg(long, default_value = "1h")]
        prune_interval: String,
    },
    /// Manage API tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    /// Set a repository's retention policy.
    Policy {
        /// Repository, `owner/name`.
        repo: String,
        #[arg(long)]
        after_finalize: Option<String>,
        #[arg(long)]
        unfinalized_cap: Option<String>,
    },
    /// Prune eligible traces now.
    Prune {
        /// Only this repository (default: all).
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "30d")]
        after_finalize: String,
        #[arg(long, default_value = "90d")]
        unfinalized_cap: String,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Create a token and print it once.
    Create {
        /// Label for the token.
        #[arg(long)]
        name: String,
        /// Comma-separated repositories: `owner/name`, `owner/*` or `*`.
        #[arg(long)]
        repos: String,
        /// Allow reads only: pushes, holds, pending reports and prune are refused.
        #[arg(long)]
        read_only: bool,
    },
    /// List tokens (not their values).
    List,
    /// Revoke every token with this name.
    Revoke {
        #[arg(long)]
        name: String,
    },
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("ghf-server: {msg}");
    process::exit(1);
}

fn duration(s: &str, what: &str) -> i64 {
    parse_duration_ms(s).unwrap_or_else(|| die(format!("invalid {what} {s:?}; use e.g. 30d")))
}

fn policy(after: &str, cap: &str) -> Policy {
    Policy {
        after_finalize_ms: duration(after, "--after-finalize"),
        unfinalized_cap_ms: duration(cap, "--unfinalized-cap"),
        ..Policy::default()
    }
}

fn main() {
    let cli = Cli::parse();
    let Some(dir) = cli.data_dir else {
        die("--data-dir (or GHF_DATA_DIR) is required");
    };
    let store = Store::open(&dir).unwrap_or_else(|e| die(e.message));
    match cli.command {
        Command::Token(TokenCommand::Create {
            name,
            repos,
            read_only,
        }) => {
            for r in repos.split(',').map(str::trim) {
                let ok = r == "*"
                    || ghf_proto::api::valid_repo(r)
                    || r.strip_suffix("/*")
                        .is_some_and(|o| ghf_proto::api::valid_repo(&format!("{o}/x")));
                if !ok {
                    die(format!("invalid repository scope {r:?}"));
                }
            }
            let token = store
                .create_token(&name, &repos, read_only)
                .unwrap_or_else(|e| die(e.message));
            println!("{token}");
        }
        Command::Token(TokenCommand::List) => {
            for t in store.list_tokens().unwrap_or_else(|e| die(e.message)) {
                let at = std::time::UNIX_EPOCH + Duration::from_millis(t.created_at as u64);
                println!(
                    "{}\t{}\t{}\t{}{}",
                    t.name,
                    t.repos,
                    humantime::format_rfc3339_seconds(at),
                    if t.read_only {
                        "read-only"
                    } else {
                        "read-write"
                    },
                    if t.revoked { "\trevoked" } else { "" }
                );
            }
        }
        Command::Token(TokenCommand::Revoke { name }) => {
            let n = store.revoke_token(&name).unwrap_or_else(|e| die(e.message));
            println!("revoked {n} token(s)");
        }
        Command::Policy {
            repo,
            after_finalize,
            unfinalized_cap,
        } => {
            if !ghf_proto::api::valid_repo(&repo) {
                die("repository must look like owner/name");
            }
            let a = after_finalize.map(|s| duration(&s, "--after-finalize"));
            let c = unfinalized_cap.map(|s| duration(&s, "--unfinalized-cap"));
            store
                .set_policy(&repo, a, c)
                .unwrap_or_else(|e| die(e.message));
            let p = store
                .policy(&repo, &Policy::default())
                .unwrap_or_else(|e| die(e.message));
            println!("{}", serde_json::to_string(&p).expect("serialize"));
        }
        Command::Prune {
            repo,
            dry_run,
            after_finalize,
            unfinalized_cap,
        } => {
            let p = policy(&after_finalize, &unfinalized_cap);
            let repos = match repo {
                Some(r) => vec![r],
                None => store.repos().unwrap_or_else(|e| die(e.message)),
            };
            for r in repos {
                let rep =
                    prune::prune_repo(&store, &r, &p, dry_run).unwrap_or_else(|e| die(e.message));
                println!("{r}\t{}", serde_json::to_string(&rep).expect("serialize"));
            }
        }
        Command::Serve {
            listen,
            tls_cert,
            tls_key,
            after_finalize,
            unfinalized_cap,
            prune_interval,
        } => {
            let p = policy(&after_finalize, &unfinalized_cap);
            let interval = duration(&prune_interval, "--prune-interval");
            let server_id = store.server_id().unwrap_or_else(|e| die(e.message));
            let state = Arc::new(http::AppState {
                store: Mutex::new(store),
                server_id: server_id.clone(),
                policy: p,
                secure_cookies: tls_cert.is_some(),
            });
            let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| die(e));
            rt.block_on(serve(
                state,
                listen,
                tls_cert.zip(tls_key),
                interval,
                server_id,
            ));
        }
    }
}

async fn serve(
    state: http::Shared,
    listen: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
    prune_interval_ms: i64,
    server_id: String,
) {
    if prune_interval_ms > 0 {
        let s = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(prune_interval_ms as u64));
            tick.tick().await;
            loop {
                tick.tick().await;
                let s = s.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let store = s.store.lock().expect("store lock");
                    for repo in store.repos().unwrap_or_default() {
                        match prune::prune_repo(&store, &repo, &s.policy, false) {
                            Ok(r) if !r.pruned.is_empty() => eprintln!(
                                "ghf-server: pruned {} trace(s) in {repo}, freed {} bytes",
                                r.pruned.len(),
                                r.bytes_freed
                            ),
                            Ok(_) => {}
                            Err(e) => eprintln!("ghf-server: prune {repo} failed: {}", e.message),
                        }
                    }
                })
                .await;
            }
        });
    }
    let app = http::router(state);
    let scheme = if tls.is_some() { "https" } else { "http" };
    eprintln!(
        "ghf-server {} ({server_id}) listening on {scheme}://{listen}",
        env!("CARGO_PKG_VERSION")
    );
    match tls {
        Some((cert, key)) => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .unwrap_or_else(|e| die(format!("TLS: {e}")));
            let handle = axum_server::Handle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                shutdown().await;
                h.graceful_shutdown(Some(Duration::from_secs(10)));
            });
            axum_server::bind_rustls(listen, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .unwrap_or_else(|e| die(e));
        }
        None => {
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .unwrap_or_else(|e| die(format!("bind {listen}: {e}")));
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
                .await
                .unwrap_or_else(|e| die(e));
        }
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
