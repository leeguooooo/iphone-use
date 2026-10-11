//! Screen recording: the live H.264 stream written to an MP4.
//!
//! A recording subscribes to the [`VideoHub`] like any viewer (so the
//! pipeline runs while it records), asks for a keyframe, drops everything
//! before the first IDR, and streams each access unit into an
//! [`Mp4Writer`] on disk (`<dir>/<UTC time>.mp4`, written as `.part` and
//! renamed when complete). It ends on [`Recording::stop`], or by itself at
//! the [`Limits`] (10 minutes / 200 MB by default) or when the stream goes
//! away; `stop` then still returns the finished file.
//!
//! Timestamps: the stream's `pts_micros` paces the file, except where it
//! disagrees with the wall clock by more than a second (the upstream
//! restarted, or switched mode, and its clock with it); the wall clock wins
//! there. A viewer that falls behind loses frames, so after a lag the
//! recording waits for the next keyframe (and asks for one) rather than
//! writing frames whose references are gone.
//!
//! One recording per daemon: [`Recorder::global`] holds it.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Serialize;
use tokio::sync::{broadcast, oneshot};

use crate::mp4::Mp4Writer;
use crate::video::{H264Frame, VideoHub, VideoMode};

/// When a recording stops by itself.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_duration: Duration,
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(10 * 60),
            max_bytes: 200 * 1024 * 1024,
        }
    }
}

/// A finished recording.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingSummary {
    pub path: PathBuf,
    pub seconds: f64,
    pub frames: u64,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    /// Why it ended before `stop`: "max_duration", "max_bytes" or
    /// "stream_ended". `None` when stopped on request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<&'static str>,
}

/// A running recording, as far as it got.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingStatus {
    pub path: PathBuf,
    pub seconds: f64,
    pub frames: u64,
    pub bytes: u64,
    /// It already stopped by itself (a limit, or the stream ended).
    pub ended: Option<&'static str>,
}

/// Where frames come from: the [`VideoHub`], or a fake in tests.
pub trait FrameSource: Send + Sync + 'static {
    /// A receiver of access units, and a guard that keeps the subscription
    /// counted while it lives.
    fn subscribe(&self) -> (broadcast::Receiver<H264Frame>, Box<dyn Send>);
    fn request_keyframe(&self);
}

impl FrameSource for Arc<VideoHub> {
    fn subscribe(&self) -> (broadcast::Receiver<H264Frame>, Box<dyn Send>) {
        let mut subscription = VideoHub::subscribe(self, VideoMode::Performance);
        // `Subscription` implements Drop, so swap the receiver out and keep
        // the rest as the guard.
        let (_, placeholder) = broadcast::channel(1);
        let frames = std::mem::replace(&mut subscription.frames, placeholder);
        (frames, Box::new(subscription))
    }

    fn request_keyframe(&self) {
        VideoHub::request_keyframe(self);
    }
}

#[derive(Debug, Default)]
struct Progress {
    frames: u64,
    bytes: u64,
    micros: u64,
    ended: Option<&'static str>,
}

/// A recording in progress.
pub struct Recording {
    path: PathBuf,
    progress: Arc<Mutex<Progress>>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<RecordingSummary>>,
}

impl Recording {
    /// Start recording `hub` into a new file in `dir` (created if missing).
    pub fn start(hub: Arc<VideoHub>, dir: &Path, limits: Limits) -> anyhow::Result<Self> {
        Self::start_from(hub, dir, limits)
    }

