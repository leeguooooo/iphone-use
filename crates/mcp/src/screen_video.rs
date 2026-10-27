//! Bounded MCP relay for the daemon's native H.264 stream. No iframe credentials.
use crate::client::DaemonClient;
use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use rmcp::model::{CallToolResult, Content};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Mutex;

const MAX_PACKET: usize = 512 * 1024;
const MAX_BATCH: usize = 768 * 1024;
const IDLE: Duration = Duration::from_secs(2);
// Rotate well before DaemonClient's 15-second absolute request deadline.
const CONNECTION_AGE: Duration = Duration::from_secs(10);
const FRAME_GAP: Duration = Duration::from_secs(3);

#[derive(Default, Clone)]
pub struct Relay(Arc<RelayState>);
#[derive(Default)]
struct RelayState {
    slot: Mutex<Option<Active>>,
    polling: Mutex<()>,
}
struct Active {
    response: reqwest::Response,
    parser: Parser,
    scope: String,
    serial: u64,
    opened_at: tokio::time::Instant,
    last_packet: tokio::time::Instant,
}
#[derive(Default)]
struct Parser {
    bytes: Vec<u8>,
    need_key: bool,
}
impl Parser {
    fn push(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if self.bytes.len() + bytes.len() > MAX_BATCH + MAX_PACKET {
            bail!("video buffer limit exceeded");
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn packet(&mut self) -> anyhow::Result<Option<Value>> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let size = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
        if !(10..=MAX_PACKET).contains(&size) {
            bail!("invalid video packet length");
        }
        if self.bytes.len() < size + 4 {
            return Ok(None);
        }
        let flags = self.bytes[4];
        if flags & !1 != 0 {
            bail!("unknown video packet flags");
        }
        let key = flags & 1 != 0;
        let pts = u64::from_be_bytes(self.bytes[5..13].try_into().unwrap());
        // JavaScript timestamps must remain exact.
        if pts > (1u64 << 53) - 1 {
            bail!("video timestamp exceeds exact range");
        }
        let packet = (!self.need_key || key).then(|| {
            json!({
                "keyframe":key,"pts_micros":pts,"data":STANDARD.encode(&self.bytes[13..size+4])
            })
        });
        self.bytes.drain(..size + 4);
        if key {
            self.need_key = false;
        }
        Ok(packet)
    }
}
impl Relay {
    pub async fn frame(&self, daemon: &DaemonClient, reset: bool) -> CallToolResult {
        let (status, video) = match tokio::time::timeout(Duration::from_secs(6), async {
            tokio::join!(daemon.status(), self.poll(daemon, reset))
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let mut result =
                    CallToolResult::success(vec![Content::text("iphone-use video timed out")]);
                result.structured_content = Some(
                    json!({"ok":false,"clear_frame":true,"video_unavailable":true,"error":"video timed out"}),
                );
                return result;
            }
        };
        let mut body = match status {
            Ok(status) => crate::screen::status_summary(&status, daemon.owner()),
            Err(_) => json!({"error":"status unavailable"}),
        };
        body["frame"] = Value::Null;
        body["ok"] = false.into();
        match video {
            Ok(video)
                if body["private_secs"].is_null()
                    && body["error"].is_null()
                    && body["released"] != true =>
            {
                body["video"] = video;
                body["ok"] = true.into();
            }
            Ok(_) => {
                body["clear_frame"] = true.into();
            }
            Err(_) => {
                body["clear_frame"] = true.into();
                body["video_unavailable"] = true.into();
                body["error"] = "native video unavailable; using screenshots".into();
            }
        }
        let mut result = CallToolResult::success(vec![Content::text("iphone-use screen video")]);
        result.structured_content = Some(body);
        result
    }

