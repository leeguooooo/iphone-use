// Minimal HTTP/1.1 server for the iphone-use native runner: one request per connection,
// `Connection: close`, JSON bodies.
//
// The single-long-running-XCTest-method + NWListener design is adapted from
// callstack/agent-device (MIT License, Copyright (c) 2026 Callstack), RunnerTests+Transport.swift.
// See runner/README.md for the full attribution and license text.

import CryptoKit
import Foundation
import Network

struct HTTPRequest {
  let method: String
  let path: String
  /// The request target exactly as sent (path plus `?query`): what a signature covers.
  var target: String = ""
  let query: [String: String]
  let headers: [String: String]
  let body: Data

  /// The JSON object body, or an empty dictionary when there is no body.
  func jsonObject() throws -> [String: Any] {
    if body.isEmpty { return [:] }
    let parsed = try JSONSerialization.jsonObject(with: body, options: [.fragmentsAllowed])
    guard let object = parsed as? [String: Any] else {
      throw RunnerError.invalidArgument("request body must be a JSON object")
    }
    return object
  }

  enum ParseResult {
    case incomplete
    case invalid(String)
    case complete(HTTPRequest)
  }

  /// The request line and headers, once they are complete (nil while they are not).
  struct Head {
    let method: String
    let target: String
    let headers: [String: String]
    let end: Data.Index
  }

  static func head(_ data: Data) -> Result<Head, ParseFailure>? {
    guard let headerEnd = data.range(of: Data("\r\n\r\n".utf8)) else {
      return data.count > 64 * 1024 ? .failure(ParseFailure("request header too large")) : nil
    }
    let head = String(decoding: data.subdata(in: data.startIndex..<headerEnd.lowerBound), as: UTF8.self)
    var lines = head.components(separatedBy: "\r\n")
    guard !lines.isEmpty else { return .failure(ParseFailure("empty request")) }
    let requestLine = lines.removeFirst().split(separator: " ", omittingEmptySubsequences: true)
    guard requestLine.count >= 2 else { return .failure(ParseFailure("malformed request line")) }
    var headers: [String: String] = [:]
    for line in lines {
      guard let colon = line.firstIndex(of: ":") else { continue }
      let name = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
      let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
      headers[name] = value
    }
    return .success(Head(method: String(requestLine[0]).uppercased(), target: String(requestLine[1]),
                         headers: headers, end: headerEnd.upperBound))
  }

  struct ParseFailure: Error {
    let message: String
    init(_ message: String) { self.message = message }
  }

  static func parse(_ data: Data) -> ParseResult {
    let parsedHead: Head
    switch head(data) {
    case nil: return .incomplete
    case .failure(let failure)?: return .invalid(failure.message)
    case .success(let value)?: parsedHead = value
    }
    let headers = parsedHead.headers
    let contentLength = headers["content-length"].flatMap { Int($0) } ?? 0
    if contentLength < 0 || contentLength > RunnerHTTPServer.maxBodyBytes {
      return .invalid("content-length out of range")
    }
    let bodyStart = parsedHead.end
    if data.count - (bodyStart - data.startIndex) < contentLength { return .incomplete }
    let body = data.subdata(in: bodyStart..<(bodyStart + contentLength))

    let target = parsedHead.target
    var path = target
    var query: [String: String] = [:]
    if let questionMark = target.firstIndex(of: "?") {
      path = String(target[..<questionMark])
      let rawQuery = String(target[target.index(after: questionMark)...])
      for pair in rawQuery.split(separator: "&") {
        let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
        let key = parts[0].removingPercentEncoding ?? parts[0]
        let value = parts.count > 1 ? (parts[1].replacingOccurrences(of: "+", with: " ").removingPercentEncoding ?? parts[1]) : ""
        query[key] = value
      }
    }
    if path.count > 1 && path.hasSuffix("/") { path.removeLast() }
    return .complete(
      HTTPRequest(method: parsedHead.method, path: path, target: target, query: query,
                  headers: headers, body: body)
    )
  }
}

struct HTTPResponse {
  var status: Int
  var body: Data
  var headers: [String: String] = [:]

