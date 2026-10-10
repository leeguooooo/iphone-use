import CoreGraphics
import Foundation

/// How one finger on the picture becomes one `/control` action, as pure
/// values so every rule is unit-tested. The touch surface feeds it points
/// already normalized to the phone screen (0…1, zoom and pan undone).
///
/// The model is the browser client's, decided on lift: a quick touch is a
/// tap; a still hold past `holdThreshold` is a long press; movement before
/// the hold is a swipe; movement after it is a drag.
enum GestureMapper {
    /// A still finger this long is a long press (and, moved after, a drag).
    static let holdThreshold: TimeInterval = 0.5
    /// Movement under this many points (on this screen) is still a tap.
    static let tapSlop: CGFloat = 10

    /// A release faster than this (normalized phone heights per second) is a
    /// flick: the phone gets a longer, faster swipe so its own scrolling
    /// carries on the way it would under a real finger.
    static let flickVelocity: Double = 1.2
    /// The flick's extra distance, as seconds of the release velocity.
    static let flickCarry: Double = 0.12
    /// The fastest swipe the device runner is asked for.
    static let minimumSwipeMs = 80
    static let maximumSwipeMs = 900

    struct Sample: Equatable {
        /// On the phone screen, 0…1.
        var x: Double
        var y: Double
        /// Seconds, any clock.
        var t: TimeInterval
    }

    /// What the touch was, from its first and last point, how far it moved
    /// on this screen, whether it was held still first, and its release
    /// velocity on the phone screen (normalized units per second).
    static func action(start: Sample, end: Sample, movedPoints: CGFloat, heldBeforeMoving: Bool,
                       releaseVelocity: CGVector = .zero) -> PhoneAction {
        let elapsedMs = Int(((end.t - start.t) * 1000).rounded())
        if movedPoints > tapSlop {
            if heldBeforeMoving {
                return .drag(x1: start.x, y1: start.y, x2: end.x, y2: end.y,
                             holdMs: Int(holdThreshold * 1000),
                             durationMs: min(maximumSwipeMs, max(120, elapsedMs - Int(holdThreshold * 1000))))
            }
            return swipe(start: start, end: end, elapsedMs: elapsedMs, releaseVelocity: releaseVelocity)
        }
        if heldBeforeMoving || elapsedMs >= Int(holdThreshold * 1000) {
            return .longPress(x: start.x, y: start.y, durationMs: min(10_000, max(600, elapsedMs)))
        }
        return .tap(x: start.x, y: start.y)
    }

    /// A swipe that keeps the finger's speed. A slow drag lands exactly
    /// where the finger stopped; a flick is extended along its direction
    /// (and kept fast), so a quick flick scrolls a long list the way it
    /// would on the phone instead of nudging it by the finger's few points.
    static func swipe(start: Sample, end: Sample, elapsedMs: Int, releaseVelocity v: CGVector) -> PhoneAction {
        var x2 = end.x
        var y2 = end.y
        var duration = min(maximumSwipeMs, max(120, elapsedMs))
        let speed = hypot(Double(v.dx), Double(v.dy))
        if speed > flickVelocity {
            let carry = min(speed, 6) * flickCarry
            let ux = Double(v.dx) / speed
            let uy = Double(v.dy) / speed
            (x2, y2) = clampAlongRay(fromX: end.x, fromY: end.y, dx: ux * carry, dy: uy * carry)
            let distance = hypot(x2 - start.x, y2 - start.y)
            // As fast as the finger left, never slower than it moved.
            duration = min(duration, max(minimumSwipeMs, Int(distance / speed * 1000)))
        }
        return .swipe(x1: start.x, y1: start.y, x2: x2, y2: y2, durationMs: duration)
    }

    /// `from + (dx, dy)`, shortened so it stays on the screen (0…1).
    static func clampAlongRay(fromX x: Double, fromY y: Double, dx: Double, dy: Double) -> (Double, Double) {
        var scale = 1.0
        if dx > 0 { scale = min(scale, (1 - x) / dx) }
        if dx < 0 { scale = min(scale, -x / dx) }
        if dy > 0 { scale = min(scale, (1 - y) / dy) }
        if dy < 0 { scale = min(scale, -y / dy) }
        scale = max(0, scale)
        return (min(max(x + dx * scale, 0), 1), min(max(y + dy * scale, 0), 1))
    }

    /// The release velocity from the last few samples (the last ~80 ms),
    /// in normalized units per second; zero when the finger stopped before
    /// lifting.
    static func releaseVelocity(_ samples: [Sample], window: TimeInterval = 0.08) -> CGVector {
        guard let last = samples.last else { return .zero }
        let recent = samples.filter { last.t - $0.t <= window }
        guard let first = recent.first, last.t - first.t > 0.008 else { return .zero }
        let dt = last.t - first.t
        return CGVector(dx: (last.x - first.x) / dt, dy: (last.y - first.y) / dt)
    }
}

/// Zoom and pan of the picture, as pure values: the scale (1 = fit) and
/// the offset of the zoomed picture's center, in view points.
struct ZoomState: Equatable {
    static let maximum: CGFloat = 5
    /// What a two-finger double tap zooms to.
    static let step: CGFloat = 2.5

    var scale: CGFloat = 1
    var offset: CGSize = .zero

    var isZoomed: Bool { scale > 1.01 }

    /// Scale by `factor` around `anchor` (view coordinates, relative to the
    /// view's center), keeping the point under the fingers where it is.
    mutating func zoom(by factor: CGFloat, around anchor: CGPoint) {
        let newScale = min(max(scale * factor, 1), Self.maximum)
        let applied = newScale / scale
        offset = CGSize(width: anchor.x - (anchor.x - offset.width) * applied,
                        height: anchor.y - (anchor.y - offset.height) * applied)
        scale = newScale
    }

    /// Keep the zoomed picture covering the view: never pan past an edge.
    /// `content` is the picture's size at scale 1, `view` the view's.
    mutating func clamp(content: CGSize, view: CGSize) {
        if scale <= 1.001 {
            scale = 1
            offset = .zero
            return
        }
        // On an axis where the zoomed picture is still smaller than the
        // view, it stays centered.
        let maxX = max(0, (content.width * scale - view.width) / 2)
        let maxY = max(0, (content.height * scale - view.height) / 2)
        offset.width = min(max(offset.width, -maxX), maxX)
        offset.height = min(max(offset.height, -maxY), maxY)
    }

    /// Where a point in the view lands on the unzoomed picture (both
    /// relative to the view's center).
    func unzoomed(_ point: CGPoint) -> CGPoint {
        CGPoint(x: (point.x - offset.width) / scale, y: (point.y - offset.height) / scale)
    }
}
