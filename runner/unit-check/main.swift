// Mac-side unit check of the runner's device-independent logic: HTTP parsing and error envelopes,
// WDA locator strategies (accessibility id, class name, predicate string, class chain) and W3C
// pointer-action parsing. Run with runner/unit-check.sh.

import CoreGraphics
import Foundation

var failures = 0
var checks = 0

func check(_ condition: @autoclosure () throws -> Bool, _ message: String, line: Int = #line) {
  checks += 1
  do {
    if try !condition() {
      failures += 1
      print("FAIL (line \(line)): \(message)")
    }
  } catch {
    failures += 1
    print("FAIL (line \(line)): \(message): threw \(error)")
  }
}

func expectThrows(_ code: String, _ message: String, line: Int = #line, _ body: () throws -> Void) {
  checks += 1
  do {
    try body()
    failures += 1
    print("FAIL (line \(line)): \(message): did not throw")
  } catch let error as RunnerError {
    if error.code != code {
      failures += 1
      print("FAIL (line \(line)): \(message): threw \(error.code), expected \(code)")
    }
  } catch {
    failures += 1
    print("FAIL (line \(line)): \(message): threw \(error)")
  }
}

// MARK: HTTP

do {
  let raw = Data("GET /source?format=json&excluded_attributes=visible,accessible HTTP/1.1\r\nHost: x\r\n\r\n".utf8)
  if case .complete(let request) = HTTPRequest.parse(raw) {
    check(request.method == "GET" && request.path == "/source", "request line")
    check(request.query["excluded_attributes"] == "visible,accessible", "query")
  } else {
    check(false, "GET parses")
  }
  let post = Data("POST /session/S/actions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".utf8)
  if case .complete(let request) = HTTPRequest.parse(post) {
    check((try? request.jsonObject())?.isEmpty == true, "empty JSON body")
  } else {
    check(false, "POST parses")
  }
  if case .incomplete = HTTPRequest.parse(Data("POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}".utf8)) {
  } else {
    check(false, "short body waits for more bytes")
  }
  let response = HTTPResponse.from(RunnerError.noSuchAlert())
  let body = try JSONSerialization.jsonObject(with: response.body) as? [String: Any]
  let value = body?["value"] as? [String: Any]
  check(response.status == 404 && value?["error"] as? String == "no such alert", "no such alert envelope")
  check(String(decoding: response.serialized().prefix(24), as: UTF8.self).hasPrefix("HTTP/1.1 404 Not Found"),
        "404 status line (WdaClient matches '404 Not Found')")
  let session = HTTPResponse.value(["sessionId": "S"], sessionId: "S")
  let sessionBody = try JSONSerialization.jsonObject(with: session.body) as? [String: Any]
  check(sessionBody?["sessionId"] as? String == "S", "top-level sessionId")
}

// MARK: Tree + locators

func node(_ type: String, label: String? = nil, id: String? = nil, value: String? = nil,
          placeholder: String? = nil, rect: [Double] = [0, 0, 10, 10], focused: Bool = false,
          children: [[String: Any]] = []) -> [String: Any] {
  var dictionary: [String: Any] = [
    "type": "XCUIElementType" + type,
    "label": label as Any? ?? NSNull(), "name": (id ?? label) as Any? ?? NSNull(),
    "value": value as Any? ?? NSNull(),
    "rawIdentifier": id as Any? ?? NSNull(), "placeholderValue": placeholder as Any? ?? NSNull(),
    "rect": ["x": rect[0], "y": rect[1], "width": rect[2], "height": rect[3]],
    "isEnabled": "1", "isFocused": focused ? "1" : "0",
  ]
  if !children.isEmpty { dictionary["children"] = children }
  return dictionary
}

