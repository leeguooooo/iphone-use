//! `iphone-use-mcp --listen <addr>`: the same tools over MCP streamable HTTP
//! (rmcp's `StreamableHttpService`), for agents that cannot spawn a stdio
//! server or run on another machine.
//!
//! Every request must carry `Authorization: Bearer $IPHONE_USE_MCP_TOKEN`
//! when that variable is set; without it the server only binds a loopback
//! address. Each MCP session gets its own handler and its own phone owner
//! name (`<owner>-<n>`), so two connected agents never share one lease.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::{client::DaemonClient, server::PhoneHandler};

/// The environment variable holding the bearer token HTTP clients present.
pub const TOKEN_ENV: &str = "IPHONE_USE_MCP_TOKEN";
/// Where the MCP endpoint is served.
pub const MCP_PATH: &str = "/mcp";

/// `IPHONE_USE_MCP_TOKEN`, when set to something non-blank.
pub fn token_from_env() -> Option<String> {
    std::env::var(TOKEN_ENV)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// Refuse an address other machines can reach unless a token guards it: the
/// tools drive a real phone.
pub fn check_bind(addr: &SocketAddr, token: Option<&str>) -> anyhow::Result<()> {
    if addr.ip().is_loopback() || token.is_some() {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to serve MCP on {addr} without a token: anyone who can reach it could drive \
         the phone. Set {TOKEN_ENV} (clients send Authorization: Bearer <token>), or listen on \
         127.0.0.1"
    )
}

/// Serve on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    daemon: DaemonClient,
    token: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    check_bind(&addr, token.as_deref())?;
    let profile = Arc::new(crate::server::ToolProfile::from_env());
    let sessions = AtomicU64::new(0);
    let factory = move || {
        let n = sessions.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(PhoneHandler::with_shared_profile(
            daemon.for_session(n),
            profile.clone(),
        ))
    };
    // rmcp checks `Host` against loopback names by default (DNS rebinding);
    // a token-guarded public listener is reached by whatever name the
    // network gives it, and the token is what keeps others out.
    let mut config = StreamableHttpServerConfig::default();
    if !addr.ip().is_loopback() {
        config = config.disable_allowed_hosts();
    }
    let service = StreamableHttpService::new(factory, LocalSessionManager::default().into(), config);
    let app = axum::Router::new()
        .route_service(MCP_PATH, service)
        .layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_bearer,
        ));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// 401 unless the request presents the configured bearer token.
async fn require_bearer(
    State(token): State<Arc<Option<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = token.as_deref() else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if presented.is_some_and(|presented| same_secret(presented, expected)) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        format!(
            r#"{{"ok":false,"error":"unauthorized","hint":"send Authorization: Bearer <{TOKEN_ENV}>"}}"#
        ),
    )
        .into_response()
}

