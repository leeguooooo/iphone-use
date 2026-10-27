//! Remove UI-derived diagnostics after authorization changes without inventing a new receipt.
use serde_json::{json, Value};
const RECEIPT_KEYS: &[&str] = &[
    "ok",
    "error",
    "outcome",
    "retry_safe",
    "transport",
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
                if key == "detail" {
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
#[cfg(test)]
mod tests {
    use super::*;
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
