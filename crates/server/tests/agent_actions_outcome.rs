//! `POST /agent/actions` — what the failed STEP did is not what the BATCH did.
//!
//! Hardware acceptance produced a failure body carrying `applied_actions: 2`
//! next to `outcome: "not_sent"`. Both were true of different things: two
//! actions really had reached the phone, and the step that failed really had
//! not been sent. Read as a batch verdict, it says the opposite of what
//! happened.
//!
//! `outcome` is frozen — callers read it and it keeps meaning the failed step.
//! `failed_step_outcome` is the same value under an honest name, and
//! `batch_outcome` is the batch's own verdict. `retry_safe` is unchanged and
//! remains the only authorisation to send a batch again; none of these three
//! fields may be used to infer it.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state, build_state_with_wda, mock_wda, mock_wda_with_apps};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// An application with no children: readable, but nothing matches a locator.
const BARE_TREE: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication",
    "label":"测试应用",
    "rect":{"x":0,"y":0,"width":390,"height":844},
    "children":[]
}}"#;

async fn post_actions(base: Option<&str>, body: &str) -> (StatusCode, serde_json::Value) {
    let state = match base {
        Some(base) => build_state_with_wda(base),
        None => build_state(None),
    };
    let app = server::http::router(state);
    let request = Request::builder()
        .method("POST")
        .uri("/agent/actions")
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// The shape found on hardware: an action lands, then an expectation fails.
#[test]
fn an_applied_action_before_a_failed_expectation_is_a_partial_batch() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((Duration::ZERO, BARE_TREE.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"wait_for","expect":{"present":[{"label":"nothing here"}]},
                 "timeout_ms":200,"poll_ms":50}
            ]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        assert!(
            json["applied_actions"].as_u64().is_some_and(|n| n > 0),
            "the first action must have been applied: {json}"
        );
        // Frozen: existing callers keep reading the failed step here.
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(
            json["batch_outcome"], "partially_applied",
            "actions reached the phone, so the batch is not `nothing_applied`: {json}"
        );
        assert_eq!(
            json["retry_safe"], false,
            "a batch that already applied actions is never safe to replay: {json}"
        );
    });
}

/// The last action's fate is unknown. That must NOT be dressed up as a partial
/// batch, which would assert the earlier actions are settled and only the last
/// one is in doubt.
#[test]
fn an_unknown_step_makes_the_whole_batch_unknown() {
    block(async {
        let wda = mock_wda(|request, index| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            // First mutation lands; the second gets no answer at all.
            if index >= 2 {
                return None;
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"action","action":{"type":"home"}}
            ]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        // Pin WHICH step failed: without this, a first-step failure would
        // reach the same `unknown` verdict and the test would pass for the
        // wrong reason.
        assert_eq!(
            json["applied_actions"], 1,
            "the first action must have applied: {json}"
        );
        assert_eq!(
            json["failed_step"], 1,
            "it must be the SECOND step whose outcome is unknown: {json}"
        );
        assert_eq!(json["failed_step_outcome"], "unknown", "{json}");
        assert_eq!(
            json["batch_outcome"], "unknown",
            "an unknown step outcome must not be reported as a known partial: {json}"
        );
        assert_ne!(json["batch_outcome"], "partially_applied", "{json}");
        assert_eq!(json["retry_safe"], false, "{json}");
    });
}

/// Nothing ran at all: refused by local validation before the first step.
#[test]
fn a_locally_refused_batch_applied_nothing() {
    block(async {
        let (status, json) = post_actions(
            None,
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"action","action":{"type":"uninstall","bundle":"com.example.app"}}
            ]}"#,
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_actions_request", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(
            json["batch_outcome"], "nothing_applied",
            "a batch refused before its first step applied nothing: {json}"
        );
        assert_eq!(json["retry_safe"], true, "{json}");
    });
}

/// Zero actions applied, but the first step's fate is unknown. `nothing_applied`
/// would be a claim the daemon cannot make.
#[test]
fn zero_applied_actions_with_an_unknown_first_step_is_still_unknown() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            // The very first mutation goes out and is never answered.
            None
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        assert_eq!(json["applied_actions"], 0, "{json}");
        assert_eq!(json["failed_step_outcome"], "unknown", "{json}");
        assert_eq!(
            json["batch_outcome"], "unknown",
            "a count of zero does not prove nothing was applied when the \
             outcome itself is unknown: {json}"
        );
        assert_eq!(json["retry_safe"], false, "{json}");
    });
}