    /// Sequential viewer only. Cancellation owns/drops its transport and parser;
    /// an idle panel loses its subscription after two seconds, including if its
    /// host never sends teardown. A new connection must start at a keyframe.
    pub async fn poll(&self, daemon: &DaemonClient, reset: bool) -> anyhow::Result<Value> {
        let _polling = self
            .0
            .polling
            .try_lock()
            .context("video poll already in flight")?;
        // An idle cleanup briefly touches the slot too; it is not another poll.
        let mut slot = self.0.slot.lock().await;
        let mut active = slot.take();
        if reset {
            active = None;
        }
        let before = daemon.screen_scope().await?;
        let mut restarted = active.is_none();
        if active.as_ref().is_some_and(|v| v.scope != before) {
            // Never deliver queued data from a former privacy incarnation.
            bail!("screen scope changed; clear video");
        }
        if active
            .as_ref()
            .is_some_and(|value| value.opened_at.elapsed() >= CONNECTION_AGE)
        {
            active = None;
        }
        if active.is_none() {
            let response = daemon.screen_video().await?;
            if response
                .headers()
                .get("x-video-format")
                .and_then(|v| v.to_str().ok())
                != Some("iphone-use-h264-annexb-v1")
            {
                bail!("unsupported video format");
            }
            let scope = response
                .headers()
                .get("x-ipu-screen-scope")
                .and_then(|v| v.to_str().ok())
                .context("video scope missing")?
                .to_owned();
            if scope != before {
                bail!("screen scope changed; clear video");
            }
            active = Some(Active {
                response,
                parser: Parser {
                    need_key: true,
                    ..Default::default()
                },
                scope,
                serial: 0,
                opened_at: tokio::time::Instant::now(),
                last_packet: tokio::time::Instant::now(),
            });
            restarted = true;
        }
        let mut active = active.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        let mut packets = Vec::new();
        let mut encoded = 0;
        for _ in 0..64 {
            let prior = active.parser.bytes.len();
            if let Some(packet) = active.parser.packet()? {
                active.last_packet = tokio::time::Instant::now();
                encoded += packet["data"].as_str().unwrap().len();
                packets.push(packet);
                if packets.len() >= 16 || encoded >= MAX_BATCH {
                    break;
                }
                continue;
            }
            if active.parser.bytes.len() < prior {
                continue;
            }
            match tokio::time::timeout_at(deadline, active.response.chunk()).await {
                Ok(Ok(Some(bytes))) => active.parser.push(&bytes)?,
                Ok(Ok(None)) => bail!("video stream ended; clear video"),
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => break,
            }
        }
        if active.last_packet.elapsed() >= FRAME_GAP {
            bail!("native video stopped producing frames");
        }
        // Capture may have been revoked while its transport delivered buffered
        // packets. A fresh status fence is mandatory before publishing any batch.
        if daemon.screen_scope().await? != active.scope {
            bail!("screen scope changed; clear video");
        }
        let scope = active.scope.clone();
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed) + 1;
        active.serial = serial;
        *slot = Some(active);
        drop(slot);
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE).await;
            if let Some(state) = weak.upgrade() {
                let mut state = state.slot.lock().await;
                if state.as_ref().is_some_and(|value| value.serial == serial) {
                    *state = None;
                }
            }
        });
        Ok(
            json!({"format":"iphone-use-h264-annexb-v1","scope":scope,"reset":restarted,"packets":packets}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(key: bool) -> Vec<u8> {
        let mut bytes = 13u32.to_be_bytes().to_vec();
        bytes.push(u8::from(key));
        bytes.extend_from_slice(&1234u64.to_be_bytes());
        bytes.extend_from_slice(&[0, 0, 1, 0x65]);
        bytes
    }
    async fn daemon(
        change_scope: bool,
    ) -> (DaemonClient, tokio::task::JoinHandle<()>, Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let epoch = Arc::new(AtomicU64::new(0));
        let shared = epoch.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let epoch = shared.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    loop {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        request.push(byte[0]);
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 8192 {
                            return;
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    assert!(
                        !request.contains("x-phone-control"),
                        "viewer cannot control"
                    );
                    assert!(request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fixture"));
                    if request.starts_with("GET /agent/h264?") {
                        let header=format!("HTTP/1.1 200 OK\r\nx-video-format: iphone-use-h264-annexb-v1\r\nx-ipu-screen-scope: boot-{}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",epoch.load(Ordering::Acquire));
                        stream.write_all(header.as_bytes()).await.unwrap();
                        let packet = frame(true);
                        stream
                            .write_all(format!("{:x}\r\n", packet.len()).as_bytes())
                            .await
                            .unwrap();
                        stream.write_all(&packet).await.unwrap();
                        stream.write_all(b"\r\n").await.unwrap();
                        if change_scope {
                            epoch.fetch_add(1, Ordering::AcqRel);
                        }
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                        loop {
                            let mut byte = [0];
                            tokio::select! {
                                _=stream.read(&mut byte)=>break,
                                _=tokio::time::sleep_until(deadline)=>break,
                                _=tokio::time::sleep(Duration::from_secs(1))=>{
                                    let packet=frame(false);
                                    if stream.write_all(format!("{:x}\r\n",packet.len()).as_bytes()).await.is_err()
                                        || stream.write_all(&packet).await.is_err()
                                        || stream.write_all(b"\r\n").await.is_err(){break;}
                                }
                            }
                        }
                    } else {
                        assert!(request.starts_with("GET /agent/status "));
                        let response=format!("HTTP/1.1 200 OK\r\nx-ipu-screen-scope: boot-{}\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{{\"ok\":true}}",epoch.load(Ordering::Acquire));
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        (DaemonClient::new(url, Some("fixture".into())), task, epoch)
    }
    #[tokio::test]
    async fn app_result_is_video_only_without_credentials() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        let result = relay.frame(&daemon, true).await;
        let body = result.structured_content.unwrap();
        assert_eq!(body["ok"], true);
        assert!(body["frame"].is_null());
        assert_eq!(body["video"]["packets"].as_array().unwrap().len(), 1);
        assert!(!body.to_string().contains("fixture"));
        assert!(!body.to_string().contains("127.0.0.1"));
        assert_eq!(result.content.len(), 1);
        task.abort();
    }
    #[tokio::test]
    async fn video_errors_do_not_disclose_daemon_urls_or_credentials() {
        let daemon = DaemonClient::new("http://127.0.0.1:1", Some("private-fixture-token".into()));
        let result = Relay::default().frame(&daemon, true).await;
        let body = result.structured_content.unwrap();
        assert_eq!(body["clear_frame"], true);
        assert_eq!(body["video_unavailable"], true);
        assert!(body["video"].is_null());
        assert!(!body.to_string().contains("127.0.0.1"));
        assert!(!body.to_string().contains("private-fixture-token"));
    }
    #[tokio::test]
    async fn batches_reuse_transport_and_idle_viewer_drops_it() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        let first = relay.poll(&daemon, false).await.unwrap();
        assert_eq!(first["reset"], true);
        assert_eq!(first["packets"].as_array().unwrap().len(), 1);
        let second = relay.poll(&daemon, false).await.unwrap();
        assert_eq!(second["reset"], false);
        assert!(second["packets"].as_array().unwrap().is_empty());
        tokio::time::sleep(IDLE + Duration::from_millis(100)).await;
        assert!(relay.0.slot.lock().await.is_none());
        task.abort();
    }
    #[tokio::test]
    async fn continuous_view_rotates_before_http_deadline_without_png_fallback() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        let started = tokio::time::Instant::now();
        let mut resets = 0;
        let mut polls = 0;
        while started.elapsed() < Duration::from_secs(16) {
            let body = relay
                .poll(&daemon, false)
                .await
                .expect("continuous viewing must not hit the 15-second HTTP deadline");
            resets += usize::from(body["reset"] == true);
            polls += 1;
        }
        assert!(resets >= 2, "a new transport/keyframe must precede expiry");
        assert!(polls > 20);
        task.abort();
    }

    #[tokio::test]
    async fn two_viewers_keep_independent_subscriptions() {
        let (daemon, task, _) = daemon(false).await;
        let first = Relay::default();
        let second = Relay::default();
        let (a, b) = tokio::join!(first.poll(&daemon, false), second.poll(&daemon, false));
        for body in [a.unwrap(), b.unwrap()] {
            assert_eq!(body["reset"], true);
            assert_eq!(body["packets"].as_array().unwrap().len(), 1);
        }
        first.poll(&daemon, true).await.unwrap();
        let b = second.poll(&daemon, false).await.unwrap();
        assert_eq!(
            b["reset"], false,
            "one viewer reconnecting cannot reset another"
        );
        task.abort();
    }

    #[tokio::test]
    async fn idle_cleanup_lock_is_not_a_second_foreground_poll() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        let lock = relay.0.slot.lock().await;
        let reader = relay.clone();
        let poll = tokio::spawn(async move { reader.poll(&daemon, false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !poll.is_finished(),
            "cleanup may briefly own the state slot"
        );
        drop(lock);
        assert!(poll.await.unwrap().is_ok());
        task.abort();
    }

    #[tokio::test]
    async fn stalled_video_is_not_returned_as_a_healthy_empty_batch() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        relay.poll(&daemon, false).await.unwrap();
        relay.0.slot.lock().await.as_mut().unwrap().last_packet =
            tokio::time::Instant::now() - FRAME_GAP;
        let body = relay
            .frame(&daemon, false)
            .await
            .structured_content
            .unwrap();
        assert_eq!(body["clear_frame"], true);
        assert_eq!(body["video_unavailable"], true);
        assert!(body["video"].is_null());
        assert!(relay.0.slot.lock().await.is_none());
        task.abort();
    }

    #[tokio::test]
    async fn post_read_scope_change_discards_buffered_packets() {
        let (daemon, task, _) = daemon(true).await;
        let relay = Relay::default();
        assert!(relay
            .poll(&daemon, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("scope changed"));
        assert!(relay.0.slot.lock().await.is_none());
        task.abort();
    }
    #[tokio::test]
    async fn pre_read_scope_change_drops_existing_subscription() {
        let (daemon, task, epoch) = daemon(false).await;
        let relay = Relay::default();
        relay.poll(&daemon, false).await.unwrap();
        epoch.fetch_add(2, Ordering::AcqRel);
        assert!(relay.poll(&daemon, false).await.is_err());
        assert!(relay.0.slot.lock().await.is_none());
        task.abort();
    }
    #[tokio::test]
    async fn cancelled_read_does_not_retain_a_subscription() {
        let (daemon, task, _) = daemon(false).await;
        let relay = Relay::default();
        relay.poll(&daemon, false).await.unwrap();
        let work = relay.clone();
        let handle = tokio::spawn(async move { work.poll(&daemon, false).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        handle.abort();
        let _ = handle.await;
        assert!(relay.0.slot.lock().await.is_none());
        task.abort();
    }
    #[test]
    fn fragmented_packet_and_keyframe_gate() {
        let mut parser = Parser {
            need_key: true,
            ..Default::default()
        };
        parser.push(&frame(false)).unwrap();
        assert!(parser.packet().unwrap().is_none());
        assert!(parser.bytes.is_empty());
        let key = frame(true);
        for byte in &key[..key.len() - 1] {
            parser.push(&[*byte]).unwrap();
            assert!(parser.packet().unwrap().is_none());
        }
        parser.push(&key[key.len() - 1..]).unwrap();
        let result = parser.packet().unwrap().unwrap();
        assert_eq!(result["keyframe"], true);
        assert_eq!(result["pts_micros"], 1234);
        assert!(parser.bytes.is_empty());
        parser.push(&frame(false)).unwrap();
        assert!(parser.packet().unwrap().is_some());
    }
    #[test]
    fn malformed_frames_and_oversize_buffers_refuse() {
        for size in [0, 9, (MAX_PACKET + 1) as u32, u32::MAX] {
            let mut parser = Parser::default();
            parser.push(&size.to_be_bytes()).unwrap();
            assert!(parser.packet().is_err());
        }
        let mut bytes = frame(true);
        bytes[4] = 2;
        let mut parser = Parser::default();
        parser.push(&bytes).unwrap();
        assert!(parser.packet().is_err());
        assert!(Parser::default()
            .push(&vec![0; MAX_BATCH + MAX_PACKET + 1])
            .is_err());
    }
}
