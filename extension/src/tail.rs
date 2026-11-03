//! `ghf tail`: print recent checkpoints across every trace in the repository,
//! and with `-f`, keep printing them as they land.
//!
//! It reads the local store, which all worktrees of a repository share, so
//! one tail shows every agent working in this repository on this machine,
//! plus anything `ghf sync` pulls in. It only reads: no lease, no capture,
//! no sync.

use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Write};
use std::time::Duration;

use ghf_proto::{ChangeStatus, CheckpointRecord, Response, TraceRecord, TraceState};
use serde_json::json;

use crate::error::Result;
use crate::store::Store;

const POLL: Duration = Duration::from_millis(250);
const AUTHOR_WIDTH: usize = 22;
const TRACE_WIDTH: usize = 18;

pub struct Options {
    pub follow: bool,
    /// How many existing checkpoints to print first.
    pub lines: usize,
    /// Trace id (or prefix) or trace name.
    pub trace: Option<String>,
    /// Author prefix, e.g. `claude-code` matches `claude-code:<session>`.
    pub author: Option<String>,
    pub annotations: BTreeMap<String, String>,
    pub stat: bool,
    pub json: bool,
}

impl Options {
    fn wants_trace(&self, t: Option<&TraceRecord>, trace_id: &str) -> bool {
        match &self.trace {
            None => true,
            Some(f) => {
                trace_id.starts_with(f.as_str())
                    || t.and_then(|t| t.name.as_deref()) == Some(f.as_str())
            }
        }
    }

    fn wants(&self, c: &CheckpointRecord, t: Option<&TraceRecord>) -> bool {
        self.wants_trace(t, &c.trace_id)
            && self
                .author
                .as_deref()
                .is_none_or(|a| c.author.starts_with(a))
            && self
                .annotations
                .iter()
                .all(|(k, v)| c.annotations.get(k) == Some(v))
    }

    /// Trace lifecycle lines are about traces, not checkpoints, so they are
    /// left out when filtering by author or annotation.
    fn wants_trace_events(&self) -> bool {
        self.author.is_none() && self.annotations.is_empty()
    }
}

/// Print, and follow if asked. Returns when done, on a store error, or when
/// stdout goes away (e.g. piped into `head`).
pub fn run(store: &Store, opts: &Options) -> Result<()> {
    let paint = Paint::new(opts.json);
    let mut out = std::io::stdout().lock();
    let mut traces = trace_map(store)?;

    let all = store.checkpoints_after(0)?;
    let mut last = all.last().map(|(row, _)| *row).unwrap_or(0);
    let shown: Vec<&CheckpointRecord> = all
        .iter()
        .map(|(_, c)| c)
        .filter(|c| opts.wants(c, traces.get(&c.trace_id)))
        .collect();
    let skip = shown.len().saturating_sub(opts.lines);
    for c in &shown[skip..] {
        if emit_checkpoint(&mut out, &paint, opts, c, traces.get(&c.trace_id)).is_err() {
            return Ok(());
        }
    }
    if !opts.follow {
        return Ok(());
    }

    let started = ghf_proto::id::now_ms();
    loop {
        std::thread::sleep(POLL);
        // Checkpoints first: a checkpoint and its new trace commit together,
        // so every checkpoint read here has its trace in the map read next.
        let new = store.checkpoints_after(last)?;
        let now = trace_map(store)?;
        // Everything that happened since the last poll, put in time order.
        // On equal times a trace opens before its checkpoints and closes
        // after them.
        let mut events: Vec<(i64, u8, Event)> = Vec::new();
        for (id, t) in &now {
            if !opts.wants_trace_events() || !opts.wants_trace(Some(t), id) {
                continue;
            }
            match traces.get(id) {
                // Opened since we started, possibly closed within one poll.
                // Older traces that sync pulls in are history, not news.
                None if t.created_at >= started => {
                    events.push((t.created_at, 0, Event::Trace("opened", t)));
                    if t.state != TraceState::Open {
                        events.push((state_at(t), 2, Event::Trace(t.state.as_str(), t)));
                    }
                }
                Some(old) if old.state != t.state => {
                    events.push((state_at(t), 2, Event::Trace(t.state.as_str(), t)));
                }
                _ => {}
            }
        }
        if let Some((row, _)) = new.last() {
            last = *row;
        }
        for (_, c) in &new {
            if opts.wants(c, now.get(&c.trace_id)) {
                events.push((c.created_at, 1, Event::Checkpoint(c)));
            }
        }
        events.sort_by_key(|(at, rank, _)| (*at, *rank));
        let mut ok = true;
        for (_, _, e) in &events {
            ok &= match e {
                Event::Trace(what, t) => emit_trace(&mut out, &paint, opts, what, t),
                Event::Checkpoint(c) => {
                    emit_checkpoint(&mut out, &paint, opts, c, now.get(&c.trace_id))
                }
            }
            .is_ok();
        }
        if !ok {
            return Ok(());
        }
        traces = now;
    }
}