/// The batch never starts because the backend cannot drive: also zero actions,
/// and it should say so in the same words as every other zero-action refusal.
#[test]
fn a_batch_refused_before_it_starts_reports_nothing_applied() {
    block(async {
        // The fixture state has no WDA configured, so a well-formed batch is
        // refused after validation and before the first step.
        let (status, json) = post_actions(
            None,
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(json["error"], "wda_not_configured", "{json}");
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(json["batch_outcome"], "nothing_applied", "{json}");
        assert_eq!(json["retry_safe"], true, "{json}");
    });
}

/// `observe: true` hands back the screen the batch ended on, so the agent's
/// next decision needs no separate `/agent/elements` round trip.
#[test]
fn an_observed_batch_returns_the_screen_it_ended_on() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((Duration::ZERO, BARE_TREE.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}],"observe":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true, "{json}");
        assert!(json["snapshot"].is_string(), "{json}");
        assert_eq!(json["elements"][0]["label"], "测试应用", "{json}");
        assert!(json["settle"].is_object(), "{json}");

        let (_, plain) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;
        assert!(plain.get("elements").is_none(), "no observation unless asked: {plain}");
    });
}

/// An alert step marked `if_present` passes when no alert is up, so an
/// occasional system prompt can be written into a flow; without it the same
/// step still fails as `no_alert`.
#[test]
fn an_optional_alert_step_passes_when_no_alert_is_up() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/alert/text") {
                // Real WDA answers "no such alert" with HTTP 404.
                let body = r#"{"value":{"error":"no such alert","message":"no modal dialog is open"}}"#;
                return Some((
                    Duration::ZERO,
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                ));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[
                {"kind":"action","action":{"type":"alert","button":"允许粘贴","if_present":true}},
                {"kind":"action","action":{"type":"home"}}
            ]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true, "{json}");
        assert_eq!(json["completed"], 2, "{json}");
        assert_eq!(json["steps"][0]["skipped"], "no_alert", "{json}");

        let (_, strict) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"alert","button":"允许粘贴"}}]}"#,
        )
        .await;
        assert_eq!(strict["error"], "no_alert", "{strict}");
    });
}

/// A batched `tap_locator` whose target sits under iOS 26's floating search
/// field scrolls it into view before clicking (hardware, iPhone 13 Settings:
/// a plain click on 通用 under the pill ACKed and did nothing).
#[test]
fn a_covered_locator_tap_reveals_before_clicking() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let scrolled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let moved = scrolled.clone();
        let wda = mock_wda(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/scrollTo") {
                moved.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if request.contains("/source?format=json") {
                // After the reveal scroll the row sits mid-screen, clear of
                // the pill: the read the tap makes before clicking sees that.
                let y = if moved.load(std::sync::atomic::Ordering::SeqCst) {
                    400
                } else {
                    745
                };
                return Some((Duration::ZERO, settings_tree_with_general_at(y, 51)));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                return Some((Duration::ZERO, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#.to_string()));
            }
            if request.contains("/element/E1/rect") {
                return Some((Duration::ZERO, r#"{"value":{"x":16,"y":400,"width":358,"height":51}}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"通用","kind":"Button"}}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let seen = seen.lock().unwrap();
        let scroll = seen.iter().position(|r| r.contains("/E1/scrollTo")).expect("a reveal scroll");
        let click = seen.iter().position(|r| r.contains("/E1/click")).expect("an element click");
        assert!(scroll < click, "reveal before click: {seen:?}");
    });
}

/// The iPhone 13 Settings top page with 通用 at `y` (height `h`) and iOS 26's
/// floating search pill at y 778–806.
fn settings_tree_with_general_at(y: u32, h: u32) -> String {
    format!(
        r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"设置","rect":{{"x":0,"y":0,"width":390,"height":844}},"children":[
            {{"type":"XCUIElementTypeButton","label":"电池","rect":{{"x":16,"y":661,"width":358,"height":49}}}},
            {{"type":"XCUIElementTypeButton","label":"通用","rect":{{"x":16,"y":{y},"width":358,"height":{h}}}}},
            {{"type":"XCUIElementTypeSearchField","label":"搜索","rect":{{"x":28,"y":778,"width":334,"height":28}}}}]}}}}"#
    )
}

