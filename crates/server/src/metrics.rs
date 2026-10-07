//! Task-level metrics for agent runs.
//!
//! `timing` records one request at a time. This module groups those requests
//! into *runs* — one agent task — and summarises each run: how many tool/HTTP
//! calls it took, how many were batches, observed actions or flow replays,
//! how many came back stale or `outcome_unknown`, what failed, and how long
//! calls took (p50/p95).
//!
//! The contract, agreed with the chrome-use side:
//!
//! - **Calls, not model turns.** The daemon only sees HTTP/tool calls. A run
//!   reports `model_round_trips` only when the caller declared a complete
//!   trace at run start — before any call — and delivered every model turn id
//!   at run end (including final turns that made no tool call), all valid,
//!   distinct and within [`MAX_TURNS`]. Anything else is `null`, and a trace
//!   that was declared but not delivered cleanly sets `trace_incomplete`.
//!   Per-call turn ids are not collected: a partial count is never reported.
//! - **Runs need a caller identity.** A run is keyed by a validated owner plus
//!   either an explicit run id ([`RunKey::Explicit`]) or an idle-gap split
//!   ([`RunKey::Inferred`]). Without a valid owner a call is `unattributed`
//!   and never grouped — an explicit run id alone is not an identity.
//! - **Attribution at start, exactly once.** [`RunAggregator::begin`] assigns
//!   a call to its run and hands out a single-use [`CallToken`];
//!   [`RunAggregator::finish`] consumes it. A duplicate or unknown token is
//!   rejected and counted. Every run has a never-reused `incarnation`, so a
//!   token from a closed run can never land in a newer one.
//! - **Honest totals.** Any drop, rejection or call still in flight at close
//!   makes the run `incomplete`; per-call figures cover `stored_calls`, which
//!   is reported. Wall time (`ended_ms`) tracks every finished call.
//! - **Concurrency.** Runner intervals are clipped to their call's span and
//!   capped per call and per run. `runner_busy_union_ms` counts overlaps once,
//!   `runner_summed_ms` is the plain sum; both are `null` when intervals were
//!   dropped or a sum overflowed.
//! - **Bounded.** Open runs, events, intervals and closed summaries are
//!   capped; idle runs close, and any run is force-closed after
//!   [`MAX_RUN_LIFETIME_MS`] even with calls in flight. Every drop is counted
//!   in [`AggregatorStats`].
//! - **Persistence.** Summaries hold only enums, numbers and validated ids.
//!   [`SummaryLog`] writes whole lines under an exclusive file lock to a
//!   regular, single-link, owner-only file in a private directory, refusing
//!   symlinks; the rotated generation is kept owner-only too.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Calls from one owner further apart than this start a new inferred run.
pub const IDLE_GAP_MS: u64 = 120_000;
/// An explicit run with no activity for this long is closed as expired.
pub const EXPLICIT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
/// Any run is force-closed this long after it started, even with calls in
/// flight (a hung call must not keep a run open forever).
pub const MAX_RUN_LIFETIME_MS: u64 = 12 * 60 * 60 * 1000;
/// At most this many runs are open at once; the stalest is evicted.
pub const MAX_OPEN_RUNS: usize = 256;
/// Calls stored per run; later calls are counted but not stored.
pub const MAX_EVENTS_PER_RUN: usize = 5_000;
/// Calls in flight per run; more are rejected.
pub const MAX_IN_FLIGHT_PER_RUN: usize = 64;
/// Runner intervals accepted per call.
pub const MAX_INTERVALS_PER_CALL: usize = 8;
/// Runner intervals stored per run.
pub const MAX_INTERVALS_PER_RUN: usize = 20_000;
/// Closed summaries kept in memory until [`RunAggregator::drain_closed`].
pub const MAX_CLOSED: usize = 1_000;
/// Owners, run ids and turn ids longer than this are rejected.
pub const MAX_ID_LEN: usize = 64;
/// Turn ids accepted per run trace.
pub const MAX_TURNS: usize = 10_000;

/// What kind of call this was, for the usage counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// A single action or read.
    Single,
    /// `/agent/actions` with several steps.
    Batch,
    /// A flow replay.
    Flow,
}

/// Failure classes that may be persisted. Unknown error codes map to
/// [`FailureClass::Other`] — free-form text is never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ElementNotFound,
    ElementNotVisible,
    ElementOccluded,
    AmbiguousElementLabel,
    ValueNotApplied,
    ExpectationTimeout,
    PhoneOwned,
    NotDrivable,
    Transport,
    Other,
}

impl FailureClass {
    /// Map a daemon error code to a class; anything unrecognised is `Other`.
    pub fn from_code(code: &str) -> Self {
        match code {
            "element_not_found" => Self::ElementNotFound,
            "element_not_visible" => Self::ElementNotVisible,
            "element_occluded" => Self::ElementOccluded,
            "ambiguous_element_label" => Self::AmbiguousElementLabel,
            "value_not_applied" => Self::ValueNotApplied,
            "expectation_timeout" => Self::ExpectationTimeout,
            "phone_owned" => Self::PhoneOwned,
            "not_drivable" | "wda_unavailable_or_unsupported" => Self::NotDrivable,
            "transport_error" => Self::Transport,
            _ => Self::Other,
        }
    }
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "class")]
pub enum Outcome {
    Ok,
    /// Refused because the caller's snapshot was out of date.
    Stale,
    /// The action may or may not have happened.
    OutcomeUnknown,
    Failed(FailureClass),
}

/// The start of a call, as the caller identified itself.
#[derive(Debug, Clone)]
pub struct CallStart {
    /// Wall-clock start, ms since the Unix epoch.
    pub start_ms: u64,
    /// The caller's `X-Phone-Owner`, unvalidated.
    pub owner: Option<String>,
    /// The caller's explicit run id, unvalidated.
    pub run_id: Option<String>,
}

/// The end of a call.
#[derive(Debug, Clone)]
pub struct CallEnd {
    pub end_ms: u64,
    pub kind: CallKind,
    /// The call asked for the settled change back (`observe` / `return=delta`).
    pub observed: bool,
    pub outcome: Outcome,
    /// Intervals (start_ms, end_ms) during which the runner served this call.
    pub runner_intervals: Vec<(u64, u64)>,
}

