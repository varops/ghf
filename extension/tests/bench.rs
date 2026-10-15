//! Benchmark harness (PRD §9 "measurable benchmark harness", §11 targets).
//!
//! Run with:
//!   cargo test --release -p git-hf --test bench -- --ignored --nocapture
//!
//! Tunables: GHF_BENCH_FILES (default 2000), GHF_BENCH_EDITS (default 200),
//! GHF_BENCH_FILE_SIZE bytes (default 4096).

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1000.0
}

fn dir_bytes(p: &Path) -> u64 {
    let mut total = 0;
    for e in fs::read_dir(p).into_iter().flatten().flatten() {
        let md = e.metadata().unwrap();
        total += if md.is_dir() {
            dir_bytes(&e.path())
        } else {
            md.len()
        };
    }
    total
}

/// Deterministic pseudo-random generator so runs are comparable.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
#[ignore]
fn bench_capture_sync_restore() {
    let files = env_usize("GHF_BENCH_FILES", 2000);
    let edits = env_usize("GHF_BENCH_EDITS", 200);
    let size = env_usize("GHF_BENCH_FILE_SIZE", 4096);
    let root = tempdir("bench");
    let server = Server::start(&root, &["--prune-interval", "0s"]);
    let (_, work) = repo(&root);
    let mut rng = Lcg(42);
    let line = "let value = compute(input, options).expect(\"ok\");\n";
    for i in 0..files {
        let dir = work.join(format!("src/m{:02}/s{:02}", i % 40, i % 7));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i}.rs")), line.repeat(size / line.len())).unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "fixture"]);
    let m = connected(&root, "a", &server);
    m.hf(
        &work,
        &[
            "init",
            "--server",
            &server.url,
            "--repository",
            "acme/bench",
            "--trust",
        ],
    )
    .ok();

    let t = Instant::now();
    m.hf(&work, &["checkpoint"]).ok();
    let first = t.elapsed();

    let mut lat = Vec::with_capacity(edits);
    for e in 0..edits {
        for _ in 0..(1 + rng.next() % 3) {
            let i = rng.next() as usize % files;
            let p = work.join(format!("src/m{:02}/s{:02}/f{i}.rs", i % 40, i % 7));
            let mut c = fs::read_to_string(&p).unwrap();
            c.push_str(&format!("// edit {e}\n"));
            fs::write(&p, c).unwrap();
        }
        let t = Instant::now();
        let r = m.hf(&work, &["checkpoint"]);
        lat.push(t.elapsed());
        r.ok();
    }
    lat.sort();
    let local = dir_bytes(&work.join(".git/ghf"));

    let t = Instant::now();
    let s = m.hf(&work, &["sync"]).ok().clone();
    let sync = t.elapsed();
    let server_bytes = dir_bytes(&server.data);

    // Cross-machine restore of the latest checkpoint.
    let b = connected(&root, "b", &server);
    let wb = clone(&root.join("origin.git"), &root.join("b"));
    fs::copy(work.join(".ghf"), wb.join(".ghf")).unwrap();
    b.hf(&wb, &["trust"]).ok();
    let latest = m.hf(&work, &["trace", "log", "-n", "1"]).ok()["checkpoints"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let t = Instant::now();
    b.hf(&wb, &["trace", "restore", &latest]).ok();
    let restore = t.elapsed();

    let repo_bytes = files * size;
    let summary = serde_json::json!({
        "files": files,
        "edits": edits,
        "repo_bytes": repo_bytes,
        "first_checkpoint_ms": first.as_secs_f64() * 1000.0,
        "checkpoint_ms": {"p50": pct(&lat, 0.5), "p95": pct(&lat, 0.95), "max": pct(&lat, 1.0)},
        "local_store_bytes_incl_metadata": local,
        "server_store_bytes_incl_metadata": server_bytes,
        "server_bytes_per_checkpoint": server_bytes as f64 / (edits + 1) as f64,
        "sync": {"ms": sync.as_secs_f64() * 1000.0, "objects": s["pushed_objects"], "bytes": s["pushed_bytes"]},
        "cross_machine_restore_ms": restore.as_secs_f64() * 1000.0,
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
