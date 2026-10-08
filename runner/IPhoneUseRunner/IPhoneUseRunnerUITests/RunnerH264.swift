// H.264 screen stream, encoded on the phone by VideoToolbox. Served on the MJPEG port at
// `GET /h264?fps=30&scale=50&kbps=2500` (RunnerMJPEG.swift hands those connections over), so it
// needs no extra relay. The daemon used to receive MJPEG (~69 KiB a frame, 15 Mbit/s at 28 fps)
// and re-encode it on the Mac; with this stream it passes the phone's own encode through.
//
// Two modes (`mode=performance|quality`; explicit fps/scale/kbps/gop/skip override them):
//   performance (default) - half size, up to 30 fps, 1.5 Mbit/s, Main profile, keyframe fallback 10 s;
//   quality - full size, up to 60 fps (whatever capture sustains), 10 Mbit/s, High profile, 10 s.
// Keyframes are on demand (a new viewer, or any byte a client sends; the daemon forwards viewers'
// `POST /agent/h264/keyframe`). Over TCP nothing is lost in transit, so the fallback interval only
// bounds a viewer that cannot ask; at full size each keyframe is ~25 KiB, so a short one would be
// most of a still screen's bandwidth. An unchanged screen is neither decoded
// nor encoded (`skip=0` turns that off): the capture's encoded bytes are compared with the
// previous capture's, and only a heartbeat frame goes out every second, so viewers and the
// daemon's 8 s inactivity timeout know the stream is alive.
//
// Response: an HTTP/1.0 header block, then messages in the daemon's `/agent/h264` framing
// (crates/server/src/video.rs):
//
//   [u32 BE length of the rest][u8 flags][u64 BE pts µs][Annex-B access unit]
//
// flags bit 0 = keyframe (SPS + PPS in band), bit 1 = the content band of the frame is one flat
// colour (an app hiding its screen from capture; see crates/server/src/redaction.rs). The daemon
// strips bit 1 before forwarding. Any byte the client sends asks for a keyframe, so one upstream
// connection can serve viewers that join later.
//
// One capture thread feeds one decode+encode thread (latest wins, so a slow encode drops stale
// captures instead of queueing them); both serve every client and run only while one is connected.
// Overlapping the two lets full-size quality mode reach the frame rate capture alone allows.

import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import Network
import VideoToolbox

/// Viewers that cannot keep up: when one is cut off, and how often stalls may cost a keyframe.
enum StreamStall {
  /// A client still writing one frame after this long is gone in all but name; it is dropped so
  /// it stops costing a keyframe per frame and holding the capture loop alive.
  static let maxSendingSeconds = 5.0
  /// At most one stall keyframe a second, however many viewers stall: each is ~25 KiB at full size.
  static let keyframeMinInterval = 1.0

  static func isStuck(sendingSince: Date?, now: Date) -> Bool {
    guard let sendingSince else { return false }
    return now.timeIntervalSince(sendingSince) > maxSendingSeconds
  }

  static func keyframeAllowed(last: Date, now: Date) -> Bool {
    now.timeIntervalSince(last) >= keyframeMinInterval
  }
}

final class RunnerH264Stream {
  struct Settings: Equatable {
    var fps = 30
    var scalePercent = 50
    var kbps = 1500
    var mode = Mode.performance
    /// Longest gap between keyframes. Keyframes are otherwise sent on demand.
    var keyframeSeconds = 10.0
    var highProfile = false
    var skipUnchanged = true

    enum Mode: String { case performance, quality }

    static func preset(_ mode: Mode) -> Settings {
      switch mode {
      case .performance:
        return Settings()
      case .quality:
        return Settings(fps: 60, scalePercent: 100, kbps: 10_000, mode: .quality,
                        keyframeSeconds: 10, highProfile: true, skipUnchanged: true)
      }
    }
  }

  /// While the screen does not change, one frame a second still goes out.
  static let heartbeatSeconds = 1.0

  static let flagKeyframe: UInt8 = 0x01
  static let flagBlank: UInt8 = 0x02