  /// WDA-style success envelope: `{"value": <value>}`.
  static func value(_ value: Any, headers: [String: String] = [:], sessionId: String? = nil) -> HTTPResponse {
    var envelope: [String: Any] = ["value": value]
    if let sessionId { envelope["sessionId"] = sessionId }
    return HTTPResponse(status: 200, body: encode(envelope), headers: headers)
  }

  /// WDA-style error envelope: `{"value": {"error": code, "message": message}}`.
  static func error(_ status: Int, _ code: String, _ message: String) -> HTTPResponse {
    HTTPResponse(status: status, body: encode(["value": ["error": code, "message": message]]))
  }

  static func from(_ error: Error) -> HTTPResponse {
    if let runnerError = error as? RunnerError {
      return .error(runnerError.status, runnerError.code, runnerError.message)
    }
    return .error(500, "unknown error", String(describing: error))
  }

  static func encode(_ object: Any) -> Data {
    do {
      return try JSONSerialization.data(withJSONObject: object, options: [.withoutEscapingSlashes])
    } catch {
      return Data(#"{"value":{"error":"unknown error","message":"response is not JSON-serializable"}}"#.utf8)
    }
  }

  func serialized() -> Data {
    var head = "HTTP/1.1 \(status) \(Self.reason(status))\r\n"
    head += "Content-Type: application/json; charset=utf-8\r\n"
    head += "Content-Length: \(body.count)\r\n"
    head += "Connection: close\r\n"
    for (name, value) in headers.sorted(by: { $0.key < $1.key }) {
      head += "\(name): \(value)\r\n"
    }
    head += "\r\n"
    var data = Data(head.utf8)
    data.append(body)
    return data
  }

  static func reason(_ status: Int) -> String {
    switch status {
    case 200: return "OK"
    case 400: return "Bad Request"
    case 401: return "Unauthorized"
    case 404: return "Not Found"
    case 405: return "Method Not Allowed"
    case 413: return "Payload Too Large"
    case 500: return "Internal Server Error"
    case 501: return "Not Implemented"
    case 503: return "Service Unavailable"
    default: return "Status"
    }
  }
}

/// A failed request, carried to the WDA error envelope `{"value":{"error":code,"message":...}}`.
/// The codes are WDA's / W3C's, so the daemon's 404 checks ("no such alert", stale elements) hold.
struct RunnerError: Error {
  let status: Int
  let code: String
  let message: String

  static func invalidArgument(_ message: String) -> RunnerError {
    RunnerError(status: 400, code: "invalid argument", message: message)
  }

  static func notFound(_ message: String, code: String = "no such element") -> RunnerError {
    RunnerError(status: 404, code: code, message: message)
  }

  static func failed(_ message: String) -> RunnerError {
    RunnerError(status: 500, code: "unknown error", message: message)
  }

  static func unsupported(_ message: String) -> RunnerError {
    RunnerError(status: 501, code: "unsupported operation", message: message)
  }

  static func invalidSelector(_ message: String) -> RunnerError {
    RunnerError(status: 400, code: "invalid selector", message: message)
  }

  static func staleElement(_ id: String) -> RunnerError {
    RunnerError(
      status: 404, code: "stale element reference",
      message: "The previously found element \(id) is not present in the current view anymore")
  }

  static func noSuchAlert() -> RunnerError {
    RunnerError(
      status: 404, code: "no such alert",
      message: "An attempt was made to operate on a modal dialog when one was not open")
  }
}

/// Whether a queued command is too stale to run: its client is gone, or a state-changing request
/// (anything but GET) waited longer than the daemon would have kept waiting for it.
enum CommandAge {
  static let maxQueuedMs = 10_000.0

  static func shouldDrop(method: String, waitedMs: Double, connectionGone: Bool) -> Bool {
    if connectionGone { return true }
    return method != "GET" && waitedMs > maxQueuedMs
  }
}

/// Requests answered on the capture queue instead of main: GET screenshots and on-device settle.
/// Anything the capture handler declines (a failed off-main capture) falls through to main.
enum CaptureLane {
  static func claims(_ request: HTTPRequest) -> Bool {
    guard request.method == "GET" else { return false }
    let path = route(request.path)
    return path == "/screenshot" || path == "/wda/settle"
  }

