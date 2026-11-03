//! Opt-in, metadata-only diagnostic timelines. Never a replay source.
//!
//! The HTTP/MCP owner calls `start` ONLY for an explicit run `trace:true`,
//! using the authenticated caller's owner namespace and opaque run IDs.
//! Keep this store on AppState in an Arc<Mutex<TraceStore>>; hold the lock across
//! each operation. At completion, recheck the original action's privacy AND
//! authorization scope and pass `private_flag=true` if either is invalid.
//! Capture `active_handle` when work starts, then call `record_for` on completion
//! to fence late responses against their original run incarnation. `record` is
//! a compatibility helper for immediate records, not delayed request completion.
//! Call `end` on owner revocation/run end so later actions cannot join that trace.
//! No scope checks or transport wiring are implemented here.
//!
//! Only bounded memory is retained: 16 runs, 200 events/run, 128-byte IDs.
//! Completed runs are evicted oldest-first; active runs are never evicted.
//! Full event buffers drop NEW events (sequence/drop counts expose truncation).
//! Export is explicit and owner-scoped. Restart/eviction loses the trace; there
//! is no disk persistence, durable audit guarantee, or automatic replay.

use serde_json::{json, Map, Value};
use std::collections::VecDeque;

pub const MAX_RUNS: usize = 16;
pub const MAX_EVENTS: usize = 200;
const MAX_ID_BYTES: usize = 128;
const MAX_NUMBER: u64 = 1_000_000;

#[derive(Default)]
pub struct TraceStore {
    runs: VecDeque<Run>,
    last_generation: u64,
}

/// Owned snapshot of a run incarnation. Capture once; do not refresh at completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceHandle {
    pub run_id: String,
    pub generation: u64,
}

struct Run {
    owner: String,
    id: String,
    generation: u64,
    active: bool,
    seen: u64,
    dropped: u64,
    events: Vec<Value>,
}

impl TraceStore {
    /// Read-only preflight. Hold the AppState store lock across preflight and
    /// start when coordinating with metrics; this is not a reservation.
    /// Retained ended IDs cannot be reused. Evicted IDs get a fresh generation.
    pub fn can_start(&self, owner: &str, run_id: &str) -> bool {
        if !valid_id(owner) || !valid_id(run_id) {
            return false;
        }
        if self
            .runs
            .iter()
            .any(|r| r.owner == owner && r.active && r.id != run_id)
        {
            return false;
        }
        if let Some(run) = self
            .runs
            .iter()
            .find(|r| r.owner == owner && r.id == run_id)
        {
            return run.active;
        }
        self.last_generation < u64::MAX
            && (self.runs.len() < MAX_RUNS || self.runs.iter().any(|r| !r.active))
    }

    /// Idempotent only for the same ACTIVE owner/run. Never reopens an ended run.
    pub fn start(&mut self, owner: &str, run_id: &str) -> bool {
        if !self.can_start(owner, run_id) {
            return false;
        }
        if self
            .runs
            .iter()
            .any(|r| r.owner == owner && r.id == run_id && r.active)
        {
            return true;
        }
        if self.runs.len() == MAX_RUNS {
            let Some(index) = self.runs.iter().position(|r| !r.active) else {
                return false;
            };
            self.runs.remove(index);
        }
        // Preflight checks overflow before eviction; generations never wrap or
        // reset when old runs leave the bounded retention queue.
        self.last_generation += 1;
        self.runs.push_back(Run {
            owner: owner.into(),
            id: run_id.into(),
            generation: self.last_generation,
            active: true,
            seen: 0,
            dropped: 0,
            events: Vec::new(),
        });
        true
    }

    pub fn active_handle(&self, owner: &str) -> Option<TraceHandle> {
        let run = self.runs.iter().find(|r| r.owner == owner && r.active)?;
        Some(TraceHandle {
            run_id: run.id.clone(),
            generation: run.generation,
        })
    }

    pub fn end(&mut self, owner: &str, run_id: &str) {
        if let Some(run) = self
            .runs
            .iter_mut()
            .find(|r| r.owner == owner && r.id == run_id)
        {
            run.active = false;
        }
    }

    /// Access revocation ends the active trace without discarding its history.
    pub fn end_owner(&mut self, owner: &str) {
        for run in self
            .runs
            .iter_mut()
            .filter(|run| run.owner == owner && run.active)
        {
            run.active = false;
        }
    }

