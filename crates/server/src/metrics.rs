//! Task-level metrics for agent runs.
//!
//! `timing` records one request at a time. This module groups those requests
//! into *runs* — one agent task — and summarises each run: how many tool/HTTP
//! calls it took, how many were batches, observed actions or flow replays,
//! how many came back stale or `outcome_unknown`, what failed, and how long
//! calls took (p50/p95).
//!
//! Definitions, agreed with the chrome-use side so both report the same thing:
//!
//! - **Calls, not model turns.** The daemon only sees HTTP/tool calls. A run
//!   reports `model_round_trips` only when the caller declared a complete
//!   trace at run start and handed over the unique ids of *every* model turn
//!   at run end (including final turns that made no tool call). Anything less
//!   is `null`, and the turn ids callers did attach to individual calls are
//!   counted in the separately named `model_round_trips_caller_reported_partial`.
//! - **Runs need an identity.** A run is keyed by owner + explicit run id
//!   ([`RunKey::Explicit`]); without a run id, calls from the same owner are
//!   split wherever the owner was idle longer than [`IDLE_GAP_MS`]
//!   ([`RunKey::Inferred`]). Calls with neither are `unattributed` and never
//!   merged. Owners and run ids from callers are validated; invalid ones count
//!   as `invalid_identity` and are treated as absent.
//! - **Attribution at start.** A call is assigned to its run when it starts
//!   ([`RunAggregator::begin`]) and finished with the token it got, so a call
//!   that completes late still lands in the run it belongs to. A run closed
//!   while calls were in flight is marked `incomplete`; a call finishing after
//!   its run closed is counted as `late_events`.
//! - **Closed runs stay closed.** Reusing a closed run id opens a new run with
//!   the next `generation`, counted in `reopened_runs`.
//! - **Concurrency.** Runner intervals are clipped to their call's span
//!   (`clipped_intervals` / `rejected_intervals`). `runner_busy_union_ms`
//!   counts overlaps once; `runner_summed_ms` is the plain sum. Neither is
//!   subtracted from wall time to "derive" daemon time.
//! - **No silent wrap.** Sums use checked arithmetic; an overflow makes the
//!   field `null` and sets `arithmetic_overflow`.
//! - **Bounded.** Open runs, events per run, closed summaries and remembered
//!   closed ids are capped; explicit runs expire after [`EXPLICIT_TTL_MS`].
//!   Every drop or force-close is counted in [`AggregatorStats`].
//! - **Persistence.** Summaries hold only enums, numbers and validated ids —
//!   never free-form error text — and are written by one serialized writer
//!   ([`SummaryLog`]) to a 0600 file with one rotated generation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Calls from one owner further apart than this start a new inferred run.
pub const IDLE_GAP_MS: u64 = 120_000;
/// An explicit run with no activity for this long is closed as expired.
pub const EXPLICIT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
/// At most this many runs are open at once; the stalest is evicted.
pub const MAX_OPEN_RUNS: usize = 256;
/// Calls stored per run; later calls are counted but not stored.
pub const MAX_EVENTS_PER_RUN: usize = 5_000;
/// Closed summaries kept in memory until [`RunAggregator::drain_closed`].
pub const MAX_CLOSED: usize = 1_000;
/// Closed explicit run ids remembered to detect reuse.
pub const MAX_REMEMBERED_CLOSED: usize = 4_096;
/// Owners and run ids longer than this are rejected.
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
    /// Model turn ids the caller attached to this call (a partial trace).
    pub model_turn_ids: Vec<String>,
}

/// Returned by [`RunAggregator::begin`]; hand it back to `finish`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallToken {
    key: Option<RunKey>,
    start_ms: u64,
}

/// Identity of a run. The owner is always part of it, so the same run id
/// from two callers never merges, and an explicit id can never collide with
/// an inferred key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunKey {
    /// The caller named the run. `owner` is `None` for a caller that sent a
    /// run id but no owner: still an explicit boundary, in its own namespace.
    /// `generation` counts reuses of a closed id.
    Explicit {
        owner: Option<String>,
        run_id: String,
        generation: u32,
    },
    /// Split by idle gaps; only for callers with an owner.
    Inferred { owner: String, seq: u64 },
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
    /// Evicted to stay under [`MAX_OPEN_RUNS`].
    Evicted,
}