  private let queue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.h264")
  private let lock = NSLock()
  private var clients: [ObjectIdentifier: Client] = [:]
  private var capturing = false
  private var settings = Settings()
  private var forceKeyframe = true
  /// A viewer dropped a frame and has caught up; it needs a keyframe (rate-limited).
  private var stallKeyframeWanted = false
  private var lastStallKeyframe = Date.distantPast
  private var blank = false
  private var stats = (frames: 0, bytes: 0, since: Date(), fps: 0.0, kbps: 0.0, captureMs: 0.0, encodeMs: 0.0,
                       captures: 0, skipped: 0, skipRatio: 0.0, decodeMs: 0.0)

  /// Capture → encoder handoff. Only the newest capture waits; a keyframe request carries over.
  private struct Pending {
    var capture: Data
    var settings: Settings
    var keyframe: Bool
    var generation: Int
  }
  private let handoff = NSCondition()
  private var pending: Pending?
  private var encoderRunning = false
  /// Bumped per capture run (under `handoff`): a capture loop that is exiting must not clear, and
  /// the encoder must not encode, a capture from another run.
  private var generation = 0

  private final class Client {
    let connection: NWConnection
    var sendingSince: Date?
    var gotKeyframe = false
    init(_ connection: NWConnection) { self.connection = connection }
  }

  /// A connection whose request (`head`) asked for `/h264`.
  func attach(_ connection: NWConnection, head: String) {
    let requested = Self.settings(from: head)
    let response = [
      "HTTP/1.0 200 OK",
      "Server: iphone-use runner H.264",
      "Connection: close",
      "Cache-Control: no-cache, private",
      "Content-Type: application/octet-stream",
      "X-Video-Format: iphone-use-h264-annexb-v1",
      "", "",
    ].joined(separator: "\r\n")
    connection.send(content: Data(response.utf8), completion: .contentProcessed { [weak self] error in
      guard let self else { return }
      if error != nil {
        connection.cancel()
        return
      }
      self.add(Client(connection), settings: requested)
    })
  }

  func statusValue() -> [String: Any] {
    lock.lock()
    defer { lock.unlock() }
    return [
      "clients": clients.count,
      "mode": settings.mode.rawValue,
      "fps": settings.fps,
      "scale": settings.scalePercent,
      "kbps": settings.kbps,
      "achievedFps": (stats.fps * 10).rounded() / 10,
      "achievedKbps": stats.kbps.rounded(),
      "captureMs": (stats.captureMs * 10).rounded() / 10,
      "decodeMs": (stats.decodeMs * 10).rounded() / 10,
      "encodeMs": (stats.encodeMs * 10).rounded() / 10,
      "keyframeSeconds": settings.keyframeSeconds,
      "skipUnchanged": settings.skipUnchanged,
      "skippedRatio": (stats.skipRatio * 100).rounded() / 100,
    ]
  }

  static func settings(from head: String) -> Settings {
    let requestLine = head.split(separator: "\r\n", maxSplits: 1).first.map(String.init) ?? head
    let target = requestLine.split(separator: " ").dropFirst().first.map(String.init) ?? ""
    guard let query = target.split(separator: "?", maxSplits: 1).dropFirst().first else { return Settings() }
    let pairs = query.split(separator: "&").compactMap { pair -> (String, String)? in
      let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
      return parts.count == 2 ? (parts[0], parts[1]) : nil
    }
    // The mode picks the preset; explicit values then override it.
    let mode = pairs.last { $0.0 == "mode" }.flatMap { Settings.Mode(rawValue: $0.1) } ?? .performance
    var result = Settings.preset(mode)
    for (key, raw) in pairs {
      guard let value = Int(raw) else { continue }
      switch key {
      case "fps": result.fps = min(60, max(1, value))
      case "scale": result.scalePercent = min(100, max(10, value))
      case "kbps": result.kbps = min(20_000, max(200, value))
      case "gop": result.keyframeSeconds = Double(min(60, max(1, value)))
      case "skip": result.skipUnchanged = value != 0
      default: break
      }
    }
    return result
  }

  // MARK: - Clients

