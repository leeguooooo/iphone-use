//! Live video as H.264.
//!
//! WDA serves the phone screen as MJPEG: every frame a full JPEG, ~120 KiB at
//! 660×1434, which is 25–40 Mbit/s at 28 fps. Fine on a LAN, unusable over a
//! phone's cellular link. This module turns that into a compact H.264 stream:
//! one upstream MJPEG connection per daemon, each JPEG decoded (zune-jpeg) and
//! re-encoded on the Mac's hardware encoder (VideoToolbox), and the encoded
//! access units fanned out to every subscriber.
//!
//! The encoder runs only while somebody watches. A new subscriber forces an
//! IDR so its decoder has an entry point, and a slow encoder never queues:
//! only the newest JPEG waits for it, older ones are dropped.
//!
//! Wire format of `GET /agent/h264` (see [`frame_message`]): a stream of
//! messages, each `[u32 BE length of the rest][u8 flags][u64 BE pts µs][Annex-B
//! access unit]`, flags bit 0 = keyframe. Keyframes carry SPS and PPS in-band,
//! so a client can derive the codec string from the SPS and start decoding at
//! any keyframe.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct H264Frame {
    /// Annex-B bytes; SPS+PPS+IDR on keyframes.
    pub data: Bytes,
    pub keyframe: bool,
    pub pts_micros: u64,
}

/// Flags byte, bit 0.
pub const FLAG_KEYFRAME: u8 = 0x01;

/// Serialize one frame for the `/agent/h264` stream.
pub fn frame_message(frame: &H264Frame) -> Bytes {
    let rest = 1 + 8 + frame.data.len();
    let mut out = Vec::with_capacity(4 + rest);
    out.extend_from_slice(&(rest as u32).to_be_bytes());
    out.push(if frame.keyframe { FLAG_KEYFRAME } else { 0 });
    out.extend_from_slice(&frame.pts_micros.to_be_bytes());
    out.extend_from_slice(&frame.data);
    Bytes::from(out)
}

// ---------------------------------------------------------------------------
// MJPEG splitting
// ---------------------------------------------------------------------------

/// Splits a `multipart/x-mixed-replace` MJPEG byte stream into JPEG frames.
///
/// WDA labels every part with `Content-Length`, which is the exact and cheap
/// path. A part without one falls back to scanning for the JPEG end marker
/// (`FF D9`), which entropy-coded data cannot contain because every `FF` there
/// is followed by a stuffed `00`.
#[derive(Default)]
pub struct MjpegSplitter {
    buf: Vec<u8>,
}

/// Upper bound on buffered bytes, so a stream that never yields a frame (a
/// misconfigured upstream) cannot grow the buffer without limit.
const MAX_BUFFERED: usize = 8 * 1024 * 1024;

