// WebDriverAgent-compatible routes, so the iphone-use daemon's WdaClient (crates/server/src/wda.rs)
// can point PHONE_REMOTE_WDA_URL at this runner unchanged. Request and response shapes follow
// what WdaClient sends and parses; element finds run over the private AX tree (RunnerElements.swift).

import Foundation
import UIKit
import XCTest

extension RunnerTests {
  /// POSTs that can change what is on screen (everything except session setup, settings and finds).
  static func mayChangeScreen(_ path: String) -> Bool {
    !(path == "/session" || path.hasSuffix("/appium/settings") || path.hasSuffix("/elements")
      || path.hasSuffix("/element") || path == "/shutdown")
  }

  /// Matches `parts` against a pattern such as "element/:id/attribute/:name"; returns the
  /// `:captures` in order.
  private func match(_ parts: [String], _ pattern: String) -> [String]? {
    let patternParts = pattern.split(separator: "/").map(String.init)
    guard patternParts.count == parts.count else { return nil }
    var captures: [String] = []
    for (expected, actual) in zip(patternParts, parts) {
      if expected.hasPrefix(":") {
        captures.append(actual)
      } else if expected != actual {
        return nil
      }
    }
    return captures
  }

  /// Answers WDA routes, with or without a `/session/:sid` prefix (any sid is accepted).
  /// Returns nil for paths that are not WDA routes, so the native routes get them.
  func routeWDA(_ request: HTTPRequest) throws -> HTTPResponse? {
    var parts = request.path.split(separator: "/").map(String.init)
    let method = request.method

    if parts == ["session"] {
      guard method == "POST" else { return nil }
      return try createSession(request)
    }
    if parts.first == "session", parts.count >= 2 {
      parts.removeFirst(2)
      if parts.isEmpty {
        switch method {
        case "DELETE": return .value(NSNull(), sessionId: Self.sessionID)
        case "GET": return .value(["sessionId": Self.sessionID, "capabilities": capabilities()], sessionId: Self.sessionID)
        default: return nil
        }
      }
    }

    switch method {
    case "GET":
      if match(parts, "status") != nil { return .value(statusValue(), sessionId: Self.sessionID) }
      if match(parts, "source") != nil { return try source(request) }
      if match(parts, "screenshot") != nil { return try screenshot() }
      if match(parts, "window/size") != nil { return windowSize() }
      if match(parts, "window/rect") != nil {
        let size = windowSizePoints()
        return .value(["x": 0, "y": 0, "width": size.width, "height": size.height])
      }
      if match(parts, "appium/settings") != nil { return .value(wdaSettings) }
      if match(parts, "wda/locked") != nil { return .value(lockedOnMain()) }
      if match(parts, "wda/apps/list") != nil { return appsList() }
      if match(parts, "wda/activeAppInfo") != nil { return activeAppInfo() }
      if match(parts, "element/active") != nil { return try activeElement() }
      if match(parts, "alert/text") != nil { return try alertText() }
      if match(parts, "wda/alert/buttons") != nil { return try alertButtons() }
      if let id = match(parts, "element/:id/rect")?.first { return try elementRect(id) }
      if let captures = match(parts, "element/:id/attribute/:name") {
        return try elementAttribute(captures[0], captures[1])
      }
      if let id = match(parts, "element/:id/text")?.first {
        let node = try freshNode(id)
        return .value(node.label ?? node.value ?? "")
      }
      if let id = match(parts, "element/:id/displayed")?.first {
        return .value(try freshNode(id).isVisible(screen: windowSizePoints()))
      }
      if let id = match(parts, "element/:id/enabled")?.first { return .value(try freshNode(id).isEnabled) }
      if let id = match(parts, "element/:id/selected")?.first {
        _ = try freshNode(id)
        return .value(false)
      }
      if let id = match(parts, "element/:id/name")?.first { return .value(try freshNode(id).type) }
      return nil

    case "POST":
      if match(parts, "appium/settings") != nil { return try updateSettings(request) }
      if match(parts, "actions") != nil { return try performActions(request) }
      if match(parts, "elements") != nil { return try findElements(request, from: nil) }
      if match(parts, "element") != nil { return try findElement(request, from: nil) }
      if let id = match(parts, "element/:id/elements")?.first { return try findElements(request, from: id) }
      if let id = match(parts, "element/:id/element")?.first { return try findElement(request, from: id) }
      if let id = match(parts, "element/:id/click")?.first { return try clickElement(id) }
      if let id = match(parts, "element/:id/value")?.first { return try setElementValue(id, request) }
      if let id = match(parts, "element/:id/clear")?.first { return try clearElement(id) }
      if let captures = match(parts, "wda/element/:id/:action") {
        return try elementGesture(captures[0], captures[1], request)
      }
      if let id = match(parts, "wda/pickerwheel/:id/select")?.first { return try pickerWheelSelect(id, request) }
      if match(parts, "wda/pressButton") != nil { return try pressButton(request) }
      if match(parts, "wda/homescreen") != nil { return try home() }
      if match(parts, "wda/apps/launch") != nil || match(parts, "wda/apps/activate") != nil {
        return try launchApp(request)
      }
      if match(parts, "wda/keys") != nil { return try wdaKeys(request) }
      if match(parts, "wda/keyboard/dismiss") != nil { return try dismissKeyboard(request) }
      if match(parts, "url") != nil { return try openURL(request) }
      if match(parts, "wda/lock") != nil { return try lockScreen() }
      if match(parts, "wda/unlock") != nil { return try unlockScreen() }
      if match(parts, "alert/accept") != nil { return try alertPress(request, accept: true) }
      if match(parts, "alert/dismiss") != nil { return try alertPress(request, accept: false) }
      return nil

    case "DELETE":
      return nil

    default:
      return nil
    }
  }