/// Summary of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub key: RunKey,
    pub inferred: bool,
    pub closed: Closed,
    /// Calls were still in flight when the run closed.
    pub incomplete: bool,
    pub in_flight_at_close: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    /// Every finished call, including any beyond the stored cap.
    pub tool_calls: u64,
    /// Calls counted but not stored (beyond [`MAX_EVENTS_PER_RUN`]): the
    /// other figures cover only the stored ones.
    pub dropped_events: u64,
    pub batch_calls: u64,
    pub flow_calls: u64,
    pub observed_calls: u64,
    pub stale: u64,
    pub outcome_unknown: u64,
    pub failures: BTreeMap<FailureClass, u64>,
    pub call_p50_ms: Option<u64>,
    pub call_p95_ms: Option<u64>,
    /// Union of clipped runner intervals; `None` on overflow.
    pub runner_busy_union_ms: Option<u64>,
    /// Plain sum of clipped runner intervals; `None` on overflow.
    pub runner_summed_ms: Option<u64>,
    pub clipped_intervals: u64,
    pub rejected_intervals: u64,
    /// Total model turns — only with a declared and delivered complete trace.
    pub model_round_trips: Option<u64>,
    /// Distinct turn ids callers attached to calls; never a total.
    pub model_round_trips_caller_reported_partial: Option<u64>,
    /// A sum overflowed; the affected fields are `null`.
    pub arithmetic_overflow: bool,
}

/// Losses, force-closes and rejections so far: surfaced, never silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatorStats {
    /// Calls with neither owner nor run id: not attributable to any task.
    pub unattributed_events: u64,
    /// Owners or run ids that failed validation (treated as absent).
    pub invalid_identity: u64,
    /// Calls whose times were unusable (end before start, overflow).
    pub invalid_events: u64,
    /// Calls counted but not stored because a run hit its event cap.
    pub dropped_events: u64,
    /// Calls that finished after their run had closed.
    pub late_events: u64,
    /// Open runs force-closed to stay under the open-run cap.
    pub evicted_runs: u64,
    /// Explicit runs closed by the TTL.
    pub expired_runs: u64,
    /// Closed run ids that were used again (each opened a new generation).
    pub reopened_runs: u64,
    /// Closed summaries discarded because nobody drained them in time.
    pub dropped_summaries: u64,
}

#[derive(Debug, Default)]
struct OpenRun {
    calls: Vec<Finished>,
    total_calls: u64,
    in_flight: u64,
    first_start_ms: Option<u64>,
    last_activity_ms: u64,
    trace_declared: bool,
}

#[derive(Debug)]
struct Finished {
    start_ms: u64,
    end_ms: u64,
    end: CallEnd,
}

/// Groups calls into runs.
#[derive(Debug, Default)]
pub struct RunAggregator {
    open: BTreeMap<RunKey, OpenRun>,
    /// Current inferred run per owner.
    inferred_current: BTreeMap<String, RunKey>,
    next_seq: u64,
    /// Highest closed generation per (owner, run id), bounded.
    closed_explicit: BTreeMap<(Option<String>, String), u32>,
    closed_order: VecDeque<(Option<String>, String)>,
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

    fn checked_id(&mut self, id: Option<String>) -> Option<String> {
        let id = id?;
        if valid_id(&id) {
            Some(id)
        } else {
            self.stats.invalid_identity += 1;
            None
        }
    }

    /// The open key for an explicit (owner, run id), opening the next
    /// generation if the id was closed before.
    fn explicit_key(&mut self, owner: Option<String>, run_id: String, now_ms: u64) -> RunKey {
        let live = self.open.keys().find(|key| {
            matches!(key, RunKey::Explicit { owner: o, run_id: r, .. } if *o == owner && *r == run_id)
        });
        if let Some(key) = live {
            return key.clone();
        }
        let generation = match self.closed_explicit.get(&(owner.clone(), run_id.clone())) {
            Some(last) => {
                self.stats.reopened_runs += 1;
                last.saturating_add(1)
            }
            None => 0,
        };
        let key = RunKey::Explicit {
            owner,
            run_id,
            generation,
        };
        self.open_run(&key, now_ms);
        key
    }