    /// Compatibility helper: capture the current incarnation and delegate.
    /// Delayed work must instead capture `active_handle` before it starts.
    pub fn record(
        &mut self,
        owner: &str,
        action_json: &Value,
        result_json: &Value,
        at_ms: u64,
        private_flag: bool,
    ) {
        let Some(handle) = self.active_handle(owner) else {
            return;
        };
        self.record_for(
            owner,
            &handle,
            action_json,
            result_json,
            at_ms,
            private_flag,
        );
    }

    /// Append only to the still-active original owner/run/generation. Private,
    /// revoked, ended, evicted, or stale-handle actions leave no event/counter.
    /// No raw request/result value is cloned or retained, even temporarily.
    pub fn record_for(
        &mut self,
        owner: &str,
        handle: &TraceHandle,
        action_json: &Value,
        result_json: &Value,
        at_ms: u64,
        private_flag: bool,
    ) {
        if private_flag {
            return;
        }
        let Some(run) = self.runs.iter_mut().find(|r| {
            r.owner == owner
                && r.active
                && r.id == handle.run_id
                && r.generation == handle.generation
        }) else {
            return;
        };
        run.seen = run.seen.saturating_add(1);
        if run.events.len() == MAX_EVENTS {
            run.dropped = run.dropped.saturating_add(1);
            return;
        }
        run.events.push(json!({
            "seq": run.seen,
            "at_ms": at_ms,
            "action": redact_action(action_json),
            "result": redact_result(result_json),
        }));
    }

    pub fn export(&self, owner: &str, run_id: &str) -> Option<Value> {
        let run = self
            .runs
            .iter()
            .find(|r| r.owner == owner && r.id == run_id)?;
        Some(json!({
            "schema": "action_trace.v1",
            "run_id": run.id,
            "generation": run.generation,
            "active": run.active,
            "metadata_only": true,
            "replayable": false,
            "storage": "bounded_memory",
            "last_sequence": run.seen,
            "dropped": run.dropped,
            "truncated": run.dropped != 0,
            "events": run.events,
        }))
    }

