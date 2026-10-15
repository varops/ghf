//! Harness adapters: automatic capture after agent edits.
//!
//! `git hf hook <harness>` reads the harness's hook payload on stdin, takes a
//! checkpoint in the repository the edit happened in, and answers in the
//! harness's own format. Outside a ghf-enabled repository it does nothing.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::capture::{self, Request};
use crate::config::{self, CaptureMode};
use crate::error::{Context, Failure, Result};
use crate::git::Repo;
use crate::remote;
use crate::workspace::Workspace;
use crate::{err, fail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    #[value(name = "claude-code")]
    ClaudeCode,
    Cursor,
}

const CLAUDE_MATCHER: &str = "Write|Edit|MultiEdit|NotebookEdit|Bash";

fn command(h: Harness) -> &'static str {
    match h {
        Harness::ClaudeCode => "git hf hook claude-code",
        Harness::Cursor => "git hf hook cursor",
    }
}

struct Payload {
    dir: PathBuf,
    writer: String,
}

fn parse_payload(h: Harness, v: &Value) -> Payload {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match h {
        Harness::ClaudeCode => Payload {
            dir: s("cwd").map(PathBuf::from).unwrap_or(cwd),
            writer: format!(
                "claude-code:{}",
                s("session_id").unwrap_or_else(|| "unknown".into())
            ),
        },
        Harness::Cursor => {
            let from_file = s("file_path")
                .map(PathBuf::from)
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .filter(|p| p.is_dir());
            let root = v
                .get("workspace_roots")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(PathBuf::from);
            Payload {
                dir: from_file.or(root).unwrap_or(cwd),
                writer: format!(
                    "cursor:{}",
                    s("conversation_id").unwrap_or_else(|| "unknown".into())
                ),
            }
        }
    }
}

/// Run a hook. Returns the process exit code; writes harness-format output.
pub fn run(h: Harness) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin()
        .take(16 * 1024 * 1024)
        .read_to_string(&mut input);
    let payload = parse_payload(h, &serde_json::from_str(&input).unwrap_or(Value::Null));

    // Not a Git repo, or not ghf-enabled: stay out of the way.
    let Ok(repo) = Repo::discover(&payload.dir) else {
        return 0;
    };
    let cfg = match config::RepoConfig::load(&repo.root) {
        Ok(Some(c)) => c,
        Ok(None) => return 0,
        Err(f) => return report(h, &f, true),
    };
    let strict = cfg.capture_mode().ok() == Some(CaptureMode::Strict);
    let git_dir = repo.git_dir.clone();
    let ws = match Workspace::with_config(repo, cfg) {
        Ok(ws) => ws,
        Err(f) => {
            capture::note_failure(None, Some(&git_dir), &f, strict);
            return report(h, &f, strict);
        }
    };
    let req = Request {
        writer: std::env::var("GHF_WRITER").unwrap_or(payload.writer),
        trace_name: std::env::var("GHF_TRACE_NAME").ok(),
        ..Default::default()
    };
    match capture::checkpoint(&ws, &req) {
        Ok(o) => {
            if o.created {
                remote::spawn_background(&ws);
            }
            0
        }
        Err(f) => {
            capture::note_failure(Some(&ws), Some(&ws.repo.git_dir), &f, strict);
            report(h, &f, strict)
        }
    }
}

fn report(h: Harness, f: &Failure, strict: bool) -> i32 {
    let msg = format!("ghf: checkpoint failed: {} ({})", f.message, f.code);
    eprintln!("{msg}");
    match (h, strict) {
        // Claude Code reads JSON on exit 0. Strict mode stops the agent.
        (Harness::ClaudeCode, true) => {
            println!(
                "{}",
                json!({
                    "continue": false,
                    "stopReason": format!("{msg} [strict mode, E_CAPTURE_FAILED]"),
                    "systemMessage": msg,
                })
            );
            0
        }
        (Harness::ClaudeCode, false) => {
            println!(
                "{}",
                json!({"systemMessage": format!("{msg}; continuing (best-effort)")})
            );
            0
        }
        (Harness::Cursor, true) => ghf_proto::ErrorCode::CaptureFailed.exit_code() as i32,
        (Harness::Cursor, false) => 0,
    }
}

// ---------------------------------------------------------------------------
// install / uninstall

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    /// Shared project settings, committed with the repository.
    Project,
    /// Project settings for this checkout only (Claude Code: settings.local.json).
    Local,
    /// The user's settings in the home directory.
    User,
}

