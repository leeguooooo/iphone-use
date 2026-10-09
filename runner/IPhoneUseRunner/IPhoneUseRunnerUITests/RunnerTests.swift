// iphone-use native runner: a single long-running XCTest method that serves a small WDA-shaped
// HTTP API for the iphone-use daemon.
//
// The overall shape (one test method hosting an NWListener, blocking in XCTWaiter, swallowing
// recorded issues so the runner survives XCTest failures) is adapted from callstack/agent-device
// (MIT License, Copyright (c) 2026 Callstack), RunnerTests.swift. See runner/README.md.

import Network
import UIKit
import XCTest

final class RunnerTests: XCTestCase {
  static let springBoardBundleID = "com.apple.springboard"
  /// WDA's ports, so existing relays and daemon config work unchanged.
  static let defaultPort: UInt16 = 8100
  static let defaultMJPEGPort: UInt16 = 9100
  static let defaultMaxDepth = 64
  static let defaultMaxNodes = 5000
  static let defaultExtensionCalls = 8

  var server: RunnerHTTPServer?
  /// This launch's request authentication (both ports).
  var auth: RunnerAuth?
  var mjpeg: RunnerMJPEGServer?
  var serveExpectation: XCTestExpectation?
  /// Why the command listener stopped, when it failed rather than being shut down.
  var listenFailure: Error?
  let busyLock = NSLock()
  var busySince: Date?
  /// Issues XCTest recorded while the current request ran (fallback XCUI paths report through here).
  var recordedIssues: [String] = []

  // WDA-compatible surface (RunnerWDA.swift).
  /// The one session id this runner hands out; any id is accepted in session paths.
  static let sessionID = UUID().uuidString
  static let systemVersion = UIDevice.current.systemVersion
  let elements = ElementRegistry()
  /// Holds off Auto-Lock while the daemon says the phone is in use (`POST /wda/keepawake`).
  let keepAwake = KeepAwake()
  /// Last alert scan and when it ran; reused for a second while nothing was POSTed (WdaClient asks
  /// /alert/text and /wda/alert/buttons back to back).
  var alertCache: (at: Date, alert: FoundAlert?)?
  /// Where a request's time went, by part (ms and call count), sent back as X-IPU-<Part>-Ms and
  /// X-IPU-<Part>-Calls. Reset per request.
  var requestTiming: [String: (ms: Double, calls: Int)] = [:]

  /// Times `body` as part `name` of the current request.
  func timed<T>(_ name: String, _ body: () throws -> T) rethrows -> T {
    let started = Date()
    defer {
      let entry = requestTiming[name] ?? (0, 0)
      requestTiming[name] = (entry.ms + Date().timeIntervalSince(started) * 1000, entry.calls + 1)
    }
    return try body()
  }
  /// appium/settings values, accepted and echoed (the runner never waits for idle anyway).
  var wdaSettings: [String: Any] = ["waitForIdleTimeout": 0, "animationCoolOffTimeout": 0]

  override func setUp() {
    continueAfterFailure = true
    // Only enforced when xcodebuild runs with -test-timeouts-enabled YES; keep it out of reach
    // anyway so a timeout-enabled launch does not kill the server after the default 10 minutes.
    executionTimeAllowance = 365 * 24 * 60 * 60
  }

  /// The runner must outlive any XCTest failure: a recorded issue would otherwise end the serving
  /// test case. Issues are logged and attached to the request that caused them.
  override func record(_ issue: XCTIssue) {
    if serveEnded {
      super.record(issue)
      return
    }
    NSLog("ipu-runner: xctest issue suppressed: %@", issue.compactDescription)
    recordedIssues.append(issue.compactDescription)
  }

  /// Set once the serve loop is over: from then on a failure is the test's own verdict (a listener
  /// that never came up), not a request's, and must reach xcodebuild.
  var serveEnded = false