let screen = CGSize(width: 390, height: 844)
let tree = UINode(raw: node("Application", label: "Settings", rect: [0, 0, 390, 844], children: [
  node("Window", rect: [0, 0, 390, 844], children: [
    node("Button", label: "Done", rect: [300, 40, 60, 30]),
    node("Button", label: "OK", id: "ok-button", rect: [20, 40, 60, 30]),
    node("TextField", label: "SpotlightSearchField", placeholder: "Search", rect: [10, 100, 300, 40], focused: true),
    node("Picker", rect: [0, 600, 390, 200], children: [
      node("PickerWheel", value: "June", rect: [0, 600, 130, 200]),
      node("PickerWheel", value: "6", rect: [130, 600, 130, 200]),
      node("PickerWheel", value: "2026", rect: [260, 600, 130, 200]),
    ]),
    node("Cell", rect: [0, 900, 390, 44], children: [node("StaticText", label: "Offscreen", rect: [0, 900, 100, 44])]),
    // iOS 26 SpringBoard's search pill: the container and both children carry the label.
    node("Other", label: "搜索", id: "spotlight-pill", rect: [164, 688, 61, 30], children: [
      node("Image", label: "搜索", id: "magnifyingglass", rect: [176, 697, 11, 11]),
      node("StaticText", label: "搜索", rect: [190, 695, 24, 14]),
    ]),
  ]),
]), parent: nil, pid: 1)

func labels(_ nodes: [UINode]) -> [String] { nodes.map { $0.label ?? $0.value ?? "?" } }

do {
  check(labels(try Locator.find(using: "accessibility id", value: "ok-button", root: tree, screen: screen)) == ["OK"],
        "accessibility id matches identifier")
  check(labels(try Locator.find(using: "accessibility id", value: "Done", root: tree, screen: screen)) == ["Done"],
        "accessibility id matches label")
  check(try Locator.find(using: "accessibility id", value: "OK", root: tree, screen: screen).isEmpty,
        "accessibility id ignores the label of an element that has an identifier (WDA name)")
  let pillText = try Locator.find(using: "accessibility id", value: "搜索", root: tree, screen: screen)
  check(pillText.count == 1 && pillText.first?.type == "XCUIElementTypeStaticText",
        "accessibility id 搜索 names only the pill's text, as on WDA")
  check(try Locator.find(using: "accessibility id", value: "spotlight-pill", root: tree, screen: screen).count == 1,
        "accessibility id finds the pill by its identifier")
  check(try Locator.find(using: "class name", value: "XCUIElementTypePickerWheel", root: tree, screen: screen).count == 3,
        "class name")
  let dismiss = "type == 'XCUIElementTypeButton' AND (name IN {'Hide keyboard', 'Done'} OR label IN {'Hide keyboard', 'Done'})"
  check(labels(try Locator.find(using: "predicate string", value: dismiss, root: tree, screen: screen)) == ["Done"],
        "keyboard-dismiss predicate")
  let spotlight = "type == 'XCUIElementTypeTextField' AND (label == 'SpotlightSearchField' OR placeholderValue IN {'搜索', 'Search', '検索'})"
  check(try Locator.find(using: "predicate string", value: spotlight, root: tree, screen: screen).count == 1,
        "spotlight predicate")
  check(try Locator.find(using: "predicate string", value: "focused == 1", root: tree, screen: screen).count == 1,
        "focused == 1")
  check(labels(try Locator.find(using: "predicate string", value: "type == 'XCUIElementTypeStaticText' AND visible == 0",
                                root: tree, screen: screen)) == ["Offscreen"], "geometric visible")
  check(try Locator.find(using: "predicate string", value: "(label == 'OK' OR name == 'OK') AND enabled == 1",
                         root: tree, screen: screen).count == 1, "daemon locator predicate")
  expectThrows("invalid selector", "bad predicate") {
    _ = try Locator.find(using: "predicate string", value: "label ==", root: tree, screen: screen)
  }
  expectThrows("invalid selector", "xpath unsupported") {
    _ = try Locator.find(using: "xpath", value: "//*", root: tree, screen: screen)
  }

  check(labels(try Locator.find(using: "class chain", value: "**/XCUIElementTypePickerWheel", root: tree, screen: screen))
        == ["June", "6", "2026"], "class chain descendants")
  check(labels(try Locator.find(using: "class chain", value: "**/XCUIElementTypeButton[`label == \"OK\"`]",
                                root: tree, screen: screen)) == ["OK"], "class chain predicate")
  check(labels(try Locator.find(using: "class chain", value: "XCUIElementTypeWindow/XCUIElementTypeButton[2]",
                                root: tree, screen: screen)) == ["OK"], "class chain child index")
  check(labels(try Locator.find(using: "class chain", value: "**/XCUIElementTypePickerWheel[-1]",
                                root: tree, screen: screen)) == ["2026"], "class chain negative index")
  check(try Locator.find(using: "class chain", value: "**/XCUIElementTypeCell[$label == 'Offscreen'$]",
                         root: tree, screen: screen).count == 1, "class chain descendant predicate")
  check(try Locator.find(using: "class chain", value: "Button", root: tree, screen: screen).isEmpty,
        "short type name, direct children only")
  expectThrows("invalid selector", "class chain ending in **") {
    _ = try Locator.find(using: "class chain", value: "XCUIElementTypeWindow/**", root: tree, screen: screen)
  }
}