/// Single-use receipt from [`RunAggregator::begin`]. Deliberately not
/// `Clone`; a forged or repeated id is still detected by `finish`.
#[derive(Debug, PartialEq, Eq)]
pub struct CallToken {
    key: Option<RunKey>,
    id: u64,
    start_ms: u64,
}

/// Identity of a run: always a validated owner, and an `incarnation` that is
/// never reused within the aggregator's life.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunKey {
    Explicit {
        owner: String,
        run_id: String,
        incarnation: u64,
    },
    Inferred {
        owner: String,
        incarnation: u64,
    },
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Closed {
    /// The caller ended it (run_end).
    Ended,
    /// The owner went idle past the gap (inferred runs).
    Idle,
    /// An explicit run saw no activity for [`EXPLICIT_TTL_MS`].
    Expired,
    /// Open longer than [`MAX_RUN_LIFETIME_MS`].
    LifetimeExceeded,
    /// Evicted to stay under [`MAX_OPEN_RUNS`].
    Evicted,
}

/// Summary of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub key: RunKey,
    pub inferred: bool,
    pub closed: Closed,
    /// Something is missing: calls in flight at close, dropped or rejected
    /// calls, or dropped intervals.
    pub incomplete: bool,
    pub in_flight_at_close: u64,
    pub started_ms: u64,
    /// Latest end of any finished call, stored or not.
    pub ended_ms: u64,
    /// Every finished call.
    pub tool_calls: u64,
    /// Calls whose details were kept; per-call figures below cover these.
    pub stored_calls: u64,
    pub dropped_events: u64,
    pub rejected_events: u64,
    pub batch_calls: u64,
    pub flow_calls: u64,
    pub observed_calls: u64,
    pub stale: u64,
    pub outcome_unknown: u64,
    pub failures: BTreeMap<FailureClass, u64>,
    /// Over `stored_calls`.
    pub call_p50_ms: Option<u64>,
    pub call_p95_ms: Option<u64>,
    /// `None` when intervals were dropped or the sum overflowed.
    pub runner_busy_union_ms: Option<u64>,
    pub runner_summed_ms: Option<u64>,
    pub clipped_intervals: u64,
    pub rejected_intervals: u64,
    pub dropped_intervals: u64,
    /// Only for a declared and cleanly delivered complete trace.
    pub model_round_trips: Option<u64>,
    /// A complete trace was declared but the delivery was missing, invalid,
    /// duplicated, too large, or declared after calls began.
    pub trace_incomplete: bool,
    pub arithmetic_overflow: bool,
}

/// Losses, force-closes and rejections so far: surfaced, never silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatorStats {
    /// Calls without a valid owner: not attributable to any task.
    pub unattributed_events: u64,
    /// Owners, run ids or turn ids that failed validation.
    pub invalid_identity: u64,
    /// Calls rejected: end before start, too many in flight.
    pub rejected_events: u64,
    /// Finishes with a token that was already used or never issued.
    pub duplicate_finishes: u64,
    /// Calls counted but not stored because a run hit its event cap.
    pub dropped_events: u64,
    /// Runner intervals dropped by the per-call or per-run caps.
    pub dropped_intervals: u64,
    /// Calls that finished after their run had closed.
    pub late_events: u64,
    pub evicted_runs: u64,
    pub expired_runs: u64,
    pub lifetime_closed_runs: u64,
    /// Closed summaries discarded because nobody drained them in time.
    pub dropped_summaries: u64,
    /// Calls that turned out unauthenticated: discarded, never counted.
    pub unauthenticated_events: u64,
}

#[derive(Debug)]
struct Stored {
    duration_ms: u64,
    kind: CallKind,
    observed: bool,
    outcome: Outcome,
}

#[derive(Debug, Default)]
struct OpenRun {
    calls: Vec<Stored>,
    intervals: Vec<(u64, u64)>,
    total_calls: u64,
    dropped_events: u64,
    rejected_events: u64,
    clipped: u64,
    rejected_intervals: u64,
    dropped_intervals: u64,
    /// Token ids issued and not yet finished.
    outstanding: BTreeSet<u64>,
    started_ms: u64,
    ended_ms: u64,
    last_activity_ms: u64,
    /// `Some(true)` declared before any call; `Some(false)` declared late.
    trace_declared: Option<bool>,
}

/// Groups calls into runs.
#[derive(Debug, Default)]
pub struct RunAggregator {
    open: BTreeMap<RunKey, OpenRun>,
    /// Live explicit run per (owner, run id).
    explicit_live: BTreeMap<(String, String), RunKey>,
    /// Current inferred run per owner.
    inferred_current: BTreeMap<String, RunKey>,
    next_incarnation: u64,
    next_token: u64,
    closed: VecDeque<RunSummary>,
    stats: AggregatorStats,
}

/// Accept an id from a caller only if it is short and plain.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'))
}

