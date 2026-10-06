// W3C Actions (pointer sources) → touch paths for IPURBridge synthesizeTouchPaths:name:.
// Pure logic, no XCTest, so runner/unit-check.sh can exercise it on the Mac.

import CoreGraphics
import Foundation

enum W3CActions {
  static func number(_ object: [String: Any], _ key: String) -> Double? {
    if let value = object[key] as? NSNumber { return value.doubleValue }
    if let value = object[key] as? String { return Double(value) }
    return nil
  }

  /// Turns one W3C pointer source's `actions` into touch paths (one per down…up). Moves while the
  /// pointer is down are sampled every ~16 ms; a down…up with no time between them is held for
  /// 50 ms so iOS sees a tap. Each step is `{"type": "down"|"move"|"up", "x", "y", "t" (s)}`.
  /// `elementCenter` resolves an element-origin pointerMove.
  static func pointerPaths(
    _ actions: [[String: Any]], elementCenter: (String) throws -> CGPoint
  ) throws -> [[[String: Any]]] {
    var paths: [[[String: Any]]] = []
    var current: [[String: Any]] = []
    var t = 0.0
    var position = CGPoint.zero
    var downAt: Double?
    func step(_ type: String, _ point: CGPoint, _ offset: Double) -> [String: Any] {
      ["type": type, "x": Double(point.x), "y": Double(point.y), "t": offset]
    }
    for action in actions {
      let duration = max(0, number(action, "duration") ?? 0) / 1000
      switch action["type"] as? String {
      case "pause":
        t += duration
      case "pointerMove":
        let dx = CGFloat(number(action, "x") ?? 0)
        let dy = CGFloat(number(action, "y") ?? 0)
        var target = CGPoint(x: dx, y: dy)
        if let origin = action["origin"] as? String, origin == "pointer" {
          target = CGPoint(x: position.x + dx, y: position.y + dy)
        } else if let elementID = ElementReference.id(from: action["origin"]) {
          let center = try elementCenter(elementID)
          target = CGPoint(x: center.x + dx, y: center.y + dy)
        }
        if downAt != nil {
          let samples = max(1, min(120, Int((duration / 0.016).rounded(.up))))
          for index in 1...samples {
            let fraction = Double(index) / Double(samples)
            let point = CGPoint(
              x: position.x + (target.x - position.x) * CGFloat(fraction),
              y: position.y + (target.y - position.y) * CGFloat(fraction))
            current.append(step("move", point, t + duration * fraction))
          }
        }
        t += duration
        position = target
      case "pointerDown":
        if downAt == nil {
          current = [step("down", position, t)]
          downAt = t
        }
      case "pointerUp", "pointerCancel":
        if let down = downAt {
          t = max(t, down + 0.05)
          current.append(step("up", position, t))
          paths.append(current)
          current = []
          downAt = nil
        }
      default:
        throw RunnerError.invalidArgument("unsupported pointer action '\(action["type"] ?? "nil")'")
      }
    }
    if let down = downAt {
      current.append(step("up", position, max(t, down + 0.05)))
      paths.append(current)
    }
    return paths
  }
}

/// WDA / W3C element reference objects.
enum ElementReference {
  static let w3cKey = "element-6066-11e4-a52e-4f735466cecf"

  static func make(_ id: String) -> [String: Any] { ["ELEMENT": id, w3cKey: id] }

  static func id(from value: Any?) -> String? {
    guard let object = value as? [String: Any] else { return nil }
    return object[w3cKey] as? String ?? object["ELEMENT"] as? String
  }
}
