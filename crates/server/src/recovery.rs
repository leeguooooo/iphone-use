//! Diagnostic recovery never changes the original action's fate or authorizes replay.
use serde_json::Value;

pub fn advice(body: &Value) -> Option<&'static str> {
    if body.get("ok") != Some(&Value::Bool(false)) {
        return None;
    }
    let code = body.get("error")?.as_str()?;
    match code {
        "element_not_found" | "ambiguous_element" | "ambiguous_element_label" | "stale_element_snapshot" | "invalid_element_target" =>
            Some("Inspect the fresh image for the current page, target and overlays; choose a fresh visible target before another action. A missing tree match does not prove absence."),
        "element_occluded" | "element_not_visible" =>
            Some("Inspect the fresh image for an overlay or an off-screen target. Handle a visible overlay or select a suitable scroll region before acting; old coordinates are not proof the target is reachable."),
        "wait_for_timeout" | "postcondition_failed" =>
            Some("Inspect the actual page and result before planning remaining work. A failed expectation does not undo earlier actions; never replay a submission or the whole batch automatically."),
        "no_scroll_progress" | "scroll_no_progress" =>
            Some("Inspect the current list, region, overlays and end marker before deciding whether to scroll again. No progress does not prove an empty or complete list."),
        "outcome_unknown" | "text_partially_typed" => {
            let message=body.pointer("/runner_error/message").or_else(||body.pointer("/detail/runner_error/message"))?.as_str()?;
            message.contains("input target changed").then_some("Input may have reached a different field. Inspect the original and current fields and enter only confirmed missing work; never resend the full input or resume an unknown chunk.")
        }
        _=>None,
    }
}

/// A list can retain useful partial rows even when it cannot establish coverage.
/// Keep its success/found/coverage verdict; only attach diagnostic observations.
pub fn list_advice(body: &Value) -> Option<&'static str> {
    if body.get("found") == Some(&Value::Bool(true)) {
        return None;
    }
    if let Some(advice) = advice(body) {
        return Some(advice);
    }
    if body.get("error").and_then(Value::as_str) == Some("runner_source_failed") {
        return Some("The list tree could not be read. Inspect the current screen and status before choosing another read or action; no failed swipe or whole collection is automatically replayed.");
    }
    if body.get("complete") == Some(&Value::Bool(true))
        || !body.get("rows").is_some_and(Value::is_array)
    {
        return None;
    }
    match body.get("stop_reason").and_then(Value::as_str) {
        Some("no_rows"|"no_progress"|"duplicate_page"|"read_failed"|"swipe_failed"|"deadline")=>
            Some("The collected rows are partial and the end is not proven. Inspect the current region, overlays and end marker before planning more work. Keep existing rows and never replay an uncertain swipe or the whole collection."),
        _=>None,
    }
}