enum Event<'a> {
    Trace(&'a str, &'a TraceRecord),
    Checkpoint(&'a CheckpointRecord),
}

/// When a trace reached its current state.
fn state_at(t: &TraceRecord) -> i64 {
    match t.state {
        TraceState::Open => None,
        TraceState::Closed => t.closed_at,
        TraceState::Finalized => t.finalized_at,
        TraceState::Pruned => t.pruned_at,
    }
    .unwrap_or(t.created_at)
}

fn trace_map(store: &Store) -> Result<HashMap<String, TraceRecord>> {
    Ok(store
        .traces()?
        .into_iter()
        .map(|t| (t.record.id.clone(), t.record))
        .collect())
}

fn emit_checkpoint(
    out: &mut impl Write,
    p: &Paint,
    opts: &Options,
    c: &CheckpointRecord,
    t: Option<&TraceRecord>,
) -> std::io::Result<()> {
    if opts.json {
        let trace = json!({"id": c.trace_id, "name": t.and_then(|t| t.name.clone())});
        let resp = Response::ok(
            "tail",
            json!({"event": "checkpoint", "checkpoint": c, "trace": trace}),
        );
        return writeln!(out, "{}", serde_json::to_string(&resp).expect("serializes"));
    }

    let mut line = format!(
        "{}  {}  {}  {}  {}",
        p.dim(&clock(c.created_at)),
        p.color(author_color(&c.author), &fit(&c.author, AUTHOR_WIDTH)),
        fit(&trace_label(t, &c.trace_id), TRACE_WIDTH),
        p.dim(&format!("#{:<4}", c.seq)),
        counts(p, c),
    );
    for (k, v) in &c.annotations {
        let badge = format!("{k}={v}");
        line.push_str("  ");
        line.push_str(&match (k.as_str(), v.as_str()) {
            ("tests", "pass") => p.color("1;32", &badge),
            ("tests", "fail") => p.color("1;31", &badge),
            _ => p.dim(&badge),
        });
    }
    if let Some(m) = c
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        line.push_str("  ");
        line.push_str(m.lines().next().unwrap_or_default());
    }
    writeln!(out, "{line}")?;

    if opts.stat {
        for ch in &c.changes {
            let (tag, code) = match ch.status {
                ChangeStatus::Added => ("A", "32"),
                ChangeStatus::Modified => ("M", "33"),
                ChangeStatus::Deleted => ("D", "31"),
                ChangeStatus::Renamed => ("R", "36"),
                ChangeStatus::Mode => ("T", "35"),
            };
            let path = match &ch.from {
                Some(from) => format!("{from} → {}", ch.path),
                None => ch.path.clone(),
            };
            writeln!(out, "          {} {path}", p.color(code, tag))?;
        }
    }
    Ok(())
}