  // MARK: - Session

  private func capabilities() -> [String: Any] {
    [
      "device": UIDevice.current.userInterfaceIdiom == .pad ? "ipad" : "iphone",
      "sdkVersion": Self.systemVersion,
      "browserName": NSNull(),
      "CFBundleIdentifier": NSNull(),
    ]
  }

  private func createSession(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = (try? request.jsonObject()) ?? [:]
    let capabilities = body["capabilities"] as? [String: Any]
    let always = capabilities?["alwaysMatch"] as? [String: Any] ?? [:]
    let first = (capabilities?["firstMatch"] as? [[String: Any]])?.first ?? [:]
    let desired = body["desiredCapabilities"] as? [String: Any] ?? [:]
    let bundle = (always["bundleId"] ?? first["bundleId"] ?? desired["bundleId"]) as? String
    if let bundle, !bundle.isEmpty {
      _ = try activate(bundle)
    }
    let value: [String: Any] = ["sessionId": Self.sessionID, "capabilities": self.capabilities()]
    return .value(value, sessionId: Self.sessionID)
  }

  private func updateSettings(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let settings = body["settings"] as? [String: Any] ?? [:]
    for (key, value) in settings {
      wdaSettings[key] = value
    }
    mjpeg?.apply(settings)
    return .value(wdaSettings)
  }

  // MARK: - Trees

  /// The foreground app's tree with live AX elements on every node.
  func foregroundTree(_ target: Foreground? = nil) throws -> UINode {
    let target = target ?? foreground()
    if let element = target.element {
      let tree = IPURBridge.wdaTree(
        forAXElement: element, maxDepth: Self.defaultMaxDepth, maxNodes: Self.defaultMaxNodes,
        extensionCallLimit: Self.defaultExtensionCalls,
        rememberKey: target.pid > 0 ? String(target.pid) : nil, includeElements: true)
      if (tree[IPURTreeOkKey] as? Bool) == true, let root = tree[IPURTreeRootKey] as? [String: Any] {
        return UINode(raw: root, parent: nil, pid: target.pid)
      }
      NSLog("ipu-runner: element tree via private AX failed: %@", tree[IPURTreeErrorKey] as? String ?? "?")
    }
    let application = target.application
    var snapshot: XCUIElementSnapshot?
    var failure: Error?
    IPURBridge.performWithoutQuiescence(application) {
      do { snapshot = try application.snapshot() } catch { failure = error }
    }
    guard let snapshot else {
      throw RunnerError.failed("cannot read the foreground tree: \(failure.map { String(describing: $0) } ?? "no snapshot")")
    }
    let tree = IPURBridge.wdaTree(forSnapshot: snapshot as AnyObject, maxNodes: Self.defaultMaxNodes, includeElements: true)
    guard let root = tree[IPURTreeRootKey] as? [String: Any] else {
      throw RunnerError.failed(tree[IPURTreeErrorKey] as? String ?? "snapshot serialization failed")
    }
    return UINode(raw: root, parent: nil, pid: target.pid)
  }

  /// SpringBoard's tree when SpringBoard is active but not the chosen foreground app (a banner,
  /// a system sheet over an app), else nil.
  private func springBoardTreeIfRelevant(foregroundPID: Int32) -> UINode? {
    guard let springBoard = IPURBridge.systemApplicationElement() else { return nil }
    let pid = IPURBridge.pid(forAXElement: springBoard)
    guard pid > 0, pid != foregroundPID,
          IPURBridge.activeApplicationPIDs().contains(NSNumber(value: pid))
    else { return nil }
    let tree = IPURBridge.wdaTree(
      forAXElement: springBoard, maxDepth: Self.defaultMaxDepth, maxNodes: Self.defaultMaxNodes,
      extensionCallLimit: 0, rememberKey: String(pid), includeElements: true)
    guard let root = tree[IPURTreeRootKey] as? [String: Any] else { return nil }
    return UINode(raw: root, parent: nil, pid: pid)
  }

