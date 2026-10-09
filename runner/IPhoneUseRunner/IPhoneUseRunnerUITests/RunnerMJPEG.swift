// MJPEG screen stream in WebDriverAgent's wire format, so the daemon's MjpegSplitter
// (crates/server/src/video.rs) reads it unchanged:
//
//   HTTP/1.0 200 OK ... Content-Type: multipart/x-mixed-replace; boundary=--BoundaryString
//   --BoundaryString\r\nContent-type: image/jpg\r\nContent-Length: N\r\n\r\n<jpeg>\r\n  (per frame)
//
// Frames are captured on a dedicated thread, never on main, and only while a client is
// connected. Framerate / scaling / quality follow WDA's appium settings (mjpegServerFramerate,
// mjpegScalingFactor %, mjpegServerScreenshotQuality %).

import Foundation
import Network

final class RunnerMJPEGServer {
  struct Settings {
    var framerate = 10  // WDA's defaults
    var scalingPercent = 100
    var qualityPercent = 25
  }

  private let listener: NWListener
  private let auth: RunnerAuth
  private let queue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.mjpeg")
  private let lock = NSLock()
  private var settings = Settings()
  private var clients: [ObjectIdentifier: Client] = [:]
  private var capturing = false
  private var stats = (frames: 0, since: Date(), fps: 0.0, lastPath: "-", lastBytes: 0, lastCaptureMs: 0.0)
  /// Takes `GET /h264…` connections on this port (RunnerH264.swift); everything else is MJPEG.
  let h264 = RunnerH264Stream()

  private final class Client {
    let connection: NWConnection
    var sendingSince: Date?
    init(_ connection: NWConnection) { self.connection = connection }
  }

  init(port: UInt16, auth: RunnerAuth) throws {
    self.auth = auth
    guard let endpointPort = NWEndpoint.Port(rawValue: port) else {
      throw RunnerError.invalidArgument("invalid MJPEG port \(port)")
    }
    let parameters = NWParameters.tcp
    parameters.allowLocalEndpointReuse = true
    listener = try NWListener(using: parameters, on: endpointPort)
  }

  func start() {
    listener.stateUpdateHandler = { [weak self] state in
      if case .ready = state {
        NSLog("ipu-runner: MJPEG stream on port %d", Int(self?.listener.port?.rawValue ?? 0))
      } else if case .failed(let error) = state {
        NSLog("ipu-runner: MJPEG listener failed: %@", String(describing: error))
      }
    }
    listener.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
    listener.start(queue: queue)
  }

  func stop() { listener.cancel() }

  /// Applies whichever mjpeg* keys `values` carries (appium/settings body).
  func apply(_ values: [String: Any]) {
    func int(_ key: String) -> Int? {
      (values[key] as? NSNumber)?.intValue ?? (values[key] as? String).flatMap { Int($0) }
    }
    lock.lock()
    if let framerate = int("mjpegServerFramerate") { settings.framerate = min(60, max(1, framerate)) }
    if let scaling = int("mjpegScalingFactor") { settings.scalingPercent = min(100, max(1, scaling)) }
    if let quality = int("mjpegServerScreenshotQuality") { settings.qualityPercent = min(100, max(1, quality)) }
    let current = settings
    lock.unlock()
    NSLog("ipu-runner: MJPEG settings fps=%d scale=%d%% quality=%d%%",
          current.framerate, current.scalingPercent, current.qualityPercent)
  }

  /// Settings plus what the stream achieves, for /status.
  func statusValue() -> [String: Any] {
    lock.lock()
    defer { lock.unlock() }
    return [
      "clients": clients.count,
      "framerate": settings.framerate,
      "scalingFactor": settings.scalingPercent,
      "quality": settings.qualityPercent,
      "achievedFps": (stats.fps * 10).rounded() / 10,
      "capturePath": stats.lastPath,
      "lastFrameBytes": stats.lastBytes,
      "lastCaptureMs": (stats.lastCaptureMs * 10).rounded() / 10,
    ]
  }

  // MARK: - Connections

  private func accept(_ connection: NWConnection) {
    let client = Client(connection)
    connection.stateUpdateHandler = { [weak self, weak client] state in
      switch state {
      case .failed, .cancelled:
        if let client { self?.remove(client) }
      default:
        break
      }
    }
    connection.start(queue: queue)
    // Read the whole request head, check its signature, then answer with the stream headers.
    readHead(connection, buffer: Data()) { [weak self] requestHead, raw in
      guard let self else {
        connection.cancel()
        return
      }
      if let refusal = self.auth.check(method: requestHead.method, target: requestHead.target,
                                       authorization: requestHead.headers["authorization"], body: Data()) {
        NSLog("ipu-runner: refused video %@ %@ from %@: %@", requestHead.method,
              requestHead.target.components(separatedBy: "?")[0],
              String(describing: connection.endpoint), refusal.message)
        connection.stateUpdateHandler = nil
        connection.send(content: RunnerAuth.refusalResponse(refusal).serialized(), isComplete: true,
                        completion: .contentProcessed { _ in connection.cancel() })
        return
      }
      let request = raw
      if request.hasPrefix("GET /h264") {
        connection.stateUpdateHandler = nil
        self.h264.attach(connection, head: request)
        return
      }
      let head = [
        "HTTP/1.0 200 OK",
        "Server: iphone-use runner MJPEG",
        "Connection: close",
        "Max-Age: 0",
        "Expires: 0",
        "Cache-Control: no-cache, private",
        "Pragma: no-cache",
        "Content-Type: multipart/x-mixed-replace; boundary=--BoundaryString",
        "", "",
      ].joined(separator: "\r\n")
      connection.send(content: Data(head.utf8), completion: .contentProcessed { [weak self] error in
        guard let self else { return }
        if error != nil {
          connection.cancel()
          return
        }
        self.add(client)
      })
      self.drain(connection)
    }
  }

