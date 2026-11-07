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
/// Body bytes a request may have in flight before the relay acks them (the
/// viewer took them). A live picture past this drops whole frames; anything
/// else waits.
const WINDOW: usize = 512 * 1024;
/// How often the phone pings the relay (answered without waking its object).
#[cfg(not(test))]
const PING_EVERY: Duration = Duration::from_secs(15);
#[cfg(test)]
const PING_EVERY: Duration = Duration::from_millis(300);
/// No message at all for this long means the link is dead: the phone left
/// the Wi-Fi it dialled over, and the socket will never say so (no FIN, no
/// RST reaches it). Give up on it and dial again over whatever network the
/// phone has now (cellular). Three missed pongs.
#[cfg(not(test))]
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
#[cfg(test)]
const SILENCE_LIMIT: Duration = Duration::from_secs(1);
/// Each step of dialling the relay (TCP, TLS, WebSocket handshake).
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

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
                let started = std::time::Instant::now();
                match run(&relay, &state_dir, port).await {
                    Ok(()) => backoff = 1,
                    Err(error) => tracing::warn!("relay tunnel: {error}"),
                }
                // A tunnel that worked for a while and then broke (the phone
                // changed networks) is dialled again at once.
                if started.elapsed() > SILENCE_LIMIT * 2 {
                    backoff = 1;
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}

const IDENTITY_FILE: &str = "relay-identity.json";

fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join(IDENTITY_FILE)
}

fn read_identity(state_dir: &Path) -> Option<Identity> {
    let text = std::fs::read_to_string(identity_path(state_dir)).ok()?;
    serde_json::from_str::<Identity>(&text).ok()
}

