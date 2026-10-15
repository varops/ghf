//! Test harness: temporary Git repos, simulated machines, a real server.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-hf"))
}

fn server_bin() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-q", "-p", "ghf-server"])
                .status()
                .expect("build ghf-server");
            assert!(status.success(), "building ghf-server failed");
            bin().parent().unwrap().join("ghf-server")
        })
        .clone()
}

pub fn tempdir(tag: &str) -> PathBuf {
    let base = std::env::var("GHF_TEST_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let mut rand = [0u8; 6];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut rand))
        .unwrap();
    let hex: String = rand.iter().map(|b| format!("{b:02x}")).collect();
    let dir = base.join(format!("ghf-test-{tag}-{hex}"));
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A simulated machine: its own user config dir (trust, machine id).
#[derive(Clone)]
pub struct Machine {
    pub config: PathBuf,
    pub env: Vec<(String, String)>,
}

pub struct Run {
    pub code: i32,
    pub json: Value,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> &Value {
        assert_eq!(
            self.code, 0,
            "expected success, got {}: {}\n{}",
            self.code, self.stdout, self.stderr
        );
        &self.json["data"]
    }

    pub fn err_code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or_else(|| {
            panic!(
                "expected an error envelope: {}\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

impl Machine {
    pub fn new(root: &Path, name: &str) -> Self {
        Self {
            config: root.join(format!("config-{name}")),
            env: vec![("GHF_SYNC".into(), "manual".into())],
        }
    }

    pub fn with(mut self, k: &str, v: &str) -> Self {
        self.env.retain(|(ek, _)| ek != k);
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn cmd(&self, dir: &Path) -> Command {
        let mut c = Command::new(bin());
        c.current_dir(dir)
            .env_remove("GHF_TOKEN")
            .env_remove("GHF_WRITER")
            .env_remove("GHF_STRICT")
            .env("GHF_CONFIG_DIR", &self.config)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    /// Run `git hf <args> --json`.
    pub fn hf(&self, dir: &Path, args: &[&str]) -> Run {
        self.hf_env(dir, args, &[])
    }

    pub fn hf_env(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut c = self.cmd(dir);
        c.args(args).arg("--json").stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        Run {
            code: out.status.code().unwrap_or(-1),
            json: serde_json::from_str(stdout.trim()).unwrap_or(Value::Null),
            stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run `git hf hook <harness>` with a payload on stdin.
    pub fn hook(&self, dir: &Path, harness: &str, payload: &Value) -> (i32, String) {
        let mut child = self
            .cmd(dir)
            .args(["hook", harness])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

/// A fresh repository with one commit, pushed to a bare origin.
pub fn repo(root: &Path) -> (PathBuf, PathBuf) {
    let origin = root.join("origin.git");
    git(
        root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    let work = root.join("work");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::write(work.join("README.md"), "hello\n").unwrap();
    fs::write(work.join(".gitignore"), "target/\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["push", "-q", "origin", "main"]);
    (origin, work)
}

pub fn clone(origin: &Path, to: &Path) -> PathBuf {
    git(
        to.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            to.to_str().unwrap(),
        ],
    );
    to.to_path_buf()
}

pub struct Server {
    child: Child,
    pub url: String,
    pub data: PathBuf,
    pub token: String,
    args: Vec<String>,
    port: u16,
}

impl Server {
    pub fn start(root: &Path, extra: &[&str]) -> Self {
        let data = root.join("server-data");
        let out = Command::new(server_bin())
            .args(["--data-dir", data.to_str().unwrap()])
            .args(["token", "create", "--name", "test", "--repos", "acme/*"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let child = Self::spawn(&data, port, &args);
        let s = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            data,
            token,
            args,
            port,
        };
        s.wait_ready();
        s
    }

    fn spawn(data: &Path, port: u16, args: &[String]) -> Child {
        Command::new(server_bin())
            .args(["--data-dir", data.to_str().unwrap(), "serve", "--listen"])
            .arg(format!("127.0.0.1:{port}"))
            .args(args)
            .env("GHF_GC_GRACE", "0s")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_ready(&self) {
        let start = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn restart(&mut self) {
        self.stop();
        self.child = Self::spawn(&self.data, self.port, &self.args);
        self.wait_ready();
    }

    pub fn admin(&self, args: &[&str]) -> String {
        let out = Command::new(server_bin())
            .args(["--data-dir", self.data.to_str().unwrap()])
            .args(args)
            .env("GHF_GC_GRACE", "0s")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Machine set up to talk to `server`: trusted and authenticated.
pub fn connected(root: &Path, name: &str, server: &Server) -> Machine {
    Machine::new(root, name).with("GHF_TOKEN", &server.token)
}

/// Snapshot of everything Git considers history: refs and object files.
pub fn git_state(dir: &Path) -> (String, Vec<String>) {
    let refs = git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let objects_dir = PathBuf::from(git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .join("objects");
    let mut objects = Vec::new();
    fn walk(p: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(p).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                walk(&e.path(), out);
            } else {
                out.push(e.path().display().to_string());
            }
        }
    }
    walk(&objects_dir, &mut objects);
    objects.sort();
    (refs, objects)
}