    /// [`Recording::start`] with any frame source.
    pub fn start_from<S: FrameSource>(
        source: S,
        dir: &Path,
        limits: Limits,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create recordings dir {}", dir.display()))?;
        let path = new_path(dir);
        let part = path.with_extension("mp4.part");
        let file = File::create(&part).with_context(|| format!("create {}", part.display()))?;
        let writer = Mp4Writer::new(BufWriter::new(file)).context("write MP4 header")?;
        let (frames, guard) = source.subscribe();
        source.request_keyframe();
        let progress = Arc::new(Mutex::new(Progress::default()));
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(record(
            source,
            frames,
            guard,
            writer,
            part,
            path.clone(),
            limits,
            Arc::clone(&progress),
            stop_rx,
        ));
        Ok(Self {
            path,
            progress,
            stop: Some(stop_tx),
            task,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn status(&self) -> RecordingStatus {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        RecordingStatus {
            path: self.path.clone(),
            seconds: progress.micros as f64 / 1e6,
            frames: progress.frames,
            bytes: progress.bytes,
            ended: progress.ended,
        }
    }

    /// Stop and finish the file. Fails (and leaves no file) when no frame
    /// was recorded.
    pub async fn stop(mut self) -> anyhow::Result<RecordingSummary> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        (&mut self.task).await.context("recording task failed")?
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Dropped without `stop`: the task still finishes the file.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// `<dir>/<UTC time>.mp4`, with a counter when that name is taken.
pub(crate) fn new_path(dir: &Path) -> PathBuf {
    let stamp = crate::apps::now_rfc3339().replace(':', "-");
    let mut path = dir.join(format!("{stamp}.mp4"));
    let mut n = 2;
    while path.exists() || path.with_extension("mp4.part").exists() {
        path = dir.join(format!("{stamp}-{n}.mp4"));
        n += 1;
    }
    path
}

/// Maps the stream's timestamps onto a monotonic recording clock.
struct Timeline {
    last: Option<(u64, Instant, u64)>, // (source pts, arrival, recording µs)
}

impl Timeline {
    fn place(&mut self, pts: u64, now: Instant) -> u64 {
        let micros = match self.last {
            None => 0,
            Some((last_pts, last_at, last_micros)) => {
                let wall = now.duration_since(last_at).as_micros() as u64;
                let delta = match pts.checked_sub(last_pts) {
                    Some(delta) if delta.abs_diff(wall) <= 1_000_000 => delta,
                    _ => wall,
                };
                last_micros + delta.max(1)
            }
        };
        self.last = Some((pts, now, micros));
        micros
    }
}

#[allow(clippy::too_many_arguments)]
async fn record<S: FrameSource>(
    source: S,
    mut frames: broadcast::Receiver<H264Frame>,
    guard: Box<dyn Send>,
    mut writer: Mp4Writer<BufWriter<File>>,
    part: PathBuf,
    path: PathBuf,
    limits: Limits,
    progress: Arc<Mutex<Progress>>,
    mut stop: oneshot::Receiver<()>,
) -> anyhow::Result<RecordingSummary> {
    let started = Instant::now();
    let mut timeline = Timeline { last: None };
    let mut need_keyframe = true;
    let mut ended = None;
    let deadline = tokio::time::sleep(limits.max_duration);
    tokio::pin!(deadline);
    let outcome: anyhow::Result<()> = loop {
        let frame = tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = &mut deadline => {
                ended = Some("max_duration");
                break Ok(());
            }
            frame = frames.recv() => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                need_keyframe = true;
                source.request_keyframe();
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                ended = Some("stream_ended");
                break Ok(());
            }
        };
        if need_keyframe {
            if !(frame.keyframe || crate::mp4::is_idr(&frame.data)) {
                continue;
            }
            need_keyframe = false;
        }
        let micros = timeline.place(frame.pts_micros, Instant::now());
        if Duration::from_micros(micros) > limits.max_duration {
            ended = Some("max_duration");
            break Ok(());
        }
        if writer.bytes() + frame.data.len() as u64 > limits.max_bytes {
            ended = Some("max_bytes");
            break Ok(());
        }
        // Small synchronous writes into a BufWriter: frames are tens of KB.
        match writer.push(&frame.data, micros) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => break Err(anyhow::Error::new(error).context("write recording")),
        }
        let mut progress = progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.frames = writer.frames();
        progress.bytes = writer.bytes();
        progress.micros = writer.elapsed_micros();
    };
    drop(guard);
    drop(frames);
    if ended.is_some() {
        progress.lock().unwrap_or_else(|e| e.into_inner()).ended = ended;
        // Keep the task alive until `stop`, so the summary is collected.
        let _ = stop.await;
    }
    let result = outcome.and_then(|()| {
        let (out, summary) = writer
            .finish()
            .map_err(|error| anyhow::anyhow!("no video frames were recorded ({error})"))?;
        out.into_inner()
            .map_err(|error| error.into_error())
            .and_then(|file| file.sync_all())
            .context("flush recording")?;
        std::fs::rename(&part, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(summary)
    });
    match result {
        Ok(summary) => Ok(RecordingSummary {
            path,
            seconds: summary.duration_micros as f64 / 1e6,
            frames: summary.frames,
            bytes: summary.bytes,
            width: summary.width,
            height: summary.height,
            ended,
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&part);
            tracing::debug!(
                "recording ended without a file after {:?}",
                started.elapsed()
            );
            Err(error)
        }
    }
}

/// The daemon's one recording slot.
#[derive(Default)]
pub struct Recorder {
    current: tokio::sync::Mutex<Option<Recording>>,
}

impl Recorder {
    /// The process-wide recorder.
    pub fn global() -> &'static Recorder {
        static RECORDER: OnceLock<Recorder> = OnceLock::new();
        RECORDER.get_or_init(Recorder::default)
    }

    /// Start recording into `dir`. Fails while another recording runs.
    pub async fn start(
        &self,
        hub: Arc<VideoHub>,
        dir: &Path,
        limits: Limits,
    ) -> anyhow::Result<RecordingStatus> {
        self.start_from(hub, dir, limits).await
    }

    pub async fn start_from<S: FrameSource>(
        &self,
        source: S,
        dir: &Path,
        limits: Limits,
    ) -> anyhow::Result<RecordingStatus> {
        let mut current = self.current.lock().await;
        if let Some(recording) = current.as_ref() {
            anyhow::bail!(
                "already recording to {} (stop it first)",
                recording.path().display()
            );
        }
        let recording = Recording::start_from(source, dir, limits)?;
        let status = recording.status();
        *current = Some(recording);
        Ok(status)
    }

    /// Stop the current recording; `Ok(None)` when none runs.
    pub async fn stop(&self) -> anyhow::Result<Option<RecordingSummary>> {
        let recording = self.current.lock().await.take();
        match recording {
            Some(recording) => recording.stop().await.map(Some),
            None => Ok(None),
        }
    }

    /// The current recording's progress, if any.
    pub async fn status(&self) -> Option<RecordingStatus> {
        self.current.lock().await.as_ref().map(Recording::status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    struct FakeSource {
        tx: broadcast::Sender<H264Frame>,
        keyframe_requests: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FrameSource for FakeSource {
        fn subscribe(&self) -> (broadcast::Receiver<H264Frame>, Box<dyn Send>) {
            (self.tx.subscribe(), Box::new(()))
        }
        fn request_keyframe(&self) {
            self.keyframe_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn fake() -> (
        FakeSource,
        broadcast::Sender<H264Frame>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (tx, _) = broadcast::channel(256);
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = FakeSource {
            tx: tx.clone(),
            keyframe_requests: Arc::clone(&requests),
        };
        (source, tx, requests)
    }

    /// The crate's `core` dependency shadows std's `core`, which
    /// `#[tokio::test]` expands to, so tests build their own runtime.
    fn block_on<F: std::future::Future<Output = ()>>(test: F) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(test);
    }

    /// Wait until the recording has written `frames` samples.
    async fn wait_for(recorder: &Recorder, frames: u64) {
        for _ in 0..500 {
            if recorder.status().await.is_some_and(|s| s.frames >= frames) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("recording never reached {frames} frames");
    }

    fn frames_of(stream: &[u8]) -> Vec<H264Frame> {
        crate::mp4::tests::access_units(stream)
            .into_iter()
            .enumerate()
            .map(|(index, unit)| H264Frame {
                keyframe: crate::mp4::is_idr(&unit),
                data: Bytes::from(unit),
                pts_micros: 50_000_000 + index as u64 * 1_000_000 / 30,
            })
            .collect()
    }

    #[test]
    fn the_timeline_follows_pts_unless_the_wall_clock_disagrees() {
        let mut timeline = Timeline { last: None };
        let t0 = Instant::now();
        assert_eq!(timeline.place(9_000_000, t0), 0);
        assert_eq!(
            timeline.place(9_033_333, t0 + Duration::from_millis(30)),
            33_333
        );
        // The upstream restarted: its clock went back to zero.
        assert_eq!(timeline.place(0, t0 + Duration::from_millis(80)), 83_333);
        // A duplicate timestamp still moves forward.
        assert_eq!(timeline.place(0, t0 + Duration::from_millis(80)), 83_334);
    }

    #[test]
    fn a_recording_starts_at_a_keyframe_and_ffprobe_reads_it() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let Some(stream) = crate::mp4::tests::ffmpeg_clip(dir.path()) else {
                return;
            };
            let frames = frames_of(&stream);
            let (source, tx, requests) = fake();
            let recorder = Recorder::default();
            let out = dir.path().join("recordings");
            recorder
                .start_from(source, &out, Limits::default())
                .await
                .unwrap();
            assert!(recorder
                .start_from(fake().0, &out, Limits::default())
                .await
                .is_err());
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
            // Start mid-GOP: frames 10..30 are P frames and get dropped.
            for frame in &frames[10..] {
                tx.send(frame.clone()).unwrap();
            }
            wait_for(&recorder, 30).await;
            let summary = recorder.stop().await.unwrap().unwrap();
            assert_eq!(summary.frames, 30);
            assert_eq!(summary.ended, None);
            assert_eq!((summary.width, summary.height), (320, 240));
            assert!((summary.seconds - 1.0).abs() < 0.01, "{}", summary.seconds);
            assert_eq!(summary.path.extension().unwrap(), "mp4");
            assert_eq!(
                summary.bytes,
                std::fs::metadata(&summary.path).unwrap().len()
            );
            assert!(recorder.stop().await.unwrap().is_none());

            let probe = crate::mp4::tests::ffprobe(&summary.path);
            assert_eq!(probe["streams"][0]["codec_name"], "h264");
            assert_eq!(probe["streams"][0]["nb_read_frames"], "30");
        })
    }

    #[test]
    fn a_limit_ends_the_recording_and_stop_still_returns_it() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let Some(stream) = crate::mp4::tests::ffmpeg_clip(dir.path()) else {
                return;
            };
            let frames = frames_of(&stream);
            let (source, tx, _) = fake();
            let recorder = Recorder::default();
            let limits = Limits {
                max_duration: Duration::from_millis(500),
                max_bytes: u64::MAX,
            };
            recorder
                .start_from(source, dir.path(), limits)
                .await
                .unwrap();
            for frame in &frames {
                let _ = tx.send(frame.clone());
            }
            for _ in 0..500 {
                if recorder.status().await.unwrap().ended.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let summary = recorder.stop().await.unwrap().unwrap();
            assert_eq!(summary.ended, Some("max_duration"));
            assert_eq!(summary.frames, 16, "frames 0..=15 fit in 0.5 s");
            assert!(summary.path.exists());
        })
    }

    #[test]
    fn stopping_before_any_keyframe_fails_and_leaves_no_file() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (source, tx, _) = fake();
            let recorder = Recorder::default();
            recorder
                .start_from(source, dir.path(), Limits::default())
                .await
                .unwrap();
            tx.send(H264Frame {
                data: Bytes::from_static(&[0, 0, 0, 1, 0x41, 0x9A]),
                keyframe: false,
                pts_micros: 0,
            })
            .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            let error = recorder.stop().await.unwrap_err();
            assert!(error.to_string().contains("no video frames"), "{error:#}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        })
    }
}
