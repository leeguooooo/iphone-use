// The iphone-use daemon, hosted inside the runner (crates/ios-embed, linked only when build.sh
// is given IPU_EMBED_LIB). It serves the agent API on the phone's own 127.0.0.1, against this
// runner, so an app on the phone can hand it a goal with no Mac in the request path. The runner
// is the one process on the phone that keeps running while another app is in front.
//
// The entry points are looked up at run time: a runner built without the library starts and
// serves exactly as before, with `daemon.linked = false` in /status.

import Darwin
import Foundation

enum RunnerDaemon {
  static let defaultPort: UInt16 = 8787

  private typealias Start = @convention(c) (
    UnsafePointer<CChar>?, UInt16, UInt16, UInt16, UnsafePointer<CChar>?, UnsafePointer<CChar>?, UnsafePointer<CChar>?
  ) -> Int32
  private typealias State = @convention(c) () -> Int32
  private typealias ErrorText = @convention(c) () -> UnsafePointer<CChar>?
  private typealias PairCode = @convention(c) (UnsafeMutablePointer<CChar>?, Int) -> Int32

  private static func symbol<T>(_ name: String, as: T.Type) -> T? {
    guard let pointer = dlsym(UnsafeMutableRawPointer(bitPattern: -2), name) else { return nil }  // RTLD_DEFAULT
    return unsafeBitCast(pointer, to: T.self)
  }

  static var linked: Bool { symbol("ipu_daemon_start", as: Start.self) != nil }
  private(set) static var port: UInt16?

  /// Starts the daemon unless IPU_DAEMON_PORT=0. Returns at once; the daemon runs on its own
  /// thread (never this one: main serves the runner commands the daemon sends).
  static func start(environment: [String: String], runnerPort: UInt16, mjpegPort: UInt16) {
    guard let start = symbol("ipu_daemon_start", as: Start.self) else {
      NSLog("ipu-runner: embedded daemon not linked")
      return
    }
    let port = environment["IPU_DAEMON_PORT"].flatMap { UInt16($0) } ?? defaultPort
    guard port > 0 else {
      NSLog("ipu-runner: embedded daemon off (IPU_DAEMON_PORT=0)")
      return
    }
    // The agent token defaults to the runner's own token: whoever may drive the runner may
    // drive its daemon, and nobody else (any app on the phone can reach loopback).
    let runnerToken = environment["IPU_RUNNER_TOKEN"]
    let agentToken = environment["IPU_DAEMON_TOKEN"] ?? runnerToken
    let password = environment["IPU_DAEMON_PASSWORD"]
    // Loopback unless a developer opens it (IPU_DAEMON_HOST=0.0.0.0) to reach it from a Mac.
    let host = environment["IPU_DAEMON_HOST"]
    let result = withOptionalCString(host) { host in withOptionalCString(runnerToken) { runner in
      withOptionalCString(agentToken) { agent in
        withOptionalCString(password) { pass in
          start(host, port, runnerPort, mjpegPort, runner, agent, pass)
        }
      }
    } }
    self.port = port
    NSLog("ipu-runner: embedded daemon start on %@:%d → %d", host ?? "127.0.0.1", Int(port), Int(result))
  }

  static func statusValue() -> [String: Any] {
    var value: [String: Any] = ["linked": linked]
    guard let state = symbol("ipu_daemon_state", as: State.self) else { return value }
    let code = state()
    value["state"] = ["not_started", "serving", "failed"][Int(max(0, min(2, code)))]
    if let port { value["port"] = Int(port) }
    if let text = symbol("ipu_daemon_error", as: ErrorText.self)?(), code == 2 {
      value["error"] = String(cString: text)
    }
    return value
  }

  /// `POST /daemon/pair-code`: a one-time code from the daemon's own pairing, for the iPhone app
  /// on this phone (the Mac turns it into `iphoneuse://pair?u=http://127.0.0.1:<port>&c=<code>`).
  /// Signed like every runner request, so only whoever drives this runner can mint one.
  static func pairCodeResponse() -> HTTPResponse {
    guard let pairCode = symbol("ipu_daemon_pair_code", as: PairCode.self) else {
      return .error(503, "daemon not linked", "this runner was built without the on-phone daemon")
    }
    var buffer = [CChar](repeating: 0, count: 128)
    let length = buffer.withUnsafeMutableBufferPointer { pairCode($0.baseAddress, $0.count) }
    guard length > 0 else {
      return .error(503, "daemon not serving", "the on-phone daemon is not serving (see daemon in /status)")
    }
    let code = String(cString: buffer)
    return .value(["code": code, "port": Int(port ?? defaultPort)])
  }

  private static func withOptionalCString<R>(_ value: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
    guard let value else { return body(nil) }
    return value.withCString { body($0) }
  }
}
