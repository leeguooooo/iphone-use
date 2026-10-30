//! The cloud relay tunnel: lets a phone outside this network reach the daemon
//! with no Mac and no open port.
//!
//! The daemon dials out to `<relay>/t/<id>` over a WebSocket and keeps it
//! open. Requests that reach `<relay>/p/<id>/<path>` arrive on it as frames;
//! each is replayed against this daemon on loopback and the answer is streamed
//! back, so the daemon's own authentication applies unchanged. See
//! `relay/src/index.ts` for the frame format.
//!
//! The relay address comes from `IPHONE_USE_RELAY_URL`, or, for the on-phone
//! daemon, from `relay.json` a Mac drops into the app's Documents
//! (`iphone-use setup-app --connect-to … --relay …`). The phone's id and key
//! are made once and kept in the state directory: the relay pins the first key
//! an id presents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Largest body chunk sent in one frame.
const CHUNK: usize = 256 * 1024;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Identity {
    id: String,
    key: String,
}

/// Where requests reach this daemon through `relay`: `<relay>/p/<id>`.
pub fn public_url(relay: &str, state_dir: &Path) -> Option<String> {
    let identity = load_identity(state_dir).ok()?;
    Some(format!("{}/p/{}", relay.trim_end_matches('/'), identity.id))
}

/// Keep the tunnel to the relay that `relay_url` names open for as long as the
/// process runs. `relay_url` is asked again before each attempt, so a relay
/// configured later is picked up.
pub fn spawn(relay_url: impl Fn() -> Option<String> + Send + 'static, state_dir: PathBuf, port: u16) {
    tokio::spawn(async move {
        let mut backoff = 1u64;
        loop {
            if let Some(relay) = relay_url() {
                match run(&relay, &state_dir, port).await {
                    Ok(()) => backoff = 1,
                    Err(error) => tracing::warn!("relay tunnel: {error}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}

fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join("relay-identity.json")
}

fn load_identity(state_dir: &Path) -> Result<Identity, String> {
    let path = identity_path(state_dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(identity) = serde_json::from_str::<Identity>(&text) {
            return Ok(identity);
        }
    }
    let mut id = [0u8; 16];
    let mut key = [0u8; 32];
    getrandom::fill(&mut id).map_err(|e| e.to_string())?;
    getrandom::fill(&mut key).map_err(|e| e.to_string())?;
    let identity = Identity { id: URL_SAFE_NO_PAD.encode(id), key: URL_SAFE_NO_PAD.encode(key) };
    std::fs::write(&path, serde_json::to_string(&identity).map_err(|e| e.to_string())?)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(identity)
}

/// One tunnel session, until the relay or the network drops it.
async fn run(relay: &str, state_dir: &Path, port: u16) -> Result<(), String> {
    let identity = load_identity(state_dir)?;
    let url = format!("{}/t/{}", relay.trim_end_matches('/'), identity.id)
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = url.as_str().into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", identity.key).parse().map_err(|_| "bad key".to_string())?,
    );
    let uri = request.uri().clone();
    let host = uri.host().ok_or("relay url has no host")?.to_string();
    let tls = uri.scheme_str() == Some("wss");
    let port_num = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port_num))
        .await
        .map_err(|e| format!("connect {host}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let (socket, _) = if tls {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("tls {host}: {e}"))?;
        let (ws, response) = tokio_tungstenite::client_async(request, Box::new(stream) as Box<dyn Io>)
            .await
            .map_err(|e| format!("handshake: {e}"))?;
        (ws, response)
    } else {
        tokio_tungstenite::client_async(request, Box::new(tcp) as Box<dyn Io>)
            .await
            .map_err(|e| format!("handshake: {e}"))?
    };
    tracing::info!("relay tunnel open: {}/p/{}", relay.trim_end_matches('/'), identity.id);

    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let pinger = {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(25)).await;
                // The relay answers this text without waking its object.
                if out_tx.send(Message::Text("ping".into())).await.is_err() {
                    break;
                }
            }
        })
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let mut tasks: HashMap<u32, tokio::task::JoinHandle<()>> = HashMap::new();
    let result = loop {
        let Some(message) = stream.next().await else { break Ok(()) };
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break Ok(()),
            Ok(_) => continue,
            Err(e) => break Err(e.to_string()),
        };
        if text.as_str() == "pong" {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(text.as_str()) else { continue };
        let Some(id) = frame["id"].as_u64().map(|id| id as u32) else { continue };
        tasks.retain(|_, task| !task.is_finished());
        match frame["t"].as_str() {
            Some("req") => {
                let task = tokio::spawn(serve(client.clone(), port, id, frame, out_tx.clone()));
                tasks.insert(id, task);
            }
            Some("cancel") => {
                if let Some(task) = tasks.remove(&id) {
                    task.abort();
                }
            }
            _ => {}
        }
    };
    for (_, task) in tasks {
        task.abort();
    }
    pinger.abort();
    writer.abort();
    result
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Replay one relayed request on loopback and stream the answer back.
async fn serve(client: reqwest::Client, port: u16, id: u32, frame: serde_json::Value, out: mpsc::Sender<Message>) {
    let method = frame["m"].as_str().unwrap_or("GET");
    let path = frame["p"].as_str().unwrap_or("/");
    let reply = |status: u16, headers: Vec<(String, String)>| {
        Message::Text(serde_json::json!({ "t": "res", "id": id, "s": status, "h": headers }).to_string().into())
    };
    let end = || Message::Text(serde_json::json!({ "t": "end", "id": id }).to_string().into());
    if !path.starts_with('/') {
        let _ = out.send(reply(400, Vec::new())).await;
        let _ = out.send(end()).await;
        return;
    }
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else { return };
    let mut request = client.request(method, format!("http://127.0.0.1:{port}{path}"));
    for pair in frame["h"].as_array().into_iter().flatten() {
        if let (Some(name), Some(value)) = (pair[0].as_str(), pair[1].as_str()) {
            request = request.header(name, value);
        }
    }
    if let Some(body) = frame["b"].as_str().filter(|b| !b.is_empty()) {
        request = request.body(STANDARD.decode(body).unwrap_or_default());
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            let _ = out.send(reply(502, Vec::new())).await;
            let _ = out.send(end()).await;
            return;
        }
    };
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();
    if out.send(reply(response.status().as_u16(), headers)).await.is_err() {
        return;
    }
    let mut body = response.bytes_stream();
    while let Some(Ok(chunk)) = body.next().await {
        for piece in chunk.chunks(CHUNK) {
            let mut frame = Vec::with_capacity(4 + piece.len());
            frame.extend_from_slice(&id.to_be_bytes());
            frame.extend_from_slice(piece);
            if out.send(Message::Binary(frame.into())).await.is_err() {
                return;
            }
        }
    }
    let _ = out.send(end()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("ipu-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = load_identity(&dir).unwrap();
        let second = load_identity(&dir).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.key, second.key);
        assert!(first.id.len() >= 22 && first.key.len() >= 32);
        assert_eq!(
            public_url("https://relay.example/", &dir).unwrap(),
            format!("https://relay.example/p/{}", first.id)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
