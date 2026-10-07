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
//! - **Calls, not model turns.** The daemon only sees HTTP/tool calls.
//!   `model_round_trips` is filled only when the caller reports it (a trace),
//!   otherwise it stays `null` — never inferred from call counts.
//! - **Runs.** A run is keyed by an explicit run id (`X-Agent-Run`, or an MCP
//!   session's run_start/run_end). Without one, calls from the same owner are
//!   split wherever the owner was idle longer than [`IDLE_GAP_MS`], and the run
//!   is marked `inferred: true`. Calls are never summed across runs.
//! - **Concurrency.** `runner_busy_union_ms` is the union of the intervals the
//!   runner was serving this run (overlaps counted once); `runner_summed_ms`
//!   is the plain sum of those durations. They are reported separately and
//!   neither is subtracted from wall time to "derive" daemon time.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Calls from one owner further apart than this start a new inferred run.
pub const IDLE_GAP_MS: u64 = 120_000;

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

/// How a call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "class")]
pub enum Outcome {
    Ok,
    /// Refused because the caller's snapshot was out of date.
    Stale,
    /// The action may or may not have happened.
    OutcomeUnknown,
    /// Failed; the string is a short failure class (`element_not_found`, …).
    Failed(String),
}

/// One tool/HTTP call as the metrics see it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEvent {
    /// Wall-clock start, ms since the Unix epoch.
    pub start_ms: u64,
    /// Wall-clock duration of the call.
    pub duration_ms: u64,
    pub owner: Option<String>,
    /// Explicit run id from the caller, if any.
    pub run_id: Option<String>,
    pub kind: CallKind,
    /// The call asked for the settled change back (`observe` / `return=delta`).
    pub observed: bool,
    pub outcome: Outcome,
    /// Intervals (start_ms, end_ms) during which the runner served this call.
    pub runner_intervals: Vec<(u64, u64)>,
    /// Model round trips the caller reported for the work leading to this
    /// call (a trace). `None` when the caller did not say.
    pub model_round_trips: Option<u64>,
}

/// Summary of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The caller's run id, or a generated `inferred:<owner>:<start>` key.
    pub run_id: String,
    /// True when the run boundary was inferred from an idle gap.
    pub inferred: bool,
    pub owner: Option<String>,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub tool_calls: u64,
    pub batch_calls: u64,
    pub flow_calls: u64,
    pub observed_calls: u64,
    pub stale: u64,
    pub outcome_unknown: u64,
    pub failures: BTreeMap<String, u64>,
    pub call_p50_ms: Option<u64>,
    pub call_p95_ms: Option<u64>,
    /// Union of runner-serving intervals (overlaps counted once).
    pub runner_busy_union_ms: u64,
    /// Plain sum of runner-serving durations.
    pub runner_summed_ms: u64,
    /// Sum of caller-reported model round trips; `None` if no call reported any.
    pub model_round_trips: Option<u64>,
}

/// Groups calls into runs. Feed calls in start order.
#[derive(Debug, Default)]
pub struct RunAggregator {
    open: BTreeMap<String, Vec<CallEvent>>,
    /// Last call end per owner for inferred runs: (run key, end_ms).
    inferred_tail: BTreeMap<String, (String, u64)>,
    closed: Vec<RunSummary>,
}

impl RunAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one call to its run.
    pub fn push(&mut self, call: CallEvent) {
        let key = match &call.run_id {
            Some(id) => id.clone(),
            None => {
                let owner = call.owner.clone().unwrap_or_default();
                let end = call.start_ms + call.duration_ms;
                let previous = self.inferred_tail.get(&owner).cloned();
                match previous {
                    Some((key, last_end))
                        if call.start_ms.saturating_sub(last_end) <= IDLE_GAP_MS =>
                    {
                        self.inferred_tail
                            .insert(owner, (key.clone(), end.max(last_end)));
                        key
                    }
                    previous => {
                        if let Some((old_key, _)) = previous {
                            self.close(&old_key);
                        }
                        let key = format!("inferred:{owner}:{}", call.start_ms);
                        self.inferred_tail.insert(owner, (key.clone(), end));
                        key
                    }
                }
            }
        };
        self.open.entry(key).or_default().push(call);
    }

    /// Close an explicit run (MCP run_end) or any open run by key.
    pub fn close(&mut self, run_key: &str) -> Option<RunSummary> {
        let calls = self.open.remove(run_key)?;
        self.inferred_tail.retain(|_, (key, _)| key != run_key);
        let summary = summarize(run_key, &calls);
        self.closed.push(summary.clone());
        Some(summary)
    }

    /// Close inferred runs whose owner has been idle past the gap at `now_ms`.
    pub fn close_idle(&mut self, now_ms: u64) -> Vec<RunSummary> {
        let stale: Vec<String> = self
            .inferred_tail
            .values()
            .filter(|(_, end)| now_ms.saturating_sub(*end) > IDLE_GAP_MS)
            .map(|(key, _)| key.clone())
            .collect();
        stale.iter().filter_map(|key| self.close(key)).collect()
    }

    /// Summaries of every run closed so far.
    pub fn closed(&self) -> &[RunSummary] {
        &self.closed
    }

    /// A live summary of a run that is still open.
    pub fn peek(&self, run_key: &str) -> Option<RunSummary> {
        self.open
            .get(run_key)
            .map(|calls| summarize(run_key, calls))
    }
}

