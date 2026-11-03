//! Additive action receipts. No reads, probes, retries, or business-success inference.
//!
//! Call `enrich` on mutation responses after all receipts and privacy filtering.
//! `action_evidence` describes delivery separately from the observed UI postcondition;
//! `recovery.action` is advice, never authorization to replay. Existing fields win.
use serde_json::{json, Value};

/// Augment an action response without changing any existing field.
///
/// A final successful observed `wait_for`, or the server's explicit
/// `verification` receipt for element-value readback, verifies only that scope.
/// A snapshot, native ACK, alert, or `ok:true` never proves an
/// app/business outcome. Non-object bodies are left alone. Repeated calls are
/// idempotent; call once after the response is final, not before adding receipts.
pub fn enrich(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    let (delivery, basis) = delivery(body);
    let omitted = body["observation_omitted"] == true;
    let mut sources: Vec<&str> = ["elements", "delta", "observation"]
        .into_iter()
        .filter(|key| !omitted && available_observation(&body[*key]))
        .collect();
    let verified_source = if omitted { None } else { verified_wait(body) };
    if let Some(source) = verified_source {
        sources.push(source);
    }
    let marker = element_value_marker(body).or_else(|| {
        let last = body.get("steps")?.as_array()?.last()?;
        (body["ok"] == true && last["kind"] == "action")
            .then(|| element_value_marker(last))
            .flatten()
            .map(|(status, reason, _)| (status, reason, "steps[-1].verification"))
    });
    let postcondition = if !omitted {
        marker.map(|(status, reason, source)| {
            json!({
                "status":status,"scope":"element_value","reason":reason,
                "source":source,"business_outcome":"unverified"
            })
        })
    } else {
        None
    };
    let evidence = json!({
        "delivery": {"status": delivery, "basis": basis},
        "postcondition": postcondition.unwrap_or_else(|| json!({
            "status": if verified_source.is_some() { "verified" }
                else if omitted || (sources.is_empty() && body.get("steps").is_none()) { "unavailable" }
                else { "unverified" },
            "scope": "ui_expectation_only",
            "source": verified_source,
            "business_outcome": "unverified"
        })),
        "after_state": {
            "status": if sources.is_empty() { "unavailable" } else { "available" },
            "sources": sources
        },
        "alert": {
            "status": if omitted { "unavailable" }
                else if body.get("alert").is_some_and(Value::is_object) { "present" }
                else { "unavailable" },
            "source": if !omitted && body.get("alert").is_some_and(Value::is_object) {
                json!("alert")
            } else { Value::Null }
        }
    });
    let recovery = recovery(body, delivery);
    let map = body.as_object_mut().expect("object checked above");
    map.entry("action_evidence").or_insert(evidence);
    if let Some(recovery) = recovery {
        let existing = map.entry("recovery").or_insert_with(|| json!({}));
        if let Some(existing) = existing.as_object_mut() {
            existing.entry("action").or_insert(recovery);
        }
    }
}

// This marker is produced by the server after live readback, never by runner
// ACK or request parameters. Copy only known metadata; no actual/expected value.
fn element_value_marker(body: &Value) -> Option<(&'static str, &'static str, &'static str)> {
    if body["ok"] != true || skipped(body) || body["observation_omitted"] == true {
        return None;
    }
    let marker = &body["verification"];
    if marker["scope"] != "element_value" || marker["application_commit"] != "not_verified" {
        return None;
    }
    match (marker["status"].as_str(), marker["reason"].as_str()) {
        (Some("verified"), Some("element_value")) => {
            Some(("verified", "element_value", "verification"))
        }
        (Some("unverified"), Some("secure_field")) => {
            Some(("unverified", "secure_field", "verification"))
        }
        (Some("unverified"), _) => Some(("unverified", "readback_not_verified", "verification")),
        _ => None,
    }
}