  func testServe() throws {
    let patched = IPURBridge.installQuiescenceBypass()
    NSLog("ipu-runner: quiescence bypass installed on %@", patched.joined(separator: ", "))
    NSLog("ipu-runner: private AX client %@, event synthesis %@",
          IPURBridge.axClient() == nil ? "MISSING" : "available",
          IPURBridge.eventSynthesisAvailable() ? "available" : "MISSING")

    let environment = ProcessInfo.processInfo.environment
    let port = environment["IPU_RUNNER_PORT"].flatMap { UInt16($0) } ?? Self.defaultPort
    // Never logged: only whether there is one.
    let auth = RunnerAuth(token: environment["IPU_RUNNER_TOKEN"])
    self.auth = auth
    NSLog("ipu-runner: request signing %@", auth.configured
          ? "required (per-launch token)"
          : "required, but no IPU_RUNNER_TOKEN was given: every request will be refused")
    let expectation = XCTestExpectation(description: "ipu-runner serves until /shutdown")
    serveExpectation = expectation

    let server = try RunnerHTTPServer(
      port: port,
      auth: auth,
      inlineHandler: { [weak self] request in self?.inlineResponse(request) },
      captureHandler: { [weak self] request in self?.captureResponse(request) },
      mainHandler: { [weak self] request in
        guard let self else { return .error(503, "unknown error", "runner is shutting down") }
        return self.handleOnMain(request)
      }
    )
    server.onFailure = { [weak self] error in
      DispatchQueue.main.async {
        self?.listenFailure = error
        self?.serveExpectation?.fulfill()
      }
    }
    self.server = server
    server.start()

    // MJPEG stream; IPU_RUNNER_MJPEG_PORT=0 turns it off. A failure here must not take the
    // command API down with it.
    let mjpegPort = environment["IPU_RUNNER_MJPEG_PORT"].flatMap { UInt16($0) } ?? Self.defaultMJPEGPort
    if mjpegPort > 0 {
      do {
        let mjpeg = try RunnerMJPEGServer(port: mjpegPort, auth: auth)
        mjpeg.start()
        self.mjpeg = mjpeg
      } catch {
        NSLog("ipu-runner: MJPEG server not started: %@", String(describing: error))
      }
    }

    // Block this test (and keep the main run loop spinning for the request handlers) for a year,
    // or until POST /shutdown.
    let result = XCTWaiter.wait(for: [expectation], timeout: 365 * 24 * 60 * 60)
    NSLog("ipu-runner: serve loop ended (%@)", String(describing: result))
    server.stop()
    mjpeg?.stop()
    // A runner that never served must not end as a passing test: xcodebuild would print
    // "TEST EXECUTE SUCCEEDED" for a phone nobody can drive. The usual cause is another runner
    // still holding the port on the phone.
    if let failure = listenFailure {
      let busy = (failure as? NWError).map { error -> Bool in
        if case .posix(let code) = error { return code == .EADDRINUSE }
        return false
      } ?? false
      let message = busy
        ? "ipu-runner: port \(port) on the phone is already in use — another runner (or a previous one still exiting) holds it"
        : "ipu-runner: could not listen on port \(port): \(failure)"
      NSLog("%@", message)
      serveEnded = true
      XCTFail(message)
    }
  }

  // MARK: - Dispatch

  /// Answered on the transport queue: liveness must not wait behind a slow command on main.
  func inlineResponse(_ request: HTTPRequest) -> HTTPResponse? {
    // A keep-awake renewal only moves a deadline; it must not wait behind a slow tree read.
    if request.path == "/wda/keepawake"
      || (request.path.hasPrefix("/session/") && request.path.hasSuffix("/wda/keepawake")) {
      return keepAwakeResponse(request)
    }
    guard request.method == "GET" else { return nil }
    if request.path == "/status" {
      return .value(statusValue(), sessionId: Self.sessionID)
    }
    // WDA serves the lock state without a session; the daemon reads it first precisely because a
    // locked phone stalls everything else, so it must not queue behind main either.
    if request.path == "/wda/locked"
      || (request.path.hasPrefix("/session/") && request.path.hasSuffix("/wda/locked")) {
      var known = ObjCBool(false)
      let locked = IPURBridge.isScreenLocked(&known)
      if known.boolValue { return .value(locked) }
      return nil  // unknown: answered on main (see RunnerWDA.locked)
    }
    return nil
  }

  /// Answered on the capture queue (see CaptureLane). nil hands the request to main.
  func captureResponse(_ request: HTTPRequest) -> HTTPResponse? {
    let started = Date()
    var response: HTTPResponse
    switch CaptureLane.route(request.path) {
    case "/screenshot":
      var failure: NSString?
      guard let png = autoreleasepool(invoking: { IPURBridge.requestedPNGScreenshot(error: &failure) }) else {
        NSLog("ipu-runner: off-main screenshot failed (%@); capturing on main", (failure as String?) ?? "-")
        return nil
      }
      response = .value(png.base64EncodedString())
    case "/wda/settle":
      response = settleScreen(request)
    default:
      return nil
    }
    response.headers["X-IPU-Ms"] = String(Int(Date().timeIntervalSince(started) * 1000))
    return response
  }

  /// The phone's Wi-Fi (en0) IPv4 address, or NSNull off Wi-Fi: setup reads it to reach an
  /// iOS 15/16 runner directly over the LAN.
  static func lanAddress() -> Any {
    let address = RunnerHTTPServer.deviceAddress()
    return address == "127.0.0.1" ? NSNull() : address
  }

  func statusValue() -> [String: Any] {
    busyLock.lock()
    let busySince = self.busySince
    busyLock.unlock()
    let bundle = Bundle(for: RunnerTests.self)
    let bundleID = bundle.bundleIdentifier ?? "com.leeguoo.iphone-use.runner"
    let version = bundle.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    // Ready only when the screen can be read: without the AX client every tree read fails, so an
    // unconditional `ready: true` sent callers into commands that could not work.
    let axClient = IPURBridge.axClient() != nil
    let synthesis = IPURBridge.eventSynthesisAvailable()
    var value: [String: Any] = [
      "ready": axClient,
      "capabilities": ["axClient": axClient, "eventSynthesis": synthesis],
      "message": axClient
        ? "iphone-use native runner is ready to accept commands"
        : "iphone-use native runner is up but cannot read the screen (accessibility client unavailable)",
      "state": "success",
      "sessionId": Self.sessionID,
      "os": ["name": "iOS", "version": Self.systemVersion, "sdkVersion": Self.systemVersion],
      "ios": ["ip": Self.lanAddress() as Any],
      "build": ["productBundleIdentifier": bundleID, "version": version, "runner": "iphone-use-native"],
      "bundle": bundleID,
      "version": version,
      "busy": busySince != nil,
      // Only an authenticated caller ever reads this: setup takes it as proof that the runner
      // refuses unsigned requests, which is what makes a LAN relay safe to enable.
      "auth": ["required": true, "scheme": RunnerAuth.scheme],
    ]
    if let mjpeg {
      value["mjpeg"] = mjpeg.statusValue()
      value["h264"] = mjpeg.h264.statusValue()
    }
    if let busySince {
      value["busyMs"] = Int(Date().timeIntervalSince(busySince) * 1000)
    }
    value["keepAwake"] = keepAwake.statusValue()
    return value
  }