  /// Re-snapshots a registered element. `depth` 1 reads the element alone (fresh rect/value);
  /// larger depths read its subtree. Elements without a live AX element answer from the stored
  /// node. A live element that cannot be read any more is stale (404, as WDA).
  func freshNode(_ id: String, depth: Int = 1) throws -> UINode {
    guard let stored = elements.node(id) else { throw RunnerError.staleElement(id) }
    guard let axElement = stored.axElement else { return stored }
    let tree = IPURBridge.wdaTree(
      forAXElement: axElement, maxDepth: depth, maxNodes: Self.defaultMaxNodes,
      extensionCallLimit: depth > 1 ? Self.defaultExtensionCalls : 0, rememberKey: nil,
      includeElements: true)
    guard (tree[IPURTreeOkKey] as? Bool) == true, let root = tree[IPURTreeRootKey] as? [String: Any] else {
      NSLog("ipu-runner: element %@ re-read failed: %@", id, tree[IPURTreeErrorKey] as? String ?? "?")
      throw RunnerError.staleElement(id)
    }
    let node = UINode(raw: root, parent: nil, pid: stored.pid)
    // A zero frame on a re-read means the element left the hierarchy.
    if node.rect == .zero, stored.rect != .zero { throw RunnerError.staleElement(id) }
    return node
  }

  // MARK: - Finding elements

  private func locator(_ request: HTTPRequest) throws -> (using: String, value: String) {
    let body = try request.jsonObject()
    guard let using = body["using"] as? String, let value = body["value"] as? String else {
      throw RunnerError.invalidArgument("'using' and 'value' must be strings")
    }
    return (using, value)
  }

  private func findNodes(_ request: HTTPRequest, from id: String?) throws -> [UINode] {
    let (using, value) = try locator(request)
    let screen = windowSizePoints()
    if let id {
      let scope = try freshNode(id, depth: Self.defaultMaxDepth)
      return try Locator.find(using: using, value: value, root: scope, screen: screen)
    }
    let target = foreground()
    let root = try foregroundTree(target)
    let found = try Locator.find(using: using, value: value, root: root, screen: screen)
    if !found.isEmpty { return found }
    if let springBoard = springBoardTreeIfRelevant(foregroundPID: target.pid) {
      return try Locator.find(using: using, value: value, root: springBoard, screen: screen)
    }
    return []
  }

  private func findElements(_ request: HTTPRequest, from id: String?) throws -> HTTPResponse {
    let nodes = try findNodes(request, from: id)
    return .value(nodes.map { ElementReference.make(elements.register($0)) })
  }

  private func findElement(_ request: HTTPRequest, from id: String?) throws -> HTTPResponse {
    guard let node = try findNodes(request, from: id).first else {
      let (using, value) = (try? locator(request)) ?? ("?", "?")
      throw RunnerError.notFound("unable to find an element using '\(using)', value '\(value)'")
    }
    return .value(ElementReference.make(elements.register(node)))
  }

  private func activeElement() throws -> HTTPResponse {
    let root = try foregroundTree()
    let focused = root.descendants().filter(\.isFocused)
    let inputs: Set<String> = [
      "XCUIElementTypeTextField", "XCUIElementTypeSecureTextField", "XCUIElementTypeTextView",
      "XCUIElementTypeSearchField",
    ]
    guard let node = focused.first(where: { inputs.contains($0.type) }) ?? focused.last else {
      throw RunnerError.notFound("there is no focused element")
    }
    return .value(ElementReference.make(elements.register(node)))
  }

  // MARK: - Element reads

  private func rectValue(_ rect: CGRect) -> [String: Any] {
    ["x": rect.origin.x, "y": rect.origin.y, "width": rect.width, "height": rect.height]
  }

  private func elementRect(_ id: String) throws -> HTTPResponse {
    .value(rectValue(try freshNode(id).rect))
  }

  private func elementAttribute(_ id: String, _ name: String) throws -> HTTPResponse {
    let node = try freshNode(id)
    let screen = windowSizePoints()
    func orNull(_ value: Any?) -> Any { value ?? NSNull() }
    switch name {
    case "value", "wdValue": return .value(orNull(node.value))
    case "label", "wdLabel": return .value(orNull(node.label))
    case "name", "wdName": return .value(orNull(node.name))
    case "type", "wdType": return .value(node.type)
    case "rawIdentifier", "identifier": return .value(orNull(node.identifier))
    case "placeholderValue", "wdPlaceholderValue": return .value(orNull(node.placeholder))
    case "enabled", "wdEnabled", "isEnabled": return .value(node.isEnabled)
    case "visible", "wdVisible", "isVisible", "displayed", "hittable", "isHittable":
      return .value(node.isVisible(screen: screen))
    case "accessible", "wdAccessible", "isAccessible": return .value(true)
    case "focused", "hasFocus", "wdFocused", "isFocused": return .value(node.isFocused)
    case "selected", "wdSelected", "isSelected": return .value(false)
    case "rect", "wdRect", "frame": return .value(rectValue(node.rect))
    default: return .value(orNull(node.predicateObject(screen: screen)[name]))
    }
  }

  // MARK: - Element actions

  /// Taps a point through synthesis, falling back to XCUICoordinate.
  func tapPoint(_ point: CGPoint) throws {
    _ = try gesture("tap", synthesized: { IPURBridge.synthesizeTap(at: point, pid: 0) }) { app in
      coordinate(app, point).tap()
    }
  }