fn skipped(body: &Value) -> bool {
    body["skipped"] == true
        || body["skipped"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
}

fn available_observation(value: &Value) -> bool {
    (value.is_object()
        && !value.as_object().is_some_and(|v| v.is_empty())
        && value["read"] != false
        && value["stale"] != true
        && value.get("read_error").is_none())
        || value.is_array()
}

/// Only a terminal wait qualifies: a later input could invalidate an earlier
/// matched expectation. Its receipt is server-generated, not a supplied `expect`.
fn verified_wait(body: &Value) -> Option<&'static str> {
    if body["ok"] != true {
        return None;
    }
    let last = body.get("steps")?.as_array()?.last()?;
    (last["kind"] == "wait_for"
        && last["ok"] == true
        && last["observation"]["read"] == true
        && available_observation(&last["observation"]))
    .then_some("steps[-1].observation")
}

fn positive(body: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| body[*key].as_u64().is_some_and(|n| n > 0))
}

fn delivery(body: &Value) -> (&'static str, &'static str) {
    let mut delivered_step = false;
    if let Some(steps) = body.get("steps").and_then(Value::as_array) {
        for step in steps.iter().filter(|step| step["kind"] == "action") {
            match delivery(step).0 {
                "uncertain" => return ("uncertain", "uncertain_action_step"),
                "partial" => return ("partial", "partial_action_step"),
                "delivered" => delivered_step = true,
                _ => {}
            }
        }
    }
    // Aggregate evidence takes precedence over a failed step's `not_sent`.
    if positive(
        body,
        &[
            "characters_uncertain",
            "swipes_uncertain",
            "native_mutations_uncertain",
        ],
    ) || body["outcome"] == "unknown"
        || body["batch_outcome"] == "unknown"
        || body["swipe_outcome"] == "unknown"
        || body["system_capture"]["requested"] == true && body["ok"] != true
        || body["app_lifecycle"]["launch_attempted"] == true && body["ok"] != true
    {
        return ("uncertain", "uncertain_delivery_receipt");
    }
    if body["outcome"] == "partial" || body["batch_outcome"] == "partially_applied" {
        return ("partial", "partial_delivery_receipt");
    }
    if positive(
        body,
        &[
            "applied_actions",
            "characters_confirmed",
            "swipes_confirmed",
            "swipes",
            "native_mutations_confirmed",
        ],
    ) || body["detail"]["clear_applied"] == true
        || body["app_lifecycle"]["terminate_acknowledged"] == true
    {
        return ("delivered", "delivery_receipt");
    }
    // Successful action step receipts are ACKs even if the batch later failed.
    if delivered_step {
        return ("delivered", "action_step_receipt");
    }
    if skipped(body) {
        return ("not_sent", "skipped");
    }
    if body
        .get("steps")
        .and_then(Value::as_array)
        .is_some_and(|steps| {
            !steps.is_empty()
                && steps.iter().all(|step| {
                    matches!(step["kind"].as_str(), Some("wait_for" | "pause")) || skipped(step)
                })
        })
        && body["applied_actions"]
            .as_u64()
            .is_none_or(|count| count == 0)
    {
        return ("not_sent", "no_input_steps");
    }
    match body["outcome"].as_str() {
        Some("applied" | "no_effect") => ("delivered", "outcome"),
        Some("not_sent") => ("not_sent", "outcome"),
        Some(_) => ("uncertain", "unrecognized_outcome"),
        None if body["batch_outcome"] == "nothing_applied" => ("not_sent", "batch_outcome"),
        None if body["ok"] == true
            && (body["transport"] == "runner" || body["kind"] == "action") =>
        {
            ("delivered", "runner_acknowledgement")
        }
        _ => ("unavailable", "no_delivery_receipt"),
    }
}