  private func add(_ client: Client, settings requested: Settings) {
    lock.lock()
    clients[ObjectIdentifier(client)] = client
    settings = requested  // the newest viewer's request wins; the daemon sends one upstream
    forceKeyframe = true
    let startCapture = !capturing
    if startCapture {
      capturing = true
      stats = (0, 0, Date(), 0, 0, 0, 0, 0, 0, 0, 0)
    }
    let count = clients.count
    lock.unlock()
    NSLog("ipu-runner: H.264 client connected (%d total)", count)
    listen(client)
    if startCapture {
      let thread = Thread { [weak self] in self?.captureLoop() }
      thread.name = "ipu-runner-h264-capture"
      thread.qualityOfService = .userInitiated
      thread.start()
    }
  }

  /// Reads from the client: any byte asks for a keyframe; EOF or an error ends it.
  private func listen(_ client: Client) {
    client.connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) { [weak self, weak client] data, _, isComplete, error in
      guard let self, let client else { return }
      if let data, !data.isEmpty {
        self.lock.lock()
        self.forceKeyframe = true
        self.lock.unlock()
      }
      if isComplete || error != nil {
        self.remove(client)
        client.connection.cancel()
        return
      }
      self.listen(client)
    }
  }

  private func remove(_ client: Client) {
    lock.lock()
    let removed = clients.removeValue(forKey: ObjectIdentifier(client)) != nil
    let count = clients.count
    lock.unlock()
    if removed { NSLog("ipu-runner: H.264 client left (%d remaining)", count) }
  }

  // MARK: - Capture + encode

  private func captureLoop() {
    NSLog("ipu-runner: H.264 capture started")
    handoff.lock()
    let startEncoder = !encoderRunning
    encoderRunning = true
    generation += 1
    let run = generation
    pending = nil
    handoff.unlock()
    if startEncoder {
      let thread = Thread { [weak self] in self?.encodeLoop() }
      thread.name = "ipu-runner-h264-encode"
      thread.qualityOfService = .userInitiated
      thread.start()
    }
    var lastCapture: Data?
    var lastSettings: Settings?
    var lastSentAt = Date.distantPast
    while true {
      lock.lock()
      if clients.isEmpty {
        capturing = false
        lock.unlock()
        break
      }
      let current = settings
      var keyframe = forceKeyframe
      forceKeyframe = false
      if stallKeyframeWanted, StreamStall.keyframeAllowed(last: lastStallKeyframe, now: Date()) {
        stallKeyframeWanted = false
        lastStallKeyframe = Date()
        keyframe = true
      }
      lock.unlock()

      let frameStart = Date()
      let interval = 1.0 / Double(max(1, current.fps))
      var error: NSString?
      let capture: Data? = autoreleasepool {
        IPURBridge.screenCapture(withQuality: 0.85, path: nil, error: &error)
      }
      guard let capture else {
        NSLog("ipu-runner: H.264 capture failed: %@", (error as String?) ?? "unknown")
        Thread.sleep(forTimeInterval: 0.5)
        continue
      }
      let captureMs = Date().timeIntervalSince(frameStart) * 1000
      // The same screen captures to the same bytes: skip the decode and the encode, unless a
      // keyframe was asked for, the settings changed, or the heartbeat is due.
      let unchanged = current.skipUnchanged && lastSettings == current && capture == lastCapture
      let heartbeatDue = frameStart.timeIntervalSince(lastSentAt) >= Self.heartbeatSeconds
      let skip = unchanged && !keyframe && !heartbeatDue
      lock.lock()
      stats.captures += 1
      stats.captureMs = captureMs
      if skip { stats.skipped += 1 }
      lock.unlock()
      if !skip {
        lastCapture = capture
        lastSettings = current
        lastSentAt = frameStart
        handoff.lock()
        let carried = pending?.generation == run && pending?.keyframe == true
        pending = Pending(capture: capture, settings: current, keyframe: keyframe || carried, generation: run)
        handoff.signal()
        handoff.unlock()
      }
      let remaining = interval - Date().timeIntervalSince(frameStart)
      if remaining > 0 { Thread.sleep(forTimeInterval: remaining) }
    }
    handoff.lock()
    if pending?.generation == run { pending = nil }
    handoff.broadcast()
    handoff.unlock()
    NSLog("ipu-runner: H.264 capture stopped (no clients)")
  }

  /// Decodes and encodes the newest capture; exits once capture has stopped.
  private func encodeLoop() {
    var encoder: Encoder?
    var frameIndex = 0
    var lastBlankCheck = Date.distantPast
    let started = Date()
    while true {
      handoff.lock()
      while pending == nil {
        lock.lock()
        let stillCapturing = capturing
        lock.unlock()
        if !stillCapturing {
          encoderRunning = false
          handoff.unlock()
          encoder?.invalidate()
          return
        }
        _ = handoff.wait(until: Date().addingTimeInterval(0.5))
      }
      let job = pending!
      pending = nil
      let stale = job.generation != generation
      handoff.unlock()
      if stale { continue }

      let decodeStart = Date()
      let image: CGImage? = autoreleasepool {
        IPURBridge.decodeScreenCapture(job.capture, scale: Double(job.settings.scalePercent) / 100)
      }
      guard let image else {
        NSLog("ipu-runner: H.264 frame could not be decoded")
        continue
      }
      // H.264 wants even dimensions; drop the odd last row/column.
      let width = image.width & ~1
      let height = image.height & ~1
      var rebuilt = false
      if encoder == nil || encoder?.width != width || encoder?.height != height
        || encoder?.settings != job.settings {
        encoder?.invalidate()
        encoder = Encoder(width: width, height: height, settings: job.settings) { [weak self] data, isKey, pts in
          self?.broadcast(data, keyframe: isKey, pts: pts)
        }
        rebuilt = true
        if encoder == nil {
          NSLog("ipu-runner: H.264 encoder could not start (%dx%d)", width, height)
          Thread.sleep(forTimeInterval: 1)
          continue
        }
      }
      guard let encoder, let buffer = encoder.pixelBuffer(drawing: image) else { continue }
      let decodeMs = Date().timeIntervalSince(decodeStart) * 1000
      // A few times a second is plenty to notice a protected screen; a still one is checked on
      // its heartbeat.
      let now = Date()
      if now.timeIntervalSince(lastBlankCheck) >= 0.3 {
        lastBlankCheck = now
        let flat = Self.contentBandIsFlat(buffer)
        lock.lock()
        blank = flat
        lock.unlock()
      }
      frameIndex += 1
      let encodeStart = Date()
      encoder.encode(buffer, pts: encodeStart.timeIntervalSince(started),
                     forceKeyframe: job.keyframe || rebuilt || frameIndex == 1)
      let encodeMs = Date().timeIntervalSince(encodeStart) * 1000
      lock.lock()
      stats.decodeMs = decodeMs
      stats.encodeMs = encodeMs
      lock.unlock()
    }
  }

  private func broadcast(_ annexB: Data, keyframe: Bool, pts: Double) {
    lock.lock()
    let flags = (keyframe ? Self.flagKeyframe : 0) | (blank ? Self.flagBlank : 0)
    let now = Date()
    stats.frames += 1
    stats.bytes += annexB.count
    let window = now.timeIntervalSince(stats.since)
    if window >= 2 {
      stats.fps = Double(stats.frames) / window
      stats.kbps = Double(stats.bytes) * 8 / 1000 / window
      stats.skipRatio = stats.captures > 0 ? Double(stats.skipped) / Double(stats.captures) : 0
      stats.frames = 0
      stats.bytes = 0
      stats.captures = 0
      stats.skipped = 0
      stats.since = now
    }
    // A viewer gets nothing until its first keyframe; after that a client still writing the
    // previous message drops this one, rather than queueing (latency) or decoding a gap (smear),
    // and asks for one keyframe once its write completes. A client stuck writing for seconds is
    // cut off.
    var ready: [Client] = []
    var stuck: [Client] = []
    for client in clients.values {
      if StreamStall.isStuck(sendingSince: client.sendingSince, now: now) {
        stuck.append(client)
        continue
      }
      if !client.gotKeyframe && !keyframe { continue }
      if client.sendingSince != nil {
        client.gotKeyframe = false
        continue
      }
      client.gotKeyframe = true
      client.sendingSince = now
      ready.append(client)
    }
    for client in stuck { clients.removeValue(forKey: ObjectIdentifier(client)) }
    lock.unlock()
    for client in stuck {
      NSLog("ipu-runner: H.264 client stuck writing for over %.0f s; dropping it", StreamStall.maxSendingSeconds)
      client.connection.cancel()
    }
    if ready.isEmpty { return }

    var message = Data(capacity: 13 + annexB.count)
    var length = UInt32(1 + 8 + annexB.count).bigEndian
    withUnsafeBytes(of: &length) { message.append(contentsOf: $0) }
    message.append(flags)
    var micros = UInt64(max(0, pts) * 1_000_000).bigEndian
    withUnsafeBytes(of: &micros) { message.append(contentsOf: $0) }
    message.append(annexB)

    for client in ready {
      client.connection.send(content: message, completion: .contentProcessed { [weak self, weak client] error in
        guard let self, let client else { return }
        self.lock.lock()
        client.sendingSince = nil
        if !client.gotKeyframe { self.stallKeyframeWanted = true }
        self.lock.unlock()
        if error != nil { client.connection.cancel() }
      })
    }
  }

  // MARK: - Blank check (same test as crates/server/src/redaction.rs band_is_flat)

  static func contentBandIsFlat(_ buffer: CVPixelBuffer) -> Bool {
    CVPixelBufferLockBaseAddress(buffer, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(buffer) else { return false }
    let width = CVPixelBufferGetWidth(buffer)
    let height = CVPixelBufferGetHeight(buffer)
    let rowBytes = CVPixelBufferGetBytesPerRow(buffer)
    return bandIsFlat(width: width, height: height, rowBytes: rowBytes,
                      pixels: base.assumingMemoryBound(to: UInt8.self))
  }

  static func bandIsFlat(width: Int, height: Int, rowBytes: Int, pixels: UnsafePointer<UInt8>) -> Bool {
    guard width > 0, height > 0 else { return false }
    let top = Int(Double(height) * 0.07)
    let bottom = Int(Double(height) * 0.88)
    let step = max(2, min(width, height) / 120)
    var buckets: [Int: Int] = [:]
    var samples: [(Int, Int, Int)] = []
    var y = top
    while y < bottom {
      var x = 0
      while x < width {
        let i = y * rowBytes + x * 4
        let p = (Int(pixels[i]), Int(pixels[i + 1]), Int(pixels[i + 2]))
        buckets[(p.0 / 16) << 16 | (p.1 / 16) << 8 | (p.2 / 16), default: 0] += 1
        samples.append(p)
        x += step
      }
      y += step
    }
    guard let (key, _) = buckets.max(by: { $0.value < $1.value }) else { return false }
    let centre = (((key >> 16) & 0xFF) * 16 + 8, ((key >> 8) & 0xFF) * 16 + 8, (key & 0xFF) * 16 + 8)
    let tolerance = 10 + 8
    let same = samples.filter {
      abs($0.0 - centre.0) <= tolerance && abs($0.1 - centre.1) <= tolerance && abs($0.2 - centre.2) <= tolerance
    }.count
    return Double(same) >= Double(samples.count) * 0.985
  }

  // MARK: - VideoToolbox

  final class Encoder {
    let width: Int
    let height: Int
    let settings: Settings
    private var session: VTCompressionSession?
    private var pool: CVPixelBufferPool?
    private let output: (Data, Bool, Double) -> Void

    init?(width: Int, height: Int, settings: Settings, output: @escaping (Data, Bool, Double) -> Void) {
      self.width = width
      self.height = height
      self.settings = settings
      self.output = output
      var created: VTCompressionSession?
      let status = VTCompressionSessionCreate(
        allocator: nil, width: Int32(width), height: Int32(height),
        codecType: kCMVideoCodecType_H264, encoderSpecification: nil,
        imageBufferAttributes: nil, compressedDataAllocator: nil,
        outputCallback: nil, refcon: nil, compressionSessionOut: &created)
      guard status == noErr, let created else { return nil }
      session = created
      let bitrate = settings.kbps * 1000
      let properties: [CFString: Any] = [
        kVTCompressionPropertyKey_RealTime: kCFBooleanTrue!,
        kVTCompressionPropertyKey_AllowFrameReordering: kCFBooleanFalse!,
        kVTCompressionPropertyKey_ProfileLevel: settings.highProfile
          ? kVTProfileLevel_H264_High_AutoLevel : kVTProfileLevel_H264_Main_AutoLevel,
        kVTCompressionPropertyKey_AverageBitRate: bitrate,
        // Bytes per second over one second: caps bursts at 1.5× the average.
        kVTCompressionPropertyKey_DataRateLimits: [bitrate * 3 / 2 / 8, 1] as CFArray,
        kVTCompressionPropertyKey_ExpectedFrameRate: settings.fps,
        // Keyframes come on demand; this only bounds a lossy viewer's wait for a clean one.
        kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: settings.keyframeSeconds,
      ]
      for (key, value) in properties {
        VTSessionSetProperty(created, key: key, value: value as CFTypeRef)
      }
      VTCompressionSessionPrepareToEncodeFrames(created)
      let attributes: [CFString: Any] = [
        kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
        kCVPixelBufferWidthKey: width,
        kCVPixelBufferHeightKey: height,
        kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        kCVPixelBufferCGBitmapContextCompatibilityKey: true,
      ]
      CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &pool)
      if pool == nil { return nil }
    }

    deinit { invalidate() }

    func invalidate() {
      if let session {
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
      }
      session = nil
    }

    /// A pooled BGRA buffer with `image` drawn into it.
    func pixelBuffer(drawing image: CGImage) -> CVPixelBuffer? {
      guard let pool else { return nil }
      var buffer: CVPixelBuffer?
      guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer) == kCVReturnSuccess, let buffer else {
        return nil
      }
      CVPixelBufferLockBaseAddress(buffer, [])
      defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
      guard let context = CGContext(
        data: CVPixelBufferGetBaseAddress(buffer), width: width, height: height,
        bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(buffer),
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)
      else { return nil }
      context.interpolationQuality = .none
      context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
      return buffer
    }

    func encode(_ buffer: CVPixelBuffer, pts: Double, forceKeyframe: Bool) {
      guard let session else { return }
      let time = CMTime(seconds: pts, preferredTimescale: 1_000_000)
      let frameProperties: CFDictionary? = forceKeyframe
        ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue!] as CFDictionary : nil
      VTCompressionSessionEncodeFrame(
        session, imageBuffer: buffer, presentationTimeStamp: time, duration: .invalid,
        frameProperties: frameProperties, infoFlagsOut: nil
      ) { [weak self] status, _, sample in
        guard status == noErr, let sample, let self else { return }
        guard let annexB = Self.annexB(sample) else { return }
        self.output(annexB.data, annexB.keyframe, pts)
      }
    }

    /// AVCC sample (length-prefixed NAL units) → Annex-B, with SPS + PPS in front of keyframes.
    static func annexB(_ sample: CMSampleBuffer) -> (data: Data, keyframe: Bool)? {
      guard let block = CMSampleBufferGetDataBuffer(sample),
            let format = CMSampleBufferGetFormatDescription(sample) else { return nil }
      var keyframe = true
      if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
        as? [[CFString: Any]], let first = attachments.first,
        let notSync = first[kCMSampleAttachmentKey_NotSync] as? Bool {
        keyframe = !notSync
      }
      let startCode: [UInt8] = [0, 0, 0, 1]
      var out = Data()
      var headerLength: Int32 = 4
      if keyframe {
        var count = 0
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
          format, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil,
          parameterSetCountOut: &count, nalUnitHeaderLengthOut: &headerLength)
        for index in 0..<count {
          var pointer: UnsafePointer<UInt8>?
          var size = 0
          if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format, parameterSetIndex: index, parameterSetPointerOut: &pointer,
            parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil
          ) == noErr, let pointer {
            out.append(contentsOf: startCode)
            out.append(pointer, count: size)
          }
        }
      }
      let total = CMBlockBufferGetDataLength(block)
      var bytes = [UInt8](repeating: 0, count: total)
      guard CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: total, destination: &bytes) == noErr
      else { return nil }
      let lengthSize = Int(headerLength)
      var offset = 0
      while offset + lengthSize <= total {
        var nalLength = 0
        for i in 0..<lengthSize { nalLength = (nalLength << 8) | Int(bytes[offset + i]) }
        offset += lengthSize
        guard nalLength > 0, offset + nalLength <= total else { break }
        out.append(contentsOf: startCode)
        out.append(contentsOf: bytes[offset..<(offset + nalLength)])
        offset += nalLength
      }
      return (out, keyframe)
    }
  }
}