    /// One metadata header (without events), then one event per line; final LF.
    /// Uses the same owner check and sanitized data as the JSON export.
    pub fn export_jsonl(&self, owner: &str, run_id: &str) -> Option<String> {
        let mut header = self.export(owner, run_id)?;
        let events = header.as_object_mut()?.remove("events")?;
        let mut lines = vec![header.to_string()];
        lines.extend(events.as_array()?.iter().map(Value::to_string));
        Some(lines.join("\n") + "\n")
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

// Closed vocabulary: unknown request strings can never become output strings.
#[derive(Clone, Copy)]
enum ActionKind {
    Tap,
    DoubleTap,
    LongPress,
    Swipe,
    Drag,
    Text,
    SetValue,
    Batch,
    Key,
    Home,
    Launch,
    Terminate,
    Alert,
    TapLocator,
    Perform,
    Scroll,
    Shortcut,
    WaitFor,
    Pause,
    Unknown,
}

impl ActionKind {
    fn parse(value: &Value) -> Self {
        match value.get("type").and_then(Value::as_str) {
            Some("tap") => Self::Tap,
            Some("double_tap") => Self::DoubleTap,
            Some("long_press") => Self::LongPress,
            Some("swipe") => Self::Swipe,
            Some("drag") => Self::Drag,
            Some("text") => Self::Text,
            Some("set_value") => Self::SetValue,
            Some("batch") => Self::Batch,
            Some("key") => Self::Key,
            Some("home") => Self::Home,
            Some("launch" | "launch_app") => Self::Launch,
            Some("terminate" | "terminate_app") => Self::Terminate,
            Some("alert") => Self::Alert,
            Some("tap_locator") => Self::TapLocator,
            Some("perform") => Self::Perform,
            Some("scroll") => Self::Scroll,
            Some("shortcut") => Self::Shortcut,
            Some("wait_for") => Self::WaitFor,
            Some("pause") => Self::Pause,
            None if value.get("type").is_none() => {
                match value.get("kind").and_then(Value::as_str) {
                    Some("wait_for") => Self::WaitFor,
                    Some("pause") => Self::Pause,
                    _ if value.get("steps").is_some_and(Value::is_array) => Self::Batch,
                    _ => Self::Unknown,
                }
            }
            _ => Self::Unknown,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::DoubleTap => "double_tap",
            Self::LongPress => "long_press",
            Self::Swipe => "swipe",
            Self::Drag => "drag",
            Self::Text => "text",
            Self::SetValue => "set_value",
            Self::Batch => "batch",
            Self::Key => "key",
            Self::Home => "home",
            Self::Launch => "launch_app",
            Self::Terminate => "terminate_app",
            Self::Alert => "alert",
            Self::TapLocator => "tap_locator",
            Self::Perform => "perform",
            Self::Scroll => "scroll",
            Self::Shortcut => "shortcut",
            Self::WaitFor => "wait_for",
            Self::Pause => "pause",
            Self::Unknown => "unknown",
        }
    }
}

fn bounded_uint(value: &Value) -> Option<Value> {
    value.as_u64().filter(|n| *n <= MAX_NUMBER).map(Value::from)
}

fn redact_action(value: &Value) -> Value {
    let kind = ActionKind::parse(value);
    let mut out = json!({"kind": kind.name()});
    // Unknown actions have no inferred schema, even if their fields look familiar.
    if matches!(kind, ActionKind::Unknown) {
        return out;
    }
    let input = match kind {
        ActionKind::Text => Some(("text", "text_chars")),
        ActionKind::SetValue => Some(("value", "value_chars")),
        _ => None,
    };
    if let Some((input_key, count_key)) = input {
        if let Some(text) = value.get(input_key).and_then(Value::as_str) {
            // Unicode scalar count, capped; no content or input length buffers.
            out[count_key] = text
                .chars()
                .take(MAX_NUMBER as usize + 1)
                .count()
                .min(MAX_NUMBER as usize)
                .into();
        }
    }
    let target_kind = if value.get("locator").is_some() {
        "locator"
    } else if value.get("element").is_some() {
        "element"
    } else if value.get("label").is_some() {
        "label"
    } else if value.get("rect").is_some() {
        "rect"
    } else if value.get("x").is_some() && value.get("y").is_some() {
        "point"
    } else {
        "none"
    };
    let mut target = json!({"kind": target_kind});
    if let Some(index) = value.get("element").and_then(bounded_uint) {
        target["index"] = index;
    }
    if let Some(rect) = value.get("rect").and_then(Value::as_object) {
        let mut clean = Map::new();
        for key in ["x", "y", "width", "height"] {
            if let Some(n) = rect
                .get(key)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= MAX_NUMBER as f64)
            {
                clean.insert(key.into(), json!(n));
            }
        }
        if clean.len() == 4 {
            target["rect"] = Value::Object(clean);
        }
    }
    out["target"] = target;
    out
}

fn redact_result(value: &Value) -> Value {
    let mut out = Map::new();
    for key in ["ok", "retry_safe"] {
        if let Some(flag) = value.get(key).and_then(Value::as_bool) {
            out.insert(key.into(), flag.into());
        }
    }
    if let Some(error) = value.get("error") {
        let code = match error.as_str() {
            Some("outcome_unknown") => "outcome_unknown",
            Some("not_sent") => "not_sent",
            Some("control_scope_changed") => "control_scope_changed",
            Some("stale_element_snapshot") => "stale_element_snapshot",
            Some("invalid_element_snapshot") => "invalid_element_snapshot",
            Some("element_not_found") => "element_not_found",
            Some("ambiguous_element_label") => "ambiguous_element_label",
            Some("unsupported_control") => "unsupported_control",
            Some("invalid_request") => "invalid_request",
            Some("text_partially_typed") => "text_partially_typed",
            _ => "unknown",
        };
        out.insert("error_code".into(), code.into());
    }
    for (key, allowed) in [
        (
            "outcome",
            &["applied", "not_sent", "no_effect", "partial", "unknown"][..],
        ),
        (
            "delivery",
            &[
                "not_sent",
                "delivered",
                "delivered_verified",
                "delivered_unverified",
                "unknown",
            ][..],
        ),
        ("verification", &["verified", "unverified", "unknown"][..]),
    ] {
        if let Some(raw) = value.get(key).and_then(Value::as_str) {
            let safe = allowed
                .iter()
                .copied()
                .find(|item| *item == raw)
                .unwrap_or("unknown");
            out.insert(key.into(), safe.into());
        }
    }
    // Flat receipt counts only. Never recursively search arbitrary result data.
    for key in [
        "completed",
        "applied_actions",
        "failed_step",
        "characters_confirmed",
        "characters_uncertain",
        "characters_total",
        "swipes",
        "swipes_confirmed",
        "swipes_uncertain",
        "native_mutations_confirmed",
        "native_mutations_uncertain",
    ] {
        if let Some(count) = value.get(key).and_then(bounded_uint) {
            out.insert(key.into(), count);
        }
    }
    if let Some(marker) = enum_metadata(
        &value["verification"],
        &[
            ("status", &["verified", "unverified"]),
            ("scope", &["element_value"]),
            ("reason", &["element_value", "secure_field"]),
            ("application_commit", &["not_verified"]),
        ],
    ) {
        out.insert("verification".into(), marker);
    }
    if let Some(evidence) = redact_evidence(&value["action_evidence"]) {
        out.insert("action_evidence".into(), evidence);
    }
    Value::Object(out)
}

/// Reconstruct one known metadata object; never copy unknown keys or strings.
/// Missing/malformed fields stay absent, unknown enum strings become `unknown`.
fn enum_metadata(value: &Value, fields: &[(&str, &[&str])]) -> Option<Value> {
    let object = value.as_object()?;
    let mut out = Map::new();
    for (key, allowed) in fields {
        if let Some(raw) = object.get(*key).and_then(Value::as_str) {
            let safe = allowed
                .iter()
                .copied()
                .find(|item| *item == raw)
                .unwrap_or("unknown");
            out.insert((*key).into(), safe.into());
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn redact_evidence(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut out = Map::new();
    for (key, fields) in [
        (
            "delivery",
            &[
                (
                    "status",
                    &[
                        "delivered",
                        "not_sent",
                        "partial",
                        "uncertain",
                        "unavailable",
                    ][..],
                ),
                (
                    "basis",
                    &[
                        "uncertain_action_step",
                        "partial_action_step",
                        "uncertain_delivery_receipt",
                        "partial_delivery_receipt",
                        "delivery_receipt",
                        "action_step_receipt",
                        "skipped",
                        "no_input_steps",
                        "outcome",
                        "unrecognized_outcome",
                        "batch_outcome",
                        "runner_acknowledgement",
                        "no_delivery_receipt",
                    ][..],
                ),
            ][..],
        ),
        (
            "postcondition",
            &[
                ("status", &["verified", "unverified", "unavailable"][..]),
                ("scope", &["element_value", "ui_expectation_only"][..]),
                (
                    "reason",
                    &["element_value", "secure_field", "readback_not_verified"][..],
                ),
                (
                    "source",
                    &[
                        "verification",
                        "steps[-1].verification",
                        "steps[-1].observation",
                    ][..],
                ),
                ("business_outcome", &["unverified"][..]),
            ][..],
        ),
        (
            "after_state",
            &[("status", &["available", "unavailable"][..])][..],
        ),
        (
            "alert",
            &[
                ("status", &["present", "unavailable"][..]),
                ("source", &["alert"][..]),
            ][..],
        ),
    ] {
        if let Some(clean) = object
            .get(key)
            .and_then(|block| enum_metadata(block, fields))
        {
            out.insert(key.into(), clean);
        }
    }
    if let Some(sources) = value["after_state"]["sources"].as_array() {
        // Iterate the vocabulary, not caller keys: bounded, unique, static output.
        let clean: Vec<&str> = ["elements", "delta", "observation", "steps[-1].observation"]
            .into_iter()
            .filter(|source| sources.iter().any(|v| v.as_str() == Some(*source)))
            .collect();
        out.entry("after_state").or_insert_with(|| json!({}))["sources"] = json!(clean);
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(store: &mut TraceStore) {
        store.record(
            "owner",
            &json!({"type":"home"}),
            &json!({"ok":true}),
            12,
            false,
        );
    }

    #[test]
    fn shortcuts_are_recognized_without_retaining_arbitrary_names() {
        assert_eq!(
            redact_action(&json!({"type":"shortcut","name":"SECRET"}))["kind"],
            "shortcut"
        );
        assert!(!redact_action(&json!({"type":"shortcut","name":"SECRET"}))
            .to_string()
            .contains("SECRET"));
    }

    #[test]
    fn opt_in_idempotence_and_owner_isolation() {
        let mut s = TraceStore::default();
        record(&mut s);
        assert!(s.export("owner", "run").is_none());
        assert!(s.start("owner", "run"));
        record(&mut s);
        assert!(s.start("owner", "run"));
        assert!(!s.start("owner", "other"));
        assert!(s.export("intruder", "run").is_none());
        assert!(s.export_jsonl("intruder", "run").is_none());
        s.end("intruder", "run");
        s.end("owner", "other");
        assert_eq!(s.export("owner", "run").unwrap()["active"], true);
        assert!(s.start("intruder", "run"));
        assert_eq!(s.export("intruder", "run").unwrap()["events"], json!([]));
        assert_eq!(
            s.export("owner", "run").unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn private_and_ended_scopes_leave_no_trace() {
        let mut s = TraceStore::default();
        s.start("owner", "run");
        s.record(
            "owner",
            &json!({"type":"text","text":"SECRET"}),
            &json!({}),
            1,
            true,
        );
        s.record("unknown", &json!({}), &json!({}), 1, false);
        assert_eq!(s.export("owner", "run").unwrap()["last_sequence"], 0);
        record(&mut s);
        s.end("owner", "run");
        assert!(!s.can_start("owner", "run"));
        assert!(!s.start("owner", "run"));
        record(&mut s);
        assert_eq!(s.export("owner", "run").unwrap()["last_sequence"], 1);
        assert_eq!(s.export("owner", "run").unwrap()["active"], false);
        assert!(s.start("owner", "next"));
    }

    #[test]
    fn slow_request_from_a_cannot_enter_b() {
        let mut s = TraceStore::default();
        assert!(s.active_handle("owner").is_none());
        assert!(s.start("owner", "A"));
        let a = s.active_handle("owner").unwrap();
        assert!(s.start("owner", "A"));
        assert_eq!(s.active_handle("owner"), Some(a.clone()));
        assert!(!s.can_start("owner", "B"));
        s.end("owner", "A");
        assert!(s.active_handle("owner").is_none());
        assert!(s.start("owner", "B"));
        let b = s.active_handle("owner").unwrap();
        assert!(b.generation > a.generation);
        s.record_for(
            "owner",
            &a,
            &json!({"type":"text","text":"SECRET_A"}),
            &json!({"ok":true}),
            1,
            false,
        );
        let before = s.export("owner", "B").unwrap();
        assert_eq!(before["last_sequence"], 0);
        assert_eq!(before["dropped"], 0);
        assert_eq!(s.export("owner", "A").unwrap()["last_sequence"], 0);
        s.record_for(
            "owner",
            &b,
            &json!({"type":"home"}),
            &json!({"ok":true}),
            2,
            false,
        );
        // Compatibility records still delegate to the current incarnation.
        record(&mut s);
        assert_eq!(s.export("owner", "B").unwrap()["last_sequence"], 2);
    }

    #[test]
    fn evicted_run_id_reuse_gets_a_new_generation() {
        let mut s = TraceStore::default();
        assert!(s.start("owner", "A"));
        let original = s.active_handle("owner").unwrap();
        s.end("owner", "A");
        assert!(!s.start("owner", "A"));
        for n in 0..MAX_RUNS - 1 {
            let owner = format!("other{n}");
            assert!(s.start(&owner, "r"));
            s.end(&owner, "r");
        }
        assert!(s.start("evictor", "r"));
        assert!(s.export("owner", "A").is_none());
        assert!(s.can_start("owner", "A"));
        assert!(s.start("owner", "A"));
        let reused = s.active_handle("owner").unwrap();
        assert_eq!(original.run_id, reused.run_id);
        assert!(reused.generation > original.generation);
        s.record_for(
            "owner",
            &original,
            &json!({"type":"tap"}),
            &json!({"ok":false}),
            1,
            false,
        );
        assert_eq!(s.export("owner", "A").unwrap()["last_sequence"], 0);
        s.record_for(
            "owner",
            &reused,
            &json!({"type":"home"}),
            &json!({"ok":true}),
            2,
            false,
        );
        assert_eq!(s.export("owner", "A").unwrap()["last_sequence"], 1);
    }

    #[test]
    fn handle_checks_owner_privacy_and_ended_state() {
        let mut s = TraceStore::default();
        s.start("owner", "r");
        s.start("other", "r");
        let handle = s.active_handle("owner").unwrap();
        let other = s.active_handle("other").unwrap();
        let action = json!({"type":"home"});
        let result = json!({"ok":true});
        s.record_for("other", &handle, &action, &result, 1, false);
        s.record_for("owner", &other, &action, &result, 1, false);
        s.record_for("owner", &handle, &action, &result, 1, true);
        s.end("owner", "r");
        s.record_for("owner", &handle, &action, &result, 1, false);
        for owner in ["owner", "other"] {
            assert_eq!(s.export(owner, "r").unwrap()["last_sequence"], 0);
            assert_eq!(s.export(owner, "r").unwrap()["dropped"], 0);
        }
    }

    #[test]
    fn preflight_is_read_only_and_agrees_with_start_at_capacity() {
        let mut s = TraceStore::default();
        for n in 0..MAX_RUNS {
            assert!(s.start(&format!("o{n}"), "r"));
        }
        let generation = s.last_generation;
        assert!(s.can_start("o0", "r"));
        assert!(!s.can_start("o0", "other"));
        assert!(!s.can_start("extra", "r"));
        assert!(!s.can_start("", "r"));
        s.end("o0", "r");
        assert!(!s.can_start("o0", "r"));
        assert!(s.can_start("extra", "r"));
        assert!(s.export("o0", "r").is_some());
        assert_eq!(s.last_generation, generation);
        assert_eq!(s.runs.len(), MAX_RUNS);
        assert!(s.start("extra", "r"));
        assert!(s.export("o0", "r").is_none());
    }

    #[test]
    fn generation_exhaustion_never_wraps_or_evicts() {
        let mut s = TraceStore::default();
        s.last_generation = u64::MAX - 1;
        assert!(s.start("owner", "last"));
        assert_eq!(s.active_handle("owner").unwrap().generation, u64::MAX);
        assert!(s.can_start("owner", "last"));
        assert!(s.start("owner", "last"));
        s.end("owner", "last");
        assert!(!s.can_start("next", "r"));
        assert!(!s.start("next", "r"));
        assert!(s.export("owner", "last").is_some());
        assert_eq!(s.last_generation, u64::MAX);
    }

    #[test]
    fn actual_action_names_and_batch_steps_keep_only_safe_metadata() {
        for (action, expected) in [
            (
                json!({"type":"launch_app","bundle":"SECRET_BUNDLE","app":"SECRET_APP","url":"SECRET_URL"}),
                "launch_app",
            ),
            (
                json!({"type":"terminate_app","bundle":"SECRET_BUNDLE"}),
                "terminate_app",
            ),
            (
                json!({"kind":"wait_for","expect":{"present":[{"label":"SECRET_LABEL"}]}}),
                "wait_for",
            ),
            (json!({"kind":"pause","ms":123}), "pause"),
            (
                json!({"type":"batch","steps":[{"text":"SECRET_TEXT"}]}),
                "batch",
            ),
            (
                json!({"steps":[{"kind":"action","action":{"type":"text","text":"SECRET_TEXT"}}]}),
                "batch",
            ),
        ] {
            let clean = redact_action(&action);
            assert_eq!(clean, json!({"kind":expected,"target":{"kind":"none"}}));
            assert!(!clean.to_string().contains("SECRET"));
        }
        assert_eq!(
            redact_action(
                &json!({"type":"set_value","element":3,"snapshot":"SECRET_SNAPSHOT","value":"秘密🙂","text":"SECRET_TEXT"})
            ),
            json!({"kind":"set_value","value_chars":3,"target":{"kind":"element","index":3}})
        );
        assert!(
            redact_action(&json!({"type":"set_value","value":{"text":"SECRET"}}))
                .get("value_chars")
                .is_none()
        );
        assert_eq!(
            redact_action(&json!({"type":"SECRET_TYPE","kind":"pause"})),
            json!({"kind":"unknown"})
        );
        assert_eq!(
            redact_action(&json!({"type":false,"kind":"wait_for"})),
            json!({"kind":"unknown"})
        );
        assert_eq!(
            redact_action(&json!({"kind":"SECRET_KIND"})),
            json!({"kind":"unknown"})
        );
        let large = json!({"type":"set_value","value":"界".repeat(MAX_NUMBER as usize + 10)});
        assert_eq!(redact_action(&large)["value_chars"], MAX_NUMBER);
    }

    #[test]
    fn event_bound_sequences_and_drop_counts() {
        let mut s = TraceStore::default();
        s.start("owner", "run");
        for _ in 0..MAX_EVENTS + 3 {
            record(&mut s);
        }
        s.record("owner", &Value::Null, &Value::Null, 0, true);
        let export = s.export("owner", "run").unwrap();
        assert_eq!(export["events"].as_array().unwrap().len(), MAX_EVENTS);
        assert_eq!(export["events"][0]["seq"], 1);
        assert_eq!(export["events"][MAX_EVENTS - 1]["seq"], MAX_EVENTS);
        assert_eq!(export["last_sequence"], MAX_EVENTS + 3);
        assert_eq!(export["dropped"], 3);
        assert_eq!(export["truncated"], true);
    }

    #[test]
    fn run_bound_preserves_active_and_evicts_oldest_ended() {
        let mut s = TraceStore::default();
        for n in 0..MAX_RUNS {
            assert!(s.start(&format!("o{n}"), "r"));
        }
        assert!(!s.start("extra", "r"));
        s.end("o3", "r");
        s.end("o5", "r");
        assert!(s.start("extra", "r"));
        assert_eq!(s.runs.len(), MAX_RUNS);
        assert!(s.export("o3", "r").is_none());
        assert!(s.export("o5", "r").is_some());
        assert!(s.export("o0", "r").is_some());
        assert!(s.start("extra2", "r"));
        assert!(s.export("o5", "r").is_none());
    }

    #[test]
    fn ids_and_numeric_metadata_are_bounded() {
        let mut s = TraceStore::default();
        for bad in ["".to_owned(), "x".repeat(129), "x\ny".into()] {
            assert!(!s.start(&bad, "r"));
            assert!(!s.start("o", &bad));
        }
        assert!(s.start(&"x".repeat(128), "r"));
        assert_eq!(
            redact_action(&json!({"type":"tap","element":1000001}))["target"].get("index"),
            None
        );
        assert_eq!(
            redact_action(&json!({"type":"tap","element":-1}))["target"].get("index"),
            None
        );
        assert_eq!(
            redact_result(&json!({"swipes":1000001,"completed":"1","applied_actions":-1})),
            json!({})
        );
        for bad in [json!(-1), json!(1000001), json!("123"), Value::Null] {
            let action = json!({"type":"tap","rect":{"x":bad,"y":0,"width":1,"height":1}});
            assert!(redact_action(&action)["target"].get("rect").is_none());
        }
        let action = json!({"type":"tap","element":7,"rect":{"x":1,"y":2,"width":3,"height":4,"label":"SECRET"}});
        assert_eq!(
            redact_action(&action)["target"],
            json!({"kind":"element","index":7,"rect":{"x":1.0,"y":2.0,"width":3.0,"height":4.0}})
        );
    }

    #[test]
    fn secret_canaries_never_survive_json_or_jsonl() {
        let mut s = TraceStore::default();
        s.start("owner", "run");
        let action = json!({"type":"text","text":"秘密🙂","label":"SECRET_LABEL","identifier":"SECRET_ID","value":"SECRET_VALUE","url":"SECRET_URL","location":"SECRET_LOCATION","snapshot":"SECRET_SNAPSHOT","screenshot":"SECRET_IMAGE","locator":{"label":"SECRET_LOCATOR","kind":"SECRET_KIND"}});
        let result = json!({"ok":false,"error":"SECRET_ERROR","hint":"SECRET_HINT","text":"SECRET_TEXT","input":"SECRET_INPUT","remaining_text":"SECRET_REMAINING","snapshot":{"ok":"SECRET_NESTED"},"delivery":"SECRET_DELIVERY","verification":"SECRET_VERIFICATION","retry_safe":false,"characters_total":3,"counts":{"SECRET_KEY":7}});
        s.record("owner", &action, &result, 15, false);
        let exported = s.export("owner", "run").unwrap();
        let event = &exported["events"][0];
        assert_eq!(
            event["action"],
            json!({"kind":"text","text_chars":3,"target":{"kind":"locator"}})
        );
        assert_eq!(
            event["result"],
            json!({"ok":false,"error_code":"unknown","delivery":"unknown","verification":"unknown","retry_safe":false,"characters_total":3})
        );
        let jsonl = s.export_jsonl("owner", "run").unwrap();
        assert!(!jsonl.contains("SECRET"));
        assert!(!jsonl.contains("秘密"));
        let lines: Vec<Value> = jsonl
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].get("events").is_none());
        assert_eq!(&lines[1], event);
        assert!(jsonl.ends_with('\n'));
    }

    #[test]
    fn closed_vocabularies_and_known_receipts_only() {
        assert_eq!(
            redact_action(&json!({"type":"SECRET","text":"SECRET","element":1})),
            json!({"kind":"unknown"})
        );
        assert_eq!(redact_action(&Value::Null), json!({"kind":"unknown"}));
        assert_eq!(
            redact_result(
                &json!({"error":{"code":"SECRET"},"ok":"SECRET","verification":{ "text":"SECRET" },"arbitrary_count":3})
            ),
            json!({"error_code":"unknown"})
        );
        assert_eq!(
            redact_result(
                &json!({"ok":true,"error":"stale_element_snapshot","outcome":"not_sent","delivery":"delivered_unverified","verification":"unverified","retry_safe":true,"swipes":2,"swipes_confirmed":1,"swipes_uncertain":1})
            ),
            json!({"ok":true,"error_code":"stale_element_snapshot","outcome":"not_sent","delivery":"delivered_unverified","verification":"unverified","retry_safe":true,"swipes":2,"swipes_confirmed":1,"swipes_uncertain":1})
        );
        assert_eq!(redact_result(&Value::Null), json!({}));
    }

    #[test]
    fn server_evidence_and_element_verification_metadata_survive() {
        let evidence = json!({
            "delivery":{"status":"delivered","basis":"runner_acknowledgement"},
            "postcondition":{"status":"verified","scope":"element_value","reason":"element_value","source":"verification","business_outcome":"unverified"},
            "after_state":{"status":"available","sources":["elements","delta","observation","steps[-1].observation"]},
            "alert":{"status":"present","source":"alert"}
        });
        let verification = json!({"status":"verified","scope":"element_value","reason":"element_value","application_commit":"not_verified"});
        let result = json!({"ok":true,"action_evidence":evidence,"verification":verification,"native_mutations_confirmed":2,"native_mutations_uncertain":0});
        assert_eq!(redact_result(&result), result);
        let secure = json!({"verification":{"status":"unverified","scope":"element_value","reason":"secure_field","application_commit":"not_verified"}});
        assert_eq!(redact_result(&secure), secure);
        let terminal_wait = json!({"action_evidence":{"postcondition":{"status":"verified","scope":"ui_expectation_only","source":"steps[-1].observation","business_outcome":"unverified"}}});
        assert_eq!(redact_result(&terminal_wait), terminal_wait);
    }

    #[test]
    fn nested_evidence_is_reconstructed_not_copied() {
        let result = json!({
            "verification":{"status":"SECRET_STATUS","scope":"SECRET_SCOPE","reason":"SECRET_REASON","application_commit":"SECRET_COMMIT","actual":"SECRET_INPUT","expected":"SECRET_EXPECTED","text":"SECRET_TEXT"},
            "action_evidence":{
                "delivery":{"status":"SECRET_DELIVERY","basis":"SECRET_BASIS","hint":"SECRET_HINT"},
                "postcondition":{"status":"verified","scope":"element_value","reason":"SECRET_REASON","source":"SECRET_SOURCE","business_outcome":"verified","value":"SECRET_VALUE"},
                "after_state":{"status":"available","sources":["SECRET_SOURCE","elements","elements",{"label":"SECRET_LABEL"},["SECRET_NESTED"]],"snapshot":"SECRET_SNAPSHOT"},
                "alert":{"status":"present","source":"alert","text":"SECRET_ALERT"},
                "screenshot":"SECRET_IMAGE"
            },
            "recovery":{"action":{"next_step":"SECRET_RECOVERY"}}
        });
        let mut s = TraceStore::default();
        s.start("owner", "run");
        s.record("owner", &json!({"type":"tap"}), &result, 1, false);
        let export = s.export("owner", "run").unwrap();
        let clean = &export["events"][0]["result"];
        assert_eq!(
            clean["verification"],
            json!({"status":"unknown","scope":"unknown","reason":"unknown","application_commit":"unknown"})
        );
        assert_eq!(
            clean["action_evidence"]["delivery"],
            json!({"status":"unknown","basis":"unknown"})
        );
        assert_eq!(
            clean["action_evidence"]["postcondition"]["business_outcome"],
            "unknown"
        );
        assert_eq!(
            clean["action_evidence"]["after_state"],
            json!({"status":"available","sources":["elements"]})
        );
        assert_eq!(
            clean["action_evidence"]["alert"],
            json!({"status":"present","source":"alert"})
        );
        assert!(!export.to_string().contains("SECRET"));
        assert!(!s.export_jsonl("owner", "run").unwrap().contains("SECRET"));
    }

    #[test]
    fn malformed_metadata_and_source_arrays_remain_bounded() {
        for bad in [
            Value::Null,
            json!(false),
            json!(1),
            json!([]),
            json!("SECRET"),
        ] {
            assert!(redact_evidence(&bad).is_none());
            // A legacy string verification is bucketed, other malformed types omitted.
            let clean = redact_result(&json!({"action_evidence":bad,"verification":bad}));
            assert!(clean.get("action_evidence").is_none());
            assert!(!clean.to_string().contains("SECRET"));
        }
        assert_eq!(
            redact_result(
                &json!({"verification":{"status":[],"reason":{},"scope":false},"action_evidence":{"delivery":{"status":{}},"postcondition":[],"alert":true,"after_state":{"status":1,"sources":"SECRET"}}})
            ),
            json!({})
        );
        let mut sources = vec![json!("elements"); 1000];
        sources.push(json!("SECRET"));
        sources.push(json!("observation"));
        assert_eq!(
            redact_evidence(&json!({"after_state":{"sources":sources}})).unwrap(),
            json!({"after_state":{"sources":["elements","observation"]}})
        );
    }
}
