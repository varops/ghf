//! Phase timing for diagnosing capture latency. Enabled by `GHF_TIMING=1`;
//! prints to stderr.

use std::cell::Cell;
use std::time::Instant;

pub struct Timer {
    on: bool,
    last: Cell<Instant>,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            on: std::env::var_os("GHF_TIMING").is_some(),
            last: Cell::new(Instant::now()),
        }
    }

    pub fn mark(&self, what: &str) {
        if self.on {
            let now = Instant::now();
            eprintln!(
                "ghf timing: {what}: {:.1} ms",
                (now - self.last.get()).as_secs_f64() * 1000.0
            );
            self.last.set(now);
        }
    }
}
