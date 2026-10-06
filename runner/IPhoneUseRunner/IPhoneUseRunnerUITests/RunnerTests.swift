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
  var mjpeg: RunnerMJPEGServer?
  var serveExpectation: XCTestExpectation?
  let busyLock = NSLock()
  var busySince: Date?
  /// Issues XCTest recorded while the current request ran (fallback XCUI paths report through here).
  var recordedIssues: [String] = []

  // WDA-compatible surface (RunnerWDA.swift).
  /// The one session id this runner hands out; any id is accepted in session paths.
  static let sessionID = UUID().uuidString
  static let systemVersion = UIDevice.current.systemVersion
  let elements = ElementRegistry()
  /// Last alert scan and when it ran; reused for a second while nothing was POSTed (WdaClient asks
  /// /alert/text and /wda/alert/buttons back to back).
  var alertCache: (at: Date, alert: FoundAlert?)?
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
    NSLog("ipu-runner: xctest issue suppressed: %@", issue.compactDescription)
    recordedIssues.append(issue.compactDescription)
  }

  func testServe() throws {
    let patched = IPURBridge.installQuiescenceBypass()
    NSLog("ipu-runner: quiescence bypass installed on %@", patched.joined(separator: ", "))
    NSLog("ipu-runner: private AX client %@, event synthesis %@",
          IPURBridge.axClient() == nil ? "MISSING" : "available",
          IPURBridge.eventSynthesisAvailable() ? "available" : "MISSING")

    let environment = ProcessInfo.processInfo.environment
    let port = environment["IPU_RUNNER_PORT"].flatMap { UInt16($0) } ?? Self.defaultPort
    let expectation = XCTestExpectation(description: "ipu-runner serves until /shutdown")
    serveExpectation = expectation

    let server = try RunnerHTTPServer(
      port: port,
      inlineHandler: { [weak self] request in self?.inlineResponse(request) },
      mainHandler: { [weak self] request in
        guard let self else { return .error(503, "unknown error", "runner is shutting down") }
        return self.handleOnMain(request)
      }
    )
    server.onFailure = { [weak self] _ in
      DispatchQueue.main.async { self?.serveExpectation?.fulfill() }
    }
    self.server = server
    server.start()

    // MJPEG stream; IPU_RUNNER_MJPEG_PORT=0 turns it off. A failure here must not take the
    // command API down with it.
    let mjpegPort = environment["IPU_RUNNER_MJPEG_PORT"].flatMap { UInt16($0) } ?? Self.defaultMJPEGPort
    if mjpegPort > 0 {
      do {
        let mjpeg = try RunnerMJPEGServer(port: mjpegPort)
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
  }

  // MARK: - Dispatch

  /// Answered on the transport queue: liveness must not wait behind a slow command on main.
  func inlineResponse(_ request: HTTPRequest) -> HTTPResponse? {
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

  func statusValue() -> [String: Any] {
    busyLock.lock()
    let busySince = self.busySince
    busyLock.unlock()
    let bundle = Bundle(for: RunnerTests.self)
    let bundleID = bundle.bundleIdentifier ?? "com.leeguoo.iphone-use.runner"
    let version = bundle.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    var value: [String: Any] = [
      "ready": true,
      "message": "iphone-use native runner is ready to accept commands",
      "state": "success",
      "sessionId": Self.sessionID,
      "os": ["name": "iOS", "version": Self.systemVersion, "sdkVersion": Self.systemVersion],
      "ios": ["ip": NSNull()],
      "build": ["productBundleIdentifier": bundleID, "version": version, "runner": "iphone-use-native"],
      "bundle": bundleID,
      "version": version,
      "busy": busySince != nil,
    ]
    if let mjpeg { value["mjpeg"] = mjpeg.statusValue() }
    if let busySince {
      value["busyMs"] = Int(Date().timeIntervalSince(busySince) * 1000)
    }
    return value
  }

  func handleOnMain(_ request: HTTPRequest) -> HTTPResponse {
    busyLock.lock()
    busySince = Date()
    busyLock.unlock()
    recordedIssues.removeAll()
    if request.method != "GET", Self.mayChangeScreen(request.path) {
      alertCache = nil
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
    var pid: Int32 = 0
    let element = IPURBridge.foregroundApplicationElement(withProbePoint: screenCenter(), pid: &pid)
    return Foreground(element: element as AnyObject?, pid: pid)
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
        return .value(root, headers: treeHeaders(tree, backend: "private-ax", pid: target.pid))
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
    if let error = synthesized() {
      NSLog("ipu-runner: %@ synthesis failed, using XCUICoordinate: %@", name, error)
      let application = foreground().application
      let issuesBefore = recordedIssues.count
      var exception: String?
      IPURBridge.performWithoutQuiescence(application) {
        exception = IPURBridge.catchException { fallback(application) }
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

  func windowSizePoints() -> CGSize {
    var size = UIScreen.main.bounds.size
    let landscape = XCUIDevice.shared.orientation.isLandscape
    if landscape != (size.width > size.height) {
      size = CGSize(width: size.height, height: size.width)
    }
    return size
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
    var candidates: [(AnyObject, Int32)] = []
    if let springBoard = IPURBridge.systemApplicationElement() {
      candidates.append((springBoard as AnyObject, IPURBridge.pid(forAXElement: springBoard)))
    }
    let target = foreground()
    if let element = target.element, !candidates.contains(where: { $0.1 == target.pid }) {
      candidates.append((element, target.pid))
    }
    var privateWorked = false
    for (element, pid) in candidates {
      let tree = IPURBridge.wdaTree(
        forAXElement: element, maxDepth: Self.defaultMaxDepth, maxNodes: Self.defaultMaxNodes,
        extensionCallLimit: 0, rememberKey: pid > 0 ? String(pid) : nil)
      guard (tree[IPURTreeOkKey] as? Bool) == true, let root = tree[IPURTreeRootKey] as? [String: Any] else {
        continue
      }
      privateWorked = true
      if let alert = firstNode(in: root, type: "XCUIElementTypeAlert") {
        return describeAlert(alert, pid: pid)
      }
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