  func handleOnMain(_ request: HTTPRequest) -> HTTPResponse {
    let started = Date()
    if request.method == "POST", KeepAwake.isInput(request.path) {
      keepAwake.noteCommand()
    }
    busyLock.lock()
    busySince = started
    busyLock.unlock()
    recordedIssues.removeAll()
    requestTiming.removeAll()
    // One active-application list and SpringBoard element per request (each list is an AX round
    // trip of ~15 ms); anything that can change the foreground app drops them.
    IPURBridge.setRequestCacheEnabled(true)
    defer { IPURBridge.setRequestCacheEnabled(false) }
    if request.method != "GET", Self.mayChangeScreen(request.path) {
      alertCache = nil
      elements.prune()
    }
    defer {
      busyLock.lock()
      busySince = nil
      busyLock.unlock()
    }
    var response: HTTPResponse = .error(500, "unknown error", "handler did not run")
    let exception = IPURBridge.catchException {
      do {
        response = try self.route(request)
      } catch {
        response = .from(error)
      }
    }
    if let exception {
      response = .error(500, "unknown error", exception)
    }
    if !recordedIssues.isEmpty {
      response.headers["X-IPU-XCTest-Issues"] = String(recordedIssues.count)
    }
    // Where synthesized touches spent their time: X-IPU-Synth-{Orientation,Build,Wait,Hold}-Ms.
    for (part, value) in IPURBridge.takeSynthesisTiming() {
      response.headers[part == "Calls" ? "X-IPU-Synth-Calls" : "X-IPU-Synth-\(part)-Ms"] =
        part == "Calls" ? value.stringValue : String(format: "%.1f", value.doubleValue)
    }
    response.headers["X-IPU-Ms"] = String(Int(Date().timeIntervalSince(started) * 1000))
    for (name, entry) in requestTiming {
      response.headers["X-IPU-\(name)-Ms"] = String(format: "%.1f", entry.ms)
      response.headers["X-IPU-\(name)-Calls"] = String(entry.calls)
    }
    return response
  }

  func route(_ request: HTTPRequest) throws -> HTTPResponse {
    if let response = try routeWDA(request) { return response }
    switch (request.method, request.path) {
    case ("GET", "/source"): return try source(request)
    case ("POST", "/tap"): return try tap(request)
    case ("POST", "/swipe"): return try swipe(request)
    case ("POST", "/longpress"): return try longPress(request)
    case ("POST", "/type"): return try typeText(request)
    case ("POST", "/home"): return try home()
    case ("POST", "/launch"): return try launch(request)
    case ("GET", "/apps/active"): return try activeApp()
    case ("GET", "/screenshot"): return try screenshot()
    case ("GET", "/alert"): return try alert()
    case ("POST", "/alert"): return try alertTap(request)
    case ("GET", "/window/size"): return windowSize()
    case ("POST", "/shutdown"):
      DispatchQueue.main.async { [weak self] in self?.serveExpectation?.fulfill() }
      return .value(["shutdown": true])
    default:
      return .error(404, "unknown command", "\(request.method) \(request.path) is not a runner endpoint")
    }
  }

  // MARK: - Foreground application

  struct Foreground {
    let element: AnyObject?
    let pid: Int32
    var bundleID: String? { IPURBridge.bundleID(forPID: pid) }

    /// An XCUIApplication for public-API fallbacks: the monitored instance for the pid, else one
    /// built from the bundle id, else SpringBoard.
    var application: XCUIApplication {
      if let app = IPURBridge.application(forPID: pid) { return app }
      return XCUIApplication(bundleIdentifier: bundleID ?? RunnerTests.springBoardBundleID)
    }
  }

  func screenCenter() -> CGPoint {
    let size = windowSizePoints()
    return CGPoint(x: size.width / 2, y: size.height / 2)
  }

  func foreground() -> Foreground {
    timed("Foreground") { resolveForeground() }
  }

  private func resolveForeground() -> Foreground {
    var pid: Int32 = 0
    // The probe point (screen size, so an orientation read) only matters when several apps are
    // active; the bridge asks for it then.
    let element = timed("ActiveApps") {
      IPURBridge.foregroundApplicationElement(probePoint: { self.timed("Orientation") { self.screenCenter() } }, pid: &pid)
    }
    if let sheet = timed("ViewService", { viewServiceOverlay(foregroundPID: pid) }) { return sheet }
    return Foreground(element: element as AnyObject?, pid: pid)
  }

