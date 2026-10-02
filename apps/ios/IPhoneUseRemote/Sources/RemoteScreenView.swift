import SwiftUI
import UIKit

/// The phone's screen plus the touch surface that drives it.
///
/// Gestures follow the browser client's model, decided on lift:
/// a quick touch is a tap; a still hold past `holdThreshold` is a long press;
/// movement before the hold is a swipe; movement after it is a drag. Each
/// becomes one `/control` action, and the touch point is drawn immediately so
/// the person sees their input before the phone's picture catches up.
final class RemoteScreenUIView: UIView {
    let video = VideoDisplayView()
    var onAction: ((PhoneAction) -> Void)?

    private let holdThreshold: TimeInterval = 0.5
    private let tapSlop: CGFloat = 10

    private var start: (point: CGPoint, time: TimeInterval)?
    private var movedAt: TimeInterval?
    private var heldBeforeMoving = false
    private var holdTimer: Timer?
    private let dot = CAShapeLayer()
    private let trail = CAShapeLayer()
    private let trailPath = UIBezierPath()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isMultipleTouchEnabled = false
        addSubview(video)
        dot.path = UIBezierPath(ovalIn: CGRect(x: -18, y: -18, width: 36, height: 36)).cgPath
        dot.fillColor = UIColor.white.withAlphaComponent(0.35).cgColor
        dot.strokeColor = UIColor.white.withAlphaComponent(0.8).cgColor
        dot.lineWidth = 2
        dot.opacity = 0
        trail.strokeColor = UIColor.white.withAlphaComponent(0.6).cgColor
        trail.fillColor = UIColor.clear.cgColor
        trail.lineWidth = 4
        trail.lineCap = .round
        layer.addSublayer(trail)
        layer.addSublayer(dot)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override func layoutSubviews() {
        super.layoutSubviews()
        video.frame = bounds
    }

    /// Normalize a point to the phone screen; nil outside the picture.
    private func normalized(_ point: CGPoint) -> (Double, Double)? {
        let rect = video.videoRect
        guard rect.width > 0, rect.height > 0, rect.insetBy(dx: -2, dy: -2).contains(point) else {
            return nil
        }
        let x = min(max((point.x - rect.minX) / rect.width, 0), 1)
        let y = min(max((point.y - rect.minY) / rect.height, 0), 1)
        return (Double(x), Double(y))
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = touches.first else { return }
        let point = touch.location(in: self)
        guard normalized(point) != nil else { return }
        start = (point, touch.timestamp)
        movedAt = nil
        heldBeforeMoving = false
        showDot(at: point)
        trailPath.removeAllPoints()
        trailPath.move(to: point)
        holdTimer?.invalidate()
        holdTimer = Timer.scheduledTimer(withTimeInterval: holdThreshold, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.start != nil, self.movedAt == nil else { return }
                self.heldBeforeMoving = true
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                self.dot.fillColor = UIColor.systemBlue.withAlphaComponent(0.45).cgColor
            }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = touches.first, let start else { return }
        let point = touch.location(in: self)
        if movedAt == nil, hypot(point.x - start.point.x, point.y - start.point.y) > tapSlop {
            movedAt = touch.timestamp
            holdTimer?.invalidate()
        }
        if movedAt != nil {
            trailPath.addLine(to: point)
            trail.path = trailPath.cgPath
            trail.opacity = 1
            dot.position = point
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        defer { finish() }
        guard let touch = touches.first, let start,
              let a = normalized(start.point) else { return }
        let point = touch.location(in: self)
        let b = normalized(point) ?? a
        let elapsedMs = Int((touch.timestamp - start.time) * 1000)
        let distance = hypot(point.x - start.point.x, point.y - start.point.y)
        let action: PhoneAction
        if distance > tapSlop {
            let durationMs = min(900, max(120, elapsedMs))
            if heldBeforeMoving {
                action = .drag(x1: a.0, y1: a.1, x2: b.0, y2: b.1,
                               holdMs: Int(holdThreshold * 1000), durationMs: durationMs)
            } else {
                action = .swipe(x1: a.0, y1: a.1, x2: b.0, y2: b.1, durationMs: durationMs)
            }
        } else if heldBeforeMoving || elapsedMs >= Int(holdThreshold * 1000) {
            action = .longPress(x: a.0, y: a.1, durationMs: min(10_000, max(600, elapsedMs)))
        } else {
            action = .tap(x: a.0, y: a.1)
        }
        onAction?(action)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish()
    }

    private func showDot(at point: CGPoint) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dot.fillColor = UIColor.white.withAlphaComponent(0.35).cgColor
        dot.position = point
        dot.opacity = 1
        trail.opacity = 0
        CATransaction.commit()
    }

    private func finish() {
        holdTimer?.invalidate()
        start = nil
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 1
        fade.toValue = 0
        fade.duration = 0.35
        dot.add(fade, forKey: "fade")
        trail.add(fade, forKey: "fade")
        dot.opacity = 0
        trail.opacity = 0
    }
}

/// SwiftUI wrapper. The model owns the stream and feeds the video view.
struct RemoteScreen: UIViewRepresentable {
    let model: RemoteModel

    func makeUIView(context: Context) -> RemoteScreenUIView {
        let view = RemoteScreenUIView()
        view.onAction = { action in model.send(action) }
        view.video.onFrame = { model.frameArrived() }
        model.attach(video: view.video)
        return view
    }

    func updateUIView(_ uiView: RemoteScreenUIView, context: Context) {}
}
