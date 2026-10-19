//! Several daemons behind one host, routed by path (`IPHONE_USE_BASE_PATH`).
//!
//! Uses a hand-built runtime for the same reason as `http_auth.rs`: the local
//! `core` crate shadows std's `core` in `#[tokio::test]`'s expansion.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::http::{self, AppState};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

use server as srv;
include!("fixtures/app_state.rs");

fn state_under(base: &str) -> Arc<AppState> {
    let mut state = fixture_app_state(Some("hunter2"));
    Arc::get_mut(&mut state).unwrap().base_path = base.to_string();
    state
}

async fn send(app: &axum::Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn location(resp: &Response<Body>) -> &str {
    resp.headers().get(header::LOCATION).unwrap().to_str().unwrap()
}

async fn login(app: &axum::Router, uri: &str) -> Response<Body> {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("password=hunter2"))
            .unwrap(),
    )
    .await
}

#[test]
fn pages_live_under_the_base_path() {
    block(async {
        let app = http::router(state_under("/ix"));

        let resp = send(&app, get("/ix/phone")).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&resp), "/ix/login");

        let resp = send(&app, get("/ix/login")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains(r#"action="/ix/login""#));

        let resp = login(&app, "/ix/login").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&resp), "/ix/phone");
        let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(set_cookie.contains("; Path=/ix;"), "{set_cookie}");
        let cookie = set_cookie.split(';').next().unwrap().to_string();

        let resp = send(
            &app,
            Request::builder()
                .uri("/ix/phone")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body)
            .contains(r#"<meta name="iphone-use-base" content="/ix">"#));
    });
}

#[test]
fn unprefixed_pages_redirect_and_the_api_stays_reachable() {
    block(async {
        let app = http::router(state_under("/ix"));

        let resp = send(&app, get("/phone?x=1")).await;
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(location(&resp), "/ix/phone?x=1");

        let resp = send(&app, get("/")).await;
        assert_eq!(location(&resp), "/ix/");

        let resp = send(&app, get("/ix")).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&resp), "/ix/phone");

        // Agents, the iOS app and LAN clients that know only the port.
        for uri in ["/agent/status", "/ix/agent/status"] {
            let resp = send(&app, get(uri)).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    });
}

#[test]
fn no_base_path_keeps_the_old_addresses() {
    block(async {
        let app = http::router(state_under(""));

        let resp = send(&app, get("/")).await;
        assert_eq!(location(&resp), "/phone");

        let resp = login(&app, "/login").await;
        assert_eq!(location(&resp), "/phone");
        let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(set_cookie.contains("; Path=/;"), "{set_cookie}");

        let resp = send(&app, get("/ix/phone")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    });
}