// MARK: W3C actions

do {
  func pointer(_ actions: [[String: Any]]) throws -> [[[String: Any]]] {
    try W3CActions.pointerPaths(actions) { id in
      guard id == "E1" else { throw RunnerError.staleElement(id) }
      return CGPoint(x: 100, y: 200)
    }
  }
  func t(_ step: [String: Any]) -> Double { step["t"] as? Double ?? -1 }

  let tap = try pointer([
    ["type": "pointerMove", "duration": 0, "x": 50, "y": 60],
    ["type": "pointerDown", "button": 0],
    ["type": "pointerUp", "button": 0],
  ])
  check(tap.count == 1 && tap[0].count == 2, "tap is one down/up path")
  check(tap.first?.first?["x"] as? Double == 50 && tap.first?.last?["type"] as? String == "up", "tap position")
  check(abs(t(tap[0][1]) - 0.05) < 1e-9, "tap held 50 ms")

  let hold = try pointer([
    ["type": "pointerMove", "duration": 0, "x": 1, "y": 1],
    ["type": "pointerDown", "button": 0],
    ["type": "pause", "duration": 800],
    ["type": "pointerUp", "button": 0],
  ])
  check(abs(t(hold[0][1]) - 0.8) < 1e-9, "long press lifts after the pause")

  let swipe = try pointer([
    ["type": "pointerMove", "duration": 0, "x": 10, "y": 500],
    ["type": "pointerDown", "button": 0],
    ["type": "pause", "duration": 80],
    ["type": "pointerMove", "duration": 250, "x": 10, "y": 100],
    ["type": "pointerUp", "button": 0],
  ])
  let moves = swipe[0].filter { $0["type"] as? String == "move" }
  check(moves.count == 16, "250 ms move sampled every ~16 ms (got \(moves.count))")
  check(moves.last?["y"] as? Double == 100 && abs(t(moves.last!) - 0.33) < 1e-9, "move ends at target and time")
  check(abs(t(moves.first!) - (0.08 + 0.25 / 16)) < 1e-9, "move starts after the pause")

  let relative = try pointer([
    ["type": "pointerMove", "duration": 0, "x": 5, "y": 5, "origin": ["element-6066-11e4-a52e-4f735466cecf": "E1"]],
    ["type": "pointerDown"], ["type": "pointerUp"],
  ])
  check(relative[0][0]["x"] as? Double == 105 && relative[0][0]["y"] as? Double == 205, "element origin")

  let unfinished = try pointer([["type": "pointerDown"], ["type": "pause", "duration": 10]])
  check(unfinished.count == 1 && unfinished[0].last?["type"] as? String == "up", "dangling down is lifted")
  expectThrows("invalid argument", "unknown pointer action") {
    _ = try pointer([["type": "wiggle"]])
  }
}

// MARK: - H.264 stream