    fn open_run(&mut self, key: &RunKey, now_ms: u64) {
        if self.open.contains_key(key) {
            return;
        }
        if self.open.len() >= MAX_OPEN_RUNS {
            let stalest = self
                .open
                .iter()
                .min_by_key(|(_, run)| run.last_activity_ms)
                .map(|(key, _)| key.clone());
            if let Some(stalest) = stalest {
                self.stats.evicted_runs += 1;
                self.close_as(&stalest, Closed::Evicted);
            }
        }
        self.open.insert(
            key.clone(),
            OpenRun {
                last_activity_ms: now_ms,
                ..OpenRun::default()
            },
        );
    }

    /// Open an explicit run (MCP run_start). `complete_trace` declares that
    /// the caller will hand over every model turn id at run end.
    pub fn start_run(
        &mut self,
        owner: Option<String>,
        run_id: String,
        now_ms: u64,
        complete_trace: bool,
    ) {
        let owner = self.checked_id(owner);
        let Some(run_id) = self.checked_id(Some(run_id)) else {
            return;
        };
        let key = self.explicit_key(owner, run_id, now_ms);
        if let Some(run) = self.open.get_mut(&key) {
            run.trace_declared = complete_trace;
        }
    }

    /// Register a call when it starts; the run it belongs to is decided now.
    pub fn begin(&mut self, start: CallStart) -> CallToken {
        let owner = self.checked_id(start.owner);
        let run_id = self.checked_id(start.run_id);
        let key = match (run_id, owner) {
            (Some(run_id), owner) => Some(self.explicit_key(owner, run_id, start.start_ms)),
            (None, Some(owner)) => Some(self.inferred_key(owner, start.start_ms)),
            (None, None) => {
                self.stats.unattributed_events += 1;
                None
            }
        };
        if let Some(run) = key.as_ref().and_then(|key| self.open.get_mut(key)) {
            run.in_flight += 1;
            run.first_start_ms = Some(
                run.first_start_ms
                    .map_or(start.start_ms, |s| s.min(start.start_ms)),
            );
            run.last_activity_ms = run.last_activity_ms.max(start.start_ms);
        }
        CallToken {
            key,
            start_ms: start.start_ms,
        }
    }

    fn inferred_key(&mut self, owner: String, start_ms: u64) -> RunKey {
        if let Some(key) = self.inferred_current.get(&owner).cloned() {
            let (fresh, busy) = self.open.get(&key).map_or((false, false), |run| {
                (
                    start_ms.saturating_sub(run.last_activity_ms) <= IDLE_GAP_MS,
                    run.in_flight > 0,
                )
            });
            if fresh {
                return key;
            }
            // Past the gap: a new run starts. A run with calls still in
            // flight stays open so they land where they started; `sweep`
            // closes it once they finish.
            if !busy {
                self.close_as(&key, Closed::Idle);
            }
        }
        self.next_seq += 1;
        let key = RunKey::Inferred {
            owner: owner.clone(),
            seq: self.next_seq,
        };
        self.inferred_current.insert(owner, key.clone());
        self.open_run(&key, start_ms);
        key
    }