impl RunAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// A caller-supplied id, validated: `Err` when present but invalid.
    fn id(&mut self, id: Option<String>) -> Result<Option<String>, ()> {
        match id {
            None => Ok(None),
            Some(id) if valid_id(&id) => Ok(Some(id)),
            Some(_) => {
                self.stats.invalid_identity += 1;
                Err(())
            }
        }
    }

    fn incarnation(&mut self) -> u64 {
        self.next_incarnation += 1;
        self.next_incarnation
    }

    fn open_run(&mut self, key: RunKey, now_ms: u64) -> RunKey {
        if self.open.len() >= MAX_OPEN_RUNS {
            let stalest = self
                .open
                .iter()
                .min_by_key(|(_, run)| run.last_activity_ms)
                .map(|(key, _)| key.clone());
            if let Some(stalest) = stalest {
                self.stats.evicted_runs += 1;
                self.close_with(&stalest, Closed::Evicted, None);
            }
        }
        self.open.insert(
            key.clone(),
            OpenRun {
                started_ms: now_ms,
                ended_ms: now_ms,
                last_activity_ms: now_ms,
                ..OpenRun::default()
            },
        );
        key
    }

    fn explicit_key(&mut self, owner: String, run_id: String, now_ms: u64) -> RunKey {
        if let Some(key) = self.explicit_live.get(&(owner.clone(), run_id.clone())) {
            return key.clone();
        }
        let key = RunKey::Explicit {
            owner: owner.clone(),
            run_id: run_id.clone(),
            incarnation: self.incarnation(),
        };
        self.explicit_live.insert((owner, run_id), key.clone());
        self.open_run(key, now_ms)
    }

    fn inferred_key(&mut self, owner: String, start_ms: u64) -> RunKey {
        if let Some(key) = self.inferred_current.get(&owner).cloned() {
            let (fresh, busy) = self.open.get(&key).map_or((false, false), |run| {
                (
                    start_ms.saturating_sub(run.last_activity_ms) <= IDLE_GAP_MS,
                    !run.outstanding.is_empty(),
                )
            });
            if fresh {
                return key;
            }
            // Past the gap: a new run starts. A run with calls in flight
            // stays open so they land where they started.
            if !busy {
                self.close_with(&key, Closed::Idle, None);
            }
        }
        let key = RunKey::Inferred {
            owner: owner.clone(),
            incarnation: self.incarnation(),
        };
        self.inferred_current.insert(owner, key.clone());
        self.open_run(key, start_ms)
    }

    /// Open an explicit run (MCP run_start). `complete_trace` declares that
    /// every model turn id will be delivered at run end. A declaration made
    /// after the run's first call does not count.
    pub fn start_run(
        &mut self,
        owner: Option<String>,
        run_id: String,
        now_ms: u64,
        complete_trace: bool,
    ) {
        let (Ok(Some(owner)), Ok(Some(run_id))) = (self.id(owner), self.id(Some(run_id))) else {
            self.stats.unattributed_events += 1;
            return;
        };
        let key = self.explicit_key(owner, run_id, now_ms);
        if let Some(run) = self.open.get_mut(&key) {
            if complete_trace {
                let early = run.total_calls == 0 && run.outstanding.is_empty();
                run.trace_declared = Some(early);
            }
        }
    }

    /// Register a call when it starts; the run it belongs to is decided now.
    pub fn begin(&mut self, start: CallStart) -> CallToken {
        self.next_token += 1;
        let id = self.next_token;
        let owner = self.id(start.owner);
        let run_id = self.id(start.run_id);
        let key = match (owner, run_id) {
            (Ok(Some(owner)), Ok(Some(run_id))) => {
                Some(self.explicit_key(owner, run_id, start.start_ms))
            }
            (Ok(Some(owner)), Ok(None)) => Some(self.inferred_key(owner, start.start_ms)),
            _ => {
                self.stats.unattributed_events += 1;
                None
            }
        };
        let key = key.filter(|key| match self.open.get_mut(key) {
            Some(run) if run.outstanding.len() < MAX_IN_FLIGHT_PER_RUN => {
                run.outstanding.insert(id);
                run.started_ms = run.started_ms.min(start.start_ms);
                run.last_activity_ms = run.last_activity_ms.max(start.start_ms);
                true
            }
            Some(run) => {
                run.rejected_events += 1;
                self.stats.rejected_events += 1;
                false
            }
            None => false,
        });
        CallToken {
            key,
            id,
            start_ms: start.start_ms,
        }
    }

    /// Finish a call started with [`begin`](Self::begin). Consumes the token.
    pub fn finish(&mut self, token: CallToken, end: CallEnd) {
        let Some(key) = token.key else {
            return; // counted at begin
        };
        let Some(run) = self.open.get_mut(&key) else {
            self.stats.late_events += 1;
            return;
        };
        if !run.outstanding.remove(&token.id) {
            self.stats.duplicate_finishes += 1;
            return;
        }
        if end.end_ms < token.start_ms {
            run.rejected_events += 1;
            self.stats.rejected_events += 1;
            return;
        }
        run.total_calls = run.total_calls.saturating_add(1);
        run.ended_ms = run.ended_ms.max(end.end_ms);
        run.last_activity_ms = run.last_activity_ms.max(end.end_ms);
        let accepted = end.runner_intervals.len().min(MAX_INTERVALS_PER_CALL);
        let over = (end.runner_intervals.len() - accepted) as u64;
        run.dropped_intervals += over;
        self.stats.dropped_intervals += over;
        for &(s, e) in &end.runner_intervals[..accepted] {
            let (cs, ce) = (s.max(token.start_ms), e.min(end.end_ms));
            if ce <= cs {
                run.rejected_intervals += 1;
                continue;
            }
            if (cs, ce) != (s, e) {
                run.clipped += 1;
            }
            if run.intervals.len() < MAX_INTERVALS_PER_RUN {
                run.intervals.push((cs, ce));
            } else {
                run.dropped_intervals += 1;
                self.stats.dropped_intervals += 1;
            }
        }
        if run.calls.len() < MAX_EVENTS_PER_RUN {
            run.calls.push(Stored {
                duration_ms: end.end_ms - token.start_ms,
                kind: end.kind,
                observed: end.observed,
                outcome: end.outcome,
            });
        } else {
            run.dropped_events += 1;
            self.stats.dropped_events += 1;
        }
    }

    /// Close an explicit run (MCP run_end). With a declared complete trace,
    /// `turn_ids` must list every model turn of the run.
    pub fn end_run(
        &mut self,
        owner: Option<String>,
        run_id: String,
        turn_ids: Option<Vec<String>>,
    ) -> Option<RunSummary> {
        let (Ok(Some(owner)), Ok(Some(run_id))) = (self.id(owner), self.id(Some(run_id))) else {
            return None;
        };
        let key = self.explicit_live.get(&(owner, run_id))?.clone();
        let trace = turn_ids.map(|ids| {
            // An empty list is a legitimate run with no model turns.
            if ids.len() > MAX_TURNS || !ids.iter().all(|id| valid_id(id)) {
                return None;
            }
            let distinct: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
            (distinct.len() == ids.len()).then_some(ids.len() as u64)
        });
        self.close_with(&key, Closed::Ended, trace)
    }

    /// Close idle inferred runs, expired explicit runs, and any run past its
    /// maximum lifetime (even with calls in flight).
    pub fn sweep(&mut self, now_ms: u64) -> Vec<RunSummary> {
        let due: Vec<(RunKey, Closed)> = self
            .open
            .iter()
            .filter_map(|(key, run)| {
                if now_ms.saturating_sub(run.started_ms) > MAX_RUN_LIFETIME_MS {
                    return Some((key.clone(), Closed::LifetimeExceeded));
                }
                if !run.outstanding.is_empty() {
                    return None;
                }
                let idle = now_ms.saturating_sub(run.last_activity_ms);
                match key {
                    RunKey::Inferred { .. } if idle > IDLE_GAP_MS => {
                        Some((key.clone(), Closed::Idle))
                    }
                    RunKey::Explicit { .. } if idle > EXPLICIT_TTL_MS => {
                        Some((key.clone(), Closed::Expired))
                    }
                    _ => None,
                }
            })
            .collect();
        due.iter()
            .filter_map(|(key, how)| self.close_with(key, *how, None))
            .collect()
    }

    /// Take every closed summary (e.g. to append them to the JSONL log).
    pub fn drain_closed(&mut self) -> Vec<RunSummary> {
        self.closed.drain(..).collect()
    }

    /// A live summary of a run that is still open.
    pub fn peek(&self, key: &RunKey) -> Option<RunSummary> {
        self.open
            .get(key)
            .map(|run| summarize(key, run, Closed::Ended, None))
    }

    /// Withdraw a call that should never have counted (it was refused as
    /// unauthenticated). A run that only this call had opened disappears.
    pub fn discard(&mut self, token: CallToken) {
        self.stats.unauthenticated_events += 1;
        let Some(key) = token.key else {
            // It was counted as unattributed at `begin`; take that back.
            self.stats.unattributed_events = self.stats.unattributed_events.saturating_sub(1);
            return;
        };
        let Some(run) = self.open.get_mut(&key) else {
            return;
        };
        run.outstanding.remove(&token.id);
        let empty =
            run.total_calls == 0 && run.outstanding.is_empty() && run.trace_declared.is_none();
        if empty {
            self.open.remove(&key);
            match &key {
                RunKey::Inferred { owner, .. } => {
                    if self.inferred_current.get(owner) == Some(&key) {
                        self.inferred_current.remove(owner);
                    }
                }
                RunKey::Explicit { owner, run_id, .. } => {
                    self.explicit_live.remove(&(owner.clone(), run_id.clone()));
                }
            }
        }
    }

    /// Live summaries of every open run.
    pub fn open_summaries(&self) -> Vec<RunSummary> {
        self.open
            .iter()
            .map(|(key, run)| summarize(key, run, Closed::Ended, None))
            .collect()
    }

    /// Losses, force-closes and rejections so far.
    pub fn stats(&self) -> AggregatorStats {
        self.stats
    }

    fn close_with(
        &mut self,
        key: &RunKey,
        how: Closed,
        trace: Option<Option<u64>>,
    ) -> Option<RunSummary> {
        let run = self.open.remove(key)?;
        match key {
            RunKey::Inferred { owner, .. } => {
                if self.inferred_current.get(owner) == Some(key) {
                    self.inferred_current.remove(owner);
                }
            }
            RunKey::Explicit { owner, run_id, .. } => {
                self.explicit_live.remove(&(owner.clone(), run_id.clone()));
            }
        }
        match how {
            Closed::Expired => self.stats.expired_runs += 1,
            Closed::LifetimeExceeded => self.stats.lifetime_closed_runs += 1,
            _ => {}
        }
        let summary = summarize(key, &run, how, trace);
        if self.closed.len() >= MAX_CLOSED {
            self.closed.pop_front();
            self.stats.dropped_summaries += 1;
        }
        self.closed.push_back(summary.clone());
        Some(summary)
    }
}

