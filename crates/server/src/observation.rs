//! Remove UI-derived diagnostics after authorization changes without inventing a new receipt.
use serde_json::{json, Value};
const RECEIPT_KEYS: &[&str] = &[
    "ok",
    "error",
    "outcome",
    "retry_safe",
    "transport",
    "system_capture",
    "app_lifecycle",
    "completed",
    "applied_actions",
    "failed_step",
    "failed_step_outcome",
    "batch_outcome",
    "steps",
    "characters_confirmed",
    "characters_uncertain",
    "characters_total",
    "swipes",
    "native_mutations_confirmed",
    "native_mutations_uncertain",
    "remaining_text",
    "detail",
    "runner_error",
    "skipped",
    "timing",
    "id",
    "job_id",
    "run_id",
    "index",
    "kind",
    "after_ms_omitted",
    "recovery",
    "image_omitted",
    "image_unavailable",
];
fn clean_runner_error(value: &mut Value) {
    if let Some(e) = value.as_object_mut() {
        e.retain(|key, _| matches!(key.as_str(), "status" | "error" | "nothing_sent"));
        if let Some(code) = e.get_mut("error") {
            if !matches!(
                code.as_str(),
                Some(
                    "invalid argument"
                        | "invalid element state"
                        | "unknown error"
                        | "no such element"
                        | "stale element reference"
                        | "unsupported operation"
                )
            ) {
                *code = json!("runner_error");
            }
        }
    } else {
        *value = Value::Null;
    }
}
fn clean(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(clean),
        Value::Object(map) => {
            map.retain(|key, _| RECEIPT_KEYS.contains(&key.as_str()));
            if map.get("kind").and_then(Value::as_str) == Some("wait_for") {
                map.remove("ok");
            }
            if let Some(detail) = map.get_mut("detail") {
                if let Some(d) = detail.as_object_mut() {
                    d.retain(|key, _| {
                        matches!(
                            key.as_str(),
                            "input_error"
                                | "clear_applied"
                                | "characters_confirmed"
                                | "characters_uncertain"
                                | "characters_total"
                                | "runner_error"
                        )
                    });
                } else {
                    *detail = Value::Null;
                }
            }
            if let Some(error) = map.get_mut("runner_error") {
                clean_runner_error(error);
            }
            if let Some(recovery) = map.get_mut("recovery") {
                if let Some(r) = recovery.as_object_mut() {
                    r.retain(|key, _| {
                        matches!(
                            key.as_str(),
                            "image_omitted" | "visual_check_required" | "replay_action"
                        )
                    });
                } else {
                    *recovery = Value::Null;
                }
            }
            for (key, value) in map.iter_mut() {
                // Timing has only controller route/count/byte/duration facts; it is not UI.
                if key == "app_lifecycle" {
                    if let Some(receipt) = value.as_object_mut() {
                        receipt.retain(|key, value| matches!(key.as_str(), "terminate_acknowledged" | "launch_attempted") && value.is_boolean());
                    } else { *value = Value::Null; }
                } else if key == "system_capture" {
                    if let Some(capture) = value.as_object_mut() {
                        capture.retain(|key, value| matches!(key.as_str(), "requested" | "saved_verified") && value.is_boolean());
                    } else {
                        *value = Value::Null;
                    }
                } else if key == "detail" {
                    if let Some(error) = value.get_mut("runner_error") {
                        clean_runner_error(error);
                    }
                } else if key != "timing" && key != "runner_error" && key != "recovery" {
                    clean(value);
                }
            }
        }
        _ => {}
    }
}
pub fn redact(mut value: Value, reason: &str) -> Value {
    if !value.is_object() || !value.get("ok").is_some_and(Value::is_boolean) {
        value =
            json!({"ok":false,"error":"outcome_unknown","outcome":"unknown","retry_safe":false});
    }
    clean(&mut value);
    if let Some(map) = value.as_object_mut() {
        map.insert("observation_omitted".into(), true.into());
        map.insert("scope_changed".into(), reason.into());
        map.insert("hint".into(),"The original action delivery verdict is unchanged; UI observations and diagnostics were withheld because the owner/privacy scope changed. Check current status and coordinate with the current owner before new work. Never replay applied or uncertain steps".into());
    }
    value
}
/// A batch's top-level verdict includes screen-derived wait results. It is
/// revoked even though individual confirmed action receipts remain valid.
pub fn redact_batch(value: Value) -> Value {
    let mut safe = redact(value, "screen_privacy_changed");
    let has_wait = safe.get("steps").and_then(Value::as_array)
        .is_some_and(|steps| steps.iter().any(|step| step["kind"] == "wait_for"));
    if let Some(map) = safe.as_object_mut() {
        map.insert("ok".into(), false.into());
        // An explicit privacy refusal is already independent of screen
        // contents. Keep its known no-write/retry verdict unchanged.
        if map.get("error").and_then(Value::as_str) != Some("screen_private") {
            map.insert("error".into(), "screen_privacy_changed".into());
            map.insert("retry_safe".into(), false.into());
        }
        // Batch progress and the failed-step position can also encode whether
        // a screen expectation matched. Applied-action receipts stay in steps.
        if has_wait { map.remove("completed"); }
        map.remove("failed_step");
        map.remove("failed_step_outcome");
    }
    safe
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_receipt_survives_privacy_redaction_without_screen_or_asset_data() {
        let receipt = json!({"requested":true,"saved_verified":false,"image":"PRIVATE",
            "destination":"PRIVATE","asset":"PRIVATE","hint":"PRIVATE"});
        let single = redact(json!({"ok":true,"outcome":"applied","system_capture":receipt}), "screen_privacy_changed");
        assert_eq!(single["system_capture"], json!({"requested":true,"saved_verified":false}));
        let batch = redact_batch(json!({"ok":true,"applied_actions":1,"retry_safe":false,
            "steps":[{"index":0,"kind":"action","ok":true,"system_capture":receipt}]}));
        assert_eq!(batch["steps"][0]["system_capture"], json!({"requested":true,"saved_verified":false}));
        assert_eq!(batch["applied_actions"], 1);
        assert_eq!(batch["retry_safe"], false);
        assert!(!single.to_string().contains("PRIVATE") && !batch.to_string().contains("PRIVATE"));
        let malformed = redact(json!({"ok":true,"system_capture":{"requested":"PRIVATE","saved_verified":"PRIVATE"}}), "screen_privacy_changed");
        assert_eq!(malformed["system_capture"], json!({}));
    }

    #[test]
    fn nested_ui_is_removed_but_partial_and_uncertain_receipts_survive() {
        let original = json!({"ok":false,"error":"text_partially_typed","outcome":"partial","batch_outcome":"partially_applied","applied_actions":1,"characters_confirmed":200,"characters_uncertain":0,"remaining_text":"caller input","retry_safe":false,"steps":[{"index":0,"kind":"wait_for","ok":true,"observation":{"label":"PRIVATE"}}],"detail":{"clear_applied":true,"runner_error":{"status":500,"error":"unknown error","message":"PRIVATE"},"candidates":["PRIVATE"]},"image":{"png_base64":"PRIVATE"}});
        let clean = redact(original.clone(), "input_privacy_changed");
        assert!(!clean.to_string().contains("PRIVATE"));
        for key in [
            "ok",
            "error",
            "outcome",
            "batch_outcome",
            "applied_actions",
            "characters_confirmed",
            "characters_uncertain",
            "remaining_text",
            "retry_safe",
        ] {
            assert_eq!(clean[key], original[key]);
        }
        assert_eq!(clean["detail"]["clear_applied"], true);
        assert_eq!(clean["detail"]["runner_error"]["status"], 500);
    }
    #[test]
    fn non_receipts_cannot_leak_free_text_or_claim_unsent() {
        for value in [
            json!(["PRIVATE"]),
            json!({"unexpected":"PRIVATE"}),
            Value::Null,
        ] {
            let safe = redact(value, "input_owner_changed");
            assert!(!safe.to_string().contains("PRIVATE"));
            assert_eq!(safe["outcome"], "unknown");
            assert_eq!(safe["retry_safe"], false);
        }
    }
}

#[cfg(test)]
mod wait_receipt_tests {
    #[test]
    fn explicit_private_refusal_keeps_the_known_no_write_verdict() {
        let safe = super::redact_batch(serde_json::json!({
            "ok":false,"error":"screen_private","applied_actions":0,"retry_safe":true,"steps":[]
        }));
        assert_eq!(safe["error"], "screen_private");
        assert_eq!(safe["retry_safe"], true);
        assert_eq!(safe["applied_actions"], 0);
    }
    #[test]
    fn revoked_wait_verdict_is_not_an_action_acknowledgement() {
        let value = serde_json::json!({"ok":true,"applied_actions":1,"steps":[
            {"index":0,"kind":"action","ok":true},
            {"index":1,"kind":"wait_for","ok":true,"observation":{"lines":["PRIVATE"]}}
        ]});
        let safe = super::redact_batch(value);
        assert_eq!(safe["ok"], false);
        assert_eq!(safe["error"], "screen_privacy_changed");
        assert_eq!(safe["applied_actions"], 1);
        assert_eq!(safe["steps"][0]["ok"], true);
        assert!(safe["steps"][1].get("ok").is_none());
        assert!(!safe.to_string().contains("PRIVATE"));
    }
}