  /// A system view service presenting over the foreground app (the in-app Safari or web sign-in
  /// sheet): reads, finds and taps go to it, since the app beneath only hosts a remote view.
  func viewServiceOverlay(foregroundPID: Int32) -> Foreground? {
    let foregroundBundle = IPURBridge.bundleID(forPID: foregroundPID)
    guard let bundle = ViewService.overlay(foregroundBundle: foregroundBundle, isPresenting: { id in
      XCUIApplication(bundleIdentifier: id).state == .runningForeground
    }) else { return nil }
    let pid = IPURBridge.pid(for: XCUIApplication(bundleIdentifier: bundle))
    guard pid > 0 else { return nil }
    return Foreground(element: IPURBridge.activeApplicationElement(forPID: pid) as AnyObject?, pid: pid)
  }

  // MARK: - Arguments

  func number(_ object: [String: Any], _ key: String) -> Double? {
    if let value = object[key] as? NSNumber { return value.doubleValue }
    if let value = object[key] as? String { return Double(value) }
    return nil
  }

  func requiredNumber(_ object: [String: Any], _ key: String) throws -> Double {
    guard let value = number(object, key), value.isFinite else {
      throw RunnerError.invalidArgument("'\(key)' must be a number")
    }
    return value
  }

  func requiredString(_ object: [String: Any], _ keys: String...) throws -> String {
    for key in keys {
      if let value = object[key] as? String, !value.isEmpty { return value }
    }
    throw RunnerError.invalidArgument("'\(keys[0])' must be a non-empty string")
  }

  // MARK: - /source

  func source(_ request: HTTPRequest) throws -> HTTPResponse {
    let explicitDepth = request.query["max_depth"].flatMap { Int($0) }
    let maxDepth = max(1, explicitDepth ?? Self.defaultMaxDepth)
    let maxNodes = max(1, request.query["max_nodes"].flatMap { Int($0) } ?? Self.defaultMaxNodes)
    // An explicit depth is honoured as asked: no frontier re-rooting past it.
    let extensionCalls = request.query["extension_calls"].flatMap { Int($0) }
      ?? (explicitDepth == nil ? Self.defaultExtensionCalls : 0)
    let forceXCUI = request.query["backend"] == "xcui"
    let target = foreground()

    var privateError: String?
    if !forceXCUI, let element = target.element {
      let tree = IPURBridge.wdaTree(
        forAXElement: element,
        maxDepth: maxDepth,
        maxNodes: maxNodes,
        extensionCallLimit: extensionCalls,
        rememberKey: target.pid > 0 ? String(target.pid) : nil
      )
      if (tree[IPURTreeOkKey] as? Bool) == true, let root = tree[IPURTreeRootKey] {
        var headers = treeHeaders(tree, backend: "private-ax", pid: target.pid)
        let scanStarted = Date()
        if let alert = alertBesideTree(
          root as? [String: Any], pid: target.pid, scanSpringBoard: request.query["alert_scan"] == "1") {
          headers["X-IPU-Alert"] = alert
          headers["X-IPU-Alert-Ms"] = String(Int(Date().timeIntervalSince(scanStarted) * 1000))
        }
        return .value(root, headers: headers)
      }
      privateError = tree[IPURTreeErrorKey] as? String ?? "private AX snapshot failed"
      NSLog("ipu-runner: private AX snapshot failed, falling back to XCUI snapshot: %@", privateError!)
    }

    let application = target.application
    var snapshot: XCUIElementSnapshot?
    var snapshotError: Error?
    IPURBridge.performWithoutQuiescence(application) {
      do {
        snapshot = try application.snapshot()
      } catch {
        snapshotError = error
      }
    }
    guard let snapshot else {
      let reason = snapshotError.map { String(describing: $0) } ?? recordedIssues.last ?? "snapshot unavailable"
      throw RunnerError.failed("source failed (private AX: \(privateError ?? "skipped"); XCUI: \(reason))")
    }
    let tree = IPURBridge.wdaTree(forSnapshot: snapshot as AnyObject, maxNodes: maxNodes)
    guard (tree[IPURTreeOkKey] as? Bool) == true, let root = tree[IPURTreeRootKey] else {
      throw RunnerError.failed(tree[IPURTreeErrorKey] as? String ?? "snapshot serialization failed")
    }
    return .value(root, headers: treeHeaders(tree, backend: "xcui-snapshot", pid: target.pid))
  }

  /// The system-alert answer for the screen a tree read just described, so the daemon need not
  /// ask `/alert/text` separately (≈0.15–0.2 s: two more snapshots). `"1"`/`"0"`, priming
  /// `alertCache` for an `/alert/text` right after; `nil` when this read cannot tell.
  ///
  /// Free when the answer is in the tree already: an alert in the front app, or SpringBoard in
  /// front (hardware: with SpringBoard's tel: prompt up, the centre probe resolves SpringBoard,
  /// so the prompt is in the tree just read). With an app in front and no alert in it, only a
  /// SpringBoard snapshot can rule one out (≈70 ms, more than half of a tree read), so that
  /// runs only when asked (`alert_scan=1`: the daemon's own "no alert" answer went stale).
  func alertBesideTree(_ root: [String: Any]?, pid: Int32, scanSpringBoard: Bool) -> String? {
    var found: FoundAlert?
    if let root, let alert = firstNode(in: root, type: "XCUIElementTypeAlert") {
      found = describeAlert(alert, pid: pid)
    } else {
      // Every other active process can hold the alert (SpringBoard, the app under a web sheet,
      // another view service). Answering "0" without looking there would prime `alertCache` with
      // a wrong "no alert" that the next `/alert/text` reuses.
      let others = AlertScan.othersThan(target: pid, in: alertCandidatePIDs(target: pid))
      if !others.isEmpty {
        guard scanSpringBoard else { return nil }
        var unreadable = false
        for other in others {
          switch alertInProcess(pid: other) {
          case .some(.some(let alert)):
            found = alert
          case .some(.none):
            continue
          case .none:
            unreadable = true
            continue
          }
          break
        }
        // An unread process can still hold the alert: say "cannot tell", not "no alert".
        if found == nil && unreadable { return nil }
      }
    }
    alertCache = (Date(), found)
    return found == nil ? "0" : "1"
  }