  /// The path without a leading `/session/<id>`.
  static func route(_ path: String) -> String {
    let parts = path.split(separator: "/", omittingEmptySubsequences: true)
    if parts.count >= 2, parts[0] == "session" {
      return "/" + parts.dropFirst(2).joined(separator: "/")
    }
    return path
  }
}

/// Request authentication for both listeners (8100 commands, 9100 video). They listen on every
/// interface of the phone, so anything on the same Wi-Fi can connect; only a request signed with
/// this launch's token gets any work done. The Mac generates the token per launch and passes it as
/// `IPU_RUNNER_TOKEN` in the test environment; it never travels over the network (a LAN relay is
/// plain HTTP). Wire format and the signed string: crates/core/src/runner_auth.rs.
///
///   Authorization: IPU-HMAC-SHA256 ts=<unix secs>, nonce=<hex>, sig=<hex HMAC-SHA256>
///
/// Each nonce is accepted once, and only within `maxSkew` of this phone's clock, so a captured
/// request can be neither replayed nor altered. With no token in the environment every request
/// is refused: a runner nobody configured must not be an open door.
final class RunnerAuth {
  static let scheme = "IPU-HMAC-SHA256"
  static let maxSkew: Int64 = 900
  /// Nonces remembered; past this the oldest half is forgotten and nothing at or before the
  /// newest forgotten timestamp is accepted any more.
  static let maxNonces = 50_000
  static let context = "ipu-runner-v1"

  enum Refusal: Equatable {
    case notConfigured, missing, malformed, skew, replay, badSignature

    var message: String {
      switch self {
      case .notConfigured: return "this runner was started without IPU_RUNNER_TOKEN; restart it with iphone-use setup"
      case .missing: return "authorization required"
      case .malformed: return "malformed authorization"
      case .skew: return "request timestamp too far from the phone's clock"
      case .replay: return "request already seen"
      case .badSignature: return "bad signature"
      }
    }
  }

  struct Credentials: Equatable {
    let ts: Int64
    let nonce: String
    let sig: Data
  }

  private let key: SymmetricKey?
  private let lock = NSLock()
  private var seen: [String: Int64] = [:]
  private var floor = Int64.min

  init(token: String?) {
    if let token, RunnerAuth.validToken(token) {
      key = SymmetricKey(data: Data(token.utf8))
    } else {
      key = nil
    }
  }

  var configured: Bool { key != nil }

  static func validToken(_ token: String) -> Bool {
    (32...128).contains(token.utf8.count) && token.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
  }

  static func hex<S: Sequence>(_ bytes: S) -> String where S.Element == UInt8 {
    bytes.map { String(format: "%02x", $0) }.joined()
  }

  static func unhex(_ text: String) -> Data? {
    let utf8 = Array(text.utf8)
    guard utf8.count % 2 == 0 else { return nil }
    var out = Data(capacity: utf8.count / 2)
    var index = 0
    while index < utf8.count {
      guard let pair = UInt8(String(decoding: utf8[index..<index + 2], as: UTF8.self), radix: 16) else { return nil }
      out.append(pair)
      index += 2
    }
    return out
  }

  static func stringToSign(method: String, target: String, ts: Int64, nonce: String, body: Data) -> String {
    "\(context)\n\(method.uppercased())\n\(target)\n\(ts)\n\(nonce)\n\(hex(SHA256.hash(data: body)))"
  }

  static func signature(token: String, method: String, target: String, ts: Int64, nonce: String, body: Data) -> Data {
    let message = Data(stringToSign(method: method, target: target, ts: ts, nonce: nonce, body: body).utf8)
    return Data(HMAC<SHA256>.authenticationCode(for: message, using: SymmetricKey(data: Data(token.utf8))))
  }