/// The identity on disk, or a new one. An id is 32 lowercase hex characters
/// (128 bits) so it also fits in a hostname (`<id>.<relay>`); ids made by
/// older releases (base64url, mixed case) keep working on the `/p/<id>` path
/// and are never replaced: the relay pins the key to the id.
fn load_identity(state_dir: &Path) -> Result<Identity, String> {
    if let Some(identity) = read_identity(state_dir) {
        return Ok(identity);
    }
    let mut id = [0u8; 16];
    let mut key = [0u8; 32];
    getrandom::fill(&mut id).map_err(|e| e.to_string())?;
    getrandom::fill(&mut key).map_err(|e| e.to_string())?;
    let identity = Identity { id: hex(&id), key: URL_SAFE_NO_PAD.encode(key) };
    let text = serde_json::to_string(&identity).map_err(|e| e.to_string())?;
    let path = identity_path(state_dir);
    // 0600 from the first byte, no symlink, no clobbering: the file only comes
    // into being here.
    match crate::runtime_dir::write_secret_in(state_dir, IDENTITY_FILE, text.as_bytes()) {
        Ok(()) => Ok(identity),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Something is there that did not read as an identity. Another
            // daemon may have written a good one just now: take that. Else it
            // is junk (or a symlink); replace it.
            if let Some(identity) = read_identity(state_dir) {
                return Ok(identity);
            }
            std::fs::remove_file(&path).map_err(|e| format!("replace {}: {e}", path.display()))?;
            crate::runtime_dir::write_secret_in(state_dir, IDENTITY_FILE, text.as_bytes())
                .map_err(|e| format!("write {}: {e}", path.display()))?;
            Ok(identity)
        }
        Err(e) => Err(format!("write {}: {e}", path.display())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `/p/<id>` with the id cut short: the id is the address of this phone, and
/// the log is not the place to hand it out.
fn redacted_public_path(id: &str) -> String {
    format!("/p/{}…", id.chars().take(4).collect::<String>())
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
    // Bounded: right after a network change a dial can hang on a route that
    // is already gone.
    let tcp = tokio::time::timeout(DIAL_TIMEOUT, tokio::net::TcpStream::connect((host.as_str(), port_num)))
        .await
        .map_err(|_| format!("connect {host}: timed out"))?
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
        let stream = tokio::time::timeout(DIAL_TIMEOUT, tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp))
            .await
            .map_err(|_| format!("tls {host}: timed out"))?
            .map_err(|e| format!("tls {host}: {e}"))?;
        let (ws, response) =
            tokio::time::timeout(DIAL_TIMEOUT, tokio_tungstenite::client_async(request, Box::new(stream) as Box<dyn Io>))
                .await
                .map_err(|_| "handshake: timed out".to_string())?
                .map_err(|e| format!("handshake: {e}"))?;
        (ws, response)
    } else {
        tokio_tungstenite::client_async(request, Box::new(tcp) as Box<dyn Io>)
            .await
            .map_err(|e| format!("handshake: {e}"))?
    };
    tracing::info!("relay tunnel open: {}{}", relay.trim_end_matches('/'), redacted_public_path(&identity.id));
    tracing::debug!("relay public url: {}/p/{}", relay.trim_end_matches('/'), identity.id);

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
                tokio::time::sleep(PING_EVERY).await;
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
    let mut tasks: HashMap<u32, (tokio::task::JoinHandle<()>, Arc<tokio::sync::Semaphore>)> = HashMap::new();
    let result = loop {
        let message = match tokio::time::timeout(SILENCE_LIMIT, stream.next()).await {
            Ok(Some(message)) => message,
            Ok(None) => break Ok(()),
            Err(_) => break Err(format!("no answer from the relay for {}s; dialling again", SILENCE_LIMIT.as_secs())),
        };
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
        // Ids are 32-bit on the wire (the binary frames carry them in 4 bytes).
        let Some(id) = frame["id"].as_u64().and_then(|id| u32::try_from(id).ok()) else { continue };
        tasks.retain(|_, (task, _)| !task.is_finished());
        match frame["t"].as_str() {
            Some("req") => {
                let window = Arc::new(tokio::sync::Semaphore::new(WINDOW));
                let task = tokio::spawn(serve(client.clone(), port, id, frame, out_tx.clone(), window.clone()));
                // The relay forgot an id it had handed out (it restarted):
                // whatever was still answering under it would now be mistaken
                // for the new request. Stop it; its window goes with it.
                if let Some((old, _)) = tasks.insert(id, (task, window)) {
                    old.abort();
                }
            }
            Some("ack") => {
                if let (Some((_, window)), Some(n)) = (tasks.get(&id), frame["n"].as_u64()) {
                    window.add_permits(n as usize);
                }
            }
            Some("cancel") => {
                if let Some((task, _)) = tasks.remove(&id) {
                    task.abort();
                }
            }
            _ => {}
        }
    };
    for (_, (task, _)) in tasks {
        task.abort();
    }
    pinger.abort();
    writer.abort();
    result
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Replay one relayed request on loopback and stream the answer back.
async fn serve(
    client: reqwest::Client,
    port: u16,
    id: u32,
    frame: serde_json::Value,
    out: mpsc::Sender<Message>,
    window: Arc<tokio::sync::Semaphore>,
) {
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
    // A live picture (`multipart/x-mixed-replace; boundary=…`) may drop frames.
    let boundary = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("multipart/x-mixed-replace"))
        .and_then(|v| v.split("boundary=").nth(1))
        .map(|b| format!("--{}", b.trim_matches('"').trim_start_matches("--")).into_bytes());
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();
    if out.send(reply(response.status().as_u16(), headers)).await.is_err() {
        return;
    }
    let send = |piece: &[u8]| {
        let mut frame = Vec::with_capacity(4 + piece.len());
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(piece);
        Message::Binary(frame.into())
    };
    let mut body = response.bytes_stream();
    let mut parts = boundary.map(MultipartSplitter::new);
    while let Some(Ok(chunk)) = body.next().await {
        if let Some(splitter) = parts.as_mut() {
            for part in splitter.push(&chunk) {
                // Too far ahead of the viewer: skip this frame, keep the newest.
                let Ok(permit) = window.try_acquire_many(part.len().min(WINDOW) as u32) else { continue };
                permit.forget();
                if out.send(send(&part)).await.is_err() {
                    return;
                }
            }
            continue;
        }
        for piece in chunk.chunks(CHUNK) {
            let Ok(permit) = window.acquire_many(piece.len() as u32).await else { return };
            permit.forget();
            if out.send(send(piece)).await.is_err() {
                return;
            }
        }
    }
    let _ = out.send(end()).await;
}

/// Cuts a multipart stream into whole parts, each starting at its boundary,
/// so frames can be dropped without corrupting the ones that are sent.
struct MultipartSplitter {
    boundary: Vec<u8>,
    buffer: Vec<u8>,
}

impl MultipartSplitter {
    fn new(boundary: Vec<u8>) -> Self {
        Self { boundary, buffer: Vec::new() }
    }

    fn find(&self, from: usize) -> Option<usize> {
        let b = &self.boundary;
        self.buffer.get(from..)?.windows(b.len()).position(|w| w == b.as_slice()).map(|i| i + from)
    }