  /// SpringBoard, the target, then every other active process, by pid.
  func alertCandidatePIDs(target: Int32) -> [Int32] {
    let springBoard = IPURBridge.systemApplicationElement().map { IPURBridge.pid(forAXElement: $0) }
    return AlertScan.candidatePIDs(
      springBoard: springBoard, target: target,
      active: IPURBridge.activeApplicationPIDs().map(\.int32Value))
  }

  /// The alert in one process's private-AX tree: `.some(alert)` or `.some(nil)` when the tree was
  /// read, `nil` when it could not be read.
  func alertInProcess(pid: Int32) -> FoundAlert?? {
    let element: AnyObject?
    if let springBoard = IPURBridge.systemApplicationElement(), IPURBridge.pid(forAXElement: springBoard) == pid {
      element = springBoard as AnyObject
    } else {
      element = IPURBridge.activeApplicationElement(forPID: pid) as AnyObject?
    }
    guard let element else { return nil }
    return alertInTree(element, pid: pid)
  }

  /// The alert in one process's AX tree, read shallow first (AlertScan.shallowDepth); the full
  /// depth is read only when that read failed or the alert's own subtree reached its cap.
  /// `.some(nil)`: read, no alert; `nil`: unreadable.
  func alertInTree(_ element: AnyObject, pid: Int32) -> FoundAlert?? {
    func read(_ depth: Int) -> [String: Any]? {
      let tree = IPURBridge.wdaTree(
        forAXElement: element, maxDepth: depth, maxNodes: Self.defaultMaxNodes,
        extensionCallLimit: 0, rememberKey: pid > 0 ? String(pid) : nil)
      guard (tree[IPURTreeOkKey] as? Bool) == true else { return nil }
      return tree[IPURTreeRootKey] as? [String: Any]
    }
    func full() -> FoundAlert?? {
      guard let root = timed("AlertFullRead", { read(Self.defaultMaxDepth) }) else { return nil }
      return .some(AlertScan.firstAlert(in: root, maxDepth: Self.defaultMaxDepth).map { describeAlert($0.alert, pid: pid) })
    }
    guard let shallow = timed("AlertRead", { read(AlertScan.shallowDepth) }) else { return full() }
    guard let found = AlertScan.firstAlert(in: shallow, maxDepth: AlertScan.shallowDepth) else { return .some(nil) }
    if found.maybeCut, let deep = full(), let alert = deep { return .some(alert) }
    return .some(describeAlert(found.alert, pid: pid))
  }

  func treeHeaders(_ tree: [String: Any], backend: String, pid: Int32) -> [String: String] {
    [
      "X-IPU-Backend": backend,
      "X-IPU-Node-Count": "\(tree[IPURTreeNodeCountKey] ?? 0)",
      "X-IPU-Depth": "\(tree[IPURTreeDepthKey] ?? 0)",
      "X-IPU-Truncated": ((tree[IPURTreeTruncatedKey] as? Bool) == true) ? "1" : "0",
      "X-IPU-Extension-Calls": "\(tree[IPURTreeExtensionCallsKey] ?? 0)",
      "X-IPU-Pid": "\(pid)",
    ]
  }

  // MARK: - Gestures

  /// Runs a synthesized gesture; when the private path is missing or fails, runs `fallback`
  /// (public XCUICoordinate API, quiescence skipped) and reports which path acted.
  func gesture(
    _ name: String,
    synthesized: () -> String?,
    fallback: (XCUIApplication) -> Void
  ) throws -> HTTPResponse {
    if let error = timed("Synthesize", synthesized) {
      NSLog("ipu-runner: %@ synthesis failed, using XCUICoordinate: %@", name, error)
      let application = foreground().application
      let issuesBefore = recordedIssues.count
      var exception: String?
      IPURBridge.performWithoutQuiescence(application) {
        exception = IPURBridge.catchException { fallback(application) }
        IPURBridge.invalidateRequestCache()
      }
      if let exception {
        throw RunnerError.failed("\(name) failed (synthesis: \(error); coordinate: \(exception))")
      }
      if recordedIssues.count > issuesBefore {
        throw RunnerError.failed("\(name) failed (synthesis: \(error); coordinate: \(recordedIssues.last!))")
      }
      return .value(NSNull(), headers: ["X-IPU-Gesture": "xcui-coordinate"])
    }
    return .value(NSNull(), headers: ["X-IPU-Gesture": "synthesized"])
  }

  func coordinate(_ application: XCUIApplication, _ point: CGPoint) -> XCUICoordinate {
    application.coordinate(withNormalizedOffset: CGVector(dx: 0, dy: 0))
      .withOffset(CGVector(dx: point.x, dy: point.y))
  }