  private func clickElement(_ id: String) throws -> HTTPResponse {
    let node = try freshNode(id)
    guard node.rect.width > 0, node.rect.height > 0 else {
      throw RunnerError.failed("element \(id) has an empty frame and cannot be clicked")
    }
    try tapPoint(node.center)
    return .value(NSNull())
  }

  /// Makes `id` the keyboard target: taps it unless it already has focus, then waits briefly for
  /// focus to land.
  private func focus(_ id: String) throws -> UINode {
    var node = try freshNode(id)
    if node.isFocused { return node }
    try tapPoint(node.center)
    let deadline = Date().addingTimeInterval(1.0)
    while Date() < deadline {
      RunLoop.current.run(until: Date().addingTimeInterval(0.1))
      if let fresh = try? freshNode(id) {
        node = fresh
        if fresh.isFocused { break }
      }
    }
    return node
  }

  /// Types text (with WebDriver private-use key codes mapped) into whatever has focus.
  func typeIntoFocus(_ text: String, frequency: UInt = 60) throws -> String {
    let mapped = Self.mapWebDriverKeys(text)
    if mapped.isEmpty { return "none" }
    if let error = IPURBridge.synthesizeText(mapped, charactersPerSecond: frequency, pid: 0) {
      NSLog("ipu-runner: text synthesis failed, using XCUIApplication.typeText: %@", error)
      let application = foreground().application
      let issuesBefore = recordedIssues.count
      var exception: String?
      IPURBridge.performWithoutQuiescence(application) {
        exception = IPURBridge.catchException { application.typeText(mapped) }
      }
      if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
        throw RunnerError.failed("typing failed (synthesis: \(error); typeText: \(failure))")
      }
      return "xcui-typetext"
    }
    return "synthesized"
  }

  /// WebDriver encodes special keys as private-use code points (U+E000…); XCTest typing wants
  /// XCUIKeyboardKey strings. Unknown private-use keys are dropped rather than typed literally.
  static func mapWebDriverKeys(_ text: String) -> String {
    var out = ""
    for scalar in text.unicodeScalars {
      switch scalar.value {
      case 0xE003: out += XCUIKeyboardKey.delete.rawValue
      case 0xE004: out += XCUIKeyboardKey.tab.rawValue
      case 0xE006, 0xE007: out += XCUIKeyboardKey.return.rawValue
      case 0xE00C: out += XCUIKeyboardKey.escape.rawValue
      case 0xE00D: out += " "
      case 0xE012: out += XCUIKeyboardKey.leftArrow.rawValue
      case 0xE013: out += XCUIKeyboardKey.upArrow.rawValue
      case 0xE014: out += XCUIKeyboardKey.rightArrow.rawValue
      case 0xE015: out += XCUIKeyboardKey.downArrow.rawValue
      case 0xE017: out += XCUIKeyboardKey.forwardDelete.rawValue
      case 0xE000...0xF8FF: continue
      default: out.unicodeScalars.append(scalar)
      }
    }
    return out
  }

  /// Joins WDA's `value` (array of strings/characters, or a string) and `text` body forms.
  private func textArgument(_ body: [String: Any], preferText: Bool) -> String? {
    if preferText, let text = body["text"] as? String { return text }
    if let array = body["value"] as? [Any] { return array.map { "\($0)" }.joined() }
    if let string = body["value"] as? String { return string }
    return body["text"] as? String
  }

  private func setElementValue(_ id: String, _ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let node = try freshNode(id)
    // Like WDA: a bare `value` on a picker wheel or slider adjusts it; `text` always types.
    if body["text"] == nil, let value = textArgument(body, preferText: false) {
      if node.type == "XCUIElementTypePickerWheel" {
        try adjust(node, id) { $0.adjust(toPickerWheelValue: value) }
        return .value(NSNull())
      }
      if node.type == "XCUIElementTypeSlider" {
        guard let position = Double(value), (0...1).contains(position) else {
          throw RunnerError.invalidArgument("slider value must be a number in 0...1, got '\(value)'")
        }
        try adjust(node, id) { $0.adjust(toNormalizedSliderPosition: CGFloat(position)) }
        return .value(NSNull())
      }
    }
    guard let text = textArgument(body, preferText: true) else {
      throw RunnerError.invalidArgument("'value' or 'text' is required")
    }
    _ = try focus(id)
    let path = try typeIntoFocus(text, frequency: UInt((body["frequency"] as? NSNumber)?.intValue ?? 60))
    return .value(NSNull(), headers: ["X-IPU-Gesture": path])
  }

  private func clearElement(_ id: String) throws -> HTTPResponse {
    var node = try focus(id)
    for _ in 0..<3 {
      let current = node.value ?? ""
      let count = (current == node.placeholder) ? 0 : current.count
      if count == 0 { break }
      _ = try typeIntoFocus(String(repeating: XCUIKeyboardKey.delete.rawValue, count: min(count, 500)), frequency: 120)
      guard let fresh = try? freshNode(id) else { break }
      node = fresh
    }
    return .value(NSNull())
  }

  /// Resolves a registered node to an XCUIElement (for the few XCUI-only APIs: picker/slider
  /// adjustment, force press). Matches by type, identity and frame; XCUI queries are slow, so this
  /// is used only where synthesis cannot stand in.
  func xcuiElement(for node: UINode) throws -> XCUIElement {
    let application = IPURBridge.application(forPID: node.pid)
      ?? XCUIApplication(bundleIdentifier: IPURBridge.bundleID(forPID: node.pid) ?? Self.springBoardBundleID)
    let type = XCUIElement.ElementType(rawValue: UInt(RunnerElementTypes.rawValue(of: node.type))) ?? .any
    var query = application.descendants(matching: type)
    if let identifier = node.identifier {
      query = query.matching(identifier: identifier)
    } else if let label = node.label {
      query = query.matching(NSPredicate(format: "label == %@", label))
    }
    var found: XCUIElement?
    let target = node.rect
    let exception = IPURBridge.catchException {
      let candidates = query.allElementsBoundByIndex
      if candidates.count == 1 {
        found = candidates[0]
        return
      }
      found = candidates.min { a, b in
        Self.frameDistance(a.frame, target) < Self.frameDistance(b.frame, target)
      }
    }
    if let exception { throw RunnerError.failed("XCUI element lookup failed: \(exception)") }
    guard let found else { throw RunnerError.notFound("no XCUI element matches \(node.type) at \(target)") }
    return found
  }

  private static func frameDistance(_ a: CGRect, _ b: CGRect) -> CGFloat {
    abs(a.minX - b.minX) + abs(a.minY - b.minY) + abs(a.width - b.width) + abs(a.height - b.height)
  }

  private func adjust(_ node: UINode, _ id: String, _ body: (XCUIElement) -> Void) throws {
    let element = try xcuiElement(for: node)
    let application = IPURBridge.application(forPID: node.pid)
    let issuesBefore = recordedIssues.count
    var exception: String?
    IPURBridge.performWithoutQuiescence(application) {
      exception = IPURBridge.catchException { body(element) }
    }
    if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
      throw RunnerError.failed("adjusting element \(id) failed: \(failure)")
    }
  }

  // MARK: - Element gestures (/wda/element/:id/:action)

  private func touch(_ steps: [(String, CGPoint, Double)]) -> [[String: Any]] {
    steps.map { ["type": $0.0, "x": $0.1.x, "y": $0.1.y, "t": $0.2] }
  }

  private func synthesizePaths(_ paths: [[[String: Any]]], name: String) throws {
    if let error = IPURBridge.synthesizeTouchPaths(paths, name: name) {
      throw RunnerError.failed("\(name) failed: \(error)")
    }
  }

  private func elementGesture(_ id: String, _ action: String, _ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let node = try freshNode(id)
    let frame = node.rect
    let center = node.center
    switch action {
    case "touchAndHold":
      let duration = max(0.05, number(body, "duration") ?? 1.0)
      if let error = IPURBridge.synthesizeLongPress(at: center, duration: duration, pid: 0) {
        throw RunnerError.failed("touchAndHold failed: \(error)")
      }
    case "tap":
      try tapPoint(center)
    case "doubleTap":
      try synthesizePaths([
        touch([("down", center, 0), ("up", center, 0.05)]),
        touch([("down", center, 0.15), ("up", center, 0.2)]),
      ], name: "ipu-double-tap")
    case "twoFingerTap":
      let offset = min(20, max(4, frame.width / 6))
      let left = CGPoint(x: center.x - offset, y: center.y)
      let right = CGPoint(x: center.x + offset, y: center.y)
      try synthesizePaths([
        touch([("down", left, 0), ("up", left, 0.1)]),
        touch([("down", right, 0), ("up", right, 0.1)]),
      ], name: "ipu-two-finger-tap")
    case "pinch":
      let scale = number(body, "scale") ?? 1
      let velocity = abs(number(body, "velocity") ?? 1)
      guard scale > 0 else { throw RunnerError.invalidArgument("'scale' must be positive") }
      let half = max(10, min(frame.width, frame.height) / 2 * 0.9)
      let start = scale >= 1 ? half / max(scale, 1) * 0.5 : half
      let end = scale >= 1 ? min(half, start * scale) : max(5, half * scale)
      let duration = min(3, max(0.25, abs(scale - 1) / max(0.1, velocity)))
      try synthesizePaths(twoFingerPaths(center: center, duration: duration) { t in
        let radius = start + (end - start) * t
        return (CGPoint(x: center.x - radius, y: center.y), CGPoint(x: center.x + radius, y: center.y))
      }, name: "ipu-pinch")
    case "rotate":
      let rotation = number(body, "rotation") ?? 0
      let velocity = abs(number(body, "velocity") ?? 1)
      let radius = max(10, min(frame.width, frame.height) / 4)
      let duration = min(3, max(0.25, abs(rotation) / max(0.1, velocity)))
      try synthesizePaths(twoFingerPaths(center: center, duration: duration) { t in
        let angle = rotation * t
        let dx = radius * cos(angle), dy = radius * sin(angle)
        return (CGPoint(x: center.x - dx, y: center.y - dy), CGPoint(x: center.x + dx, y: center.y + dy))
      }, name: "ipu-rotate")
    case "forceTouch":
      guard XCUIDevice.shared.responds(to: NSSelectorFromString("supportsPressureInteraction")),
            (XCUIDevice.shared.value(forKey: "supportsPressureInteraction") as? Bool) == true
      else {
        // WDA's exact refusal, which WdaClient maps to ForcePressUnsupported.
        throw RunnerError(status: 400, code: "invalid argument", message: "Force press is not supported on this device")
      }
      let element = try xcuiElement(for: node)
      let coordinate = element.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
      if let exception = IPURBridge.catchException({ _ = coordinate.perform(NSSelectorFromString("forcePress")) }) {
        throw RunnerError.failed("forceTouch failed: \(exception)")
      }
    case "scrollTo":
      try scrollIntoView(id)
    case "swipe", "scroll":
      guard let direction = (body["direction"] as? String)?.lowercased() else {
        throw RunnerError.invalidArgument("'direction' is required for \(action)")
      }
      let reach = action == "swipe" ? 0.8 : min(1, max(0.1, number(body, "distance") ?? 0.5))
      let dx = frame.width / 2 * reach, dy = frame.height / 2 * reach
      // swipe "up" moves the finger up; scroll "up" reveals content above (finger moves down).
      let sign: CGFloat = action == "swipe" ? 1 : -1
      var end = center
      switch direction {
      case "up": end.y -= dy * sign
      case "down": end.y += dy * sign
      case "left": end.x -= dx * sign
      case "right": end.x += dx * sign
      default: throw RunnerError.invalidArgument("unknown direction '\(direction)'")
      }
      let duration = action == "swipe" ? 0.15 : 0.5
      if let error = IPURBridge.synthesizeDrag(from: center, to: end, duration: duration, pid: 0) {
        throw RunnerError.failed("\(action) failed: \(error)")
      }
    default:
      throw RunnerError(status: 404, code: "unknown command", message: "wda/element/:id/\(action) is not supported by the native runner")
    }
    return .value(NSNull())
  }

  private func twoFingerPaths(
    center: CGPoint, duration: Double, at position: (CGFloat) -> (CGPoint, CGPoint)
  ) -> [[[String: Any]]] {
    let steps = max(4, min(60, Int(duration / 0.016)))
    var first: [(String, CGPoint, Double)] = []
    var second: [(String, CGPoint, Double)] = []
    for step in 0...steps {
      let t = CGFloat(step) / CGFloat(steps)
      let (a, b) = position(t)
      let kind = step == 0 ? "down" : "move"
      first.append((kind, a, duration * Double(t)))
      second.append((kind, b, duration * Double(t)))
    }
    let (a, b) = position(1)
    first.append(("up", a, duration))
    second.append(("up", b, duration))
    return [touch(first), touch(second)]
  }

  /// Approximates WDA's scrollTo: drags the screen toward the element until its frame sits inside
  /// the visible band, re-reading the element after each drag.
  private func scrollIntoView(_ id: String) throws {
    let screen = windowSizePoints()
    let band = CGRect(x: 0, y: screen.height * 0.12, width: screen.width, height: screen.height * 0.73)
    for _ in 0..<12 {
      let node = try freshNode(id)
      let frame = node.rect
      if frame.width > 0, frame.height > 0, band.contains(CGPoint(x: frame.midX, y: frame.midY)) { return }
      let x = screen.width / 2
      var start = CGPoint(x: x, y: screen.height * 0.65)
      var end = CGPoint(x: x, y: screen.height * 0.35)
      if frame.midY < band.minY && frame.height > 0 {
        swap(&start, &end)
      } else if frame.midX > screen.width {
        start = CGPoint(x: screen.width * 0.8, y: screen.height / 2)
        end = CGPoint(x: screen.width * 0.2, y: screen.height / 2)
      } else if frame.maxX < 0 {
        start = CGPoint(x: screen.width * 0.2, y: screen.height / 2)
        end = CGPoint(x: screen.width * 0.8, y: screen.height / 2)
      }
      if let error = IPURBridge.synthesizeDrag(from: start, to: end, duration: 0.4, pid: 0) {
        throw RunnerError.failed("scrollTo failed: \(error)")
      }
      RunLoop.current.run(until: Date().addingTimeInterval(0.35))
    }
    throw RunnerError.failed("could not scroll element \(id) into view")
  }

  private func pickerWheelSelect(_ id: String, _ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let order = (body["order"] as? String ?? "next").lowercased()
    let offset = number(body, "offset") ?? 0.2
    guard order == "next" || order == "previous" else {
      throw RunnerError.invalidArgument("'order' must be 'next' or 'previous'")
    }
    let node = try freshNode(id)
    guard node.type == "XCUIElementTypePickerWheel" else {
      throw RunnerError.invalidArgument("element \(id) is \(node.type), not a picker wheel")
    }
    let before = node.value
    let frame = node.rect
    let dy = frame.height * CGFloat(offset) * (order == "next" ? 1 : -1)
    try tapPoint(CGPoint(x: frame.midX, y: frame.midY + dy))
    // WDA reports an error when the wheel did not move; poll the value briefly.
    let deadline = Date().addingTimeInterval(1.5)
    while Date() < deadline {
      RunLoop.current.run(until: Date().addingTimeInterval(0.1))
      if let fresh = try? freshNode(id), fresh.value != before { return .value(NSNull()) }
    }
    throw RunnerError.failed("picker wheel value did not change from '\(before ?? "")' after selecting \(order)")
  }

  // MARK: - W3C actions

  private func performActions(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    guard let sources = body["actions"] as? [[String: Any]] else {
      throw RunnerError.invalidArgument("'actions' must be an array of input sources")
    }
    var paths: [[[String: Any]]] = []
    var keys = ""
    for source in sources {
      let actions = source["actions"] as? [[String: Any]] ?? []
      switch source["type"] as? String {
      case "pointer":
        paths += try W3CActions.pointerPaths(actions) { try self.freshNode($0).center }
      case "key":
        for action in actions where action["type"] as? String == "keyDown" {
          keys += action["value"] as? String ?? ""
        }
      case "none", nil:
        continue
      case let other?:
        throw RunnerError.invalidArgument("unsupported input source type '\(other)'")
      }
    }
    var used: [String] = []
    if !paths.isEmpty {
      if let error = IPURBridge.synthesizeTouchPaths(paths, name: "ipu-w3c-actions") {
        // A single tap can still go through the public coordinate API.
        if paths.count == 1, let down = paths[0].first, paths[0].count == 2,
           let x = down["x"] as? CGFloat, let y = down["y"] as? CGFloat {
          try tapPoint(CGPoint(x: x, y: y))
          used.append("xcui-coordinate")
        } else {
          throw RunnerError.failed("actions failed: \(error)")
        }
      } else {
        used.append("synthesized")
      }
    }
    if !keys.isEmpty {
      used.append(try typeIntoFocus(keys))
    }
    return .value(NSNull(), headers: ["X-IPU-Gesture": used.joined(separator: ",")])
  }

  // MARK: - Device and apps

  private func pressButton(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let name = (body["name"] as? String ?? "").lowercased()
    let button: XCUIDevice.Button
    switch name {
    case "home": button = .home
    case "volumeup": button = .volumeUp
    case "volumedown": button = .volumeDown
    default: throw RunnerError.invalidArgument("unsupported button '\(name)'; use home, volumeUp or volumeDown")
    }
    if let exception = IPURBridge.catchException({ XCUIDevice.shared.press(button) }) {
      throw RunnerError.failed("pressButton \(name) failed: \(exception)")
    }
    return .value(NSNull())
  }

  private func activate(_ bundle: String) throws -> Int32 {
    let application = XCUIApplication(bundleIdentifier: bundle)
    let issuesBefore = recordedIssues.count
    var exception: String?
    IPURBridge.performWithoutQuiescence(application) {
      exception = IPURBridge.catchException { application.activate() }
    }
    if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
      throw RunnerError.failed("launching \(bundle) failed: \(failure)")
    }
    return IPURBridge.pid(for: application)
  }

  private func launchApp(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    let bundle = try requiredString(body, "bundleId", "bundle")
    _ = try activate(bundle)
    return .value(NSNull())
  }

  /// Active apps as WDA lists them, the foreground app first (WdaClient reads the first entry as
  /// the frontmost bundle, and "only SpringBoard" as the Home Screen).
  private func appsList() -> HTTPResponse {
    let target = foreground()
    var pids = IPURBridge.activeApplicationPIDs().map(\.int32Value)
    if target.pid > 0 {
      pids.removeAll { $0 == target.pid }
      pids.insert(target.pid, at: 0)
    }
    let apps: [[String: Any]] = pids.map { pid in
      ["pid": Int(pid), "bundleId": IPURBridge.bundleID(forPID: pid).map { $0 as Any } ?? NSNull()]
    }
    return .value(apps)
  }

  private func activeAppInfo() -> HTTPResponse {
    let target = foreground()
    return .value([
      "pid": Int(target.pid),
      "bundleId": target.bundleID.map { $0 as Any } ?? NSNull(),
      "name": "",
      "processArguments": ["args": [], "env": [:]],
    ])
  }

  private func wdaKeys(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    guard let text = textArgument(body, preferText: false) else {
      throw RunnerError.invalidArgument("'value' must be an array of strings")
    }
    let frequency = UInt((body["frequency"] as? NSNumber)?.intValue ?? 60)
    let path = try typeIntoFocus(text, frequency: max(1, frequency))
    return .value(NSNull(), headers: ["X-IPU-Gesture": path])
  }

  private func dismissKeyboard(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = (try? request.jsonObject()) ?? [:]
    let names = (body["keyNames"] as? [String]) ?? ["Done", "Return", "return", "Hide keyboard"]
    let root = try foregroundTree()
    var keyboard = root.descendants().first { $0.type == "XCUIElementTypeKeyboard" }
    if keyboard == nil, let springBoard = springBoardTreeIfRelevant(foregroundPID: root.pid) {
      keyboard = springBoard.descendants().first { $0.type == "XCUIElementTypeKeyboard" }
    }
    guard let keyboard else { return .value(NSNull()) }  // no keyboard: already dismissed
    let keys = keyboard.descendants().filter {
      ($0.type == "XCUIElementTypeButton" || $0.type == "XCUIElementTypeKey")
        && (names.contains($0.label ?? "\u{0}") || names.contains($0.name ?? "\u{0}"))
    }
    guard let key = keys.first(where: { $0.isVisible(screen: windowSizePoints()) }) ?? keys.first else {
      throw RunnerError.failed("Did not know how to dismiss the keyboard. Try to dismiss it in the way supported by your application under test.")
    }
    try tapPoint(key.center)
    return .value(NSNull())
  }

  private func openURL(_ request: HTTPRequest) throws -> HTTPResponse {
    let body = try request.jsonObject()
    guard let string = body["url"] as? String, let url = URL(string: string) else {
      throw RunnerError.invalidArgument("'url' must be a valid URL string")
    }
    if #available(iOS 16.4, *) {
      if let exception = IPURBridge.catchException({ XCUIDevice.shared.system.open(url) }) {
        throw RunnerError.failed("opening \(string) failed: \(exception)")
      }
      return .value(NSNull())
    }
    // Before iOS 16.4: type the URL into Safari's address field.
    let safari = XCUIApplication(bundleIdentifier: "com.apple.mobilesafari")
    let issuesBefore = recordedIssues.count
    var exception: String?
    IPURBridge.performWithoutQuiescence(safari) {
      exception = IPURBridge.catchException {
        safari.activate()
        let field = safari.textFields.firstMatch
        field.tap()
        safari.typeText(string + "\n")
      }
    }
    if let failure = exception ?? (recordedIssues.count > issuesBefore ? recordedIssues.last : nil) {
      throw RunnerError.failed("opening \(string) through Safari failed: \(failure)")
    }
    return .value(NSNull())
  }

  /// Lock state when SpringBoardServices is unavailable to the inline path: the cover sheet
  /// (lock screen) window present in SpringBoard's tree.
  private func lockedOnMain() -> Bool {
    var known = ObjCBool(false)
    let locked = IPURBridge.isScreenLocked(&known)
    if known.boolValue { return locked }
    guard let springBoard = IPURBridge.systemApplicationElement() else { return false }
    let tree = IPURBridge.wdaTree(
      forAXElement: springBoard, maxDepth: 6, maxNodes: 500, extensionCallLimit: 0, rememberKey: nil)
    guard let root = tree[IPURTreeRootKey] as? [String: Any] else { return false }
    let node = UINode(raw: root, parent: nil, pid: 0)
    return node.descendants().contains { $0.identifier == "SBCoverSheetWindow" && $0.rect.height > 0 }
  }

  private func lockScreen() throws -> HTTPResponse {
    if lockedOnMain() { return .value(NSNull()) }
    if let error = IPURBridge.pressLockButton() { throw RunnerError.failed(error) }
    return .value(NSNull())
  }

  private func unlockScreen() throws -> HTTPResponse {
    // Without a passcode a Home press wakes and opens the phone; with one it only shows the
    // passcode pad (as with WDA, which cannot type the passcode either).
    guard lockedOnMain() else { return .value(NSNull()) }
    if let exception = IPURBridge.catchException({ XCUIDevice.shared.press(.home) }) {
      throw RunnerError.failed("unlock failed: \(exception)")
    }
    return .value(NSNull())
  }

  // MARK: - Alerts

  func cachedAlert() -> FoundAlert? {
    if let cache = alertCache, Date().timeIntervalSince(cache.at) < 1.0 { return cache.alert }
    let found = findAlert()
    alertCache = (Date(), found)
    return found
  }

  private func alertText() throws -> HTTPResponse {
    guard let alert = cachedAlert() else { throw RunnerError.noSuchAlert() }
    return .value(alert.text)
  }

  private func alertButtons() throws -> HTTPResponse {
    guard let alert = cachedAlert() else { throw RunnerError.noSuchAlert() }
    return .value(alert.buttons.map(\.label))
  }

  /// WDA picks the last button to accept and the first to dismiss when no name is given.
  private func alertPress(_ request: HTTPRequest, accept: Bool) throws -> HTTPResponse {
    let body = (try? request.jsonObject()) ?? [:]
    alertCache = nil
    guard let alert = findAlert() else { throw RunnerError.noSuchAlert() }
    let button: (label: String, rect: CGRect)?
    if let name = body["name"] as? String, !name.isEmpty {
      button = alert.buttons.first { $0.label == name }
        ?? alert.buttons.first { $0.label.localizedCaseInsensitiveCompare(name) == .orderedSame }
      guard button != nil else {
        throw RunnerError.notFound(
          "alert has no button '\(name)' (buttons: \(alert.buttons.map(\.label).joined(separator: ", ")))")
      }
    } else {
      button = accept ? alert.buttons.last : alert.buttons.first
    }
    guard let button else { throw RunnerError.failed("the alert has no buttons") }
    try tapPoint(CGPoint(x: button.rect.midX, y: button.rect.midY))
    alertCache = nil
    return .value(NSNull())
  }
}