/// A covered row the reveal scroll cannot move (the scroll failed, or the
/// row had nowhere to go) is still under the pill when the tap looks again:
/// tap the part the pill leaves clear, never the covered centre (agent-loop
/// A/B, iPhone 13: "tapped 通用", the pill took it and the list scrolled).
#[test]
fn a_label_tap_still_covered_after_the_reveal_taps_the_clear_part() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let wda = mock_wda(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((Duration::ZERO, settings_tree_with_general_at(745, 51)));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#
                        .to_string(),
                ));
            }
            if request.contains("/element/E1/rect") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"x":16,"y":745,"width":358,"height":51}}"#.to_string(),
                ));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap","label":"通用"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|r| r.contains("/E1/scrollTo")),
            "a reveal scroll: {seen:?}"
        );
        assert!(
            !seen.iter().any(|r| r.contains("/E1/click")),
            "no click on the covered centre: {seen:?}"
        );
        let tap = seen
            .iter()
            .rev()
            .find(|r| r.starts_with("POST ") && r.contains("/actions"))
            .expect("a coordinate tap on the clear part");
        // The clear band is y 745–764 (pill 778 grown by the 14 pt margin).
        let y = tap
            .split("\"y\":")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| c != '.' && !c.is_ascii_digit()).next())
            .and_then(|number| number.parse::<f64>().ok())
            .expect("a y coordinate");
        assert!((745.0..764.0).contains(&y), "tap at y {y}: {tap}");
    });
}

/// A row the reveal scroll pushes fully under the pill is refused before
/// anything is sent: `element_occluded`, no click, no coordinate tap.
#[test]
fn a_locator_tap_fully_covered_after_the_reveal_is_refused_unsent() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let scrolled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let moved = scrolled.clone();
        let wda = mock_wda(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/scrollTo") {
                moved.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if request.contains("/source?format=json") {
                let tree = if moved.load(std::sync::atomic::Ordering::SeqCst) {
                    settings_tree_with_general_at(770, 36)
                } else {
                    settings_tree_with_general_at(745, 51)
                };
                return Some((Duration::ZERO, tree));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#
                        .to_string(),
                ));
            }
            if request.contains("/element/E1/rect") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"x":16,"y":770,"width":358,"height":36}}"#.to_string(),
                ));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"通用","kind":"Button"}}}]}"#,
        )
        .await;
        assert_eq!(json["ok"], false, "{json}");
        assert!(json.to_string().contains("element_occluded"), "{json}");
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|r| r.contains("/E1/click")), "{seen:?}");
        assert!(
            !seen
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("/actions")),
            "no tap sent: {seen:?}"
        );
    });
}

/// The in-app Safari sheet on a 17 Pro Max with a link at `link_y`, below
/// the visible page when `link_y` is past 956, and the sheet's header: 关闭
/// sits inside the left edge of the bar-wide 地址 button.
fn safari_sheet_with_link_at(link_y: u32) -> String {
    format!(
        r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"Safari浏览器","rect":{{"x":0,"y":0,"width":440,"height":956}},"children":[
            {{"type":"XCUIElementTypeOther","label":"TopBrowserBar","rect":{{"x":0,"y":62,"width":440,"height":54}},"children":[
                {{"type":"XCUIElementTypeButton","label":"关闭","rect":{{"x":16,"y":65,"width":44,"height":44}}}},
                {{"type":"XCUIElementTypeButton","label":"地址","rect":{{"x":26,"y":65,"width":388,"height":44}}}}]}},
            {{"type":"XCUIElementTypeOther","label":"page","rect":{{"x":0,"y":116,"width":440,"height":4000}},"children":[
                {{"type":"XCUIElementTypeLink","label":"Acceptable Use","rect":{{"x":20,"y":{link_y},"width":120,"height":20}}}}]}}]}}}}"#
    )
}

/// A mock runner serving `tree(scrolled)` for reads and E1's frame from
/// `rect(scrolled)`, where `scrolled` turns true at the first scrollTo.
fn sheet_wda(
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tree: fn(bool) -> String,
    rect: fn(bool) -> String,
) -> support::MockWda {
    let scrolled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    mock_wda(move |request, _| {
        seen.lock().unwrap().push(request.to_string());
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, SESSION.to_string()));
        }
        if request.contains("/scrollTo") {
            scrolled.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let moved = scrolled.load(std::sync::atomic::Ordering::SeqCst);
        if request.contains("/source?format=json") {
            return Some((Duration::ZERO, tree(moved)));
        }
        if request.starts_with("POST ") && request.contains("/elements") {
            return Some((
                Duration::ZERO,
                r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#
                    .to_string(),
            ));
        }
        if request.contains("/element/E1/rect") {
            return Some((Duration::ZERO, rect(moved)));
        }
        Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
    })
}