fn emit_trace(
    out: &mut impl Write,
    p: &Paint,
    opts: &Options,
    event: &str,
    t: &TraceRecord,
) -> std::io::Result<()> {
    if opts.json {
        let resp = Response::ok(
            "tail",
            json!({"event": "trace", "change": event, "trace": t}),
        );
        return writeln!(out, "{}", serde_json::to_string(&resp).expect("serializes"));
    }
    let at = if event == "opened" {
        t.created_at
    } else {
        state_at(t)
    };
    let what = match (event, t.final_commit.as_deref()) {
        ("finalized", _) if t.abandoned => p.dim("abandoned"),
        ("finalized", Some(sha)) => {
            format!(
                "{} → {}",
                p.color("32", "finalized"),
                &sha[..sha.len().min(10)]
            )
        }
        _ => p.dim(event),
    };
    writeln!(
        out,
        "{}  {} {} {what}",
        p.dim(&clock(at)),
        p.dim("──"),
        trace_label(Some(t), &t.id)
    )
}

/// `+added ~modified -deleted`, zero parts left out. Renames and mode
/// changes count as modifications.
fn counts(p: &Paint, c: &CheckpointRecord) -> String {
    let (mut add, mut modi, mut del) = (0, 0, 0);
    for ch in &c.changes {
        match ch.status {
            ChangeStatus::Added => add += 1,
            ChangeStatus::Deleted => del += 1,
            _ => modi += 1,
        }
    }
    let mut parts = Vec::new();
    let mut width = 0;
    for (n, sign, code) in [(add, '+', "32"), (modi, '~', "33"), (del, '-', "31")] {
        if n > 0 {
            let s = format!("{sign}{n}");
            width += s.len() + 1;
            parts.push(p.color(code, &s));
        }
    }
    if parts.is_empty() {
        width = 2;
        parts.push(p.dim("·"));
    }
    // Pad by visible width, since the colored text carries escape codes.
    format!(
        "{}{}",
        parts.join(" "),
        " ".repeat(14usize.saturating_sub(width))
    )
}

/// The trace name, or `tr_…` plus the end of the id: ids start with a
/// timestamp, so traces opened together share their first characters.
fn trace_label(t: Option<&TraceRecord>, id: &str) -> String {
    t.and_then(|t| t.name.clone()).unwrap_or_else(|| {
        let tail: Vec<char> = id.chars().rev().take(6).collect();
        format!("tr_…{}", tail.into_iter().rev().collect::<String>())
    })
}

/// `HH:MM:SS` in UTC.
fn clock(ms: i64) -> String {
    let t = std::time::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    let s = humantime::format_rfc3339_seconds(t).to_string();
    s.get(11..19).unwrap_or(&s).to_owned()
}

/// Pad or cut to exactly `w` visible characters.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s}{}", " ".repeat(w - n))
    } else {
        let mut t: String = s.chars().take(w - 1).collect();
        t.push('…');
        t
    }
}

/// A stable color per author, avoiding red and green (used for test results).
fn author_color(author: &str) -> &'static str {
    const PALETTE: [&str; 8] = ["36", "33", "35", "34", "96", "95", "93", "94"];
    let h = author.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    PALETTE[(h % PALETTE.len() as u64) as usize]
}

/// ANSI colors, on only for a terminal and when `NO_COLOR` is unset.
struct Paint(bool);

impl Paint {
    fn new(json: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self(!json && !no_color && std::io::stdout().is_terminal())
    }

    fn color(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }

    fn dim(&self, s: &str) -> String {
        self.color("2", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_pads_and_cuts_by_characters() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("äbcd", 4), "äbcd");
    }

    #[test]
    fn author_colors_are_stable_and_never_red_or_green() {
        assert_eq!(author_color("claude-code:1"), author_color("claude-code:1"));
        for a in ["a", "b", "claude-code:x", "cursor:y", "watch:ran"] {
            assert!(!["31", "32"].contains(&author_color(a)));
        }
    }

    #[test]
    fn clock_is_utc_hh_mm_ss() {
        assert_eq!(clock(0), "00:00:00");
        assert_eq!(clock(3_723_000), "01:02:03");
    }
}