/// Compare without stopping at the first differing byte.
fn same_secret(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_address_needs_a_token() {
        let public: SocketAddr = "0.0.0.0:8765".parse().unwrap();
        let err = check_bind(&public, None).unwrap_err().to_string();
        assert!(err.contains(TOKEN_ENV), "{err}");
        assert!(check_bind(&public, Some("s3cret")).is_ok());
        assert!(check_bind(&"192.168.1.5:1".parse().unwrap(), None).is_err());
        assert!(check_bind(&"127.0.0.1:0".parse().unwrap(), None).is_ok());
        assert!(check_bind(&"[::1]:0".parse().unwrap(), None).is_ok());
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abd", "abc"));
        assert!(!same_secret("ab", "abc"));
    }

    /// One HTTP exchange over a fresh connection; returns status, headers
    /// (lowercased names) and body.
    async fn post(
        addr: SocketAddr,
        token: Option<&str>,
        session: Option<&str>,
        body: &serde_json::Value,
    ) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = body.to_string();
        let mut head = format!(
            "POST {MCP_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n",
            addr.port(),
            body.len()
        );
        if let Some(token) = token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(session) = session {
            head.push_str(&format!("Mcp-Session-Id: {session}\r\nMcp-Protocol-Version: 2025-06-18\r\n"));
        }
        head.push_str("\r\n");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
        // An SSE answer stays open after its one event: read until the JSON
        // arrives, not until EOF.
        let mut raw = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let mut chunk = [0_u8; 8192];
            let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
                .await
                .expect("answer within 10 s")
                .unwrap();
            raw.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&raw).to_string();
            let Some((head, body)) = text.split_once("\r\n\r\n") else {
                assert!(read != 0, "connection closed before the headers: {text:?}");
                continue;
            };
            let head = head.to_ascii_lowercase();
            let body = if head.contains("transfer-encoding: chunked") {
                dechunk(body)
            } else {
                body.to_string()
            };
            let has_body = !head.starts_with("http/1.1 200");
            if read == 0 || has_body || try_message(&body).is_some() {
                let status = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|code| code.parse().ok())
                    .unwrap_or(0);
                return (status, head, body);
            }
        }
    }

    /// A chunked body's data, as far as it has arrived.
    fn dechunk(mut body: &str) -> String {
        let mut out = String::new();
        while let Some((size, rest)) = body.split_once("\r\n") {
            let Ok(size) = usize::from_str_radix(size.trim(), 16) else { break };
            if size == 0 || rest.len() < size {
                break;
            }
            out.push_str(&rest[..size]);
            body = rest[size..].trim_start_matches("\r\n");
        }
        out
    }

    /// The JSON-RPC message in an SSE (or plain JSON) body, once whole.
    fn try_message(body: &str) -> Option<serde_json::Value> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .chain(std::iter::once(body.trim()))
            .find_map(|data| {
                let start = data.find('{')?;
                serde_json::from_str::<serde_json::Value>(&data[start..])
                    .ok()
                    .filter(|json| json.get("jsonrpc").is_some())
            })
    }

    fn message(body: &str) -> serde_json::Value {
        try_message(body).unwrap_or_else(|| panic!("no JSON-RPC message in {body:?}"))
    }

    fn initialize() -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "0"}}
        })
    }

    async fn start(token: Option<&str>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let daemon = DaemonClient::new("http://127.0.0.1:9", None);
        let token = token.map(str::to_string);
        let task = tokio::spawn(async move {
            serve(listener, daemon, token, std::future::pending()).await.unwrap();
        });
        (addr, task)
    }

    #[tokio::test]
    async fn initialize_and_list_tools_over_http_with_the_token() {
        let (addr, task) = start(Some("s3cret")).await;

        // No token, or the wrong one: 401 before any MCP handling.
        for token in [None, Some("wrong")] {
            let (status, head, body) = post(addr, token, None, &initialize()).await;
            assert_eq!(status, 401, "{head}\n{body}");
            assert!(head.contains("www-authenticate: bearer"), "{head}");
            assert!(body.contains("unauthorized"), "{body}");
        }

        let (status, head, body) = post(addr, Some("s3cret"), None, &initialize()).await;
        assert_eq!(status, 200, "{head}\n{body}");
        let init = message(&body);
        assert_eq!(init["result"]["serverInfo"]["name"], "iphone-use-mcp", "{init}");
        let session = head
            .lines()
            .find_map(|line| line.strip_prefix("mcp-session-id:"))
            .map(str::trim)
            .expect("a session id")
            .to_string();

        let initialized = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let (status, _, _) = post(addr, Some("s3cret"), Some(&session), &initialized).await;
        assert_eq!(status, 202);

        let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        // Even inside a session, a request without the token is refused.
        let (status, _, _) = post(addr, None, Some(&session), &list).await;
        assert_eq!(status, 401);
        let (status, head, body) = post(addr, Some("s3cret"), Some(&session), &list).await;
        assert_eq!(status, 200, "{head}\n{body}");
        let tools = message(&body)["result"]["tools"].as_array().cloned().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names.len(), crate::server::tool_count(), "{names:?}");
        for name in ["phone_status", "phone_logs", "phone_install_app"] {
            assert!(names.contains(&name), "{name} missing: {names:?}");
        }
        task.abort();
    }

    #[tokio::test]
    async fn a_loopback_listener_without_a_token_is_open() {
        let (addr, task) = start(None).await;
        let (status, head, body) = post(addr, None, None, &initialize()).await;
        assert_eq!(status, 200, "{head}\n{body}");
        assert_eq!(message(&body)["result"]["serverInfo"]["name"], "iphone-use-mcp");
        task.abort();
    }

    #[tokio::test]
    async fn serve_refuses_a_public_address_without_a_token() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let err = serve(
            listener,
            DaemonClient::new("http://127.0.0.1:9", None),
            None,
            std::future::pending(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing to serve MCP"), "{err}");
    }
}