fn summarize(run_key: &str, calls: &[CallEvent]) -> RunSummary {
    let inferred = calls.iter().all(|c| c.run_id.is_none());
    let started_ms = calls.iter().map(|c| c.start_ms).min().unwrap_or(0);
    let ended_ms = calls
        .iter()
        .map(|c| c.start_ms + c.duration_ms)
        .max()
        .unwrap_or(started_ms);
    let mut failures = BTreeMap::new();
    let (mut stale, mut unknown) = (0, 0);
    for call in calls {
        match &call.outcome {
            Outcome::Ok => {}
            Outcome::Stale => stale += 1,
            Outcome::OutcomeUnknown => unknown += 1,
            Outcome::Failed(class) => *failures.entry(class.clone()).or_insert(0) += 1,
        }
    }
    let mut durations: Vec<u64> = calls.iter().map(|c| c.duration_ms).collect();
    durations.sort_unstable();
    let intervals: Vec<(u64, u64)> = calls
        .iter()
        .flat_map(|c| c.runner_intervals.iter().copied())
        .collect();
    let reported: Vec<u64> = calls.iter().filter_map(|c| c.model_round_trips).collect();
    RunSummary {
        run_id: run_key.to_string(),
        inferred,
        owner: calls.iter().find_map(|c| c.owner.clone()),
        started_ms,
        ended_ms,
        tool_calls: calls.len() as u64,
        batch_calls: calls.iter().filter(|c| c.kind == CallKind::Batch).count() as u64,
        flow_calls: calls.iter().filter(|c| c.kind == CallKind::Flow).count() as u64,
        observed_calls: calls.iter().filter(|c| c.observed).count() as u64,
        stale,
        outcome_unknown: unknown,
        failures,
        call_p50_ms: percentile(&durations, 50),
        call_p95_ms: percentile(&durations, 95),
        runner_busy_union_ms: interval_union_ms(&intervals),
        runner_summed_ms: intervals.iter().map(|(s, e)| e.saturating_sub(*s)).sum(),
        model_round_trips: if reported.is_empty() {
            None
        } else {
            Some(reported.iter().sum())
        },
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

/// Total length covered by possibly overlapping `(start, end)` intervals.
pub fn interval_union_ms(intervals: &[(u64, u64)]) -> u64 {
    let mut sorted: Vec<(u64, u64)> = intervals.iter().copied().filter(|(s, e)| e > s).collect();
    sorted.sort_unstable();
    let mut total = 0;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in sorted {
        match current {
            Some((cs, ce)) if start <= ce => current = Some((cs, ce.max(end))),
            Some((cs, ce)) => {
                total += ce - cs;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((cs, ce)) = current {
        total += ce - cs;
    }
    total
}

/// Append one run summary as a JSON line (e.g. `<state dir>/agent-runs.jsonl`,
/// next to `agent-timing.jsonl`).
pub fn append_summary(path: &Path, summary: &RunSummary) -> std::io::Result<()> {
    let line = serde_json::to_string(summary).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(start: u64, dur: u64, owner: &str, run: Option<&str>) -> CallEvent {
        CallEvent {
            start_ms: start,
            duration_ms: dur,
            owner: Some(owner.to_string()),
            run_id: run.map(str::to_string),
            kind: CallKind::Single,
            observed: false,
            outcome: Outcome::Ok,
            runner_intervals: vec![(start, start + dur)],
            model_round_trips: None,
        }
    }

    #[test]
    fn union_counts_overlaps_once_and_sum_does_not() {
        let intervals = [(0, 100), (50, 150), (200, 250), (240, 260), (300, 300)];
        assert_eq!(interval_union_ms(&intervals), 150 + 60);
        let summed: u64 = intervals.iter().map(|(s, e)| e - s).sum();
        assert_eq!(summed, 100 + 100 + 50 + 20);
        assert_eq!(interval_union_ms(&[]), 0);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let values: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&values, 50), Some(10));
        assert_eq!(percentile(&values, 95), Some(19));
        assert_eq!(percentile(&[7], 95), Some(7));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn idle_gap_splits_an_owner_into_inferred_runs() {
        let mut agg = RunAggregator::new();
        agg.push(call(0, 500, "a", None));
        agg.push(call(1_000, 500, "a", None));
        // Past the idle gap: a new run starts and the first one closes.
        agg.push(call(1_500 + IDLE_GAP_MS + 1, 500, "a", None));
        let closed = agg.closed();
        assert_eq!(closed.len(), 1);
        assert!(closed[0].inferred);
        assert_eq!(closed[0].tool_calls, 2);
        assert_eq!(closed[0].run_id, "inferred:a:0");
        let rest = agg.close_idle(10 * IDLE_GAP_MS);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].tool_calls, 1);
    }

    #[test]
    fn explicit_runs_never_merge_and_are_not_inferred() {
        let mut agg = RunAggregator::new();
        agg.push(call(0, 100, "a", Some("r1")));
        agg.push(call(50, 100, "a", Some("r2")));
        agg.push(call(400, 100, "a", Some("r1")));
        let r1 = agg.close("r1").unwrap();
        let r2 = agg.close("r2").unwrap();
        assert!(!r1.inferred && !r2.inferred);
        assert_eq!((r1.tool_calls, r2.tool_calls), (2, 1));
        assert_eq!((r1.started_ms, r1.ended_ms), (0, 500));
    }

    #[test]
    fn owners_get_separate_inferred_runs() {
        let mut agg = RunAggregator::new();
        agg.push(call(0, 100, "a", None));
        agg.push(call(10, 100, "b", None));
        let closed = agg.close_idle(IDLE_GAP_MS * 3);
        assert_eq!(closed.len(), 2);
        assert!(closed.iter().all(|r| r.tool_calls == 1));
    }

    #[test]
    fn counters_failures_and_model_trips() {
        let mut agg = RunAggregator::new();
        let mut c = call(0, 100, "a", Some("r"));
        c.kind = CallKind::Batch;
        c.observed = true;
        agg.push(c);
        let mut c = call(200, 100, "a", Some("r"));
        c.outcome = Outcome::Stale;
        agg.push(c);
        let mut c = call(400, 100, "a", Some("r"));
        c.outcome = Outcome::OutcomeUnknown;
        c.kind = CallKind::Flow;
        agg.push(c);
        let mut c = call(600, 100, "a", Some("r"));
        c.outcome = Outcome::Failed("element_not_found".into());
        agg.push(c);
        let summary = agg.peek("r").unwrap();
        assert_eq!(summary.batch_calls, 1);
        assert_eq!(summary.flow_calls, 1);
        assert_eq!(summary.observed_calls, 1);
        assert_eq!((summary.stale, summary.outcome_unknown), (1, 1));
        assert_eq!(summary.failures.get("element_not_found"), Some(&1));
        assert_eq!(
            summary.model_round_trips, None,
            "no trace → null, never inferred"
        );

        let mut c = call(800, 100, "a", Some("r"));
        c.model_round_trips = Some(3);
        agg.push(c);
        assert_eq!(agg.peek("r").unwrap().model_round_trips, Some(3));
    }

    #[test]
    fn concurrent_calls_report_union_and_sum_separately() {
        let mut agg = RunAggregator::new();
        agg.push(call(0, 1_000, "a", Some("r")));
        agg.push(call(500, 1_000, "a", Some("r")));
        let s = agg.close("r").unwrap();
        assert_eq!(s.runner_busy_union_ms, 1_500);
        assert_eq!(s.runner_summed_ms, 2_000);
        assert_eq!(s.ended_ms - s.started_ms, 1_500);
    }

    #[test]
    fn summaries_append_as_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-runs.jsonl");
        let mut agg = RunAggregator::new();
        agg.push(call(0, 100, "a", Some("r")));
        let summary = agg.close("r").unwrap();
        append_summary(&path, &summary).unwrap();
        append_summary(&path, &summary).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let back: RunSummary = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back, summary);
        assert!(lines[0].contains("\"model_round_trips\":null"));
    }
}