  /// `IPU-HMAC-SHA256 ts=…, nonce=…, sig=…`, each part once, any order.
  static func parse(_ value: String) -> Credentials? {
    let trimmed = value.trimmingCharacters(in: .whitespaces)
    guard trimmed.hasPrefix(scheme + " ") else { return nil }
    var fields: [String: String] = [:]
    for part in trimmed.dropFirst(scheme.count + 1).split(separator: ",", omittingEmptySubsequences: false) {
      let pair = part.trimmingCharacters(in: .whitespaces).split(separator: "=", maxSplits: 1).map(String.init)
      guard pair.count == 2, ["ts", "nonce", "sig"].contains(pair[0]), fields[pair[0]] == nil else { return nil }
      fields[pair[0]] = pair[1]
    }
    guard let tsText = fields["ts"], let ts = Int64(tsText), let nonce = fields["nonce"], let sigText = fields["sig"],
          (16...64).contains(nonce.utf8.count), nonce.utf8.allSatisfy({ $0 < 128 && isxdigit(Int32($0)) != 0 }),
          sigText.utf8.count == 64, let sig = unhex(sigText)
    else { return nil }
    return Credentials(ts: ts, nonce: nonce.lowercased(), sig: sig)
  }

  /// Compares every byte whatever the first difference, so the time taken says nothing about
  /// how much of a guessed signature was right.
  static func constantTimeEqual(_ a: Data, _ b: Data) -> Bool {
    guard a.count == b.count else { return false }
    var difference: UInt8 = 0
    for (x, y) in zip(a, b) { difference |= x ^ y }
    return difference == 0
  }

  /// What can be refused from the headers alone (no token configured, no or a malformed
  /// Authorization, a stale timestamp), so a request is turned away before its body is read.
  func precheck(_ headers: [String: String], now: Date = Date()) -> Refusal? {
    guard key != nil else { return .notConfigured }
    guard let authorization = headers["authorization"] else { return .missing }
    guard let credentials = RunnerAuth.parse(authorization) else { return .malformed }
    if abs(Int64(now.timeIntervalSince1970) - credentials.ts) > RunnerAuth.maxSkew { return .skew }
    return nil
  }

  /// nil when `request` may run. Records the nonce, so the same request a second time is refused.
  func check(_ request: HTTPRequest, now: Date = Date()) -> Refusal? {
    check(method: request.method, target: request.target, authorization: request.headers["authorization"],
          body: request.body, now: now)
  }

  func check(method: String, target: String, authorization: String?, body: Data, now: Date = Date()) -> Refusal? {
    guard let key else { return .notConfigured }
    guard let authorization else { return .missing }
    guard let credentials = RunnerAuth.parse(authorization) else { return .malformed }
    let nowSecs = Int64(now.timeIntervalSince1970)
    if abs(nowSecs - credentials.ts) > RunnerAuth.maxSkew { return .skew }
    let message = Data(RunnerAuth.stringToSign(method: method, target: target, ts: credentials.ts,
                                               nonce: credentials.nonce, body: body).utf8)
    let expected = Data(HMAC<SHA256>.authenticationCode(for: message, using: key))
    guard RunnerAuth.constantTimeEqual(expected, credentials.sig) else { return .badSignature }
    // Only a valid signature reaches the nonce memory: nobody without the token can fill it.
    lock.lock()
    defer { lock.unlock() }
    if credentials.ts <= floor || seen[credentials.nonce] != nil { return .replay }
    if seen.count >= RunnerAuth.maxNonces {
      let cutoff = nowSecs - RunnerAuth.maxSkew
      seen = seen.filter { $0.value >= cutoff }
      floor = max(floor, cutoff - 1)
      if seen.count >= RunnerAuth.maxNonces {
        let sorted = seen.values.sorted()
        let newestForgotten = sorted[sorted.count / 2]
        seen = seen.filter { $0.value > newestForgotten }
        floor = max(floor, newestForgotten)
      }
      if credentials.ts <= floor { return .replay }
    }
    seen[credentials.nonce] = credentials.ts
    return nil
  }