/// `trace`: `None` = nothing delivered; `Some(None)` = delivered but invalid;
/// `Some(Some(n))` = n valid distinct turn ids.
fn summarize(
    key: &RunKey,
    run: &OpenRun,
    closed: Closed,
    trace: Option<Option<u64>>,
) -> RunSummary {
    let calls = &run.calls;
    let mut failures = BTreeMap::new();
    let (mut stale, mut unknown) = (0u64, 0u64);
    for call in calls {
        match call.outcome {
            Outcome::Ok => {}
            Outcome::Stale => stale += 1,
            Outcome::OutcomeUnknown => unknown += 1,
            Outcome::Failed(class) => *failures.entry(class).or_insert(0u64) += 1,
        }
    }
    let mut durations: Vec<u64> = calls.iter().map(|c| c.duration_ms).collect();
    durations.sort_unstable();
    let summed = run
        .intervals
        .iter()
        .try_fold(0u64, |acc, (s, e)| acc.checked_add(e - s));
    let union = interval_union_ms(&run.intervals);
    let overflow = summed.is_none() || union.is_none();
    let intervals_whole = run.dropped_intervals == 0;
    let in_flight = run.outstanding.len() as u64;
    let incomplete = in_flight > 0
        || run.dropped_events > 0
        || run.rejected_events > 0
        || run.dropped_intervals > 0;
    let (model_round_trips, trace_incomplete) = match run.trace_declared {
        Some(true) => match trace {
            Some(Some(turns)) if !incomplete => (Some(turns), false),
            _ => (None, true),
        },
        Some(false) => (None, true),
        None => (None, false),
    };
    RunSummary {
        key: key.clone(),
        inferred: matches!(key, RunKey::Inferred { .. }),
        closed,
        incomplete,
        in_flight_at_close: in_flight,
        started_ms: run.started_ms,
        ended_ms: run.ended_ms,
        tool_calls: run.total_calls,
        stored_calls: calls.len() as u64,
        dropped_events: run.dropped_events,
        rejected_events: run.rejected_events,
        batch_calls: calls.iter().filter(|c| c.kind == CallKind::Batch).count() as u64,
        flow_calls: calls.iter().filter(|c| c.kind == CallKind::Flow).count() as u64,
        observed_calls: calls.iter().filter(|c| c.observed).count() as u64,
        stale,
        outcome_unknown: unknown,
        failures,
        call_p50_ms: percentile(&durations, 50),
        call_p95_ms: percentile(&durations, 95),
        runner_busy_union_ms: union.filter(|_| intervals_whole),
        runner_summed_ms: summed.filter(|_| intervals_whole),
        clipped_intervals: run.clipped,
        rejected_intervals: run.rejected_intervals,
        dropped_intervals: run.dropped_intervals,
        model_round_trips,
        trace_incomplete,
        arithmetic_overflow: overflow,
    }
}

// ---------------------------------------------------------------------------
// The daemon's aggregator: one per process, fed by the `timing` layer.
// ---------------------------------------------------------------------------

