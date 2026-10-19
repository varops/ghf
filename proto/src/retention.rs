//! Prune eligibility. Pure functions shared by client and server.

use serde::{Deserialize, Serialize};

use crate::{TraceRecord, TraceState};

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Finalized traces become eligible this long after finalization.
    pub after_finalize_ms: i64,
    /// Closed, never-finalized traces become eligible this long after closing.
    pub unfinalized_cap_ms: i64,
    /// Warn this long before the unfinalized cap.
    pub warn_before_ms: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            after_finalize_ms: 30 * DAY_MS,
            unfinalized_cap_ms: 90 * DAY_MS,
            warn_before_ms: 7 * DAY_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eligibility {
    pub eligible: bool,
    /// When the trace becomes (or became) eligible, if it ever will by clock alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible_at: Option<i64>,
    /// Closed but unfinalized and inside the warning window.
    pub warn: bool,
    pub reason: String,
}

/// Clock-based eligibility. Callers must also check dependency rules
/// (unsynced checkpoints, fork bases) before deleting anything.
pub fn eligibility(trace: &TraceRecord, now: i64, policy: &Policy) -> Eligibility {
    let held = trace.holds.iter().any(|h| h.active(now));
    let (at, warn, why) = match trace.state {
        TraceState::Open => (None, false, "open"),
        TraceState::Pruned => (None, false, "already pruned"),
        TraceState::Finalized => (
            trace
                .finalized_at
                .map(|t| t.saturating_add(policy.after_finalize_ms)),
            false,
            "finalized",
        ),
        TraceState::Closed => {
            let at = trace
                .closed_at
                .map(|t| t.saturating_add(policy.unfinalized_cap_ms));
            let warn = at.is_some_and(|a| now >= a.saturating_sub(policy.warn_before_ms));
            (at, warn, "closed, not finalized")
        }
    };
    let due = at.is_some_and(|a| now >= a);
    let reason = if held {
        "held".to_owned()
    } else if due {
        format!("{why}; retention elapsed")
    } else {
        why.to_owned()
    };
    Eligibility {
        eligible: due && !held,
        eligible_at: at,
        warn: warn && !held,
        reason,
    }
}

/// Parse durations like `30d`, `12h`, `15m`, `45s`, `500ms`.
pub fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => DAY_MS,
        "w" => 7 * DAY_MS,
        _ => return None,
    };
    n.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hold;

    fn trace(state: TraceState) -> TraceRecord {
        TraceRecord {
            id: "tr_x".into(),
            name: None,
            state,
            created_at: 0,
            context_id: "wc_x".into(),
            forked_from: None,
            closed_at: Some(0),
            finalized_at: Some(0),
            final_commit: None,
            abandoned: false,
            pruned_at: None,
            holds: vec![],
        }
    }

    #[test]
    fn finalized_clock() {
        let p = Policy::default();
        let t = trace(TraceState::Finalized);
        assert!(!eligibility(&t, 29 * DAY_MS, &p).eligible);
        assert!(eligibility(&t, 30 * DAY_MS, &p).eligible);
    }

    #[test]
    fn unfinalized_cap_warns_then_expires() {
        let p = Policy::default();
        let t = trace(TraceState::Closed);
        let e = eligibility(&t, 84 * DAY_MS, &p);
        assert!(e.warn && !e.eligible);
        assert!(eligibility(&t, 90 * DAY_MS, &p).eligible);
    }

    #[test]
    fn holds_block() {
        let p = Policy::default();
        let mut t = trace(TraceState::Finalized);
        t.holds.push(Hold {
            name: "pin".into(),
            expires_at: None,
            created_at: 0,
        });
        assert!(!eligibility(&t, 100 * DAY_MS, &p).eligible);
        t.holds[0].expires_at = Some(50 * DAY_MS);
        assert!(eligibility(&t, 100 * DAY_MS, &p).eligible);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("30d"), Some(30 * DAY_MS));
        assert_eq!(parse_duration_ms("15m"), Some(900_000));
        assert_eq!(parse_duration_ms("x"), None);
    }
}