  /// What an unauthenticated client gets: a 401 that names nothing about the phone.
  static func refusalResponse(_ refusal: Refusal) -> HTTPResponse {
    var response = HTTPResponse.error(401, "unauthorized", refusal.message)
    response.headers["WWW-Authenticate"] = scheme
    return response
  }
}

/// Accepts connections on a background queue, hands each complete request to `mainHandler` on the
/// main queue (serially — XCTest and the private AX client are main-thread APIs), and answers
/// requests `inlineHandler` claims directly on the transport queue so liveness probes never wait
/// behind a slow command.
final class RunnerHTTPServer {
  static let maxBodyBytes = 8 * 1024 * 1024

  private let queue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.transport")
  private let commandQueue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.commands")
  /// Screen captures (/screenshot, /wda/settle) run here, beside the command queue: they need no
  /// main-thread API, so a tree read on main no longer delays them and they no longer delay it.
  private let captureQueue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.capture")
  private let listener: NWListener
  private let inlineHandler: (HTTPRequest) -> HTTPResponse?
  private let captureHandler: (HTTPRequest) -> HTTPResponse?
  private let mainHandler: (HTTPRequest) -> HTTPResponse
  private let auth: RunnerAuth
  var onFailure: ((Error) -> Void)?

  init(
    port: UInt16,
    auth: RunnerAuth,
    inlineHandler: @escaping (HTTPRequest) -> HTTPResponse?,
    captureHandler: @escaping (HTTPRequest) -> HTTPResponse? = { _ in nil },
    mainHandler: @escaping (HTTPRequest) -> HTTPResponse
  ) throws {
    guard let endpointPort = NWEndpoint.Port(rawValue: port) else {
      throw RunnerError.invalidArgument("invalid port \(port)")
    }
    let parameters = NWParameters.tcp
    parameters.allowLocalEndpointReuse = true
    listener = try NWListener(using: parameters, on: endpointPort)
    self.auth = auth
    self.inlineHandler = inlineHandler
    self.captureHandler = captureHandler
    self.mainHandler = mainHandler
  }

  /// The phone's Wi-Fi IPv4 address (en0), else 127.0.0.1. Only the LAN relay uses the host
  /// part; the default USB relay needs just the port.
  static func deviceAddress() -> String {
    var head: UnsafeMutablePointer<ifaddrs>?
    guard getifaddrs(&head) == 0, let first = head else { return "127.0.0.1" }
    defer { freeifaddrs(head) }
    var cursor: UnsafeMutablePointer<ifaddrs>? = first
    while let entry = cursor {
      defer { cursor = entry.pointee.ifa_next }
      guard let address = entry.pointee.ifa_addr, address.pointee.sa_family == UInt8(AF_INET),
            String(cString: entry.pointee.ifa_name) == "en0"
      else { continue }
      var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
      if getnameinfo(address, socklen_t(address.pointee.sa_len), &host, socklen_t(host.count),
                     nil, 0, NI_NUMERICHOST) == 0 {
        return String(cString: host)
      }
    }
    return "127.0.0.1"
  }

  func start() {
    listener.stateUpdateHandler = { [weak self] state in
      switch state {
      case .ready:
        let port = Int(self?.listener.port?.rawValue ?? 0)
        NSLog("ipu-runner: listening on port %d", port)
        // The line setup-wda.sh waits for (WebDriverAgent's marker, kept so the relay logic and
        // its LAN fallback read the device address the same way).
        NSLog("ServerURLHere->http://%@:%d<-ServerURLHere", RunnerHTTPServer.deviceAddress(), port)
      case .failed(let error):
        NSLog("ipu-runner: listener failed: %@", String(describing: error))
        self?.onFailure?(error)
      default:
        break
      }
    }
    listener.newConnectionHandler = { [weak self] connection in
      guard let self else { return }
      connection.start(queue: self.queue)
      self.receive(on: connection, buffer: Data())
    }
    listener.start(queue: queue)
  }

  func stop() {
    listener.cancel()
  }

