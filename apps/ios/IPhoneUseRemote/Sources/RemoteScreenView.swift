import AVFoundation
import os
import SwiftUI
import UIKit

/// What the touch surface draws the phone's screen with: the live video, or
/// the demo's recorded screens.
@MainActor
protocol ScreenDisplay: UIView {
    /// The picture's natural size, once known (its aspect sizes the screen).
    var contentSize: CGSize? { get }
    /// A picture is up (until then the surface shows a placeholder).
    var hasContent: Bool { get }
    /// Size or presence changed.
    var onContentChange: (() -> Void)? { get set }
}

extension VideoDisplayView: ScreenDisplay {
    var contentSize: CGSize? { videoSize.width > 0 && videoSize.height > 0 ? videoSize : nil }
    var hasContent: Bool { hasPicture }
}

let metricsLog = Logger(subsystem: "com.leeguoo.iphone-use.remote", category: "metrics")

/// The phone's screen plus the touch surface that drives it.
///
/// The picture is laid out at its own aspect with the phone's rounded
/// corners, edge to edge in the space it is given. One finger drives the
/// phone (`GestureMapper` decides what a touch was, on lift); two fingers
/// belong to this app: pinch to zoom, drag to pan, double-tap to zoom in or
/// back out. Touch points are converted through the zoom, so a tap on a
/// zoomed-in button lands on that button.
///
/// Every touch is drawn at once — a dot under the finger, a ring that fills
/// toward a long press, a ripple on a tap, a trail on a swipe — so the person
/// sees their input before the phone's picture catches up.
final class RemoteScreenUIView: UIView, UIGestureRecognizerDelegate {
    let display: any ScreenDisplay
    var onAction: ((PhoneAction) -> Void)?
    /// The zoom changed (1 = fit).
    var onZoom: ((CGFloat) -> Void)?

    /// Rounded, clipped frame the picture sits in; zoom transforms it.
    private let screen = UIView()
    private let placeholder = ShimmerView()
    private let overlayView = UIImageView()
    private var zoom = ZoomState()
    /// Aspect used before the first picture says otherwise: a Face ID iPhone.
    private var fallbackSize = CGSize(width: 1179, height: 2556)

    // One-finger gesture in progress.
    private weak var tracked: UITouch?
    private var startPoint: CGPoint = .zero
    /// Where the finger came down, on the phone's screen.
    private var startSample: GestureMapper.Sample?
    /// The latest few points, for the release velocity.
    private var samples: [GestureMapper.Sample] = []
    private var moved = false
    private var heldBeforeMoving = false
    private var aborted = false
    private var holdTimer: Timer?

    // Feedback.
    private let indicator = TouchIndicator()
    private let trail = CAShapeLayer()
    private let trailPath = UIBezierPath()