  func tap(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let point = CGPoint(x: try requiredNumber(body, "x"), y: try requiredNumber(body, "y"))
    return try gesture("tap", synthesized: { IPURBridge.synthesizeTap(at: point, pid: 0) }) { app in
      coordinate(app, point).tap()
    }
  }

  func swipe(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let start = CGPoint(x: try requiredNumber(body, "x1"), y: try requiredNumber(body, "y1"))
    let end = CGPoint(x: try requiredNumber(body, "x2"), y: try requiredNumber(body, "y2"))
    let duration = max(0.05, (number(body, "duration_ms") ?? 300) / 1000)
    return try gesture("swipe", synthesized: {
      IPURBridge.synthesizeDrag(from: start, to: end, duration: duration, pid: 0)
    }) { app in
      coordinate(app, start).press(forDuration: 0.05, thenDragTo: coordinate(app, end))
    }
  }

  func longPress(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let point = CGPoint(x: try requiredNumber(body, "x"), y: try requiredNumber(body, "y"))
    let duration = max(0.05, (number(body, "duration_ms") ?? 1000) / 1000)
    return try gesture("longpress", synthesized: {
      IPURBridge.synthesizeLongPress(at: point, duration: duration, pid: 0)
    }) { app in
      coordinate(app, point).press(forDuration: duration)
    }
  }

  // MARK: - Text, buttons, apps