fn recovery(body: &Value, delivery: &str) -> Option<Value> {
    let code = body.get("error").and_then(Value::as_str);
    if code.is_none() && body["ok"] != false {
        return None;
    }
    let code = code.unwrap_or("unspecified_error");
    // Explicit retry_safe alone cannot override positive/uncertain receipts.
    let may_retry = body["retry_safe"] == true
        && delivery == "not_sent"
        && body["recovery"]["replay_action"] != false;
    let next_step = match code {
        "screen_private"
        | "screen_scope_changed"
        | "control_scope_changed"
        | "list_scope_changed"
        | "input_owner_required" => "coordinate_with_owner",
        "stale_element_snapshot"
        | "invalid_element_snapshot"
        | "element_not_found"
        | "invalid_element_target" => "refresh_elements_and_reselect",
        "ambiguous_element_label" | "ambiguous_element" => "disambiguate_current_target",
        "scroll_blocked_by_alert" | "system_alert_present" | "alert_present" | "alert_blocking" => {
            "review_alert"
        }
        "no_alert" => "inspect_current_state",
        "element_disabled" | "element_not_actionable" => "wait_until_actionable",
        "runner_rejected" if body["runner_error"]["code"] == "invalid element state" => {
            "wait_until_actionable"
        }
        "unsupported_control" | "unsupported_perform_action" | "force_press_unsupported" => {
            "choose_supported_action"
        }
        "invalid_value"
        | "invalid_request"
        | "invalid_control_message"
        | "invalid_text_request"
        | "runner_rejected" => "correct_request",
        "device_unavailable"
        | "runner_not_configured"
        | "runner_pre_dispatch_failed"
        | "device_release_in_progress"
        | "device_transition_in_progress" => "check_device_status",
        _ => "inspect_current_state",
    };
    Some(json!({
        "code": code,
        "retry_allowed": may_retry,
        "replay_action": false,
        "next_step": next_step,
        "after_prerequisite": if may_retry { "reassess_before_new_action" } else { "do_not_replay" },
        "reason": if delivery != "not_sent" { "delivery_not_proven_absent" }
            else if !may_retry { "retry_not_authorized" } else { "not_sent_and_retry_safe" },
        "use_existing_after_state": body["observation_omitted"] != true
            && ["elements", "delta", "observation"].iter().any(|key| available_observation(&body[*key]))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enriched(mut body: Value) -> Value {
        let original = body.clone();
        enrich(&mut body);
        for (key, value) in original.as_object().unwrap() {
            // recovery can gain a nested additive entry.
            if key != "recovery" {
                assert_eq!(&body[key], value, "changed {key}");
            }
        }
        let once = body.clone();
        enrich(&mut body);
        assert_eq!(body, once);
        body
    }

    #[test]
    fn acknowledgements_and_screen_changes_never_verify() {
        for extra in [
            json!({}),
            json!({"snapshot":"s", "elements":[]}),
            json!({"delta":{"added":[1]}, "no_visible_change":false}),
            json!({"verified":true, "expect":{"present":["Done"]}}),
        ] {
            let mut body = json!({"ok":true,"transport":"runner"});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let body = enriched(body);
            assert_eq!(body["action_evidence"]["delivery"]["status"], "delivered");
            assert_ne!(
                body["action_evidence"]["postcondition"]["status"],
                "verified"
            );
        }
    }

    #[test]
    fn only_terminal_observed_wait_verifies_its_ui_expectation() {
        let wait = json!({"kind":"wait_for","ok":true,"observation":{"read":true,"rows":2}});
        let good = enriched(json!({"ok":true,"applied_actions":1,"steps":[wait.clone()]}));
        assert_eq!(
            good["action_evidence"]["postcondition"]["status"],
            "verified"
        );
        assert_eq!(
            good["action_evidence"]["postcondition"]["business_outcome"],
            "unverified"
        );
        for body in [
            json!({"ok":true,"steps":[wait.clone(),{"kind":"action","ok":true}]}),
            json!({"ok":false,"steps":[wait.clone()],"error":"timeout"}),
            json!({"ok":true,"observation_omitted":true,"steps":[wait]}),
            json!({"ok":true,"steps":[{"kind":"wait_for","ok":true}]}),
            json!({"ok":true,"steps":[{"kind":"wait_for","ok":true,"observation":{"read":false}}]}),
            json!({"ok":true,"steps":[{"kind":"wait_for","ok":true,"observation":{"read":true,"stale":true}}]}),
        ] {
            assert_ne!(
                enriched(body)["action_evidence"]["postcondition"]["status"],
                "verified"
            );
        }
    }

    #[test]
    fn retry_requires_explicit_not_sent_and_retry_safe() {
        for outcome in [
            "not_sent",
            "applied",
            "partial",
            "unknown",
            "no_effect",
            "future_value",
        ] {
            for retry in [true, false] {
                let body = enriched(json!({"ok":false,"error":"stale_element_snapshot",
                    "outcome":outcome,"retry_safe":retry}));
                assert_eq!(
                    body["recovery"]["action"]["retry_allowed"],
                    outcome == "not_sent" && retry
                );
                assert_eq!(body["recovery"]["action"]["replay_action"], false);
            }
        }
        for body in [
            json!({"ok":false,"retry_safe":true}),
            json!({"ok":false,"outcome":"not_sent"}),
            json!({"ok":false,"outcome":"not_sent","retry_safe":"true"}),
        ] {
            assert_eq!(enriched(body)["recovery"]["action"]["retry_allowed"], false);
        }
    }

    #[test]
    fn aggregate_receipts_block_retry_even_with_not_sent_failed_step() {
        for receipt in [
            json!({"applied_actions":1}),
            json!({"batch_outcome":"unknown"}),
            json!({"batch_outcome":"partially_applied"}),
            json!({"characters_confirmed":1}),
            json!({"characters_uncertain":1}),
            json!({"swipes_confirmed":1}),
            json!({"swipes_uncertain":1}),
            json!({"native_mutations_uncertain":1}),
            json!({"detail":{"clear_applied":true}}),
            json!({"app_lifecycle":{"terminate_acknowledged":true}}),
            json!({"app_lifecycle":{"launch_attempted":true}}),
            json!({"steps":[{"kind":"action","ok":true}]}),
        ] {
            let mut body = json!({"ok":false,"error":"runner_pre_dispatch_failed", "outcome":"not_sent","retry_safe":true});
            body.as_object_mut()
                .unwrap()
                .extend(receipt.as_object().unwrap().clone());
            assert_eq!(
                enriched(body)["recovery"]["action"]["retry_allowed"],
                false,
                "{receipt}"
            );
        }
    }

    #[test]
    fn reports_existing_after_state_and_alert_without_copying_ui_data() {
        let body = enriched(
            json!({"ok":false,"error":"scroll_blocked_by_alert","outcome":"not_sent",
            "retry_safe":true,"elements":[],"alert":{"text":"Private","buttons":["Allow"]}}),
        );
        assert_eq!(
            body["action_evidence"]["after_state"]["sources"],
            json!(["elements"])
        );
        assert_eq!(body["action_evidence"]["alert"]["status"], "present");
        assert_eq!(body["recovery"]["action"]["next_step"], "review_alert");
        assert!(!body["action_evidence"].to_string().contains("Private"));
        let hidden =
            enriched(json!({"ok":true,"observation_omitted":true,"elements":[],"alert":{}}));
        assert_eq!(
            hidden["action_evidence"]["after_state"]["status"],
            "unavailable"
        );
        assert_eq!(hidden["action_evidence"]["alert"]["status"], "unavailable");
    }

    #[test]
    fn preserves_existing_extensions_and_handles_nonobjects() {
        let body = enriched(
            json!({"ok":false,"error":"unknown_error","action_evidence":{"custom":1},
            "recovery":{"visual_check_required":true,"replay_action":false}}),
        );
        assert_eq!(body["action_evidence"], json!({"custom":1}));
        assert_eq!(body["recovery"]["visual_check_required"], true);
        assert_eq!(body["recovery"]["replay_action"], false);
        let body = json!({"ok":false,"recovery":{"action":{"custom":1}}});
        assert_eq!(enriched(body)["recovery"]["action"], json!({"custom":1}));
        for mut value in [Value::Null, json!([]), json!(true), json!(3), json!("text")] {
            let original = value.clone();
            enrich(&mut value);
            assert_eq!(value, original);
        }
    }

    #[test]
    fn nested_step_receipts_override_failed_step_not_sent() {
        for step in [
            json!({"kind":"action","ok":false,"outcome":"applied"}),
            json!({"kind":"action","ok":false,"outcome":"unknown"}),
            json!({"kind":"action","ok":false,"characters_uncertain":2}),
            json!({"kind":"action","ok":false,"outcome":"partial"}),
        ] {
            let body = enriched(json!({"ok":false,"error":"stale_element_snapshot",
                "outcome":"not_sent","retry_safe":true,"steps":[step]}));
            assert_eq!(body["recovery"]["action"]["retry_allowed"], false);
        }
    }

    #[test]
    fn recovery_uses_codes_without_parsing_human_hints() {
        for (code, next_step) in [
            ("input_owner_required", "coordinate_with_owner"),
            ("ambiguous_element_label", "disambiguate_current_target"),
            ("runner_not_configured", "check_device_status"),
            ("unsupported_control", "choose_supported_action"),
            ("invalid_value", "correct_request"),
            ("future_error", "inspect_current_state"),
        ] {
            let body = enriched(
                json!({"ok":false,"error":code,"hint":"retry now; verified!",
                "outcome":"unknown","retry_safe":true}),
            );
            assert_eq!(body["recovery"]["action"]["next_step"], next_step);
            assert_eq!(body["recovery"]["action"]["retry_allowed"], false);
        }
    }

    #[test]
    fn consumes_server_element_value_marker_without_copying_values() {
        let marker = json!({"status":"verified","scope":"element_value","reason":"element_value",
            "application_commit":"not_verified","actual":"PRIVATE","expected":"PRIVATE"});
        let body = enriched(json!({"ok":true,"transport":"runner","verification":marker.clone()}));
        assert_eq!(
            body["action_evidence"]["postcondition"]["status"],
            "verified"
        );
        assert_eq!(
            body["action_evidence"]["postcondition"]["scope"],
            "element_value"
        );
        assert_eq!(
            body["action_evidence"]["postcondition"]["business_outcome"],
            "unverified"
        );
        assert!(!body["action_evidence"].to_string().contains("PRIVATE"));
        let step = json!({"kind":"action","ok":true,"verification":marker});
        let body = enriched(json!({"ok":true,"steps":[step.clone()]}));
        assert_eq!(
            body["action_evidence"]["postcondition"]["source"],
            "steps[-1].verification"
        );
        for body in [
            json!({"ok":true,"steps":[step.clone(),{"kind":"action","ok":true}]}),
            json!({"ok":false,"steps":[step.clone()]}),
            json!({"ok":true,"observation_omitted":true,"steps":[step]}),
            json!({"ok":true,"verification":{"status":"verified","scope":"business"}}),
        ] {
            assert_ne!(
                enriched(body)["action_evidence"]["postcondition"]["status"],
                "verified"
            );
        }
    }

    #[test]
    fn secure_readback_is_explicitly_unverified() {
        let body = enriched(json!({"ok":true,"transport":"runner","verification":{
            "status":"unverified","scope":"element_value","reason":"secure_field",
            "application_commit":"not_verified"}}));
        assert_eq!(
            body["action_evidence"]["postcondition"]["status"],
            "unverified"
        );
        assert_eq!(
            body["action_evidence"]["postcondition"]["reason"],
            "secure_field"
        );
        assert_eq!(body["action_evidence"]["delivery"]["status"], "delivered");
    }

    #[test]
    fn skipped_optional_steps_are_not_delivery_acknowledgements() {
        for reason in [json!(true), json!("no_alert"), json!("not_present")] {
            let step = json!({"kind":"action","ok":true,"skipped":reason});
            assert_eq!(
                enriched(step.clone())["action_evidence"]["delivery"]["status"],
                "not_sent"
            );
            let body = enriched(
                json!({"ok":true,"completed":1,"applied_actions":0,"steps":[step.clone()]}),
            );
            assert_eq!(body["action_evidence"]["delivery"]["status"], "not_sent");
            let body = enriched(json!({"ok":false,"error":"timeout","outcome":"not_sent",
                "retry_safe":true,"steps":[{"kind":"action","ok":true},step]}));
            assert_eq!(body["action_evidence"]["delivery"]["status"], "delivered");
            assert_eq!(body["recovery"]["action"]["retry_allowed"], false);
        }
    }
}
