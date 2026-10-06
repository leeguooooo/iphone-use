// Minimal HTTP/1.1 server for the iphone-use native runner: one request per connection,
// `Connection: close`, JSON bodies.
//
// The single-long-running-XCTest-method + NWListener design is adapted from
// callstack/agent-device (MIT License, Copyright (c) 2026 Callstack), RunnerTests+Transport.swift.
// See runner/README.md for the full attribution and license text.

import Foundation
import Network

struct HTTPRequest {
  let method: String
  let path: String
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

  static func parse(_ data: Data) -> ParseResult {
    guard let headerEnd = data.range(of: Data("\r\n\r\n".utf8)) else {
      return data.count > 64 * 1024 ? .invalid("request header too large") : .incomplete
    }
    let head = String(decoding: data.subdata(in: data.startIndex..<headerEnd.lowerBound), as: UTF8.self)
    var lines = head.components(separatedBy: "\r\n")
    guard !lines.isEmpty else { return .invalid("empty request") }
    let requestLine = lines.removeFirst().split(separator: " ", omittingEmptySubsequences: true)
    guard requestLine.count >= 2 else { return .invalid("malformed request line") }
    var headers: [String: String] = [:]
    for line in lines {
      guard let colon = line.firstIndex(of: ":") else { continue }
      let name = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
      let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
      headers[name] = value
    }
    let contentLength = headers["content-length"].flatMap { Int($0) } ?? 0
    if contentLength < 0 || contentLength > RunnerHTTPServer.maxBodyBytes {
      return .invalid("content-length out of range")
    }
    let bodyStart = headerEnd.upperBound
    if data.count - (bodyStart - data.startIndex) < contentLength { return .incomplete }
    let body = data.subdata(in: bodyStart..<(bodyStart + contentLength))

    let target = String(requestLine[1])
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
      HTTPRequest(method: String(requestLine[0]).uppercased(), path: path, query: query, headers: headers, body: body)
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

/// Accepts connections on a background queue, hands each complete request to `mainHandler` on the
/// main queue (serially — XCTest and the private AX client are main-thread APIs), and answers
/// requests `inlineHandler` claims directly on the transport queue so liveness probes never wait
/// behind a slow command.
final class RunnerHTTPServer {
  static let maxBodyBytes = 8 * 1024 * 1024

  private let queue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.transport")
  private let commandQueue = DispatchQueue(label: "com.leeguoo.iphone-use.runner.commands")
  private let listener: NWListener
  private let inlineHandler: (HTTPRequest) -> HTTPResponse?
  private let mainHandler: (HTTPRequest) -> HTTPResponse
  var onFailure: ((Error) -> Void)?

  init(
    port: UInt16,
    inlineHandler: @escaping (HTTPRequest) -> HTTPResponse?,
    mainHandler: @escaping (HTTPRequest) -> HTTPResponse
  ) throws {
    guard let endpointPort = NWEndpoint.Port(rawValue: port) else {
      throw RunnerError.invalidArgument("invalid port \(port)")
    }
    let parameters = NWParameters.tcp
    parameters.allowLocalEndpointReuse = true
    listener = try NWListener(using: parameters, on: endpointPort)
    self.inlineHandler = inlineHandler
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
    if let response = inlineHandler(request) {
      finish(request, response, started: started, on: connection)
      return
    }
    // One command at a time: the serial command queue blocks on main.sync, so even if XCTest
    // spins the main run loop inside a handler (synthesis, queries) no second request can start.
    commandQueue.async { [weak self] in
      guard let self else { return }
      let response = DispatchQueue.main.sync { self.mainHandler(request) }
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