do {
  let parsed = RunnerH264Stream.settings(from: "GET /h264?fps=20&scale=40&kbps=900 HTTP/1.1\r\nHost: x\r\n\r\n")
  check(parsed == RunnerH264Stream.Settings(fps: 20, scalePercent: 40, kbps: 900), "h264 query parsed")
  let clamped = RunnerH264Stream.settings(from: "GET /h264?fps=999&scale=1&kbps=5 HTTP/1.1\r\n\r\n")
  check(clamped == RunnerH264Stream.Settings(fps: 60, scalePercent: 10, kbps: 200), "h264 query clamped")
  check(RunnerH264Stream.settings(from: "GET /h264 HTTP/1.1\r\n\r\n") == RunnerH264Stream.Settings(), "h264 defaults")
  let quality = RunnerH264Stream.settings(from: "GET /h264?mode=quality HTTP/1.1\r\n\r\n")
  check(quality == RunnerH264Stream.Settings.preset(.quality), "h264 quality preset")
  check(quality.scalePercent == 100 && quality.highProfile && quality.fps == 60, "quality = full size, High, 60 fps")
  let overridden = RunnerH264Stream.settings(from: "GET /h264?scale=60&mode=quality&gop=5&skip=0 HTTP/1.1\r\n\r\n")
  check(overridden.mode == .quality && overridden.scalePercent == 60 && overridden.keyframeSeconds == 5
        && !overridden.skipUnchanged && overridden.kbps == 10_000, "explicit values override the mode")
  let performance = RunnerH264Stream.settings(from: "GET /h264?mode=nonsense HTTP/1.1\r\n\r\n")
  check(performance == RunnerH264Stream.Settings() && performance.skipUnchanged
        && performance.keyframeSeconds == 10, "unknown mode = performance, skip on, 10 s keyframes")

  // A flat white band is blank; one with a dark stripe through it is not.
  let w = 120, h = 240
  var white = [UInt8](repeating: 255, count: w * h * 4)
  check(white.withUnsafeBufferPointer {
    RunnerH264Stream.bandIsFlat(width: w, height: h, rowBytes: w * 4, pixels: $0.baseAddress!)
  }, "flat band is blank")
  for y in 60..<180 { for x in 0..<60 { for c in 0..<3 { white[(y * w + x) * 4 + c] = 0 } } }
  check(!white.withUnsafeBufferPointer {
    RunnerH264Stream.bandIsFlat(width: w, height: h, rowBytes: w * 4, pixels: $0.baseAddress!)
  }, "band with content is not blank")

  // A real VideoToolbox encode: the first frame is a keyframe in Annex-B with SPS and PPS.
  var outputs: [(Data, Bool)] = []
  let done = DispatchSemaphore(value: 0)
  if let encoder = RunnerH264Stream.Encoder(
    width: 64, height: 128, settings: RunnerH264Stream.Settings(fps: 30, scalePercent: 50, kbps: 500),
    output: { data, key, _ in outputs.append((data, key)); done.signal() }),
    let context = CGContext(data: nil, width: 64, height: 128, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpaceCreateDeviceRGB(),
                            bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue),
    let image: CGImage = {
      context.setFillColor(red: 0.2, green: 0.4, blue: 0.8, alpha: 1)
      context.fill(CGRect(x: 0, y: 0, width: 64, height: 128))
      return context.makeImage()
    }(),
    let buffer = encoder.pixelBuffer(drawing: image) {
    encoder.encode(buffer, pts: 0, forceKeyframe: true)
    encoder.invalidate()
    _ = done.wait(timeout: .now() + 5)
    let first = outputs.first
    check(first?.1 == true, "first encoded frame is a keyframe")
    let bytes = [UInt8](first?.0 ?? Data())
    check(bytes.starts(with: [0, 0, 0, 1]) && bytes.count > 5 && bytes[4] & 0x1F == 7,
          "keyframe starts with an Annex-B SPS")
    let nalTypes = Set(stride(from: 0, to: max(0, bytes.count - 4), by: 1).compactMap { i -> UInt8? in
      bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1 && i + 4 < bytes.count
        ? bytes[i + 4] & 0x1F : nil
    })
    check(nalTypes.isSuperset(of: [7, 8, 5]), "keyframe carries SPS, PPS and an IDR slice")
  } else {
    check(false, "VideoToolbox H.264 encoder starts")
  }
}

// ScrollDrag (scrollTo without momentum)
do {
  let paths = try W3CActions.pointerPaths(
    ScrollDrag.actions(from: CGPoint(x: 200, y: 620), to: CGPoint(x: 200, y: 330))) { _ in .zero }
  check(paths.count == 1, "one touch path")
  let steps = paths[0]
  check(steps.first?["type"] as? String == "down" && steps.last?["type"] as? String == "up", "down … up")
  // Speed over the last 100 ms before the lift: below the ~250 pt/s where a list starts to glide.
  let upT = steps.last?["t"] as? Double ?? 0
  let recent = steps.filter { ($0["t"] as? Double ?? 0) >= upT - 0.1 }
  let ys = recent.compactMap { $0["y"] as? Double }
  let speed = abs((ys.last ?? 0) - (ys.first ?? 0)) / 0.1
  check(speed < 200, "tail speed \(Int(speed)) pt/s stays under the fling threshold")
  check(abs((steps.last?["y"] as? Double ?? 0) - 330) < 0.01, "ends where asked")
}