/// Closed summaries kept for `GET /agent/metrics` after they are logged.
pub const RECENT_RUNS: usize = 50;
/// Rotate `agent-runs.jsonl` past this size.
pub const RUNS_LOG_ROTATE_BYTES: u64 = 10 << 20;

struct Daemon {
    aggregator: RunAggregator,
    recent: VecDeque<RunSummary>,
}

static DAEMON: std::sync::OnceLock<std::sync::Mutex<Daemon>> = std::sync::OnceLock::new();
static RUNS_LOG: std::sync::OnceLock<SummaryLog> = std::sync::OnceLock::new();

fn daemon() -> std::sync::MutexGuard<'static, Daemon> {
    let lock = DAEMON.get_or_init(|| {
        std::sync::Mutex::new(Daemon {
            aggregator: RunAggregator::new(),
            recent: VecDeque::new(),
        })
    });
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where closed run summaries are appended (next to `agent-timing.jsonl`).
pub fn set_runs_log(path: PathBuf) {
    let _ = RUNS_LOG.set(SummaryLog::new(path, RUNS_LOG_ROTATE_BYTES));
}

/// Wall-clock now in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Register a daemon request at its start.
pub fn begin_call(start: CallStart) -> CallToken {
    daemon().aggregator.begin(start)
}

/// Finish a daemon request.
pub fn finish_call(token: CallToken, end: CallEnd) {
    daemon().aggregator.finish(token, end);
    flush(now_ms());
}

/// Withdraw an unauthenticated request.
pub fn discard_call(token: CallToken) {
    daemon().aggregator.discard(token);
}

/// `POST /agent/run` start.
pub fn start_run(owner: Option<String>, run_id: String, complete_trace: bool) {
    daemon()
        .aggregator
        .start_run(owner, run_id, now_ms(), complete_trace);
}

/// `POST /agent/run` end.
pub fn end_run(
    owner: Option<String>,
    run_id: String,
    turns: Option<Vec<String>>,
) -> Option<RunSummary> {
    let summary = daemon().aggregator.end_run(owner, run_id, turns);
    flush(now_ms());
    summary
}