  private func receive(on connection: NWConnection, buffer: Data) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { [weak self] data, _, isComplete, error in
      guard let self else {
        connection.cancel()
        return
      }
      var buffer = buffer
      if let data { buffer.append(data) }
      if buffer.count > Self.maxBodyBytes + 64 * 1024 {
        self.send(.error(413, "invalid argument", "request too large"), on: connection)
        return
      }
      switch HTTPRequest.parse(buffer) {
      case .incomplete:
        // Headers in, body still coming: a request with no usable credentials is refused now,
        // before its body is read.
        if case .success(let head)? = HTTPRequest.head(buffer), let refusal = self.auth.precheck(head.headers) {
          NSLog("ipu-runner: refused %@ from %@: %@", head.method, String(describing: connection.endpoint),
                refusal.message)
          self.send(RunnerAuth.refusalResponse(refusal), on: connection)
          return
        }
        if isComplete || error != nil {
          connection.cancel()
        } else {
          self.receive(on: connection, buffer: buffer)
        }
      case .invalid(let message):
        self.send(.error(400, "invalid argument", message), on: connection)
      case .complete(let request):
        self.dispatch(request, on: connection)
      }
    }
  }

  private func dispatch(_ request: HTTPRequest, on connection: NWConnection) {
    let started = DispatchTime.now().uptimeNanoseconds
    // Before anything else: an unauthenticated request does no work at all.
    if let refusal = auth.check(request) {
      NSLog("ipu-runner: refused %@ %@ from %@: %@", request.method, request.path,
            String(describing: connection.endpoint), refusal.message)
      send(RunnerAuth.refusalResponse(refusal), on: connection)
      return
    }
    if let response = inlineHandler(request) {
      finish(request, response, started: started, on: connection)
      return
    }
    if CaptureLane.claims(request) {
      captureQueue.async { [weak self] in
        guard let self else { return }
        if var response = self.captureHandler(request) {
          response.headers["X-IPU-Lane"] = "capture"
          self.finish(request, response, started: started, on: connection)
        } else {
          self.enqueueCommand(request, started: started, on: connection)
        }
      }
      return
    }
    enqueueCommand(request, started: started, on: connection)
  }

  private func enqueueCommand(_ request: HTTPRequest, started: UInt64, on connection: NWConnection) {
    // One command at a time: the serial command queue blocks on main.sync, so even if XCTest
    // spins the main run loop inside a handler (synthesis, queries) no second request can start.
    commandQueue.async { [weak self] in
      guard let self else { return }
      let waitedMs = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
      let gone: Bool
      switch connection.state {
      case .cancelled, .failed: gone = true
      default: gone = false
      }
      // A command the daemon already gave up on must not run late: a retried tap would land twice.
      if CommandAge.shouldDrop(method: request.method, waitedMs: waitedMs, connectionGone: gone) {
        NSLog("ipu-runner: dropped %@ %@ after %.0f ms in the queue (gone=%d)",
              request.method, request.path, waitedMs, gone ? 1 : 0)
        var response = HTTPResponse.error(
          503, "unknown error",
          String(format: "dropped after %.0f ms waiting behind other commands; nothing was executed", waitedMs))
        response.headers["X-IPU-Queued-Ms"] = String(format: "%.0f", waitedMs)
        response.headers["X-IPU-Dropped"] = "1"
        self.finish(request, response, started: started, on: connection)
        return
      }
      var response = DispatchQueue.main.sync { self.mainHandler(request) }
      response.headers["X-IPU-Queued-Ms"] = String(format: "%.0f", waitedMs)
      self.finish(request, response, started: started, on: connection)
    }
  }

  private func finish(_ request: HTTPRequest, _ response: HTTPResponse, started: UInt64, on connection: NWConnection) {
    let elapsedMs = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
    var response = response
    response.headers["Server-Timing"] = String(format: "runner;dur=%.1f", elapsedMs)
    NSLog("ipu-runner: %@ %@ -> %d (%.1f ms)", request.method, request.path, response.status, elapsedMs)
    send(response, on: connection)
  }

  private func send(_ response: HTTPResponse, on connection: NWConnection) {
    connection.send(content: response.serialized(), isComplete: true, completion: .contentProcessed { error in
      if let error {
        NSLog("ipu-runner: send failed: %@", String(describing: error))
      }
      connection.cancel()
    })
  }
}