  func typeText(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    guard let text = body["text"] as? String else {
      throw RunnerError.invalidArgument("'text' must be a string")
    }
    if text.isEmpty { return .value(NSNull()) }
    let frequency = UInt(max(1, min(1000, number(body, "frequency") ?? 60)))
    if let error = IPURBridge.synthesizeText(text, charactersPerSecond: frequency, pid: 0) {
      NSLog("ipu-runner: text synthesis failed, using XCUIApplication.typeText: %@", error)
      let application = foreground().application
      let issuesBefore = recordedIssues.count
      var exception: String?
      IPURBridge.performWithoutQuiescence(application) {
        exception = IPURBridge.catchException { application.typeText(text) }
      }
      if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
        throw RunnerError.failed("type failed (synthesis: \(error); typeText: \(failure))")
      }
      return .value(NSNull(), headers: ["X-IPU-Gesture": "xcui-typetext"])
    }
    return .value(NSNull(), headers: ["X-IPU-Gesture": "synthesized"])
  }

  func home() throws -> HTTPResponse {
    defer { IPURBridge.invalidateRequestCache() }
    if let exception = IPURBridge.catchException({ XCUIDevice.shared.press(.home) }) {
      throw RunnerError.failed("home failed: \(exception)")
    }
    return .value(NSNull())
  }

  func launch(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let bundle = try requiredString(body, "bundle", "bundleId")
    let application = XCUIApplication(bundleIdentifier: bundle)
    let issuesBefore = recordedIssues.count
    var exception: String?
    IPURBridge.performWithoutQuiescence(application) {
      exception = IPURBridge.catchException { application.activate() }
      IPURBridge.invalidateRequestCache()
    }
    if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
      throw RunnerError.failed("launch \(bundle) failed: \(failure)")
    }
    return .value(["bundle": bundle, "pid": Int(IPURBridge.pid(for: application))])
  }

  func activeApp() throws -> HTTPResponse {
    let target = foreground()
    guard target.pid > 0 else {
      throw RunnerError.notFound("no active application", code: "unknown error")
    }
    return .value([
      "bundleId": target.bundleID.map { $0 as Any } ?? NSNull(),
      "pid": Int(target.pid),
      "activePids": IPURBridge.activeApplicationPIDs(),
    ])
  }

  // MARK: - Screen

  func screenshot() throws -> HTTPResponse {
    var png = Data()
    if let exception = IPURBridge.catchException({ png = XCUIScreen.main.screenshot().pngRepresentation }) {
      throw RunnerError.failed("screenshot failed: \(exception)")
    }
    guard !png.isEmpty else { throw RunnerError.failed("screenshot returned no data") }
    return .value(png.base64EncodedString())
  }

  /// The screen in points as the UI on it is oriented: the interface orientation, never the
  /// physical one (a phone lying on its side reads landscape while its UI stays portrait, and
  /// every tap below y = 390 was refused as off screen).
  func windowSizePoints() -> CGSize {
    ScreenOrientation.size(natural: UIScreen.main.bounds.size, interfaceOrientation: IPURBridge.interfaceOrientation())
  }

  func windowSize() -> HTTPResponse {
    let size = windowSizePoints()
    return .value(["width": size.width, "height": size.height])
  }

  // MARK: - Alerts

  struct FoundAlert {
    let node: [String: Any]
    let pid: Int32
    let text: String
    let buttons: [(label: String, rect: CGRect)]
  }

  /// Looks for an XCUIElementTypeAlert in SpringBoard first (system prompts) and then in the
  /// foreground app, using the private AX snapshot. Falls back to XCUI queries when the private
  /// client is unavailable.
  func findAlert() -> FoundAlert? {
    let target = foreground()
    var privateWorked = false
    for pid in alertCandidatePIDs(target: target.pid) {
      let result: FoundAlert??
      if pid == target.pid, let element = target.element {
        // The target's element is already resolved (a view-service sheet is not always in
        // `activeApplications` under its own pid lookup).
        result = alertInTree(element, pid: pid)
      } else {
        result = alertInProcess(pid: pid)
      }
      guard let readable = result else { continue }
      privateWorked = true
      if let alert = readable { return alert }
    }
    if privateWorked { return nil }
    return findAlertWithQueries()
  }

  func firstNode(in node: [String: Any], type: String) -> [String: Any]? {
    if node["type"] as? String == type { return node }
    for child in node["children"] as? [[String: Any]] ?? [] {
      if let found = firstNode(in: child, type: type) { return found }
    }
    return nil
  }

  func describeAlert(_ alert: [String: Any], pid: Int32) -> FoundAlert {
    var texts: [String] = []
    var buttons: [(String, CGRect)] = []
    func label(_ node: [String: Any]) -> String? {
      for key in ["label", "name", "value"] {
        if let value = node[key] as? String, !value.isEmpty { return value }
      }
      return nil
    }
    func rect(_ node: [String: Any]) -> CGRect {
      let r = node["rect"] as? [String: Any] ?? [:]
      func v(_ key: String) -> CGFloat { CGFloat((r[key] as? NSNumber)?.doubleValue ?? 0) }
      return CGRect(x: v("x"), y: v("y"), width: v("width"), height: v("height"))
    }
    func walk(_ node: [String: Any]) {
      let type = node["type"] as? String
      if type == "XCUIElementTypeButton" {
        if let text = label(node) { buttons.append((text, rect(node))) }
        return
      }
      if type == "XCUIElementTypeStaticText" || type == "XCUIElementTypeTextView",
         let text = label(node), !texts.contains(text) {
        texts.append(text)
      }
      for child in node["children"] as? [[String: Any]] ?? [] { walk(child) }
    }
    for child in alert["children"] as? [[String: Any]] ?? [] { walk(child) }
    if texts.isEmpty, let title = label(alert) { texts.append(title) }
    return FoundAlert(node: alert, pid: pid, text: texts.joined(separator: "\n"), buttons: buttons)
  }

  func findAlertWithQueries() -> FoundAlert? {
    var found: FoundAlert?
    _ = IPURBridge.catchException {
      for application in [XCUIApplication(bundleIdentifier: Self.springBoardBundleID), foreground().application] {
        let alert = application.alerts.firstMatch
        guard alert.exists else { continue }
        let texts = alert.staticTexts.allElementsBoundByIndex.map(\.label).filter { !$0.isEmpty }
        let buttons = alert.buttons.allElementsBoundByIndex.map { ($0.label, $0.frame) }
        found = FoundAlert(
          node: [:], pid: IPURBridge.pid(for: application), text: texts.joined(separator: "\n"),
          buttons: buttons.map { (label: $0.0, rect: $0.1) })
        return
      }
    }
    return found
  }

  func alert() throws -> HTTPResponse {
    guard let alert = findAlert() else {
      throw RunnerError.notFound("no alert is open", code: "no such alert")
    }
    return .value([
      "text": alert.text,
      "buttons": alert.buttons.map(\.label),
      "pid": Int(alert.pid),
    ])
  }

  func alertTap(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let wanted = try requiredString(body, "button", "name")
    guard let alert = findAlert() else {
      throw RunnerError.notFound("no alert is open", code: "no such alert")
    }
    let match = alert.buttons.first { $0.label == wanted }
      ?? alert.buttons.first { $0.label.localizedCaseInsensitiveCompare(wanted) == .orderedSame }
      ?? alert.buttons.first { $0.label.localizedCaseInsensitiveContains(wanted) }
    guard let button = match else {
      throw RunnerError.notFound(
        "alert has no button '\(wanted)' (buttons: \(alert.buttons.map(\.label).joined(separator: ", ")))")
    }
    let point = CGPoint(x: button.rect.midX, y: button.rect.midY)
    _ = try gesture("alert tap", synthesized: { IPURBridge.synthesizeTap(at: point, pid: 0) }) { app in
      coordinate(app, point).tap()
    }
    return .value(["tapped": button.label])
  }
}

// MARK: - Keep awake

extension RunnerTests {
  /// `POST /wda/keepawake {"secs": N}` keeps the phone from auto-locking for the next N seconds
  /// (0 stops at once, at most 900); the daemon renews it while someone is driving the phone, so
  /// a daemon that dies or goes idle lets Auto-Lock take over again on its own.
  /// `GET /wda/keepawake` reports the state. Both also report the lock state, whether a passcode
  /// is set, so the daemon knows whether an unlock can work, and the Auto-Lock setting
  /// (`autoLockSecs`, `autoLockNever`) when ManagedConfiguration answers.
  func keepAwakeResponse(_ request: HTTPRequest) -> HTTPResponse {
    switch request.method {
    case "GET":
      break
    case "POST":
      let body: [String: Any]
      do {
        body = try request.jsonObject()
      } catch {
        return .from(error)
      }
      guard let secs = (body["secs"] as? NSNumber)?.doubleValue, secs.isFinite, secs >= 0 else {
        return .from(RunnerError.invalidArgument("keepawake needs \"secs\": a number of seconds (0 stops)"))
      }
      keepAwake.extend(seconds: min(secs, KeepAwake.maxSeconds))
    default:
      return .error(404, "unknown command", "\(request.method) \(request.path) is not a runner endpoint")
    }
    var value = keepAwake.statusValue()
    if let lock = IPURBridge.screenLockStatus() {
      value["locked"] = lock["locked"]
      value["passcodeEnabled"] = lock["passcodeEnabled"]
    }
    if let autoLock = IPURBridge.autoLockSetting() {
      value["autoLockSecs"] = autoLock["secs"]
      value["autoLockNever"] = autoLock["never"]
    }
    return .value(value)
  }
}