/// A locator tap on a link below the visible page whose reveal scroll does
/// not bring it on screen is refused, nothing sent — never `ok` for a tap
/// that touched nothing (hardware, 17 Pro Max in-app Safari sheet).
#[test]
fn an_off_screen_locator_tap_the_reveal_cannot_bring_on_screen_is_refused_unsent() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let wda = sheet_wda(
            seen.clone(),
            |_| safari_sheet_with_link_at(1500),
            |_| r#"{"value":{"x":20,"y":1500,"width":120,"height":20}}"#.to_string(),
        );
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"Acceptable Use","kind":"Link"}}}]}"#,
        )
        .await;
        assert_eq!(json["ok"], false, "{json}");
        assert!(json.to_string().contains("element_not_visible"), "{json}");
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|r| r.contains("/E1/scrollTo")),
            "a reveal scroll was tried: {seen:?}"
        );
        assert!(!seen.iter().any(|r| r.contains("/E1/click")), "{seen:?}");
        assert!(
            !seen
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("/actions")),
            "no tap sent: {seen:?}"
        );
    });
}

/// The same link once the reveal scroll brings it on screen is clicked.
#[test]
fn an_off_screen_locator_tap_is_revealed_then_clicked() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let wda = sheet_wda(
            seen.clone(),
            |moved| safari_sheet_with_link_at(if moved { 500 } else { 1500 }),
            |moved| {
                format!(
                    r#"{{"value":{{"x":20,"y":{},"width":120,"height":20}}}}"#,
                    if moved { 500 } else { 1500 }
                )
            },
        );
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"Acceptable Use","kind":"Link"}}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let seen = seen.lock().unwrap();
        let scroll = seen
            .iter()
            .position(|r| r.contains("/E1/scrollTo"))
            .expect("a reveal scroll");
        let click = seen
            .iter()
            .position(|r| r.contains("/E1/click"))
            .expect("an element click");
        assert!(scroll < click, "reveal before click: {seen:?}");
    });
}

/// A label tap on the same off-screen link — `visible:false` in the lite
/// read, like every row outside the screen — is revealed and clicked too,
/// not refused as "not drawn" before any scroll.
#[test]
fn an_off_screen_label_tap_is_revealed_then_clicked() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let wda = sheet_wda(
            seen.clone(),
            |moved| safari_sheet_with_link_at(if moved { 500 } else { 1500 }),
            |moved| {
                format!(
                    r#"{{"value":{{"x":20,"y":{},"width":120,"height":20}}}}"#,
                    if moved { 500 } else { 1500 }
                )
            },
        );
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap","label":"Acceptable Use"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let seen = seen.lock().unwrap();
        let scroll = seen
            .iter()
            .position(|r| r.contains("/E1/scrollTo"))
            .expect("a reveal scroll");
        let click = seen
            .iter()
            .position(|r| r.contains("/E1/click"))
            .expect("an element click");
        assert!(scroll < click, "reveal before click: {seen:?}");
    });
}

/// The sheet's 关闭 button, inside the left edge of the bar-wide 地址 button,
/// is clicked, not refused as `element_occluded`.
#[test]
fn a_sheet_close_button_under_the_address_bar_edge_is_clicked() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let wda = sheet_wda(
            seen.clone(),
            |_| safari_sheet_with_link_at(500),
            |_| r#"{"value":{"x":16,"y":65,"width":44,"height":44}}"#.to_string(),
        );
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"关闭","kind":"Button"}}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(!json.to_string().contains("element_occluded"), "{json}");
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|r| r.contains("/E1/click")), "{seen:?}");
        assert!(
            !seen.iter().any(|r| r.contains("/scrollTo")),
            "no reveal for a visible header button: {seen:?}"
        );
    });
}

/// A runner whose every screen read errors gets a bounded, backed-off number
/// of reads and a clear 502 — not a fixed 250 ms retry for the whole 35 s
/// budget (agent-loop A/B, iPhone 13: 137 /source calls, then a bare 504).
#[test]
fn a_screen_read_that_keeps_failing_backs_off_and_stops() {
    block(async {
        let sources = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = sources.clone();
        let wda = mock_wda(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"error":"unknown error","message":"source unavailable"}}"#
                        .to_string(),
                ));
            }
            if request.contains("/alert/text") {
                let body =
                    r#"{"value":{"error":"no such alert","message":"no modal dialog is open"}}"#;
                return Some((
                    Duration::ZERO,
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                ));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let app = server::http::router(build_state_with_wda(wda.url()));
        let started = std::time::Instant::now();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{json}");
        assert_eq!(json["error"], "wda_source_failed", "{json}");
        assert_eq!(json["source_attempts"], 8, "{json}");
        assert!(
            json["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("kept failing")),
            "{json}"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "gave up after {elapsed:?}"
        );
        // One read per attempt (a lite read; the fallback probe may add one).
        let reads = sources.load(std::sync::atomic::Ordering::SeqCst);
        assert!((8..=16).contains(&reads), "{reads} /source calls");
    });
}