pub fn settings_path(h: Harness, scope: Scope, root: &Path) -> Result<PathBuf> {
    let home = || {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| err!(Config, "HOME is not set"))
    };
    Ok(match (h, scope) {
        (Harness::ClaudeCode, Scope::Project) => root.join(".claude/settings.json"),
        (Harness::ClaudeCode, Scope::Local) => root.join(".claude/settings.local.json"),
        (Harness::ClaudeCode, Scope::User) => home()?.join(".claude/settings.json"),
        (Harness::Cursor, Scope::Project | Scope::Local) => root.join(".cursor/hooks.json"),
        (Harness::Cursor, Scope::User) => home()?.join(".cursor/hooks.json"),
    })
}

fn read_json(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(json!({})),
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| err!(Config, "{} is not valid JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).context(|| format!("read {}", path.display())),
    }
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let mut text = serde_json::to_string_pretty(v)?;
    text.push('\n');
    fs::write(path, text).context(|| format!("write {}", path.display()))
}

fn is_ours(entry: &Value, cmd: &str) -> bool {
    entry.get("command").and_then(Value::as_str) == Some(cmd)
}

/// Install, remove or inspect hooks. Returns (path, changed, installed).
pub fn manage(
    h: Harness,
    scope: Scope,
    root: &Path,
    action: &str,
) -> Result<(PathBuf, bool, bool)> {
    let path = settings_path(h, scope, root)?;
    let mut v = read_json(&path)?;
    if !v.is_object() {
        fail!(Config, "{} must hold a JSON object", path.display());
    }
    let cmd = command(h);
    let events: &[&str] = match h {
        Harness::ClaudeCode => &["PostToolUse"],
        Harness::Cursor => &["afterFileEdit", "afterShellExecution"],
    };
    let any_before = events.iter().any(|ev| has_hook(h, &v, ev, cmd));
    let mut changed = false;
    match action {
        "status" => {}
        "install" => {
            if h == Harness::Cursor && v.get("version").is_none() {
                v["version"] = json!(1);
            }
            for ev in events {
                if has_hook(h, &v, ev, cmd) {
                    continue;
                }
                let list = v
                    .as_object_mut()
                    .expect("object")
                    .entry("hooks")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(|| {
                        err!(Config, "\"hooks\" in {} is not an object", path.display())
                    })?
                    .entry(*ev)
                    .or_insert_with(|| json!([]));
                let arr = list.as_array_mut().ok_or_else(|| {
                    err!(Config, "hooks.{ev} in {} is not a list", path.display())
                })?;
                arr.push(match h {
                    Harness::ClaudeCode => json!({
                        "matcher": CLAUDE_MATCHER,
                        "hooks": [{"type": "command", "command": cmd, "timeout": 60}],
                    }),
                    Harness::Cursor => json!({"command": cmd, "timeout": 60}),
                });
                changed = true;
            }
        }
        "uninstall" => {
            for ev in events {
                let Some(arr) = v
                    .get_mut("hooks")
                    .and_then(|hk| hk.get_mut(*ev))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                match h {
                    Harness::ClaudeCode => {
                        for group in arr.iter_mut() {
                            if let Some(inner) =
                                group.get_mut("hooks").and_then(Value::as_array_mut)
                            {
                                inner.retain(|e| !is_ours(e, cmd));
                            }
                        }
                        arr.retain(|g| {
                            g.get("hooks")
                                .and_then(Value::as_array)
                                .is_none_or(|a| !a.is_empty())
                        });
                    }
                    Harness::Cursor => arr.retain(|e| !is_ours(e, cmd)),
                }
            }
            changed = any_before;
        }
        _ => unreachable!(),
    }
    if changed {
        write_json(&path, &v)?;
    }
    let installed = events.iter().all(|ev| has_hook(h, &v, ev, cmd));
    Ok((path, changed, installed))
}

fn has_hook(h: Harness, v: &Value, ev: &str, cmd: &str) -> bool {
    let Some(arr) = v
        .get("hooks")
        .and_then(|hk| hk.get(ev))
        .and_then(Value::as_array)
    else {
        return false;
    };
    match h {
        Harness::ClaudeCode => arr.iter().any(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|e| is_ours(e, cmd)))
        }),
        Harness::Cursor => arr.iter().any(|e| is_ours(e, cmd)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_reversible() {
        let dir = std::env::temp_dir().join(format!("ghf-hooks-{}", ghf_proto::id::random_hex(4)));
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .unwrap();
        for h in [Harness::ClaudeCode, Harness::Cursor] {
            let (_, changed, installed) = manage(h, Scope::Project, &dir, "install").unwrap();
            assert!(changed && installed);
            let (_, changed, _) = manage(h, Scope::Project, &dir, "install").unwrap();
            assert!(!changed);
            let (_, changed, installed) = manage(h, Scope::Project, &dir, "uninstall").unwrap();
            assert!(changed && !installed);
        }
        let v = read_json(&dir.join(".claude/settings.json")).unwrap();
        assert_eq!(v["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