impl MjpegSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every complete JPEG they finished.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame() {
            frames.push(frame);
        }
        if self.buf.len() > MAX_BUFFERED {
            self.buf.clear();
        }
        frames
    }

    fn next_frame(&mut self) -> Option<Bytes> {
        let soi = find(&self.buf, &[0xFF, 0xD8])?;
        // Headers of the part this JPEG belongs to: the text between the last
        // blank line before the SOI and the SOI itself.
        let length = content_length(&self.buf[..soi]);
        let end = match length {
            Some(len) if soi + len <= self.buf.len() => soi + len,
            Some(_) => return None, // wait for the rest of the part
            None => soi + 2 + find(&self.buf[soi + 2..], &[0xFF, 0xD9])? + 2,
        };
        let frame = Bytes::copy_from_slice(&self.buf[soi..end]);
        self.buf.drain(..end);
        Some(frame)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `Content-Length` of the most recent part header in `headers`, if any.
fn content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    let part = text.rsplit("--").next().unwrap_or(&text);
    part.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

// ---------------------------------------------------------------------------
// The hub: one upstream, one encoder, many subscribers
// ---------------------------------------------------------------------------

/// How long the pipeline lingers after its last subscriber leaves, so a page
/// reload does not tear down and rebuild the encoder.
const IDLE_LINGER: Duration = Duration::from_secs(10);

pub struct VideoHub {
    mjpeg_url: String,
    tx: tokio::sync::broadcast::Sender<H264Frame>,
    subscribers: Arc<AtomicUsize>,
    force_idr: Arc<AtomicBool>,
    running: Mutex<bool>,
    bitrate: u32,
    /// The live frame's content band is one flat colour (checked every few
    /// frames while someone watches). See [`crate::redaction`].
    frame_blank: Arc<AtomicBool>,
    /// Daemon verdict: the blank frame is a screen the app hides from
    /// capture (its tree has labelled content), not an empty one.
    capture_redacted: AtomicBool,
    /// When the verdict was last decided, and whether a check is running.
    verdict_at: Mutex<Option<std::time::Instant>>,
    verdict_running: AtomicBool,
}

/// Keeps a subscription counted; dropping it lets the pipeline wind down.
pub struct Subscription {
    pub frames: tokio::sync::broadcast::Receiver<H264Frame>,
    subscribers: Arc<AtomicUsize>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl VideoHub {
    pub fn new(mjpeg_url: String) -> Arc<Self> {
        let bitrate = std::env::var("PHONE_REMOTE_H264_KBPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|kbps| (300..=20_000).contains(kbps))
            .unwrap_or(2_500)
            * 1000;
        let (tx, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Self {
            mjpeg_url,
            tx,
            subscribers: Arc::new(AtomicUsize::new(0)),
            force_idr: Arc::new(AtomicBool::new(true)),
            running: Mutex::new(false),
            bitrate,
            frame_blank: Arc::new(AtomicBool::new(false)),
            capture_redacted: AtomicBool::new(false),
            verdict_at: Mutex::new(None),
            verdict_running: AtomicBool::new(false),
        })
    }

    /// Someone is watching and the picture they get is blank.
    pub fn watching_a_blank_frame(&self) -> bool {
        self.subscribers.load(Ordering::Acquire) > 0 && self.frame_blank.load(Ordering::Acquire)
    }

    /// The current verdict, reset as soon as the picture is no longer blank.
    pub fn capture_redacted(&self) -> bool {
        self.watching_a_blank_frame() && self.capture_redacted.load(Ordering::Acquire)
    }

    /// Claim the right to decide the verdict again: the frame is blank and the
    /// last decision is older than `max_age`. Returns false when a check is
    /// already running or none is due.
    pub fn begin_verdict(&self, max_age: Duration) -> bool {
        if !self.watching_a_blank_frame() {
            return false;
        }
        let due = self
            .verdict_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|at| at.elapsed() >= max_age);
        due && self
            .verdict_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn finish_verdict(&self, redacted: Option<bool>) {
        if let Some(redacted) = redacted {
            self.capture_redacted.store(redacted, Ordering::Release);
            *self.verdict_at.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now());
        }
        self.verdict_running.store(false, Ordering::Release);
    }

    /// Whether this build can encode at all (VideoToolbox is macOS-only).
    pub fn supported() -> bool {
        cfg!(target_os = "macos")
    }

    /// Subscribe to the encoded stream, starting the pipeline if needed.
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let frames = self.tx.subscribe();
        self.subscribers.fetch_add(1, Ordering::AcqRel);
        // The newcomer's decoder needs an entry point now, not at the next GOP.
        self.force_idr.store(true, Ordering::Release);
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if !*running {
            *running = true;
            let hub = Arc::clone(self);
            tokio::spawn(async move {
                // `run` clears `running` itself, under the lock, at the moment
                // it decides to stop; clearing it here could clobber a pipeline
                // a newer subscriber already started.
                if let Err(error) = hub.run().await {
                    tracing::warn!("h264 pipeline stopped: {error:#}");
                }
            });
        }
        Subscription {
            frames,
            subscribers: Arc::clone(&self.subscribers),
        }
    }

    /// Ask for an IDR on the next encoded frame (a subscriber fell behind).
    pub fn request_keyframe(&self) {
        self.force_idr.store(true, Ordering::Release);
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::Acquire)
    }

    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let result = self.pipeline().await;
        if result.is_err() {
            // Failed before it could decide to stop on its own.
            self.mark_stopped();
        }
        result
    }

    fn mark_stopped(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = false;
        // No picture any more: nothing is blank, nothing is redacted.
        self.frame_blank.store(false, Ordering::Release);
    }

    async fn pipeline(&self) -> anyhow::Result<()> {
        use futures_util::StreamExt;

        // Latest-wins handoff to the encoder thread: a slow encode drops stale
        // JPEGs instead of queueing them (queueing is latency).
        let slot: Arc<(Mutex<Option<Bytes>>, std::sync::Condvar)> =
            Arc::new((Mutex::new(None), std::sync::Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let encoder_thread = {
            let slot = Arc::clone(&slot);
            let stop = Arc::clone(&stop);
            let tx = self.tx.clone();
            let force_idr = Arc::clone(&self.force_idr);
            let frame_blank = Arc::clone(&self.frame_blank);
            let bitrate = self.bitrate;
            std::thread::Builder::new()
                .name("h264-encoder".into())
                .spawn(move || encoder_loop(slot, stop, tx, force_idr, frame_blank, bitrate))?
        };

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()?;
        let mut idle_since: Option<std::time::Instant> = None;
        'outer: loop {
            if encoder_thread.is_finished() {
                // The encoder could not start (or died): stop, so the next
                // subscriber starts a fresh pipeline instead of feeding a
                // thread that is gone.
                self.mark_stopped();
                anyhow::bail!("the H.264 encoder stopped");
            }
            // A relay that accepts the connection but never answers must not
            // hold the pipeline past its last viewer.
            let sent =
                tokio::time::timeout(Duration::from_secs(5), client.get(&self.mjpeg_url).send())
                    .await;
            let response = match sent {
                Ok(Ok(response)) if response.status().is_success() => response,
                outcome => {
                    match outcome {
                        Ok(Ok(response)) => {
                            tracing::debug!("h264: MJPEG upstream answered {}", response.status())
                        }
                        Ok(Err(error)) => {
                            tracing::debug!("h264: MJPEG upstream unreachable: {error:#}")
                        }
                        Err(_) => tracing::debug!("h264: MJPEG upstream sent no headers in 5 s"),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if self.should_stop(&mut idle_since) {
                        break;
                    }
                    continue;
                }
            };
            let mut body = response.bytes_stream();
            let mut splitter = MjpegSplitter::new();
            loop {
                let next = tokio::time::timeout(Duration::from_secs(1), body.next()).await;
                if self.should_stop(&mut idle_since) {
                    break 'outer;
                }
                if encoder_thread.is_finished() {
                    break; // the check at the top of the outer loop stops us
                }
                let chunk = match next {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(_))) | Ok(None) => break, // reconnect
                    Err(_) => continue,                   // a static screen sends slowly
                };
                if let Some(jpeg) = splitter.push(&chunk).pop() {
                    let (lock, ready) = &*slot;
                    *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(jpeg);
                    ready.notify_one();
                }
            }
        }
        stop.store(true, Ordering::Release);
        slot.1.notify_one();
        let _ = tokio::task::spawn_blocking(move || encoder_thread.join()).await;
        Ok(())
    }

    /// True once nobody has watched for [`IDLE_LINGER`] — and then the
    /// pipeline is already marked stopped. The decision and the mark happen
    /// under the `running` lock, which `subscribe` takes after counting
    /// itself: a newcomer either keeps this pipeline alive or finds it
    /// stopped and starts its own, never attaches to one that is leaving.
    fn should_stop(&self, idle_since: &mut Option<std::time::Instant>) -> bool {
        if self.subscribers.load(Ordering::Acquire) > 0 {
            *idle_since = None;
            return false;
        }
        let since = idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() < IDLE_LINGER {
            return false;
        }
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if self.subscribers.load(Ordering::Acquire) > 0 {
            *idle_since = None;
            return false;
        }
        *running = false;
        true
    }
}