/// Resets the idle timer every `interval` seconds until a deadline. Never acts on a locked phone
/// (a phone its owner just locked must stay locked), and skips a beat when a command ran in the
/// last interval: a tap or a swipe resets the idle timer by itself.
final class KeepAwake {
  static let maxSeconds: TimeInterval = 900
  /// Auto-Lock's shortest setting is 30 s and the screen dims before that; 10 s stays clear of both.
  static let interval: TimeInterval = 10
  /// A beat runs on main between commands; one that takes this long would stall them, so beats
  /// stop for the rest of this runner's life.
  static let slowBeat: TimeInterval = 2

  private let queue = DispatchQueue(label: "ipu.keepawake", qos: .utility)
  private let lock = NSLock()
  private var until: Date?
  private var timer: DispatchSourceTimer?
  private var lastCommand = Date.distantPast
  private var beatQueued = false
  private var disabled: String?
  private var beats = 0
  private var skippedLocked = 0
  private var lastBeat: Date?
  private var lastBeatMs: Int?
  private var lastError: String?

  /// Commands that deliver touches, keys or button presses, which reset the idle timer by
  /// themselves. A tree read, a screenshot or an app launch does not.
  static func isInput(_ path: String) -> Bool {
    let inputSuffixes = [
      "/tap", "/swipe", "/longpress", "/type", "/home", "/actions", "/click", "/value", "/clear",
      "/wda/pressButton", "/wda/homescreen", "/wda/keys", "/wda/unlock", "/alert", "/alert/accept",
      "/alert/dismiss", "/wda/keyboard/dismiss",
    ]
    return inputSuffixes.contains { path.hasSuffix($0) } || path.contains("/wda/element/")
  }

  /// An input command is running: it resets the idle timer itself.
  func noteCommand() {
    lock.lock()
    lastCommand = Date()
    lock.unlock()
  }

  func extend(seconds: TimeInterval) {
    lock.lock()
    defer { lock.unlock() }
    guard seconds > 0 else {
      until = nil
      timer?.cancel()
      timer = nil
      return
    }
    until = Date().addingTimeInterval(seconds)
    guard timer == nil else { return }
    let timer = DispatchSource.makeTimerSource(queue: queue)
    timer.schedule(deadline: .now(), repeating: Self.interval, leeway: .seconds(1))
    timer.setEventHandler { [weak self] in self?.tick() }
    self.timer = timer
    timer.resume()
  }

  private func tick() {
    lock.lock()
    guard let until, until > Date(), disabled == nil else {
      self.until = nil
      timer?.cancel()
      timer = nil
      lock.unlock()
      return
    }
    let recentCommand = Date().timeIntervalSince(lastCommand) < Self.interval
    let queued = beatQueued
    lock.unlock()
    if recentCommand || queued { return }
    var known = ObjCBool(false)
    let locked = IPURBridge.isScreenLocked(&known)
    if !known.boolValue || locked {
      lock.lock()
      skippedLocked += 1
      lock.unlock()
      return
    }
    lock.lock()
    beatQueued = true
    lock.unlock()
    // On main, between commands: the key press shares XCTest's event channel with taps, and
    // two events at once fail with "only one gesture can be performed at a time".
    DispatchQueue.main.async { [weak self] in self?.beat() }
  }

  private func beat() {
    let started = Date()
    // Re-check on main: the phone may have locked while this beat waited behind a command.
    var known = ObjCBool(false)
    let locked = IPURBridge.isScreenLocked(&known)
    let error = (!known.boolValue || locked) ? nil : IPURBridge.resetIdleTimer()
    let took = Date().timeIntervalSince(started)
    lock.lock()
    defer { lock.unlock() }
    beatQueued = false
    if !known.boolValue || locked {
      skippedLocked += 1
      return
    }
    lastBeatMs = Int(took * 1000)
    if let error {
      lastError = error
    } else {
      beats += 1
      lastBeat = Date()
    }
    if took > Self.slowBeat {
      disabled = String(format: "a keep-awake key press took %.1f s; stopped so it cannot stall commands", took)
      NSLog("ipu-runner: %@", disabled!)
    }
  }

  func statusValue() -> [String: Any] {
    lock.lock()
    defer { lock.unlock() }
    let remaining = until.map { max(0, $0.timeIntervalSinceNow) } ?? 0
    var value: [String: Any] = [
      "active": remaining > 0 && disabled == nil,
      "remainingMs": Int(remaining * 1000),
      "intervalMs": Int(Self.interval * 1000),
      "beats": beats,
      "skippedLocked": skippedLocked,
    ]
    if let lastBeat { value["lastBeatAgoMs"] = Int(-lastBeat.timeIntervalSinceNow * 1000) }
    if let lastBeatMs { value["lastBeatMs"] = lastBeatMs }
    if let lastError { value["lastError"] = lastError }
    if let disabled { value["disabled"] = disabled }
    return value
  }
}