// ScreenSettle (on-device settle)
do {
  let gray = [UInt8](repeating: 120, count: 100)
  var moved = gray
  for index in 0..<5 { moved[index] = 200 }
  check(ScreenSettle.changedPixels(gray, gray) == 0, "identical frames change nothing")
  check(ScreenSettle.changedPixels(gray, moved) == 5, "five pixels moved")
  check(ScreenSettle.changedPixels(gray, [UInt8](repeating: 125, count: 100)) == 0, "noise under the threshold is ignored")
  check(ScreenSettle.changedPixels(gray, [UInt8](repeating: 0, count: 50)) == Int.max, "size change counts as a change")

  var screen = [UInt8](repeating: 250, count: 10 * 20)
  for index in 0..<10 { screen[index] = 0 }  // status bar row: outside the content band
  check(ScreenSettle.isBlank(screen, width: 10, height: 20), "flat content band is blank")
  screen[10 * 10 + 4] = 30
  check(!ScreenSettle.isBlank(screen, width: 10, height: 20), "one dark pixel in the content band is not blank")

  var tracker = ScreenSettle.Tracker(quietMs: 150, tolerance: 2)
  check(!tracker.add(gray, atMs: 0), "one frame never settles")
  check(!tracker.add(moved, atMs: 50), "a moving frame resets the quiet window")
  check(!tracker.add(moved, atMs: 100), "quiet for 50 ms is not enough")
  check(tracker.add(moved, atMs: 210), "quiet for 160 ms settles")
  var caret = ScreenSettle.Tracker(quietMs: 100, tolerance: 6)
  var blink = gray
  blink[42] = 0
  check(!caret.add(gray, atMs: 0), "first frame")
  check(!caret.add(blink, atMs: 60), "a blinking caret within tolerance")
  check(caret.add(gray, atMs: 120), "a caret blink alone does not keep the screen unsettled")
}

// MARK: - View services

do {
  let svs = "com.apple.SafariViewService"
  check(ViewService.overlay(foregroundBundle: "com.github.stormbreaker.prod", isPresenting: { $0 == svs }) == svs,
        "a presenting SafariViewService sheet becomes the target")
  check(ViewService.overlay(foregroundBundle: "com.github.stormbreaker.prod", isPresenting: { _ in false }) == nil,
        "no sheet: the foreground app stays")
  check(ViewService.overlay(foregroundBundle: svs, isPresenting: { _ in true }) == nil,
        "already the foreground: nothing to swap")
}

// MARK: - Tap bounds (no tap outside the screen)

do {
  let screen = CGSize(width: 440, height: 956)
  check(TapBounds.onScreen(CGPoint(x: 38, y: 87), screen), "a header button's centre is on screen")
  check(!TapBounds.onScreen(CGPoint(x: 220, y: 1500), screen), "a link below the visible page is off screen")
  check(!TapBounds.onScreen(CGPoint(x: -10, y: 300), screen), "a negative x is off screen")
  check(TapBounds.onScreen(CGPoint(x: 440, y: 956), screen), "the bottom-right corner itself is on screen")
  check(!TapBounds.onScreen(CGPoint(x: 10, y: 10), .zero), "no screen size: nothing is on it")
}

// MARK: - Alert scan (alerts beside a web sheet)

do {
  // SpringBoard 30, GitHub 512 under the sheet, SafariViewService 700 (the read target).
  let pids = AlertScan.candidatePIDs(springBoard: 30, target: 700, active: [30, 512, 700])
  check(pids == [30, 700, 512], "SpringBoard, then the target, then the app under the sheet")
  check(AlertScan.othersThan(target: 700, in: pids) == [30, 512],
        "with a sheet in front, the app underneath is searched too, not only SpringBoard")
  let single = AlertScan.candidatePIDs(springBoard: 30, target: 512, active: [30, 512])
  check(AlertScan.othersThan(target: 512, in: single) == [30], "one app in front: only SpringBoard beside it")
  check(AlertScan.candidatePIDs(springBoard: nil, target: 0, active: [0, 512]) == [512], "pid 0 is skipped")
  check(AlertScan.othersThan(target: 30, in: AlertScan.candidatePIDs(springBoard: 30, target: 30, active: [30])).isEmpty,
        "SpringBoard in front with nothing else active: nothing else to search")
}

print(failures == 0 ? "unit check: \(checks) checks passed" : "unit check: \(failures) of \(checks) checks FAILED")
exit(failures == 0 ? 0 : 1)
