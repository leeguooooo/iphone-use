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
  ]),
]), parent: nil, pid: 1)

func labels(_ nodes: [UINode]) -> [String] { nodes.map { $0.label ?? $0.value ?? "?" } }

do {
  check(labels(try Locator.find(using: "accessibility id", value: "ok-button", root: tree, screen: screen)) == ["OK"],
        "accessibility id matches identifier")
  check(labels(try Locator.find(using: "accessibility id", value: "Done", root: tree, screen: screen)) == ["Done"],
        "accessibility id matches label")
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

print(failures == 0 ? "unit check: \(checks) checks passed" : "unit check: \(failures) of \(checks) checks FAILED")
exit(failures == 0 ? 0 : 1)