    /// The parts completed by `data` (a part ends where the next boundary starts).
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut parts = Vec::new();
        while let Some(start) = self.find(0) {
            let Some(next) = self.find(start + self.boundary.len()) else {
                self.buffer.drain(..start);
                break;
            };
            parts.push(self.buffer[start..next].to_vec());
            self.buffer.drain(..next);
        }
        if self.buffer.len() > 16 << 20 {
            self.buffer.clear();
        }
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_parts_come_out_whole_whatever_the_chunking() {
        let stream = b"--B\r\nA1\r\n--B\r\nBB22\r\n--B\r\nC";
        for size in [1, 3, 7, stream.len()] {
            let mut splitter = MultipartSplitter::new(b"--B".to_vec());
            let mut parts = Vec::new();
            for chunk in stream.chunks(size) {
                parts.extend(splitter.push(chunk));
            }
            assert_eq!(parts, vec![b"--B\r\nA1\r\n".to_vec(), b"--B\r\nBB22\r\n".to_vec()], "chunk {size}");
        }
    }

    /// The phone left the Wi-Fi it dialled over: the relay's socket goes
    /// quiet without ever closing. The tunnel must give up and dial again
    /// rather than wait on it forever (seen on an iPhone 17, 2026-11-07).
    #[test]
    fn a_relay_that_goes_silent_is_dropped_so_the_phone_dials_again() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(silent_relay());
    }

    async fn silent_relay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (dialled, mut accepted) = mpsc::channel::<()>(4);
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                // Complete the handshake, then never read or answer a ping.
                held.push(tokio_tungstenite::accept_async(tcp).await.unwrap());
                let _ = dialled.send(()).await;
            }
        });
        let dir = std::env::temp_dir().join(format!("ipu-relay-silent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = std::time::Instant::now();
        let result = run(&format!("http://{addr}"), &dir, 1).await;
        assert!(accepted.recv().await.is_some());
        let error = result.expect_err("a silent relay must end the tunnel");
        assert!(error.contains("no answer from the relay"), "{error}");
        let took = started.elapsed();
        assert!(took >= SILENCE_LIMIT && took < SILENCE_LIMIT * 3, "{took:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_identity_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("ipu-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = load_identity(&dir).unwrap();
        let second = load_identity(&dir).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.key, second.key);
        assert!(first.id.len() >= 22 && first.key.len() >= 32);
        // Fits a hostname label too: lowercase hex, 128 bits.
        assert_eq!(first.id.len(), 32, "{}", first.id);
        assert!(first.id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{}", first.id);
        assert_eq!(
            public_url("https://relay.example/", &dir).unwrap(),
            format!("https://relay.example/p/{}", first.id)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(identity_path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_identity_from_an_older_release_is_kept_and_junk_is_replaced() {
        let dir = std::env::temp_dir().join(format!("ipu-relay-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // base64url, mixed case: made by a release before hex ids. Never rotated.
        let old = r#"{"id":"AbC-_dEf0123456789abcd","key":"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"}"#;
        std::fs::write(identity_path(&dir), old).unwrap();
        assert_eq!(load_identity(&dir).unwrap().id, "AbC-_dEf0123456789abcd");
        // A file that is not an identity (and, being pre-existing, would make
        // an exclusive create fail) is replaced rather than fatal.
        std::fs::write(identity_path(&dir), "not json").unwrap();
        let fresh = load_identity(&dir).unwrap();
        assert_eq!(fresh.id.len(), 32);
        assert_eq!(load_identity(&dir).unwrap().id, fresh.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_line_does_not_carry_the_whole_id() {
        assert_eq!(redacted_public_path("0123456789abcdef0123456789abcdef"), "/p/0123…");
    }

    /// What the relay side of a test looks like: a WebSocket server that
    /// answers pings and sends whatever frames the test hands it, and a
    /// stand-in for this daemon on loopback that accepts, reads, and never
    /// answers, reporting when the tunnel drops the connection (the task
    /// serving that request was aborted).
    struct Harness {
        frames: mpsc::Sender<String>,
        daemon_events: mpsc::Receiver<DaemonEvent>,
        tunnel: tokio::task::JoinHandle<Result<(), String>>,
        dir: PathBuf,
    }

    #[derive(Debug, PartialEq)]
    enum DaemonEvent {
        Connected(usize),
        Dropped(usize),
    }

    async fn harness(name: &str) -> Harness {
        // The daemon the tunnel replays requests against.
        let daemon = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = daemon.local_addr().unwrap().port();
        let (events_tx, daemon_events) = mpsc::channel::<DaemonEvent>(16);
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let (mut tcp, _) = daemon.accept().await.unwrap();
                n += 1;
                let events_tx = events_tx.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let _ = events_tx.send(DaemonEvent::Connected(n)).await;
                    let mut buf = [0u8; 4096];
                    // Read the request (and never answer) until the peer goes away.
                    while let Ok(read) = tcp.read(&mut buf).await {
                        if read == 0 {
                            break;
                        }
                    }
                    let _ = events_tx.send(DaemonEvent::Dropped(n)).await;
                });
            }
        });
        // The relay.
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let (frames, mut frames_rx) = mpsc::channel::<String>(16);
        tokio::spawn(async move {
            let (tcp, _) = relay.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            loop {
                tokio::select! {
                    incoming = ws.next() => match incoming {
                        Some(Ok(Message::Text(text))) if text.as_str() == "ping" => {
                            let _ = ws.send(Message::Text("pong".into())).await;
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                    frame = frames_rx.recv() => match frame {
                        Some(text) => { let _ = ws.send(Message::Text(text.into())).await; }
                        None => { let _ = ws.close(None).await; break; }
                    },
                }
            }
        });
        let dir = std::env::temp_dir().join(format!("ipu-relay-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let relay_url = format!("http://{relay_addr}");
        let tunnel = {
            let dir = dir.clone();
            tokio::spawn(async move { run(&relay_url, &dir, port).await })
        };
        Harness { frames, daemon_events, tunnel, dir }
    }

    impl Harness {
        async fn send(&self, frame: serde_json::Value) {
            self.frames.send(frame.to_string()).await.unwrap();
        }

        async fn expect(&mut self, event: DaemonEvent) {
            let got = tokio::time::timeout(Duration::from_secs(5), self.daemon_events.recv())
                .await
                .unwrap_or_else(|_| panic!("waited 5s for {event:?}"));
            assert_eq!(got, Some(event));
        }

        async fn nothing_for(&mut self, wait: Duration) {
            if let Ok(event) = tokio::time::timeout(wait, self.daemon_events.recv()).await {
                panic!("unexpected {event:?}");
            }
        }

        async fn close(self) {
            drop(self.frames);
            let result = tokio::time::timeout(Duration::from_secs(5), self.tunnel).await.unwrap().unwrap();
            assert_eq!(result, Ok(()), "a relay that closes cleanly ends the session without error");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn req(id: u32) -> serde_json::Value {
        serde_json::json!({ "t": "req", "id": id, "m": "GET", "p": "/agent/slow", "h": [["x-test", "1"]] })
    }

    /// The relay handed out an id that is still being answered (it restarted
    /// and lost count): the request under the old id is stopped so that its
    /// answer cannot be taken for the new one.
    #[test]
    fn a_request_id_used_again_stops_the_request_still_running_under_it() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut h = harness("reuse").await;
                h.send(req(7)).await;
                h.expect(DaemonEvent::Connected(1)).await;
                h.send(req(7)).await;
                h.expect(DaemonEvent::Dropped(1)).await;
                h.expect(DaemonEvent::Connected(2)).await;
                // A different id leaves the running one alone.
                h.send(req(8)).await;
                h.expect(DaemonEvent::Connected(3)).await;
                h.nothing_for(Duration::from_millis(300)).await;
                h.close().await;
            });
    }

    /// The relay says `cancel` (the client went away, or 30 s passed): the
    /// request is stopped at once instead of running to completion for nobody.
    #[test]
    fn a_cancel_from_the_relay_stops_the_request() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut h = harness("cancel").await;
                h.send(req(9)).await;
                h.expect(DaemonEvent::Connected(1)).await;
                h.send(req(10)).await;
                h.expect(DaemonEvent::Connected(2)).await;
                h.send(serde_json::json!({ "t": "cancel", "id": 9 })).await;
                h.expect(DaemonEvent::Dropped(1)).await;
                // Cancelling an id nobody is serving is harmless.
                h.send(serde_json::json!({ "t": "cancel", "id": 999 })).await;
                h.nothing_for(Duration::from_millis(300)).await;
                h.close().await;
            });
    }

    /// The tunnel session ends (the relay closed it): every request still in
    /// flight is dropped with it, the next session starts clean.
    #[test]
    fn requests_in_flight_end_with_the_session() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut h = harness("session-end").await;
                h.send(req(11)).await;
                h.expect(DaemonEvent::Connected(1)).await;
                drop(std::mem::replace(&mut h.frames, mpsc::channel(1).0));
                h.expect(DaemonEvent::Dropped(1)).await;
                h.close().await;
            });
    }
}
