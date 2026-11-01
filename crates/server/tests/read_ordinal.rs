//! HTTP ordinal results require both metadata and tree-order scope evidence.
mod support;
use axum::{body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

#[test]
fn read_ordinal_preserves_snapshot_indexes_and_refuses_unproven_scope() {
    block(async {
        for (coverage, unknown_prefix, status, selected) in [
            ("complete", false, StatusCode::OK, Some("second")),
            ("complete", true, StatusCode::CONFLICT, None),
            ("partial", false, StatusCode::CONFLICT, None),
            ("unknown", false, StatusCode::CONFLICT, None),
        ] {
            let runner=mock_runner(move |request,_| {
                if request.contains("/source?") {
                    let prefix=if unknown_prefix {serde_json::json!({"type":"XCUIElementTypeButton","label":"Save","rawIdentifier":"unknown"})}
                        else {serde_json::json!({"type":"XCUIElementTypeStaticText","label":"Other"})};
                    let body=serde_json::json!({"value":{"type":"XCUIElementTypeApplication","label":"Fixture",
                        "rect":{"x":0,"y":0,"width":390,"height":844},"children":[prefix,
                        {"type":"XCUIElementTypeButton","label":"Save","rawIdentifier":"first","isEnabled":"0","rect":{"x":20,"y":80,"width":100,"height":40}},
                        {"type":"XCUIElementTypeButton","label":"Save","rawIdentifier":"second","isEnabled":"0","rect":{"x":20,"y":180,"width":100,"height":40}}
                    ]}}).to_string();
                    let headers=match coverage {"complete"=>"X-IPU-Truncated: 0\r\nX-IPU-Tree-Complete: 1\r\n","partial"=>"X-IPU-Truncated: 1\r\nX-IPU-Tree-Complete: 0\r\n",_=>""};
                    return Some((Duration::ZERO,format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())));
                }
                None
            });
            let selector=serde_json::json!({"kind":"Button","label":"Save","enabled":false,"index":1});
            let mut url=reqwest::Url::parse("http://daemon/agent/elements").unwrap();
            url.query_pairs_mut().append_pair("selector",&selector.to_string()).append_pair("ocr","off");
            let response=server::http::router(build_state_with_runner(runner.url())).oneshot(
                Request::builder().uri(format!("{}?{}",url.path(),url.query().unwrap())).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(),status);
            let body:serde_json::Value=serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
            assert_eq!(body["filter"]["selector"],selector);
            if let Some(id)=selected {
                assert_eq!(body["elements"][0]["identifier"],id);
                assert_eq!(body["elements"][0]["index"],3);
                assert!(body["snapshot"].is_string());
            } else {
                assert_eq!(body["error"],"ordinal_unproven");
                assert_eq!(body["elements"],serde_json::json!([]));
            }
        }
    });
}