/// Close what is due, keep recent summaries, and append them to the log
/// off the request path.
pub fn flush(now_ms: u64) {
    let closed = {
        let mut daemon = daemon();
        daemon.aggregator.sweep(now_ms);
        let closed = daemon.aggregator.drain_closed();
        for summary in &closed {
            if daemon.recent.len() >= RECENT_RUNS {
                daemon.recent.pop_front();
            }
            daemon.recent.push_back(summary.clone());
        }
        closed
    };
    if closed.is_empty() {
        return;
    }
    if let Some(log) = RUNS_LOG.get() {
        let write = move || {
            for summary in &closed {
                if let Err(error) = log.append(summary) {
                    tracing::warn!(%error, "could not append run summary");
                    break;
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(write);
            }
            Err(_) => write(),
        }
    }
}

/// `GET /agent/metrics`: open runs (optionally one owner's), recent closed
/// runs, and the loss counters.
pub fn report(owner: Option<&str>) -> serde_json::Value {
    flush(now_ms());
    let daemon = daemon();
    let mine = |summary: &&RunSummary| {
        owner.is_none_or(|owner| match &summary.key {
            RunKey::Explicit { owner: o, .. } | RunKey::Inferred { owner: o, .. } => o == owner,
        })
    };
    let open: Vec<RunSummary> = daemon.aggregator.open_summaries();
    serde_json::json!({
        "ok": true,
        "definitions": "tool_calls are HTTP calls to this daemon, not model turns; \
            model_round_trips is null unless the caller declared and delivered a complete trace",
        "open": open.iter().filter(mine).collect::<Vec<_>>(),
        "recent": daemon.recent.iter().filter(mine).collect::<Vec<_>>(),
        "stats": daemon.aggregator.stats(),
    })
}

/// Nearest-rank percentile of sorted values; `None` for an empty slice.
pub fn percentile(sorted: &[u64], pct: u32) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((pct as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// Total length covered by possibly overlapping `(start, end)` intervals;
/// `None` if the total overflows.
pub fn interval_union_ms(intervals: &[(u64, u64)]) -> Option<u64> {
    let mut sorted: Vec<(u64, u64)> = intervals.iter().copied().filter(|(s, e)| e > s).collect();
    sorted.sort_unstable();
    let mut total: u64 = 0;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in sorted {
        match current {
            Some((cs, ce)) if start <= ce => current = Some((cs, ce.max(end))),
            Some((cs, ce)) => {
                total = total.checked_add(ce - cs)?;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((cs, ce)) = current {
        total = total.checked_add(ce - cs)?;
    }
    Some(total)
}

/// Writer for `agent-runs.jsonl`. Every append takes an exclusive `flock`
/// on the file, so several writers (processes or instances) never interleave
/// lines; the file must be a regular, single-link file owned by us in a
/// directory only we can write, and is kept at mode 0600.
pub struct SummaryLog {
    path: PathBuf,
    rotate_bytes: u64,
}

impl SummaryLog {
    pub fn new(path: PathBuf, rotate_bytes: u64) -> Self {
        Self { path, rotate_bytes }
    }

    pub fn append(&self, summary: &RunSummary) -> std::io::Result<()> {
        let mut line = serde_json::to_string(summary).map_err(std::io::Error::other)?;
        line.push('\n');
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("log path has no directory"))?;
        check_private_dir(parent)?;
        // Lock a sidecar that never rotates, for the whole open / check /
        // rotate / append sequence. Locking the log itself would let a writer
        // that opened the old inode before a rotation lock it afterwards and
        // write into (or rotate away) the wrong generation.
        let mut lock_path = self.path.clone().into_os_string();
        lock_path.push(".lock");
        let lock = open_private(Path::new(&lock_path))?;
        lock_exclusive(&lock)?;
        let mut file = open_private(&self.path)?;
        if file.metadata()?.len() >= self.rotate_bytes {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            let rotated = PathBuf::from(rotated);
            drop(file);
            std::fs::rename(&self.path, &rotated)?;
            // The rotated generation stays owner-only, whatever it was.
            drop(open_private(&rotated)?);
            file = open_private(&self.path)?;
        }
        file.write_all(line.as_bytes())
        // `lock` drops here, releasing the sidecar lock after the append.
    }
}

fn check_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(std::io::Error::other(
            "log directory must be ours and not group/other-writable",
        ));
    }
    Ok(())
}

/// Open for append without following a symlink, then insist on a regular,
/// single-link file we own, and force mode 0600 on it.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 {
        return Err(std::io::Error::other(
            "log file must be a regular single-link file we own",
        ));
    }
    if meta.mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: a valid open fd; the lock is released when the file closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(at: u64, owner: Option<&str>, run: Option<&str>) -> CallStart {
        CallStart {
            start_ms: at,
            owner: owner.map(str::to_string),
            run_id: run.map(str::to_string),
        }
    }

    fn end(at: u64) -> CallEnd {
        CallEnd {
            end_ms: at,
            kind: CallKind::Single,
            observed: false,
            outcome: Outcome::Ok,
            runner_intervals: Vec::new(),
        }
    }

    /// One complete call with the runner busy for its whole span.
    fn one(agg: &mut RunAggregator, at: u64, dur: u64, owner: Option<&str>, run: Option<&str>) {
        let token = agg.begin(start(at, owner, run));
        let mut e = end(at + dur);
        e.runner_intervals = vec![(at, at + dur)];
        agg.finish(token, e);
    }

    fn end_run(agg: &mut RunAggregator, owner: &str, run: &str) -> RunSummary {
        agg.end_run(Some(owner.into()), run.into(), None).unwrap()
    }

    fn ids(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn union_counts_overlaps_once() {
        let intervals = [(0, 100), (50, 150), (200, 250), (240, 260), (300, 300)];
        assert_eq!(interval_union_ms(&intervals), Some(150 + 60));
        assert_eq!(interval_union_ms(&[]), Some(0));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let values: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&values, 50), Some(10));
        assert_eq!(percentile(&values, 95), Some(19));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn idle_gap_splits_an_owner_into_inferred_runs() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 500, Some("a"), None);
        one(&mut agg, 1_000, 500, Some("a"), None);
        one(&mut agg, 1_500 + IDLE_GAP_MS + 1, 500, Some("a"), None);
        let closed = agg.drain_closed();
        assert_eq!(closed.len(), 1);
        assert!(closed[0].inferred && closed[0].closed == Closed::Idle);
        assert_eq!(closed[0].tool_calls, 2);
        let rest = agg.sweep(10 * IDLE_GAP_MS);
        assert_eq!(rest[0].tool_calls, 1);
        assert_ne!(closed[0].key, rest[0].key);
    }

    #[test]
    fn a_late_call_lands_in_the_run_it_started_in() {
        let mut agg = RunAggregator::new();
        let first = agg.begin(start(0, Some("a"), None));
        let late = agg.begin(start(1_000, Some("a"), None));
        agg.finish(first, end(100));
        let newer = agg.begin(start(200_000, Some("a"), None));
        agg.finish(newer, end(200_100));
        agg.finish(late, end(1_100));
        let mut runs = agg.sweep(10 * IDLE_GAP_MS + 200_100);
        runs.sort_by_key(|r| r.started_ms);
        let counts: Vec<u64> = runs.iter().map(|r| r.tool_calls).collect();
        assert_eq!(counts, vec![2, 1]);
        assert_eq!((runs[0].started_ms, runs[0].ended_ms), (0, 1_100));
    }

    #[test]
    fn tokens_are_single_use_and_never_reach_a_newer_run() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("a"), Some("r")));
        // A forged copy of the same token (the type is not Clone).
        let forged = CallToken {
            key: token.key.clone(),
            id: token.id,
            start_ms: token.start_ms,
        };
        agg.finish(token, end(10));
        agg.finish(forged, end(20));
        assert_eq!(agg.stats().duplicate_finishes, 1);
        let first = end_run(&mut agg, "a", "r");
        assert_eq!(first.tool_calls, 1);

        // A token from a closed run stays late even if the same (owner, run
        // id) opens again: incarnations are never reused.
        let stale = agg.begin(start(100, Some("a"), Some("r")));
        end_run(&mut agg, "a", "r");
        let next = agg.begin(start(200, Some("a"), Some("r")));
        agg.finish(stale, end(150));
        agg.finish(next, end(250));
        assert_eq!(agg.stats().late_events, 1);
        let reopened = end_run(&mut agg, "a", "r");
        assert_eq!(reopened.tool_calls, 1);
        assert_ne!(reopened.key, first.key);
    }

    #[test]
    fn closing_with_calls_in_flight_marks_incomplete_and_counts_late() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("a"), Some("r")));
        let s = end_run(&mut agg, "a", "r");
        assert!(s.incomplete);
        assert_eq!(s.in_flight_at_close, 1);
        agg.finish(token, end(50));
        assert_eq!(agg.stats().late_events, 1);
    }

    #[test]
    fn no_valid_owner_means_no_grouping() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, None, None);
        one(&mut agg, 10, 100, None, Some("r1")); // a run id alone is no identity
        one(&mut agg, 20, 100, None, Some("r1"));
        one(&mut agg, 30, 100, Some(""), Some("r1")); // invalid owner
        one(&mut agg, 40, 100, Some(&"x".repeat(MAX_ID_LEN + 1)), None);
        one(&mut agg, 50, 100, Some("a b"), None);
        one(&mut agg, 60, 100, Some("a"), Some("bad id!"));
        assert_eq!(agg.stats().unattributed_events, 7);
        assert_eq!(agg.stats().invalid_identity, 4);
        assert!(agg.sweep(EXPLICIT_TTL_MS * 3).is_empty());
        assert!(agg.end_run(None, "r1".into(), None).is_none());
    }

    #[test]
    fn owners_are_isolated_and_closing_checks_the_owner() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, Some("a"), Some("r1"));
        one(&mut agg, 10, 100, Some("b"), Some("r1"));
        assert!(agg.end_run(Some("c".into()), "r1".into(), None).is_none());
        assert_eq!(end_run(&mut agg, "a", "r1").tool_calls, 1);
        assert_eq!(end_run(&mut agg, "b", "r1").tool_calls, 1);
        one(&mut agg, 20, 1, Some("a"), Some("inferred:a:1"));
        assert!(!end_run(&mut agg, "a", "inferred:a:1").inferred);
    }

    #[test]
    fn runner_intervals_are_clipped_and_capped() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(100, Some("a"), Some("r")));
        let mut e = end(200);
        e.runner_intervals = vec![(0, 10_000), (120, 150), (300, 400), (180, 170)];
        agg.finish(token, e);
        let s = end_run(&mut agg, "a", "r");
        assert_eq!(s.runner_busy_union_ms, Some(100));
        assert_eq!(s.runner_summed_ms, Some(130));
        assert_eq!((s.clipped_intervals, s.rejected_intervals), (1, 2));

        let token = agg.begin(start(0, Some("a"), Some("q")));
        let mut e = end(1_000);
        e.runner_intervals = (0..MAX_INTERVALS_PER_CALL as u64 + 3)
            .map(|i| (i, i + 1))
            .collect();
        agg.finish(token, e);
        let s = end_run(&mut agg, "a", "q");
        assert_eq!(s.dropped_intervals, 3);
        assert!(s.incomplete);
        assert_eq!(s.runner_busy_union_ms, None);
        assert_eq!(agg.stats().dropped_intervals, 3);
    }

    #[test]
    fn overflow_is_flagged_never_wrapped() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("a"), Some("r")));
        let mut e = end(u64::MAX);
        e.runner_intervals = vec![(0, u64::MAX), (0, u64::MAX)];
        agg.finish(token, e);
        let s = end_run(&mut agg, "a", "r");
        assert!(s.arithmetic_overflow);
        assert_eq!(s.runner_summed_ms, None);
        let token = agg.begin(start(500, Some("a"), Some("q")));
        agg.finish(token, end(100));
        let s = end_run(&mut agg, "a", "q");
        assert!(s.incomplete && s.rejected_events == 1);
    }

    #[test]
    fn counters_and_failure_classes() {
        let mut agg = RunAggregator::new();
        for (i, outcome) in [
            Outcome::Stale,
            Outcome::OutcomeUnknown,
            Outcome::Failed(FailureClass::from_code("element_not_found")),
            Outcome::Failed(FailureClass::from_code("some free text the daemon said")),
        ]
        .into_iter()
        .enumerate()
        {
            let token = agg.begin(start(i as u64 * 100, Some("a"), Some("r")));
            let mut e = end(i as u64 * 100 + 50);
            e.outcome = outcome;
            e.kind = if i == 0 {
                CallKind::Batch
            } else {
                CallKind::Flow
            };
            e.observed = i == 0;
            agg.finish(token, e);
        }
        let s = end_run(&mut agg, "a", "r");
        assert_eq!((s.batch_calls, s.flow_calls, s.observed_calls), (1, 3, 1));
        assert_eq!((s.stale, s.outcome_unknown), (1, 1));
        assert_eq!(s.failures.get(&FailureClass::Other), Some(&1));
        assert!(!serde_json::to_string(&s).unwrap().contains("free text"));
    }

    #[test]
    fn model_round_trips_need_a_clean_complete_trace() {
        let mut agg = RunAggregator::new();
        // Declared before any call, delivered cleanly: the only Some case.
        agg.start_run(Some("a".into()), "ok".into(), 0, true);
        one(&mut agg, 0, 10, Some("a"), Some("ok"));
        let s = agg
            .end_run(Some("a".into()), "ok".into(), ids(&["t1", "t2", "t3"]))
            .unwrap();
        assert_eq!((s.model_round_trips, s.trace_incomplete), (Some(3), false));

        // Declared but delivered with a duplicate, an invalid id, nothing, or
        // an empty list: null + trace_incomplete.
        for bad in [ids(&["t1", "t1"]), ids(&["t1", "bad id"]), None] {
            agg.start_run(Some("a".into()), "b".into(), 0, true);
            one(&mut agg, 0, 10, Some("a"), Some("b"));
            let s = agg.end_run(Some("a".into()), "b".into(), bad).unwrap();
            assert_eq!((s.model_round_trips, s.trace_incomplete), (None, true));
        }
        let too_many: Vec<String> = (0..=MAX_TURNS).map(|i| format!("t{i}")).collect();
        agg.start_run(Some("a".into()), "m".into(), 0, true);
        let s = agg
            .end_run(Some("a".into()), "m".into(), Some(too_many))
            .unwrap();
        assert!(s.trace_incomplete && s.model_round_trips.is_none());

        // A complete trace with no turns is a run that used no model: zero.
        agg.start_run(Some("a".into()), "z".into(), 0, true);
        one(&mut agg, 0, 10, Some("a"), Some("z"));
        let s = agg.end_run(Some("a".into()), "z".into(), ids(&[])).unwrap();
        assert_eq!((s.model_round_trips, s.trace_incomplete), (Some(0), false));

        // Declared after calls began: never a total.
        one(&mut agg, 0, 10, Some("a"), Some("late"));
        agg.start_run(Some("a".into()), "late".into(), 20, true);
        let s = agg
            .end_run(Some("a".into()), "late".into(), ids(&["t1"]))
            .unwrap();
        assert_eq!((s.model_round_trips, s.trace_incomplete), (None, true));

        // An incomplete run cannot claim a total either.
        agg.start_run(Some("a".into()), "f".into(), 0, true);
        let _in_flight = agg.begin(start(0, Some("a"), Some("f")));
        let s = agg
            .end_run(Some("a".into()), "f".into(), ids(&["t1"]))
            .unwrap();
        assert_eq!((s.model_round_trips, s.trace_incomplete), (None, true));

        // No declaration: null, and not "trace_incomplete".
        one(&mut agg, 0, 10, Some("a"), Some("n"));
        let s = agg
            .end_run(Some("a".into()), "n".into(), ids(&["t1"]))
            .unwrap();
        assert_eq!((s.model_round_trips, s.trace_incomplete), (None, false));
    }

    #[test]
    fn event_and_in_flight_caps_make_runs_incomplete() {
        let mut agg = RunAggregator::new();
        for i in 0..(MAX_EVENTS_PER_RUN as u64 + 7) {
            one(&mut agg, i, 1, Some("a"), Some("r"));
        }
        let s = end_run(&mut agg, "a", "r");
        assert_eq!(s.tool_calls, MAX_EVENTS_PER_RUN as u64 + 7);
        assert_eq!(s.stored_calls, MAX_EVENTS_PER_RUN as u64);
        assert_eq!(s.dropped_events, 7);
        assert!(s.incomplete);
        assert_eq!(
            s.ended_ms,
            MAX_EVENTS_PER_RUN as u64 + 7,
            "wall time tracks every call"
        );

        let mut agg = RunAggregator::new();
        let tokens: Vec<CallToken> = (0..MAX_IN_FLIGHT_PER_RUN as u64 + 2)
            .map(|i| agg.begin(start(i, Some("a"), Some("r"))))
            .collect();
        assert_eq!(agg.stats().rejected_events, 2);
        for (i, token) in tokens.into_iter().enumerate() {
            agg.finish(token, end(i as u64 + 10));
        }
        let s = end_run(&mut agg, "a", "r");
        assert_eq!(s.tool_calls, MAX_IN_FLIGHT_PER_RUN as u64);
        assert!(s.incomplete);
    }

    #[test]
    fn unauthenticated_calls_leave_no_trace() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("forger"), None));
        agg.discard(token);
        let token = agg.begin(start(0, None, None));
        agg.discard(token);
        assert!(agg.open_summaries().is_empty());
        assert_eq!(agg.stats().unauthenticated_events, 2);
        assert_eq!(agg.stats().unattributed_events, 0);
        // A run that already had real calls keeps them.
        one(&mut agg, 0, 10, Some("a"), Some("r"));
        let token = agg.begin(start(20, Some("a"), Some("r")));
        agg.discard(token);
        assert_eq!(end_run(&mut agg, "a", "r").tool_calls, 1);
    }

    #[test]
    fn eviction_ttl_and_lifetime() {
        let mut agg = RunAggregator::new();
        for i in 0..(MAX_OPEN_RUNS as u64 + 2) {
            one(&mut agg, i * 10, 1, Some("a"), Some(&format!("r{i}")));
        }
        assert_eq!(agg.stats().evicted_runs, 2);
        assert!(agg
            .drain_closed()
            .iter()
            .all(|s| s.closed == Closed::Evicted));

        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 1, Some("a"), Some("r"));
        assert!(agg.sweep(EXPLICIT_TTL_MS).is_empty());
        assert_eq!(agg.sweep(EXPLICIT_TTL_MS + 2)[0].closed, Closed::Expired);

        // A hung call does not keep its run open forever.
        let mut agg = RunAggregator::new();
        let hung = agg.begin(start(0, Some("a"), Some("r")));
        assert!(
            agg.sweep(EXPLICIT_TTL_MS + 2).is_empty(),
            "in flight: not expired"
        );
        let s = agg.sweep(MAX_RUN_LIFETIME_MS + 1);
        assert_eq!(s[0].closed, Closed::LifetimeExceeded);
        assert!(s[0].incomplete);
        agg.finish(hung, end(10));
        assert_eq!(agg.stats().late_events, 1);
        assert_eq!(agg.stats().lifetime_closed_runs, 1);

        let mut agg = RunAggregator::new();
        for i in 0..(MAX_CLOSED as u64 + 3) {
            let id = format!("r{i}");
            one(&mut agg, i, 1, Some("a"), Some(&id));
            agg.end_run(Some("a".into()), id, None);
        }
        assert_eq!(agg.stats().dropped_summaries, 3);
        assert_eq!(agg.drain_closed().len(), MAX_CLOSED);
    }

    fn summary() -> RunSummary {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, Some("a"), Some("r"));
        end_run(&mut agg, "a", "r")
    }

    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn the_log_fixes_modes_rotates_and_writes_whole_lines() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = private_dir();
        let path = dir.path().join("agent-runs.jsonl");
        // An existing 0644 file is tightened, not trusted.
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let log = SummaryLog::new(path.clone(), 1);
        log.append(&summary()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        log.append(&summary()).unwrap(); // past 1 byte: rotates first
        let rotated = dir.path().join("agent-runs.jsonl.1");
        assert_eq!(mode(&rotated), 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n') && text.lines().count() == 1);
        let back: RunSummary = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(back, summary());
    }

    #[test]
    fn the_log_refuses_symlinks_shared_dirs_and_hard_links() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = private_dir();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("agent-runs.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(SummaryLog::new(link, 1 << 20).append(&summary()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");

        let hard = dir.path().join("hard.jsonl");
        std::fs::hard_link(&target, &hard).unwrap();
        assert!(SummaryLog::new(hard, 1 << 20).append(&summary()).is_err());

        let shared = tempfile::tempdir().unwrap();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = shared.path().join("agent-runs.jsonl");
        assert!(SummaryLog::new(path, 1 << 20).append(&summary()).is_err());
    }

    #[test]
    fn concurrent_writers_lose_nothing_across_rotations() {
        let dir = private_dir();
        let path = dir.path().join("agent-runs.jsonl");
        let line = serde_json::to_string(&summary()).unwrap().len() as u64 + 1;
        // Rotate after every 3 lines, with 6 writers racing.
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let log = SummaryLog::new(path, line * 3);
                    for _ in 0..40 {
                        log.append(&summary()).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // Each rotation overwrites `.1`, so only the last two generations
        // survive; but the live file must be whole lines, at most 3, and no
        // writer may have appended to a rotated-away inode past the limit.
        let live = std::fs::read_to_string(&path).unwrap();
        let old = std::fs::read_to_string(dir.path().join("agent-runs.jsonl.1")).unwrap();
        for text in [&live, &old] {
            assert!(text.lines().count() <= 3, "{} lines", text.lines().count());
            assert!(text
                .lines()
                .all(|l| serde_json::from_str::<RunSummary>(l).is_ok()));
        }
        assert_eq!(old.lines().count(), 3, "a full generation was rotated");
    }

    #[test]
    fn concurrent_writers_never_interleave_lines() {
        let dir = private_dir();
        let path = dir.path().join("agent-runs.jsonl");
        let line_len = serde_json::to_string(&summary()).unwrap().len();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Separate SummaryLog values: only the file lock serialises them.
                    let log = SummaryLog::new(path, 1 << 30);
                    for _ in 0..50 {
                        log.append(&summary()).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 400);
        assert!(
            text.lines()
                .all(|line| line.len() == line_len
                    && serde_json::from_str::<RunSummary>(line).is_ok())
        );
    }
}