/// Spotlight opens through the search pill's identifier. On iOS 26 the
/// localized "搜索" also names the pill's own image and text, so a lookup that
/// matches labels found three elements and the shortcut failed as ambiguous
/// (hardware, iPhone 13: 502 `outcome_unknown`, single and batched alike).
#[test]
fn spotlight_opens_through_the_pill_identifier() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let springboard = r#"[{"bundleId":"com.apple.springboard"}]"#;
        let wda = mock_wda_with_apps(springboard, move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                let element = |id: &str| {
                    format!(r#"{{"ELEMENT":"{id}","element-6066-11e4-a52e-4f735466cecf":"{id}"}}"#)
                };
                // Hardware, iPhone 13: the pill and a same-frame wrapper share
                // the identifier.
                let found = if request.contains("spotlight-pill") {
                    vec![element("PILL"), element("WRAP")]
                } else if request.contains("SpotlightSearchField") {
                    vec![element("FIELD")]
                } else if request.contains("搜索") {
                    vec![element("PILL"), element("IMG"), element("TEXT")]
                } else {
                    vec![]
                };
                return Some((Duration::ZERO, format!(r#"{{"value":[{}]}}"#, found.join(","))));
            }
            if request.contains("/element/PILL/rect") || request.contains("/element/WRAP/rect") {
                return Some((Duration::ZERO, r#"{"value":{"x":164,"y":688,"width":61,"height":30}}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"shortcut","name":"spotlight"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true, "{json}");
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|r| r.contains("/element/PILL/click")), "{seen:?}");
    });
}

/// An uncovered `tap_locator` clicks straight away: no reveal scroll.
#[test]
fn a_clear_locator_tap_does_not_scroll() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let wda = mock_wda(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"type":"XCUIElementTypeApplication","label":"设置","rect":{"x":0,"y":0,"width":390,"height":844},"children":[
                        {"type":"XCUIElementTypeButton","label":"电池","rect":{"x":16,"y":661,"width":358,"height":49}},
                        {"type":"XCUIElementTypeSearchField","label":"搜索","rect":{"x":28,"y":778,"width":334,"height":28}}]}}"#
                        .to_string(),
                ));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                return Some((Duration::ZERO, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"电池","kind":"Button"}}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|r| r.contains("/scrollTo")), "no reveal: {seen:?}");
        assert!(seen.iter().any(|r| r.contains("/E1/click")), "{seen:?}");
    });
}

/// `tap_locator` with `"via":"point"` taps the centre of the element's LIVE
/// frame through W3C actions — never XCUIElement's click, which custom
/// controls such as Xiaohongshu's back button acknowledge and ignore.
#[test]
fn a_point_locator_tap_hits_the_live_frame_centre() {
    block(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let wda = mock_wda(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"type":"XCUIElementTypeApplication","label":"小红书","rect":{"x":0,"y":0,"width":440,"height":956},"children":[
                        {"type":"XCUIElementTypeButton","label":"返回","rect":{"x":15,"y":69,"width":30,"height":30}}]}}"#
                        .to_string(),
                ));
            }
            if request.starts_with("POST ") && request.contains("/elements") {
                return Some((Duration::ZERO, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#.to_string()));
            }
            if request.contains("/element/E1/rect") {
                return Some((Duration::ZERO, r#"{"value":{"x":15,"y":69,"width":30,"height":30}}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (status, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"返回","kind":"Button"},"via":"point"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        {
            let seen = seen.lock().unwrap();
            assert!(
                !seen.iter().any(|r| r.contains("/click")),
                "no element click: {seen:?}"
            );
            let actions = seen
                .iter()
                .find(|r| r.contains("/actions"))
                .expect("a W3C tap");
            assert!(
                actions.contains("\"x\":30") && actions.contains("\"y\":84"),
                "{actions}"
            );
        }

        let (bad, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"返回"},"via":"elsewhere"}}]}"#,
        )
        .await;
        assert_eq!(bad, StatusCode::BAD_REQUEST, "{json}");
    });
}