  /// Collects bytes until the request head is complete (at most 16 KiB, 5 s), then calls `done`
  /// with it and the head as text. Anything else closes the connection.
  private func readHead(_ connection: NWConnection, buffer: Data, started: Date = Date(),
                        done: @escaping (HTTPRequest.Head, String) -> Void) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 16 * 1024) { [weak self] data, _, isComplete, error in
      guard self != nil, error == nil else {
        connection.cancel()
        return
      }
      var buffer = buffer
      if let data { buffer.append(data) }
      switch HTTPRequest.head(buffer) {
      case .success(let head)?:
        done(head, String(decoding: buffer.prefix(upTo: head.end), as: UTF8.self))
      case .failure?:
        connection.cancel()
      case nil:
        if isComplete || buffer.count > 16 * 1024 || Date().timeIntervalSince(started) > 5 {
          connection.cancel()
        } else {
          self?.readHead(connection, buffer: buffer, started: started, done: done)
        }
      }
    }
  }

  /// Keeps reading (and discarding) so a closed client is noticed.
  private func drain(_ connection: NWConnection) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self] _, _, isComplete, error in
      if isComplete || error != nil {
        connection.cancel()
        return
      }
      self?.drain(connection)
    }
  }

  private func add(_ client: Client) {
    lock.lock()
    clients[ObjectIdentifier(client)] = client
    let startCapture = !capturing
    if startCapture {
      capturing = true
      stats = (0, Date(), 0, stats.lastPath, stats.lastBytes, stats.lastCaptureMs)
    }
    let count = clients.count
    lock.unlock()
    NSLog("ipu-runner: MJPEG client connected (%d total)", count)
    if startCapture {
      let thread = Thread { [weak self] in self?.captureLoop() }
      thread.name = "ipu-runner-mjpeg-capture"
      thread.qualityOfService = .userInitiated
      thread.start()
    }
  }

  private func remove(_ client: Client) {
    lock.lock()
    let removed = clients.removeValue(forKey: ObjectIdentifier(client)) != nil
    let count = clients.count
    lock.unlock()
    if removed { NSLog("ipu-runner: MJPEG client left (%d remaining)", count) }
  }

  // MARK: - Capture

  /// Runs while at least one client is connected; exits (and costs nothing) once the last leaves.
  private func captureLoop() {
    NSLog("ipu-runner: MJPEG capture started")
    while true {
      lock.lock()
      if clients.isEmpty {
        capturing = false
        lock.unlock()
        break
      }
      let current = settings
      lock.unlock()

      let started = Date()
      var path: NSString?
      var error: NSString?
      let jpeg = autoreleasepool {
        IPURBridge.jpegScreenshot(
          withQuality: Double(current.qualityPercent) / 100,
          scale: Double(current.scalingPercent) / 100,
          path: &path, error: &error)
      }
      let captureMs = Date().timeIntervalSince(started) * 1000
      if let jpeg {
        broadcast(jpeg, path: path as String? ?? "?", captureMs: captureMs)
      } else {
        NSLog("ipu-runner: MJPEG capture failed: %@", (error as String?) ?? "unknown")
        Thread.sleep(forTimeInterval: 0.5)
        continue
      }
      let interval = 1.0 / Double(max(1, current.framerate))
      let remaining = interval - Date().timeIntervalSince(started)
      if remaining > 0 { Thread.sleep(forTimeInterval: remaining) }
    }
    NSLog("ipu-runner: MJPEG capture stopped (no clients)")
  }

  private func broadcast(_ jpeg: Data, path: String, captureMs: Double) {
    var part = Data("--BoundaryString\r\nContent-type: image/jpg\r\nContent-Length: \(jpeg.count)\r\n\r\n".utf8)
    part.append(jpeg)
    part.append(Data("\r\n".utf8))

    lock.lock()
    let now = Date()
    stats.frames += 1
    stats.lastPath = path
    stats.lastBytes = jpeg.count
    stats.lastCaptureMs = captureMs
    let window = now.timeIntervalSince(stats.since)
    if window >= 2 {
      stats.fps = Double(stats.frames) / window
      stats.frames = 0
      stats.since = now
    }
    // A client still writing the previous frame skips this one instead of queueing it, so a
    // slow viewer gets fewer frames rather than growing latency; one stuck for seconds is cut off.
    let stuck = clients.values.filter { StreamStall.isStuck(sendingSince: $0.sendingSince, now: now) }
    for client in stuck { clients.removeValue(forKey: ObjectIdentifier(client)) }
    let ready = clients.values.filter { $0.sendingSince == nil }
    ready.forEach { $0.sendingSince = now }
    lock.unlock()
    for client in stuck {
      NSLog("ipu-runner: MJPEG client stuck writing for over %.0f s; dropping it", StreamStall.maxSendingSeconds)
      client.connection.cancel()
    }

    for client in ready {
      client.connection.send(content: part, completion: .contentProcessed { [weak self, weak client] error in
        guard let self, let client else { return }
        self.lock.lock()
        client.sendingSince = nil
        self.lock.unlock()
        if error != nil { client.connection.cancel() }
      })
    }
  }
}