    /// Finish a call started with [`begin`](Self::begin).
    pub fn finish(&mut self, token: CallToken, end: CallEnd) {
        let Some(key) = token.key else {
            return; // counted as unattributed at begin
        };
        let Some(run) = self.open.get_mut(&key) else {
            self.stats.late_events += 1;
            return;
        };
        run.in_flight = run.in_flight.saturating_sub(1);
        if end.end_ms < token.start_ms {
            self.stats.invalid_events += 1;
            return;
        }
        run.total_calls = run.total_calls.saturating_add(1);
        run.last_activity_ms = run.last_activity_ms.max(end.end_ms);
        if run.calls.len() < MAX_EVENTS_PER_RUN {
            run.calls.push(Finished {
                start_ms: token.start_ms,
                end_ms: end.end_ms,
                end,
            });
        } else {
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
        let owner = self.checked_id(owner);
        let run_id = self.checked_id(Some(run_id))?;
        let key = self
            .open
            .keys()
            .find(|key| {
                matches!(key, RunKey::Explicit { owner: o, run_id: r, .. } if *o == owner && *r == run_id)
            })?
            .clone();
        let turns = turn_ids.and_then(|ids| {
            let unique: BTreeSet<String> = ids.into_iter().filter(|id| valid_id(id)).collect();
            (!unique.is_empty() && unique.len() <= MAX_TURNS).then_some(unique.len() as u64)
        });
        self.close_with(&key, Closed::Ended, turns)
    }

    /// Close inferred runs idle past the gap and explicit runs past the TTL.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<RunSummary> {
        let due: Vec<(RunKey, Closed)> = self
            .open
            .iter()
            .filter(|(_, run)| run.in_flight == 0)
            .filter_map(|(key, run)| {
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
            .filter_map(|(key, how)| self.close_as(key, *how))
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

    /// Losses, force-closes and rejections so far.
    pub fn stats(&self) -> AggregatorStats {
        self.stats
    }

    fn close_as(&mut self, key: &RunKey, how: Closed) -> Option<RunSummary> {
        self.close_with(key, how, None)
    }

    fn close_with(&mut self, key: &RunKey, how: Closed, turns: Option<u64>) -> Option<RunSummary> {
        let run = self.open.remove(key)?;
        match key {
            RunKey::Inferred { owner, .. } => {
                if self.inferred_current.get(owner) == Some(key) {
                    self.inferred_current.remove(owner);
                }
            }
            RunKey::Explicit {
                owner,
                run_id,
                generation,
            } => {
                let id = (owner.clone(), run_id.clone());
                if self
                    .closed_explicit
                    .insert(id.clone(), *generation)
                    .is_none()
                {
                    self.closed_order.push_back(id);
                    if self.closed_order.len() > MAX_REMEMBERED_CLOSED {
                        if let Some(oldest) = self.closed_order.pop_front() {
                            self.closed_explicit.remove(&oldest);
                        }
                    }
                }
            }
        }
        if how == Closed::Expired {
            self.stats.expired_runs += 1;
        }
        let summary = summarize(key, &run, how, turns);
        if self.closed.len() >= MAX_CLOSED {
            self.closed.pop_front();
            self.stats.dropped_summaries += 1;
        }
        self.closed.push_back(summary.clone());
        Some(summary)
    }
}

fn summarize(key: &RunKey, run: &OpenRun, closed: Closed, turns: Option<u64>) -> RunSummary {
    let calls = &run.calls;
    let mut overflow = false;
    let started_ms = run
        .first_start_ms
        .or_else(|| calls.iter().map(|c| c.start_ms).min())
        .unwrap_or(0);
    let ended_ms = calls.iter().map(|c| c.end_ms).max().unwrap_or(started_ms);
    let mut failures = BTreeMap::new();
    let (mut stale, mut unknown) = (0u64, 0u64);
    for call in calls {
        match call.end.outcome {
            Outcome::Ok => {}
            Outcome::Stale => stale += 1,
            Outcome::OutcomeUnknown => unknown += 1,
            Outcome::Failed(class) => *failures.entry(class).or_insert(0u64) += 1,
        }
    }
    let mut durations: Vec<u64> = calls.iter().map(|c| c.end_ms - c.start_ms).collect();
    durations.sort_unstable();
    // Clip every runner interval to its call's span.
    let (mut clipped, mut rejected) = (0u64, 0u64);
    let mut intervals = Vec::new();
    for call in calls {
        for &(s, e) in &call.end.runner_intervals {
            let (cs, ce) = (s.max(call.start_ms), e.min(call.end_ms));
            if ce <= cs {
                rejected += 1;
                continue;
            }
            if (cs, ce) != (s, e) {
                clipped += 1;
            }
            intervals.push((cs, ce));
        }
    }
    let summed = intervals
        .iter()
        .try_fold(0u64, |acc, (s, e)| acc.checked_add(e - s));
    let union = interval_union_ms(&intervals);
    if summed.is_none() || union.is_none() {
        overflow = true;
    }
    let partial: BTreeSet<&str> = calls
        .iter()
        .flat_map(|c| c.end.model_turn_ids.iter())
        .map(String::as_str)
        .filter(|id| valid_id(id))
        .collect();
    let dropped_events = run.total_calls.saturating_sub(calls.len() as u64);
    let complete = run.trace_declared && run.in_flight == 0 && turns.is_some();
    RunSummary {
        key: key.clone(),
        inferred: matches!(key, RunKey::Inferred { .. }),
        closed,
        incomplete: run.in_flight > 0,
        in_flight_at_close: run.in_flight,
        started_ms,
        ended_ms,
        tool_calls: run.total_calls,
        dropped_events,
        batch_calls: calls
            .iter()
            .filter(|c| c.end.kind == CallKind::Batch)
            .count() as u64,
        flow_calls: calls
            .iter()
            .filter(|c| c.end.kind == CallKind::Flow)
            .count() as u64,
        observed_calls: calls.iter().filter(|c| c.end.observed).count() as u64,
        stale,
        outcome_unknown: unknown,
        failures,
        call_p50_ms: percentile(&durations, 50),
        call_p95_ms: percentile(&durations, 95),
        runner_busy_union_ms: union,
        runner_summed_ms: summed,
        clipped_intervals: clipped,
        rejected_intervals: rejected,
        model_round_trips: if complete { turns } else { None },
        model_round_trips_caller_reported_partial: (!partial.is_empty())
            .then(|| partial.len() as u64),
        arithmetic_overflow: overflow,
    }
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

/// The one writer for `agent-runs.jsonl`: serialized appends, mode 0600,
/// one rotated generation.
pub struct SummaryLog {
    path: PathBuf,
    rotate_bytes: u64,
    lock: Mutex<()>,
}

impl SummaryLog {
    pub fn new(path: PathBuf, rotate_bytes: u64) -> Self {
        Self {
            path,
            rotate_bytes,
            lock: Mutex::new(()),
        }
    }

    pub fn append(&self, summary: &RunSummary) -> std::io::Result<()> {
        let line = serde_json::to_string(summary).map_err(std::io::Error::other)?;
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?;
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.rotate_bytes) {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            std::fs::rename(&self.path, rotated)?;
        }
        append_line(&self.path, &line)
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{line}")
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
            model_turn_ids: Vec::new(),
        }
    }

    /// One complete call with the runner busy for its whole span.
    fn one(agg: &mut RunAggregator, at: u64, dur: u64, owner: Option<&str>, run: Option<&str>) {
        let token = agg.begin(start(at, owner, run));
        let mut e = end(at + dur);
        e.runner_intervals = vec![(at, at + dur)];
        agg.finish(token, e);
    }

    fn explicit(owner: &str, run: &str, generation: u32) -> RunKey {
        RunKey::Explicit {
            owner: Some(owner.into()),
            run_id: run.into(),
            generation,
        }
    }

    #[test]
    fn union_counts_overlaps_once_and_sum_does_not() {
        let intervals = [(0, 100), (50, 150), (200, 250), (240, 260), (300, 300)];
        assert_eq!(interval_union_ms(&intervals), Some(150 + 60));
        assert_eq!(interval_union_ms(&[]), Some(0));
        assert_eq!(
            interval_union_ms(&[(0, u64::MAX), (u64::MAX - 1, u64::MAX)]),
            Some(u64::MAX)
        );
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
        // A call far past the gap starts a new run — but `late` is still in
        // flight, so the first run stays open and keeps it.
        let newer = agg.begin(start(200_000, Some("a"), None));
        agg.finish(newer, end(200_100));
        agg.finish(late, end(1_100));
        let mut runs = agg.sweep(10 * IDLE_GAP_MS + 200_100);
        runs.sort_by_key(|r| r.started_ms);
        let counts: Vec<u64> = runs.iter().map(|r| r.tool_calls).collect();
        assert_eq!(counts, vec![2, 1], "first+late in the old run, newer alone");
        assert_eq!((runs[0].started_ms, runs[0].ended_ms), (0, 1_100));
        assert_eq!(runs[1].started_ms, 200_000);
        assert_eq!(agg.stats().late_events, 0);
    }

    #[test]
    fn closing_with_calls_in_flight_marks_incomplete_and_counts_late() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("a"), Some("r")));
        let s = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert!(s.incomplete);
        assert_eq!(s.in_flight_at_close, 1);
        agg.finish(token, end(50));
        assert_eq!(agg.stats().late_events, 1);
    }

    #[test]
    fn the_same_run_id_from_two_owners_never_merges() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, Some("a"), Some("r1"));
        one(&mut agg, 10, 100, Some("b"), Some("r1"));
        one(&mut agg, 20, 100, None, Some("r1"));
        let a = agg.end_run(Some("a".into()), "r1".into(), None).unwrap();
        let b = agg.end_run(Some("b".into()), "r1".into(), None).unwrap();
        let anon = agg.end_run(None, "r1".into(), None).unwrap();
        assert_eq!((a.tool_calls, b.tool_calls, anon.tool_calls), (1, 1, 1));
        // Closing checks the owner: "b" cannot close "a"'s run.
        one(&mut agg, 30, 1, Some("a"), Some("r2"));
        assert!(agg.end_run(Some("b".into()), "r2".into(), None).is_none());
        assert!(agg.peek(&explicit("a", "r2", 0)).is_some());
    }

    #[test]
    fn explicit_ids_cannot_collide_with_inferred_keys() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, Some("a"), None);
        one(&mut agg, 10, 100, Some("a"), Some("a:1"));
        one(&mut agg, 20, 100, Some("a"), Some("inferred:a:1"));
        assert_eq!(agg.peek(&explicit("a", "a:1", 0)).unwrap().tool_calls, 1);
        assert_eq!(
            agg.peek(&explicit("a", "inferred:a:1", 0))
                .unwrap()
                .tool_calls,
            1
        );
        let inferred = agg.sweep(IDLE_GAP_MS * 3);
        assert_eq!(inferred.iter().filter(|r| r.inferred).count(), 1);
    }

    #[test]
    fn a_closed_id_reopens_as_a_new_generation() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 10, Some("a"), Some("r"));
        agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        one(&mut agg, 100, 10, Some("a"), Some("r"));
        assert_eq!(agg.stats().reopened_runs, 1);
        let again = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert_eq!(again.key, explicit("a", "r", 1));
        assert_eq!(again.tool_calls, 1);
    }

    #[test]
    fn anonymous_and_invalid_callers_are_counted_not_merged() {
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, None, None);
        one(&mut agg, 10, 100, None, None);
        one(&mut agg, 20, 100, Some(""), None); // empty is invalid → anonymous
        one(&mut agg, 30, 100, Some(&"x".repeat(MAX_ID_LEN + 1)), None);
        one(&mut agg, 40, 100, Some("a b"), None);
        assert_eq!(agg.stats().unattributed_events, 5);
        assert_eq!(agg.stats().invalid_identity, 3);
        assert!(agg.sweep(IDLE_GAP_MS * 3).is_empty());
    }

    #[test]
    fn runner_intervals_are_clipped_to_their_call() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(100, Some("a"), Some("r")));
        let mut e = end(200);
        e.runner_intervals = vec![(0, 10_000), (120, 150), (300, 400), (180, 170)];
        agg.finish(token, e);
        let s = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert_eq!(s.runner_busy_union_ms, Some(100));
        assert_eq!(s.runner_summed_ms, Some(130));
        assert_eq!((s.clipped_intervals, s.rejected_intervals), (1, 2));
    }

    #[test]
    fn overflow_is_flagged_never_wrapped() {
        let mut agg = RunAggregator::new();
        let token = agg.begin(start(0, Some("a"), Some("r")));
        let mut e = end(u64::MAX);
        e.runner_intervals = vec![(0, u64::MAX), (0, u64::MAX)];
        agg.finish(token, e);
        let s = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert!(s.arithmetic_overflow);
        assert_eq!(s.runner_summed_ms, None);
        assert_eq!(s.runner_busy_union_ms, Some(u64::MAX));
        // An end before the start is rejected, not wrapped.
        let token = agg.begin(start(500, Some("a"), Some("q")));
        agg.finish(token, end(100));
        assert_eq!(agg.stats().invalid_events, 1);
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
        let s = agg.peek(&explicit("a", "r", 0)).unwrap();
        assert_eq!((s.batch_calls, s.flow_calls, s.observed_calls), (1, 3, 1));
        assert_eq!((s.stale, s.outcome_unknown), (1, 1));
        assert_eq!(s.failures.get(&FailureClass::ElementNotFound), Some(&1));
        assert_eq!(s.failures.get(&FailureClass::Other), Some(&1));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("free text"));
    }

    #[test]
    fn model_round_trips_need_a_declared_complete_trace() {
        let mut agg = RunAggregator::new();
        // Per-call turn ids alone are only a partial report.
        for i in 0..5u64 {
            let token = agg.begin(start(i * 100, Some("a"), Some("r")));
            let mut e = end(i * 100 + 50);
            if i == 0 {
                e.model_turn_ids = vec!["t1".into(), "t2".into(), "t3".into()];
            }
            agg.finish(token, e);
        }
        let s = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert_eq!(s.model_round_trips, None);
        assert_eq!(s.model_round_trips_caller_reported_partial, Some(3));

        // Turn ids at run end without a declaration at start: still null.
        one(&mut agg, 0, 10, Some("a"), Some("u"));
        let s = agg
            .end_run(
                Some("a".into()),
                "u".into(),
                Some(vec!["t1".into(), "t2".into()]),
            )
            .unwrap();
        assert_eq!(s.model_round_trips, None);

        // Declared at start and delivered at end (including turns with no
        // tool call): the only case that reports a total. Duplicates count once.
        agg.start_run(Some("a".into()), "v".into(), 0, true);
        one(&mut agg, 0, 10, Some("a"), Some("v"));
        let turns = vec!["t1".into(), "t2".into(), "t2".into(), "t3".into()];
        let s = agg
            .end_run(Some("a".into()), "v".into(), Some(turns))
            .unwrap();
        assert_eq!(s.model_round_trips, Some(3));
    }

    #[test]
    fn caps_and_ttl_count_what_they_drop() {
        let mut agg = RunAggregator::new();
        for i in 0..(MAX_EVENTS_PER_RUN as u64 + 7) {
            one(&mut agg, i, 1, Some("a"), Some("r"));
        }
        let s = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        assert_eq!(
            (s.tool_calls, s.dropped_events),
            (MAX_EVENTS_PER_RUN as u64 + 7, 7)
        );

        let mut agg = RunAggregator::new();
        for i in 0..(MAX_OPEN_RUNS as u64 + 2) {
            one(&mut agg, i * 10, 1, Some("a"), Some(&format!("r{i}")));
        }
        assert_eq!(agg.stats().evicted_runs, 2);
        let evicted = agg.drain_closed();
        assert!(evicted.iter().all(|s| s.closed == Closed::Evicted));
        assert_eq!(evicted[0].key, explicit("a", "r0", 0));

        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 1, Some("a"), Some("r"));
        assert!(agg.sweep(EXPLICIT_TTL_MS).is_empty());
        assert_eq!(agg.sweep(EXPLICIT_TTL_MS + 2)[0].closed, Closed::Expired);
        assert_eq!(agg.stats().expired_runs, 1);

        let mut agg = RunAggregator::new();
        for i in 0..(MAX_CLOSED as u64 + 3) {
            let id = format!("r{i}");
            one(&mut agg, i, 1, Some("a"), Some(&id));
            agg.end_run(Some("a".into()), id, None);
        }
        assert_eq!(agg.stats().dropped_summaries, 3);
        assert_eq!(agg.drain_closed().len(), MAX_CLOSED);
    }

    #[test]
    fn the_log_is_private_serialized_and_rotates() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-runs.jsonl");
        let mut agg = RunAggregator::new();
        one(&mut agg, 0, 100, Some("a"), Some("r"));
        let summary = agg.end_run(Some("a".into()), "r".into(), None).unwrap();
        let log = SummaryLog::new(path.clone(), 1);
        log.append(&summary).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        log.append(&summary).unwrap(); // past 1 byte: rotates first
        assert!(dir.path().join("agent-runs.jsonl.1").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let back: RunSummary = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(back, summary);
        assert!(text.contains("\"model_round_trips\":null"));
    }
}
