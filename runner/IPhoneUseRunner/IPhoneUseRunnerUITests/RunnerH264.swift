// H.264 screen stream, encoded on the phone by VideoToolbox. Served on the MJPEG port at
// `GET /h264?fps=30&scale=50&kbps=2500` (RunnerMJPEG.swift hands those connections over), so it
// needs no extra relay. The daemon used to receive MJPEG (~69 KiB a frame, 15 Mbit/s at 28 fps)
// and re-encode it on the Mac; with this stream it passes the phone's own encode through.
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
// One capture thread and one encoder serve every client, and run only while one is connected.

import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import Network
import VideoToolbox

final class RunnerH264Stream {
  struct Settings: Equatable {
    var fps = 30
    var scalePercent = 50
    var kbps = 2500
  }

  static let flagKeyframe: UInt8 = 0x01
  static let flagBlank: UInt8 = 0x02

  private let queue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.h264")
  private let lock = NSLock()
  private var clients: [ObjectIdentifier: Client] = [:]
  private var capturing = false
  private var settings = Settings()
  private var forceKeyframe = true
  private var blank = false
  private var stats = (frames: 0, bytes: 0, since: Date(), fps: 0.0, kbps: 0.0, captureMs: 0.0, encodeMs: 0.0)

  private final class Client {
    let connection: NWConnection
    var sending = false
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
      "fps": settings.fps,
      "scale": settings.scalePercent,
      "kbps": settings.kbps,
      "achievedFps": (stats.fps * 10).rounded() / 10,
      "achievedKbps": stats.kbps.rounded(),
      "captureMs": (stats.captureMs * 10).rounded() / 10,
      "encodeMs": (stats.encodeMs * 10).rounded() / 10,
    ]
  }

  static func settings(from head: String) -> Settings {
    var result = Settings()
    let requestLine = head.split(separator: "\r\n", maxSplits: 1).first.map(String.init) ?? head
    let target = requestLine.split(separator: " ").dropFirst().first.map(String.init) ?? ""
    guard let query = target.split(separator: "?", maxSplits: 1).dropFirst().first else { return result }
    for pair in query.split(separator: "&") {
      let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
      guard parts.count == 2, let value = Int(parts[1]) else { continue }
      switch parts[0] {
      case "fps": result.fps = min(60, max(1, value))
      case "scale": result.scalePercent = min(100, max(10, value))
      case "kbps": result.kbps = min(20_000, max(200, value))
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
      stats = (0, 0, Date(), 0, 0, 0, 0)
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
    var encoder: Encoder?
    var frameIndex = 0
    let started = Date()
    while true {
      lock.lock()
      if clients.isEmpty {
        capturing = false
        lock.unlock()
        break
      }
      let current = settings
      let keyframe = forceKeyframe
      forceKeyframe = false
      lock.unlock()

      let frameStart = Date()
      var error: NSString?
      let image: CGImage? = autoreleasepool {
        IPURBridge.screenImage(
          withQuality: 0.85, scale: Double(current.scalePercent) / 100, path: nil, error: &error)
      }
      guard let image else {
        NSLog("ipu-runner: H.264 capture failed: %@", (error as String?) ?? "unknown")
        Thread.sleep(forTimeInterval: 0.5)
        continue
      }
      let captureMs = Date().timeIntervalSince(frameStart) * 1000
      // H.264 wants even dimensions; drop the odd last row/column.
      let width = image.width & ~1
      let height = image.height & ~1
      if encoder == nil || encoder?.width != width || encoder?.height != height
        || encoder?.settings != current {
        encoder = Encoder(width: width, height: height, settings: current) { [weak self] data, isKey, pts in
          self?.broadcast(data, keyframe: isKey, pts: pts)
        }
        if encoder == nil {
          NSLog("ipu-runner: H.264 encoder could not start (%dx%d)", width, height)
          Thread.sleep(forTimeInterval: 1)
          continue
        }
      }
      guard let encoder, let buffer = encoder.pixelBuffer(drawing: image) else { continue }
      if frameIndex % 10 == 0 {
        let flat = Self.contentBandIsFlat(buffer)
        lock.lock()
        blank = flat
        lock.unlock()
      }
      frameIndex += 1
      let encodeStart = Date()
      let pts = Date().timeIntervalSince(started)
      encoder.encode(buffer, pts: pts, forceKeyframe: keyframe || frameIndex == 1)
      let encodeMs = Date().timeIntervalSince(encodeStart) * 1000
      lock.lock()
      stats.captureMs = captureMs
      stats.encodeMs = encodeMs
      lock.unlock()

      let interval = 1.0 / Double(max(1, current.fps))
      let remaining = interval - Date().timeIntervalSince(frameStart)
      if remaining > 0 { Thread.sleep(forTimeInterval: remaining) }
    }
    encoder?.invalidate()
    NSLog("ipu-runner: H.264 capture stopped (no clients)")
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
      stats.frames = 0
      stats.bytes = 0
      stats.since = now
    }
    // A viewer gets nothing until its first keyframe; after that a client still writing the
    // previous message drops this one and asks for a keyframe, rather than queueing (latency) or
    // decoding a gap (smear).
    var ready: [Client] = []
    for client in clients.values {
      if !client.gotKeyframe && !keyframe { continue }
      if client.sending {
        forceKeyframe = true
        client.gotKeyframe = false
        continue
      }
      client.gotKeyframe = true
      client.sending = true
      ready.append(client)
    }
    lock.unlock()
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
        client.sending = false
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
        kVTCompressionPropertyKey_ProfileLevel: kVTProfileLevel_H264_Main_AutoLevel,
        kVTCompressionPropertyKey_AverageBitRate: bitrate,
        // Bytes per second over one second: caps bursts at 1.5× the average.
        kVTCompressionPropertyKey_DataRateLimits: [bitrate * 3 / 2 / 8, 1] as CFArray,
        kVTCompressionPropertyKey_ExpectedFrameRate: settings.fps,
        kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: 2,
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