/// Decode the newest JPEG, encode it, repeat until told to stop.
fn encoder_loop(
    slot: Arc<(Mutex<Option<Bytes>>, std::sync::Condvar)>,
    stop: Arc<AtomicBool>,
    tx: tokio::sync::broadcast::Sender<H264Frame>,
    force_idr: Arc<AtomicBool>,
    frame_blank: Arc<AtomicBool>,
    bitrate: u32,
) {
    let started = std::time::Instant::now();
    let mut decoded_frames: u64 = 0;
    let mut encoder: Option<imp::Encoder> = None;
    let mut pixels: Vec<u8> = Vec::new();
    loop {
        let jpeg = {
            let (lock, ready) = &*slot;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if let Some(jpeg) = guard.take() {
                    break jpeg;
                }
                guard = ready
                    .wait_timeout(guard, Duration::from_millis(500))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        let (width, height) = match decode_bgra(&jpeg, &mut pixels) {
            Ok(size) => size,
            Err(error) => {
                tracing::debug!("h264: undecodable JPEG frame: {error}");
                continue;
            }
        };
        // A few times a second is plenty to notice a protected screen.
        if decoded_frames % 10 == 0 {
            frame_blank.store(
                crate::redaction::band_is_flat(width, height, &pixels),
                Ordering::Release,
            );
        }
        decoded_frames += 1;
        if encoder.as_ref().is_none_or(|e| e.size() != (width, height)) {
            // First frame, or the phone rotated: a new size needs a new session.
            encoder = match imp::Encoder::new(width, height, bitrate, tx.clone()) {
                Ok(encoder) => Some(encoder),
                Err(error) => {
                    tracing::warn!("h264: could not start the encoder: {error:#}");
                    return;
                }
            };
            force_idr.store(true, Ordering::Release);
        }
        let pts = started.elapsed().as_micros() as u64;
        let idr = force_idr.swap(false, Ordering::AcqRel);
        if let Some(encoder) = &encoder {
            encoder.encode_bgra(&pixels, pts, idr);
        }
    }
}

/// Decode a JPEG into tightly packed BGRA; returns `(width, height)`.
fn decode_bgra(jpeg: &[u8], out: &mut Vec<u8>) -> Result<(usize, usize), String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(jpeg, options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions().ok_or("no dimensions")?;
    out.resize(width * height * 4, 0);
    decoder.decode_into(out).map_err(|e| e.to_string())?;
    Ok((width, height))
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::H264Frame;

    pub struct Encoder;

    impl Encoder {
        pub fn new(
            _width: usize,
            _height: usize,
            _bitrate: u32,
            _tx: tokio::sync::broadcast::Sender<H264Frame>,
        ) -> anyhow::Result<Self> {
            anyhow::bail!("H.264 encoding needs VideoToolbox (macOS)")
        }
        pub fn size(&self) -> (usize, usize) {
            (0, 0)
        }
        pub fn encode_bgra(&self, _pixels: &[u8], _pts_micros: u64, _keyframe: bool) {}
    }
}

#[cfg(target_os = "macos")]
mod imp {
    //! VideoToolbox H.264 encoder fed with BGRA pixel buffers. The session,
    //! callback and Annex-B conversion follow the encoder the iPhone
    //! Mirroring backend shipped with (hardware-validated), minus the capture.

    use super::H264Frame;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_foundation::{
        kCFBooleanFalse, kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks,
        kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber, CFNumberType, CFRetained,
        CFString, CFType,
    };
    use objc2_core_media::{
        kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, CMBlockBuffer, CMSampleBuffer,
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    };
    use objc2_core_video::{
        kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    use objc2_video_toolbox::{
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
        kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Main_AutoLevel,
        VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    };

    struct Context {
        tx: tokio::sync::broadcast::Sender<H264Frame>,
    }

    pub struct Encoder {
        session: CFRetained<VTCompressionSession>,
        context: *mut Context,
        width: usize,
        height: usize,
    }

    // SAFETY: the session is only driven from the single encoder thread; the
    // context pointer is freed in Drop after the session is invalidated.
    unsafe impl Send for Encoder {}

    impl Encoder {
        pub fn new(
            width: usize,
            height: usize,
            bitrate: u32,
            tx: tokio::sync::broadcast::Sender<H264Frame>,
        ) -> anyhow::Result<Self> {
            let context = Box::into_raw(Box::new(Context { tx }));
            let mut out: *mut VTCompressionSession = std::ptr::null_mut();
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    width as i32,
                    height as i32,
                    kCMVideoCodecType_H264,
                    None,
                    None,
                    None,
                    Some(output_callback),
                    context as *mut c_void,
                    NonNull::new(&mut out).unwrap(),
                )
            };
            if status != 0 || out.is_null() {
                unsafe { drop(Box::from_raw(context)) };
                anyhow::bail!("VTCompressionSessionCreate failed: status={status}");
            }
            let session = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
            unsafe {
                set_bool(&session, kVTCompressionPropertyKey_RealTime, true)?;
                // B-frames would add a frame of latency for no visible gain.
                set_bool(
                    &session,
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    false,
                )?;
                set_string(
                    &session,
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Main_AutoLevel,
                )?;
                set_int(
                    &session,
                    kVTCompressionPropertyKey_AverageBitRate,
                    bitrate as i32,
                )?;
                set_int(&session, kVTCompressionPropertyKey_ExpectedFrameRate, 30)?;
                // A keyframe every 2 s bounds how long a lost frame smears.
                set_int(&session, kVTCompressionPropertyKey_MaxKeyFrameInterval, 60)?;
                let _ = session.prepare_to_encode_frames();
            }
            Ok(Self {
                session,
                context,
                width,
                height,
            })
        }

        pub fn size(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        /// Encode one tightly packed BGRA frame of this encoder's size.
        pub fn encode_bgra(&self, pixels: &[u8], pts_micros: u64, keyframe: bool) {
            let Some(buffer) = pixel_buffer(self.width, self.height, pixels) else {
                return;
            };
            let pts = objc2_core_media::CMTime {
                value: pts_micros as i64,
                timescale: 1_000_000,
                flags: objc2_core_media::CMTimeFlags(1),
                epoch: 0,
            };
            let invalid = objc2_core_media::CMTime {
                value: 0,
                timescale: 0,
                flags: objc2_core_media::CMTimeFlags(0),
                epoch: 0,
            };
            let properties = keyframe.then(force_keyframe);
            let mut info = VTEncodeInfoFlags(0);
            let status = unsafe {
                self.session.encode_frame(
                    &buffer,
                    pts,
                    invalid,
                    properties.as_deref(),
                    std::ptr::null_mut(),
                    &mut info,
                )
            };
            if status != 0 {
                tracing::debug!("VTCompressionSessionEncodeFrame failed: status={status}");
            }
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            let invalid = objc2_core_media::CMTime {
                value: 0,
                timescale: 0,
                flags: objc2_core_media::CMTimeFlags(0),
                epoch: 0,
            };
            unsafe {
                let _ = self.session.complete_frames(invalid);
                self.session.invalidate();
                drop(Box::from_raw(self.context));
            }
        }
    }

    /// Copy packed BGRA rows into a new pixel buffer (its rows may be padded).
    fn pixel_buffer(
        width: usize,
        height: usize,
        pixels: &[u8],
    ) -> Option<CFRetained<CVPixelBuffer>> {
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::new(&mut out).unwrap(),
            )
        };
        if status != 0 || out.is_null() {
            return None;
        }
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
        unsafe {
            if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            let row = width * 4;
            if !base.is_null() && stride >= row && pixels.len() >= row * height {
                for y in 0..height {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(y * row),
                        base.add(y * stride),
                        row,
                    );
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        Some(buffer)
    }

    extern "C-unwind" fn output_callback(
        output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: i32,
        _info_flags: VTEncodeInfoFlags,
        sample_buffer: *mut CMSampleBuffer,
    ) {
        if output_ref_con.is_null() || status != 0 || sample_buffer.is_null() {
            return;
        }
        // SAFETY: the Context outlives the session (freed after invalidate).
        let context = unsafe { &*(output_ref_con as *const Context) };
        // SAFETY: VT guarantees a valid sample for the duration of the call.
        let sample = unsafe { &*sample_buffer };
        match encoded_frame(sample) {
            Ok(frame) => {
                let _ = context.tx.send(frame);
            }
            Err(error) => tracing::debug!("h264 output: {error}"),
        }
    }

    fn is_keyframe(sample: &CMSampleBuffer) -> bool {
        let Some(array) = (unsafe { sample.sample_attachments_array(false) }) else {
            return true;
        };
        if array.count() == 0 {
            return true;
        }
        let dict = unsafe { array.value_at_index(0) };
        if dict.is_null() {
            return true;
        }
        let dict = unsafe { &*(dict as *const CFDictionary) };
        let key = unsafe { kCMSampleAttachmentKey_NotSync } as *const CFString as *const c_void;
        unsafe { dict.value(key) }.is_null()
    }

    fn encoded_frame(sample: &CMSampleBuffer) -> Result<H264Frame, String> {
        let keyframe = is_keyframe(sample);
        let pts = unsafe { sample.presentation_time_stamp() };
        let pts_micros = if pts.timescale != 0 {
            ((pts.value as i128) * 1_000_000 / pts.timescale as i128).max(0) as u64
        } else {
            0
        };
        let mut sps: &[u8] = &[];
        let mut pps: &[u8] = &[];
        let _format_hold;
        if keyframe {
            let format = unsafe { sample.format_description() }
                .ok_or("keyframe without a format description")?;
            let (mut sps_ptr, mut sps_len) = (std::ptr::null(), 0usize);
            let (mut pps_ptr, mut pps_len) = (std::ptr::null(), 0usize);
            let mut count = 0usize;
            let mut nal_header = 0i32;
            let first = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &format,
                    0,
                    &mut sps_ptr,
                    &mut sps_len,
                    &mut count,
                    &mut nal_header,
                )
            };
            let second = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &format,
                    1,
                    &mut pps_ptr,
                    &mut pps_len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if first != 0 || second != 0 || sps_ptr.is_null() || pps_ptr.is_null() {
                return Err(format!("parameter sets unavailable: {first}/{second}"));
            }
            if nal_header != 4 {
                return Err(format!("unexpected NAL length size {nal_header}"));
            }
            // SAFETY: valid while `format` is retained, which `_format_hold` does.
            sps = unsafe { std::slice::from_raw_parts(sps_ptr, sps_len) };
            pps = unsafe { std::slice::from_raw_parts(pps_ptr, pps_len) };
            _format_hold = format;
        }
        let block: CFRetained<CMBlockBuffer> =
            unsafe { sample.data_buffer() }.ok_or("sample without data")?;
        let total = unsafe { block.data_length() };
        let mut data: *mut std::os::raw::c_char = std::ptr::null_mut();
        let (mut at_offset, mut total_len) = (0usize, 0usize);
        let status = unsafe { block.data_pointer(0, &mut at_offset, &mut total_len, &mut data) };
        if status != 0 || data.is_null() || at_offset < total {
            return Err(format!("block buffer unreadable: status={status}"));
        }
        // SAFETY: `total` contiguous bytes at `data` for the call duration.
        let avcc = unsafe { std::slice::from_raw_parts(data as *const u8, total) };
        let annex_b = super::avcc_to_annex_b(avcc, keyframe.then_some((sps, pps)))?;
        Ok(H264Frame {
            data: bytes::Bytes::from(annex_b),
            keyframe,
            pts_micros,
        })
    }

    fn force_keyframe() -> CFRetained<CFDictionary> {
        unsafe {
            let key = kVTEncodeFrameOptionKey_ForceKeyFrame as *const CFString as *const c_void;
            let value = kCFBooleanTrue.expect("kCFBooleanTrue") as *const _ as *const c_void;
            let mut keys = [key];
            let mut values = [value];
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .expect("CFDictionaryCreate")
        }
    }

    unsafe fn set_int(
        session: &VTCompressionSession,
        key: &CFString,
        value: i32,
    ) -> anyhow::Result<()> {
        let number = CFNumber::new(
            None,
            CFNumberType::SInt32Type,
            &value as *const i32 as *const c_void,
        )
        .ok_or_else(|| anyhow::anyhow!("CFNumberCreate failed"))?;
        let status = VTSessionSetProperty(session.as_ref(), key, Some(number.as_ref() as &CFType));
        anyhow::ensure!(
            status == 0,
            "VTSessionSetProperty(int) failed: status={status}"
        );
        Ok(())
    }

    unsafe fn set_bool(
        session: &VTCompressionSession,
        key: &CFString,
        value: bool,
    ) -> anyhow::Result<()> {
        let flag = if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
        .ok_or_else(|| anyhow::anyhow!("kCFBoolean missing"))?;
        let status = VTSessionSetProperty(
            session.as_ref(),
            key,
            Some(&*(flag as *const _ as *const CFType)),
        );
        anyhow::ensure!(
            status == 0,
            "VTSessionSetProperty(bool) failed: status={status}"
        );
        Ok(())
    }

    unsafe fn set_string(
        session: &VTCompressionSession,
        key: &CFString,
        value: &CFString,
    ) -> anyhow::Result<()> {
        let status = VTSessionSetProperty(session.as_ref(), key, Some(value as &CFType));
        anyhow::ensure!(
            status == 0,
            "VTSessionSetProperty(string) failed: status={status}"
        );
        Ok(())
    }
}

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Convert AVCC (`[4-byte BE length][NALU]`…) to Annex-B, prepending SPS and
/// PPS when given (keyframes).
pub fn avcc_to_annex_b(avcc: &[u8], param_sets: Option<(&[u8], &[u8])>) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    if let Some((sps, pps)) = param_sets {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(sps);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(pps);
    }
    let mut i = 0usize;
    while i + 4 <= avcc.len() {
        let len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > avcc.len() {
            return Err(format!("corrupt AVCC: nal_len={len} at {i}"));
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    if i != avcc.len() {
        return Err(format!("trailing AVCC bytes: {i} of {}", avcc.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_verdict_follows_the_live_picture() {
        let hub = VideoHub::new("http://127.0.0.1:1".into());
        // Nobody watching, nothing blank: no check is due.
        assert!(!hub.begin_verdict(Duration::ZERO));
        hub.subscribers.store(1, Ordering::Release);
        assert!(
            !hub.begin_verdict(Duration::ZERO),
            "a live picture needs no verdict"
        );
        hub.frame_blank.store(true, Ordering::Release);
        assert!(hub.begin_verdict(Duration::ZERO));
        assert!(!hub.begin_verdict(Duration::ZERO), "one check at a time");
        hub.finish_verdict(Some(true));
        assert!(hub.capture_redacted());
        assert!(
            !hub.begin_verdict(Duration::from_secs(60)),
            "fresh verdicts are reused"
        );
        // The picture comes back: the verdict no longer applies.
        hub.frame_blank.store(false, Ordering::Release);
        assert!(!hub.capture_redacted());
        // A failed check leaves the old verdict and frees the slot.
        hub.frame_blank.store(true, Ordering::Release);
        assert!(hub.begin_verdict(Duration::ZERO));
        hub.finish_verdict(None);
        assert!(hub.capture_redacted());
        assert!(hub.begin_verdict(Duration::ZERO));
        hub.finish_verdict(Some(false));
        assert!(!hub.capture_redacted());
    }

    use super::*;

    fn part(jpeg: &[u8], with_length: bool) -> Vec<u8> {
        let mut out = b"--BoundaryString\r\nContent-type: image/jpg\r\n".to_vec();
        if with_length {
            out.extend_from_slice(format!("Content-Length: {}\r\n", jpeg.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(jpeg);
        out.extend_from_slice(b"\r\n");
        out
    }

    const JPEG_A: &[u8] = &[0xFF, 0xD8, 1, 2, 3, 0xFF, 0x00, 4, 0xFF, 0xD9];
    const JPEG_B: &[u8] = &[0xFF, 0xD8, 9, 8, 0xFF, 0xD9];

    #[test]
    fn stopping_is_decided_and_marked_under_the_running_lock() {
        let hub = VideoHub::new("http://127.0.0.1:1".into());
        *hub.running.lock().unwrap() = true;
        let long_ago = std::time::Instant::now() - IDLE_LINGER - Duration::from_secs(1);

        // A viewer counted before the decision keeps the pipeline running
        // and resets the idle clock.
        hub.subscribers.store(1, Ordering::Release);
        let mut idle = Some(long_ago);
        assert!(!hub.should_stop(&mut idle));
        assert!(idle.is_none());
        assert!(*hub.running.lock().unwrap());

        // Nobody watching past the linger: stop, and the mark is already
        // down when should_stop returns, so the next subscribe starts afresh.
        hub.subscribers.store(0, Ordering::Release);
        let mut idle = Some(long_ago);
        assert!(hub.should_stop(&mut idle));
        assert!(!*hub.running.lock().unwrap());

        // Inside the linger nothing changes.
        *hub.running.lock().unwrap() = true;
        let mut idle = None;
        assert!(!hub.should_stop(&mut idle));
        assert!(*hub.running.lock().unwrap());
    }

    #[test]
    fn content_length_parts_split_exactly_even_across_chunks() {
        let mut stream = part(JPEG_A, true);
        stream.extend(part(JPEG_B, true));
        let mut splitter = MjpegSplitter::new();
        let mut frames = Vec::new();
        for chunk in stream.chunks(3) {
            frames.extend(splitter.push(chunk));
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..], JPEG_A);
        assert_eq!(&frames[1][..], JPEG_B);
    }

    #[test]
    fn parts_without_a_length_fall_back_to_the_end_marker() {
        let mut stream = part(JPEG_A, false);
        stream.extend(part(JPEG_B, false));
        let frames = MjpegSplitter::new().push(&stream);
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..], JPEG_A);
        assert_eq!(&frames[1][..], JPEG_B);
    }

    #[test]
    fn an_incomplete_part_waits_for_its_remaining_bytes() {
        let stream = part(JPEG_A, true);
        let mut splitter = MjpegSplitter::new();
        assert!(splitter.push(&stream[..stream.len() - 6]).is_empty());
        assert_eq!(splitter.push(&stream[stream.len() - 6..]).len(), 1);
    }

    #[test]
    fn frame_messages_carry_length_flags_and_timestamp() {
        let frame = H264Frame {
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            keyframe: true,
            pts_micros: 0x0102,
        };
        let message = frame_message(&frame);
        assert_eq!(&message[..4], &(1u32 + 8 + 5).to_be_bytes());
        assert_eq!(message[4], FLAG_KEYFRAME);
        assert_eq!(&message[5..13], &0x0102u64.to_be_bytes());
        assert_eq!(&message[13..], &[0, 0, 0, 1, 0x65]);
    }

    #[test]
    fn avcc_converts_to_annex_b_with_parameter_sets_on_keyframes() {
        let avcc = [0, 0, 0, 2, 0x65, 0xAA];
        let out = avcc_to_annex_b(&avcc, Some((&[0x67, 1], &[0x68, 2]))).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0xAA]
        );
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 1], None).is_err());
    }

    #[test]
    fn decoding_a_real_jpeg_yields_bgra_of_its_size() {
        // 8×16, made with `sips` from a phone screenshot.
        let jpeg = include_bytes!("../tests/fixtures/tiny.jpg");
        let mut pixels = Vec::new();
        let size = decode_bgra(jpeg, &mut pixels).expect("decodes");
        assert_eq!(size, (8, 16));
        assert_eq!(pixels.len(), 8 * 16 * 4);
        assert!(decode_bgra(&jpeg[..20], &mut pixels).is_err());
    }
}