    init(display: any ScreenDisplay) {
        self.display = display
        super.init(frame: .zero)
        backgroundColor = .clear
        // A zoomed-in picture stays inside the stage, under the chrome.
        clipsToBounds = true
        isMultipleTouchEnabled = true
        isAccessibilityElement = true
        accessibilityTraits = .allowsDirectInteraction

        screen.clipsToBounds = true
        screen.layer.cornerCurve = .continuous
        screen.layer.borderColor = UIColor.white.withAlphaComponent(0.12).cgColor
        screen.layer.borderWidth = 1 / UIScreen.main.scale
        screen.backgroundColor = UIColor(white: 0.06, alpha: 1)
        addSubview(screen)
        screen.addSubview(placeholder)
        screen.addSubview(display)
        overlayView.contentMode = .scaleToFill
        overlayView.isHidden = true
        screen.addSubview(overlayView)
        display.alpha = display.hasContent ? 1 : 0
        placeholder.alpha = display.hasContent ? 0 : 1
        display.onContentChange = { [weak self] in self?.contentChanged() }

        trail.fillColor = UIColor.clear.cgColor
        trail.lineWidth = 5
        trail.lineCap = .round
        trail.lineJoin = .round
        trail.opacity = 0
        layer.addSublayer(trail)
        layer.addSublayer(indicator)

        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        pan.minimumNumberOfTouches = 2
        pan.maximumNumberOfTouches = 2
        let doubleTap = UITapGestureRecognizer(target: self, action: #selector(doubleTapped(_:)))
        doubleTap.numberOfTouchesRequired = 2
        doubleTap.numberOfTapsRequired = 2
        for recognizer in [pinch, pan, doubleTap] as [UIGestureRecognizer] {
            recognizer.delegate = self
            // A one-finger lift must reach the phone at once, not wait to
            // see whether a two-finger gesture follows.
            recognizer.delaysTouchesEnded = false
            addGestureRecognizer(recognizer)
        }
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    // MARK: layout

    /// The picture's size at zoom 1: the phone's aspect, as large as fits.
    private var fittedRect: CGRect {
        let size = display.contentSize ?? fallbackSize
        return AVMakeRect(aspectRatio: size, insideRect: bounds)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let fit = fittedRect
        screen.transform = .identity
        screen.bounds = CGRect(origin: .zero, size: fit.size)
        screen.center = CGPoint(x: bounds.midX, y: bounds.midY)
        screen.layer.cornerRadius = Theme.screenCornerRatio(for: fit.size) * min(fit.width, fit.height)
        display.frame = screen.bounds
        placeholder.frame = screen.bounds
        overlayView.frame = screen.bounds
        #if DEBUG
        // Screenshots: `-zoom 2.5` opens zoomed in on the upper third.
        let debugZoom = CGFloat(UserDefaults.standard.double(forKey: "zoom"))
        if debugZoom > 1, !debugZoomed, fit.width > 0 {
            debugZoomed = true
            zoom.zoom(by: debugZoom, around: CGPoint(x: 0, y: -fit.height * 0.3))
            reportZoom()
        }
        #endif
        zoom.clamp(content: fit.size, view: bounds.size)
        screen.transform = zoomTransform
    }

    #if DEBUG
    private var debugZoomed = false
    #endif

    private var zoomTransform: CGAffineTransform {
        CGAffineTransform(translationX: zoom.offset.width, y: zoom.offset.height)
            .scaledBy(x: zoom.scale, y: zoom.scale)
    }

    private var contentWasShown = false
    private var placeholderSince = CACurrentMediaTime()

    private func contentChanged() {
        if let size = display.contentSize { fallbackSize = size }
        UIView.animate(withDuration: 0.3, delay: 0, options: [.beginFromCurrentState, .curveEaseInOut]) {
            self.setNeedsLayout()
            self.layoutIfNeeded()
        }
        if display.hasContent, !contentWasShown {
            contentWasShown = true
            metricsLog.notice("first picture on screen \(Int((CACurrentMediaTime() - self.placeholderSince) * 1000), privacy: .public) ms after the surface appeared")
            UIView.animate(withDuration: 0.28, delay: 0, options: [.curveEaseOut]) {
                self.display.alpha = 1
                self.placeholder.alpha = 0
            } completion: { _ in
                self.placeholder.stopAnimating()
            }
        }
    }

    func setOverlay(_ image: UIImage?) {
        guard overlayView.image !== image else { return }
        overlayView.image = image
        overlayView.isHidden = image == nil
    }

    /// Dim the picture while it is not live (stalled, reconnecting).
    func setDimmed(_ dimmed: Bool) {
        let alpha: CGFloat = dimmed ? 0.45 : 1
        guard display.hasContent, abs(display.alpha - alpha) > 0.01 else { return }
        UIView.animate(withDuration: 0.3) { self.display.alpha = alpha }
    }

    // MARK: zoom

    func resetZoom(animated: Bool) {
        guard zoom != ZoomState() else { return }
        zoom = ZoomState()
        applyZoom(animated: animated)
    }

    private func applyZoom(animated: Bool) {
        let apply = { self.screen.transform = self.zoomTransform }
        if animated {
            UIView.animate(withDuration: 0.32, delay: 0, usingSpringWithDamping: 0.86, initialSpringVelocity: 0,
                           options: [.beginFromCurrentState, .allowUserInteraction], animations: apply)
        } else {
            apply()
        }
        reportZoom()
    }

    private var reportedZoom: CGFloat = 1

    /// Tell SwiftUI the zoom (shown as a chip) when it visibly changed. Never
    /// during a SwiftUI update: a reset comes from `updateUIView`, where
    /// changing state is ignored, so it is posted to the next turn.
    private func reportZoom() {
        let scale = (zoom.scale * 10).rounded() / 10
        guard scale != reportedZoom else { return }
        reportedZoom = scale
        DispatchQueue.main.async { [weak self] in self?.onZoom?(scale) }
    }

    private func relativeToCenter(_ point: CGPoint) -> CGPoint {
        CGPoint(x: point.x - bounds.midX, y: point.y - bounds.midY)
    }

    @objc private func pinched(_ pinch: UIPinchGestureRecognizer) {
        switch pinch.state {
        case .began, .changed:
            abortTouch()
            let wasZoomed = zoom.isZoomed
            zoom.zoom(by: pinch.scale, around: relativeToCenter(pinch.location(in: self)))
            pinch.scale = 1
            if wasZoomed != zoom.isZoomed { Haptics.tick() }
            applyZoom(animated: false)
        case .ended, .cancelled:
            if zoom.scale < 1.08 { zoom = ZoomState() }
            zoom.clamp(content: fittedRect.size, view: bounds.size)
            applyZoom(animated: true)
        default:
            break
        }
    }

    @objc private func panned(_ pan: UIPanGestureRecognizer) {
        guard zoom.isZoomed else { return }
        switch pan.state {
        case .began, .changed:
            abortTouch()
            let t = pan.translation(in: self)
            zoom.offset.width += t.x
            zoom.offset.height += t.y
            pan.setTranslation(.zero, in: self)
            zoom.clamp(content: fittedRect.size, view: bounds.size)
            applyZoom(animated: false)
        case .ended:
            // A little glide, like a scroll view.
            let v = pan.velocity(in: self)
            zoom.offset.width += v.x * 0.12
            zoom.offset.height += v.y * 0.12
            zoom.clamp(content: fittedRect.size, view: bounds.size)
            applyZoom(animated: true)
        default:
            break
        }
    }

    @objc private func doubleTapped(_ tap: UITapGestureRecognizer) {
        abortTouch()
        if zoom.isZoomed {
            zoom = ZoomState()
        } else {
            zoom.zoom(by: ZoomState.step, around: relativeToCenter(tap.location(in: self)))
            zoom.clamp(content: fittedRect.size, view: bounds.size)
        }
        Haptics.tick()
        applyZoom(animated: true)
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        true
    }

    // MARK: one finger → the phone

    /// A point on the phone's screen (0…1), through the zoom; nil off the picture.
    private func normalized(_ point: CGPoint) -> (Double, Double)? {
        let local = display.convert(point, from: self)
        let rect = display.bounds
        guard rect.width > 0, rect.height > 0, rect.insetBy(dx: -2, dy: -2).contains(local) else { return nil }
        return (Double(min(max(local.x / rect.width, 0), 1)), Double(min(max(local.y / rect.height, 0), 1)))
    }

    private func sample(_ touch: UITouch) -> GestureMapper.Sample? {
        guard let (x, y) = normalized(touch.location(in: self)) else { return nil }
        return .init(x: x, y: y, t: touch.timestamp)
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        // A second finger makes it a gesture for this app, not the phone.
        if tracked != nil || (event?.touches(for: self)?.count ?? touches.count) > 1 {
            abortTouch()
            return
        }
        guard let touch = touches.first, let first = sample(touch) else { return }
        tracked = touch
        aborted = false
        moved = false
        heldBeforeMoving = false
        startPoint = touch.location(in: self)
        startSample = first
        samples = [first]
        indicator.press(at: startPoint, holdAfter: GestureMapper.holdThreshold)
        trailPath.removeAllPoints()
        trailPath.move(to: startPoint)
        reportFeedbackLatency(touch)
        holdTimer?.invalidate()
        holdTimer = Timer.scheduledTimer(withTimeInterval: GestureMapper.holdThreshold, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.tracked != nil, !self.moved, !self.aborted else { return }
                self.heldBeforeMoving = true
                Haptics.hold()
                self.indicator.held()
            }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = tracked, touches.contains(touch), !aborted else { return }
        let point = touch.location(in: self)
        if let s = sample(touch) {
            samples.append(s)
            if samples.count > 24 { samples.removeFirst(samples.count - 24) }
        }
        if !moved, hypot(point.x - startPoint.x, point.y - startPoint.y) > GestureMapper.tapSlop {
            moved = true
            holdTimer?.invalidate()
            indicator.moving(dragging: heldBeforeMoving)
            trail.strokeColor = (heldBeforeMoving ? Theme.uiAccent : UIColor.white).withAlphaComponent(0.55).cgColor
            if heldBeforeMoving { Haptics.light() }
        }
        if moved {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            trailPath.addLine(to: point)
            trail.path = trailPath.cgPath
            trail.opacity = 1
            trail.strokeStart = 0
            indicator.position = point
            CATransaction.commit()
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = tracked, touches.contains(touch) else { return }
        defer { finish() }
        guard !aborted, let first = startSample else { return }
        let point = touch.location(in: self)
        let last = sample(touch) ?? GestureMapper.Sample(x: samples.last?.x ?? first.x,
                                                         y: samples.last?.y ?? first.y, t: touch.timestamp)
        var all = samples
        all.append(last)
        let action = GestureMapper.action(
            start: first, end: last,
            movedPoints: hypot(point.x - startPoint.x, point.y - startPoint.y),
            heldBeforeMoving: heldBeforeMoving,
            releaseVelocity: GestureMapper.releaseVelocity(all))
        if case .tap = action { indicator.ripple(in: layer, at: startPoint) }
        onAction?(action)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = tracked, touches.contains(touch) else { return }
        aborted = true
        finish()
    }

    /// The one-finger gesture becomes nothing (a second finger came down).
    private func abortTouch() {
        guard tracked != nil, !aborted else { return }
        aborted = true
        holdTimer?.invalidate()
        indicator.release()
        trail.opacity = 0
    }

    private func finish() {
        holdTimer?.invalidate()
        tracked = nil
        startSample = nil
        samples = []
        indicator.release()
        guard trail.opacity > 0 else { return }
        // The trail draws itself in toward where the finger lifted.
        let retract = CABasicAnimation(keyPath: "strokeStart")
        retract.fromValue = 0
        retract.toValue = 1
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 1
        fade.toValue = 0
        let group = CAAnimationGroup()
        group.animations = [retract, fade]
        group.duration = 0.3
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        trail.add(group, forKey: "finish")
        trail.opacity = 0
    }

    /// Debug builds log how long a touch takes to show: from the hardware
    /// timestamp to the frame that carries the dot.
    private func reportFeedbackLatency(_ touch: UITouch) {
        #if DEBUG
        let touched = touch.timestamp
        let link = OneShotDisplayLink { target in
            metricsLog.notice("touch feedback on screen \(Int((target - touched) * 1000), privacy: .public) ms after the touch")
        }
        link.start()
        #endif
    }
}

/// Calls back once, on the next display refresh, with that frame's target time.
@MainActor
private final class OneShotDisplayLink: NSObject {
    private var link: CADisplayLink?
    private let done: (CFTimeInterval) -> Void
    private var keepAlive: OneShotDisplayLink?

    init(_ done: @escaping (CFTimeInterval) -> Void) { self.done = done }

    func start() {
        keepAlive = self
        link = CADisplayLink(target: self, selector: #selector(tick(_:)))
        link?.add(to: .main, forMode: .common)
    }

    @objc private func tick(_ link: CADisplayLink) {
        done(link.targetTimestamp)
        link.invalidate()
        self.link = nil
        keepAlive = nil
    }
}

/// The dot under the finger: a soft halo and a solid core; a ring that fills
/// toward a long press; blue once held (a long press, or a drag after it).
final class TouchIndicator: CALayer {
    private let halo = CAShapeLayer()
    private let core = CAShapeLayer()
    private let ring = CAShapeLayer()

    override init() {
        super.init()
        bounds = CGRect(x: 0, y: 0, width: 56, height: 56)
        let center = CGPoint(x: 28, y: 28)
        halo.path = UIBezierPath(arcCenter: center, radius: 22, startAngle: 0, endAngle: .pi * 2, clockwise: true).cgPath
        halo.fillColor = UIColor.white.withAlphaComponent(0.22).cgColor
        core.path = UIBezierPath(arcCenter: center, radius: 9, startAngle: 0, endAngle: .pi * 2, clockwise: true).cgPath
        core.fillColor = UIColor.white.withAlphaComponent(0.92).cgColor
        core.shadowColor = UIColor.black.cgColor
        core.shadowOpacity = 0.35
        core.shadowRadius = 3
        core.shadowOffset = .zero
        ring.path = UIBezierPath(arcCenter: center, radius: 25, startAngle: -.pi / 2, endAngle: .pi * 1.5, clockwise: true).cgPath
        ring.fillColor = UIColor.clear.cgColor
        ring.strokeColor = UIColor.white.withAlphaComponent(0.85).cgColor
        ring.lineWidth = 2.5
        ring.lineCap = .round
        ring.strokeEnd = 0
        for layer in [halo, core, ring] {
            layer.frame = bounds
            addSublayer(layer)
        }
        opacity = 0
    }

    override init(layer: Any) { super.init(layer: layer) }
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    func press(at point: CGPoint, holdAfter hold: TimeInterval) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        removeAllAnimations()
        ring.removeAllAnimations()
        position = point
        opacity = 1
        halo.fillColor = UIColor.white.withAlphaComponent(0.22).cgColor
        core.fillColor = UIColor.white.withAlphaComponent(0.92).cgColor
        ring.strokeColor = UIColor.white.withAlphaComponent(0.85).cgColor
        ring.strokeEnd = 0
        ring.opacity = 1
        CATransaction.commit()

        let pop = CASpringAnimation(keyPath: "transform.scale")
        pop.fromValue = 0.6
        pop.toValue = 1
        pop.damping = 14
        pop.initialVelocity = 6
        pop.duration = pop.settlingDuration
        add(pop, forKey: "pop")

        // The ring starts filling only once it is clearly not a quick tap.
        let fill = CABasicAnimation(keyPath: "strokeEnd")
        fill.fromValue = 0
        fill.toValue = 1
        fill.beginTime = CACurrentMediaTime() + 0.12
        fill.duration = max(0.05, hold - 0.12)
        fill.fillMode = .forwards
        fill.isRemovedOnCompletion = false
        ring.add(fill, forKey: "fill")
    }

    func held() {
        CATransaction.begin()
        CATransaction.setAnimationDuration(0.15)
        let blue = Theme.uiAccent
        halo.fillColor = blue.withAlphaComponent(0.35).cgColor
        core.fillColor = blue.cgColor
        ring.strokeColor = blue.cgColor
        CATransaction.commit()
        let pulse = CASpringAnimation(keyPath: "transform.scale")
        pulse.fromValue = 1.25
        pulse.toValue = 1
        pulse.damping = 10
        pulse.duration = pulse.settlingDuration
        add(pulse, forKey: "pulse")
    }

    func moving(dragging: Bool) {
        ring.removeAnimation(forKey: "fill")
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        ring.opacity = dragging ? 1 : 0
        ring.strokeEnd = dragging ? 1 : 0
        CATransaction.commit()
    }

    func release() {
        ring.removeAnimation(forKey: "fill")
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = presentation()?.opacity ?? opacity
        fade.toValue = 0
        fade.duration = 0.22
        let shrink = CABasicAnimation(keyPath: "transform.scale")
        shrink.toValue = 0.7
        shrink.duration = 0.22
        add(fade, forKey: "fade")
        add(shrink, forKey: "shrink")
        opacity = 0
    }

    /// A ring spreading out from a tap: "sent".
    func ripple(in parent: CALayer, at point: CGPoint) {
        let ripple = CAShapeLayer()
        ripple.bounds = CGRect(x: 0, y: 0, width: 56, height: 56)
        ripple.position = point
        ripple.path = UIBezierPath(ovalIn: ripple.bounds.insetBy(dx: 6, dy: 6)).cgPath
        ripple.fillColor = UIColor.clear.cgColor
        ripple.strokeColor = UIColor.white.withAlphaComponent(0.8).cgColor
        ripple.lineWidth = 2
        ripple.opacity = 0
        parent.addSublayer(ripple)
        let grow = CABasicAnimation(keyPath: "transform.scale")
        grow.fromValue = 0.5
        grow.toValue = 1.7
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 0.9
        fade.toValue = 0
        let group = CAAnimationGroup()
        group.animations = [grow, fade]
        group.duration = 0.42
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        CATransaction.begin()
        CATransaction.setCompletionBlock { ripple.removeFromSuperlayer() }
        ripple.add(group, forKey: "ripple")
        CATransaction.commit()
    }
}

/// The picture's stand-in until the first frame: a dark screen with a slow
/// sheen, at the phone's shape, so the layout never jumps.
final class ShimmerView: UIView {
    private let sheen = CAGradientLayer()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = UIColor(white: 0.09, alpha: 1)
        sheen.colors = [UIColor.clear.cgColor, UIColor.white.withAlphaComponent(0.06).cgColor, UIColor.clear.cgColor]
        sheen.startPoint = CGPoint(x: 0, y: 0.35)
        sheen.endPoint = CGPoint(x: 1, y: 0.65)
        sheen.locations = [0.35, 0.5, 0.65]
        layer.addSublayer(sheen)
        isAccessibilityElement = false
        if !UIAccessibility.isReduceMotionEnabled {
            let move = CABasicAnimation(keyPath: "locations")
            move.fromValue = [-0.3, -0.15, 0]
            move.toValue = [1, 1.15, 1.3]
            move.duration = 1.6
            move.repeatCount = .infinity
            sheen.add(move, forKey: "sheen")
        }
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override func layoutSubviews() {
        super.layoutSubviews()
        sheen.frame = bounds
    }

    func stopAnimating() { sheen.removeAllAnimations() }
}

// MARK: - SwiftUI

/// What the remote's chrome tells the touch surface.
struct CanvasOptions {
    /// The picture is not live: dim it.
    var dimmed = false
    /// Bumped to zoom back out to fit.
    var resetZoom = 0
    var onZoom: (CGFloat) -> Void = { _ in }
    /// Drawn over the picture, zoomed with it (the wireframe of a screen
    /// the app hides from capture).
    var overlay: UIImage?
    /// VoiceOver: "<name>'s screen" and the keys it can press.
    var accessibilityLabel = ""
    var accessibilityActions: [(String, () -> Void)] = []
}

extension RemoteScreenUIView {
    func apply(_ options: CanvasOptions, previousReset: inout Int) {
        setDimmed(options.dimmed)
        setOverlay(options.overlay)
        onZoom = options.onZoom
        if options.resetZoom != previousReset {
            previousReset = options.resetZoom
            resetZoom(animated: true)
        }
        accessibilityLabel = options.accessibilityLabel
        accessibilityHint = String(localized: "单指操作手机；双指捏合或双击缩放")
        accessibilityCustomActions = options.accessibilityActions.map { name, run in
            UIAccessibilityCustomAction(name: name) { _ in run(); return true }
        }
    }
}

/// SwiftUI wrapper for the full screen of one phone. The session owns the
/// stream and feeds the video view; `onAction` decides where a gesture goes
/// (that phone alone, or every sync member).
struct RemoteScreen: UIViewRepresentable {
    let session: DeviceSession
    var options = CanvasOptions()
    let onAction: (PhoneAction) -> Void

    final class Coordinator {
        let attachment = VideoAttachment()
        var reset = 0
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> RemoteScreenUIView {
        let video = VideoDisplayView()
        let view = RemoteScreenUIView(display: video)
        view.onAction = onAction
        view.accessibilityIdentifier = "remote-screen"
        context.coordinator.reset = options.resetZoom
        context.coordinator.attachment.attach(video, to: session, role: .full)
        view.apply(options, previousReset: &context.coordinator.reset)
        return view
    }

    func updateUIView(_ uiView: RemoteScreenUIView, context: Context) {
        uiView.onAction = onAction
        context.coordinator.attachment.attach(nil, to: session, role: .full)
        uiView.apply(options, previousReset: &context.coordinator.reset)
    }

    static func dismantleUIView(_ uiView: RemoteScreenUIView, coordinator: Coordinator) {
        coordinator.attachment.detach()
    }
}

/// A live, non-interactive picture of one phone (a grid or sync tile).
/// Tiles stream in performance mode; a tile on screen counts as a viewer
/// for the daemon's idle release, one scrolled away or closed does not.
struct LiveVideo: UIViewRepresentable {
    let session: DeviceSession

    func makeCoordinator() -> VideoAttachment { VideoAttachment() }

    func makeUIView(context: Context) -> VideoDisplayView {
        let video = VideoDisplayView()
        video.isUserInteractionEnabled = false
        context.coordinator.attach(video, to: session, role: .tile)
        return video
    }

    func updateUIView(_ uiView: VideoDisplayView, context: Context) {
        context.coordinator.attach(nil, to: session, role: .tile)
    }

    static func dismantleUIView(_ uiView: VideoDisplayView, coordinator: VideoAttachment) {
        coordinator.detach()
    }
}

/// Keeps a video view attached to the session it shows, and detaches it
/// when the view goes away so the stream stops with it.
@MainActor
final class VideoAttachment {
    private weak var session: DeviceSession?
    private var video: VideoDisplayView?

    /// Attach `video` (or, with nil, the one already held) to `session`;
    /// a different session (the view was reused) moves it.
    func attach(_ newVideo: VideoDisplayView?, to session: DeviceSession, role: DeviceSession.ViewerRole) {
        if let newVideo { video = newVideo }
        guard let video else { return }
        if self.session === session, newVideo == nil { return }
        if let old = self.session, old !== session { old.detach(video: video) }
        self.session = session
        video.onFrame = { [weak session] in session?.frameArrived() }
        video.onNeedKeyframe = { [weak session] in session?.requestKeyframe() }
        session.attach(video: video, role: role)
    }

    func detach() {
        if let video { session?.detach(video: video) }
        session = nil
    }
}
