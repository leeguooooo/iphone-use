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

/// On-device settle (GET /wda/settle): grayscale thumbnails of consecutive frames are compared
/// here, so the daemon learns the screen stopped moving without shipping screenshots to the Mac.
/// Pure logic, exercised by runner/unit-check.sh.
enum ScreenSettle {
  /// Pixels whose gray level moved by more than `threshold` between two frames of equal size;
  /// `Int.max` when the sizes differ (rotation, a new capture size).
  static func changedPixels(_ a: [UInt8], _ b: [UInt8], threshold: Int = 10) -> Int {
    guard a.count == b.count else { return Int.max }
    var changed = 0
    for index in 0..<a.count where abs(Int(a[index]) - Int(b[index])) > threshold {
      changed += 1
    }
    return changed
  }

  /// The content band (status bar and home-indicator strips excluded) is one flat colour: the
  /// capture of an app that hides its screen. Such frames match whether or not the app moved,
  /// so they prove nothing about settling.
  static func isBlank(_ pixels: [UInt8], width: Int, height: Int, spread: Int = 8) -> Bool {
    guard width > 0, height > 0, pixels.count >= width * height else { return false }
    let top = Int(Double(height) * 0.08), bottom = Int(Double(height) * 0.90)
    guard bottom > top else { return false }
    var low = 255, high = 0
    for row in top..<bottom {
      for column in 0..<width {
        let value = Int(pixels[row * width + column])
        low = min(low, value)
        high = max(high, value)
        if high - low > spread { return false }
      }
    }
    return true
  }

  /// Tracks a stream of frames: stable once no frame changed (beyond `tolerance` pixels) for
  /// `quietMs`, judged from at least two frames.
  struct Tracker {
    let quietMs: Double
    let tolerance: Int
    private(set) var frames = 0
    private(set) var lastChanged = 0
    private var previous: [UInt8]?
    private var lastChangeAt: Double = 0

    init(quietMs: Double, tolerance: Int) {
      self.quietMs = quietMs
      self.tolerance = tolerance
    }

    /// Feeds one frame taken at `atMs`; returns whether the screen counts as settled now.
    mutating func add(_ frame: [UInt8], atMs: Double) -> Bool {
      frames += 1
      if let previous {
        lastChanged = ScreenSettle.changedPixels(previous, frame)
        if lastChanged > tolerance { lastChangeAt = atMs }
      } else {
        lastChangeAt = atMs
      }
      previous = frame
      return frames >= 2 && atMs - lastChangeAt >= quietMs
    }
  }
}

/// A scroll drag that leaves no momentum: most of the distance quickly, then a slow tail, so the
/// release speed (which the list would keep gliding with) is below UIKit's fling threshold.
/// Hardware, iPhone 17 Pro Max: a constant 380 pt/s glided on; a tail of ~150 pt/s stopped dead.
enum ScrollDrag {
  /// Fraction of the distance covered by the slow tail.
  static let tailFraction = 0.15
  static let fastMs = 220.0
  static let tailMs = 250.0

  static func actions(from start: CGPoint, to end: CGPoint) -> [[String: Any]] {
    let split = CGPoint(
      x: start.x + (end.x - start.x) * CGFloat(1 - tailFraction),
      y: start.y + (end.y - start.y) * CGFloat(1 - tailFraction))
    return [
      ["type": "pointerMove", "duration": 0, "x": Double(start.x), "y": Double(start.y)],
      ["type": "pointerDown", "button": 0],
      ["type": "pointerMove", "duration": fastMs, "x": Double(split.x), "y": Double(split.y)],
      ["type": "pointerMove", "duration": tailMs, "x": Double(end.x), "y": Double(end.y)],
      ["type": "pointerUp", "button": 0],
    ]
  }
}
