import AVFoundation
import Observation
import SwiftUI
import UIKit

/// The demo: screens recorded from a real iPhone, bundled with the app, so
/// the remote can be tried without a Mac. It drives nothing — taps on a few
/// recorded buttons move between recorded screens, and the view says so.
///
/// `demo-manifest.json` lists the screens; each tap target is a rectangle in
/// the screen's normalized coordinates (x, y, width, height).
struct DemoManifest: Decodable {
    struct Screen: Decodable {
        let id: String
        let image: String
        let taps: [Tap]
        /// Where a back swipe from the left edge goes.
        let back: String?
    }

    struct Tap: Decodable {
        let rect: [Double]
        let to: String

        func contains(x: Double, y: Double) -> Bool {
            guard rect.count == 4 else { return false }
            return x >= rect[0] && x <= rect[0] + rect[2] && y >= rect[1] && y <= rect[1] + rect[3]
        }
    }

    let start: String
    let home: String
    let screens: [Screen]

    static func bundled() -> DemoManifest? {
        guard let url = Bundle.main.url(forResource: "demo-manifest", withExtension: "json"),
              let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(DemoManifest.self, from: data)
    }
}

@MainActor
@Observable
final class DemoSession {
    private let manifest: DemoManifest
    private(set) var screen: DemoManifest.Screen
    private(set) var image: UIImage?
    var toast: String?

    init?(startingAt id: String? = nil) {
        guard let manifest = DemoManifest.bundled(),
              let first = manifest.screens.first(where: { $0.id == (id ?? manifest.start) })
                ?? manifest.screens.first(where: { $0.id == manifest.start })
        else { return nil }
        self.manifest = manifest
        self.screen = first
        self.image = Self.load(first)
    }

    private static func load(_ screen: DemoManifest.Screen) -> UIImage? {
        let name = (screen.image as NSString).deletingPathExtension
        let ext = (screen.image as NSString).pathExtension
        guard let url = Bundle.main.url(forResource: name, withExtension: ext) else { return nil }
        return UIImage(contentsOfFile: url.path)
    }

    private func go(to id: String) {
        guard let next = manifest.screens.first(where: { $0.id == id }) else { return }
        screen = next
        image = Self.load(next)
    }

    /// The same gestures the live remote sends to the phone, answered from
    /// the recording.
    func handle(_ action: PhoneAction) {
        switch action {
        case let .tap(x, y):
            if let target = screen.taps.first(where: { $0.contains(x: x, y: y) }) {
                go(to: target.to)
            } else {
                show(String(localized: "演示里只有蓝框标出的地方能点"))
            }
        case let .swipe(x1, _, x2, _, _):
            if x1 < 0.12, x2 - x1 > 0.2, let back = screen.back {
                go(to: back)
            } else {
                show(String(localized: "演示画面是录好的，不能滚动；连上自己的 Mac 后就能真实操作"))
            }
        case .home:
            go(to: manifest.home)
        case .back:
            if let back = screen.back { go(to: back) } else { show(String(localized: "已经在第一页了")) }
        case .spotlight:
            show(String(localized: "连上自己的 Mac 后，这个键会打开手机的搜索"))
        case .text, .key:
            show(String(localized: "连上自己的 Mac 后，键盘会把文字实时打到手机上"))
        case .longPress, .drag:
            show(String(localized: "演示里只能点按和从左边缘右滑返回"))
        }
    }

    func show(_ message: String) {
        toast = message
        Task {
            try? await Task.sleep(for: .seconds(3))
            if toast == message { toast = nil }
        }
    }
}

/// A recorded screen drawn the same way the live video is: at its aspect,
/// with touches mapped onto the picture.
final class DemoImageView: UIImageView, ScreenDisplay {
    /// The recorded buttons that do something, outlined so the demo shows
    /// where to tap.
    var targets: [[Double]] = [] { didSet { setNeedsLayout() } }
    var onContentChange: (() -> Void)?
    private let outlines = CAShapeLayer()

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentMode = .scaleAspectFit
        backgroundColor = .black
        outlines.fillColor = Theme.uiAccent.withAlphaComponent(0.12).cgColor
        outlines.strokeColor = Theme.uiAccent.withAlphaComponent(0.9).cgColor
        outlines.lineWidth = 2
        outlines.lineDashPattern = [6, 4]
        layer.addSublayer(outlines)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override var image: UIImage? {
        didSet { if oldValue?.size != image?.size || (oldValue == nil) != (image == nil) { onContentChange?() } }
    }

    var contentSize: CGSize? { image?.size }
    var hasContent: Bool { image != nil }

    private var contentRect: CGRect {
        guard let size = image?.size, size.width > 0, size.height > 0 else { return bounds }
        return AVMakeRect(aspectRatio: size, insideRect: bounds)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let content = contentRect
        let path = UIBezierPath()
        for rect in targets where rect.count == 4 {
            let frame = CGRect(x: content.minX + rect[0] * content.width,
                               y: content.minY + rect[1] * content.height,
                               width: rect[2] * content.width,
                               height: rect[3] * content.height)
            path.append(UIBezierPath(roundedRect: frame.insetBy(dx: 2, dy: 2), cornerRadius: 10))
        }
        outlines.frame = bounds
        outlines.path = path.cgPath
    }
}

struct DemoScreen: UIViewRepresentable {
    let session: DemoSession
    var options = CanvasOptions()

    final class Coordinator { var reset = 0 }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> RemoteScreenUIView {
        let image = DemoImageView(frame: .zero)
        image.image = session.image
        image.targets = session.screen.taps.map(\.rect)
        let view = RemoteScreenUIView(display: image)
        view.accessibilityIdentifier = "remote-screen"
        view.onAction = { action in session.handle(action) }
        context.coordinator.reset = options.resetZoom
        view.apply(options, previousReset: &context.coordinator.reset)
        return view
    }

    func updateUIView(_ uiView: RemoteScreenUIView, context: Context) {
        uiView.apply(options, previousReset: &context.coordinator.reset)
        guard let image = uiView.display as? DemoImageView, image.image !== session.image else { return }
        image.targets = session.screen.taps.map(\.rect)
        UIView.transition(with: image, duration: 0.25, options: .transitionCrossDissolve) {
            image.image = session.image
        }
    }
}

/// The demo, in the same chrome as a live phone: the keys work on the
/// recording (Home, back), the rest explain themselves.
struct DemoView: View {
    let session: DemoSession
    let exit: () -> Void
    @State private var immersive = false
    @State private var zoom: CGFloat = 1
    @State private var resetZoom = 0

    var body: some View {
        RemoteScaffold(immersive: immersive, typing: false, onExitImmersive: { immersive = false }) { axis in
            RemoteTopBar(axis: axis, backSymbol: "xmark", backLabel: "退出演示", zoom: zoom,
                         onBack: exit, onResetZoom: { resetZoom += 1 }, onImmersive: { immersive = true }) {
                if axis == .horizontal {
                    DeviceBadge(name: String(localized: "演示"), health: .attention,
                                caption: String(localized: "录制的画面，不连接任何手机"))
                } else {
                    Chip(text: String(localized: "演示"), color: Theme.attention)
                }
            }
        } screen: {
            ZStack {
                DemoScreen(session: session, options: CanvasOptions(
                    resetZoom: resetZoom,
                    onZoom: { zoom = $0 },
                    accessibilityLabel: String(localized: "演示画面"),
                    accessibilityActions: [
                        (String(localized: "主屏幕"), { session.handle(.home) }),
                        (String(localized: "返回"), { session.handle(.back) }),
                    ]))
                ToastLayer(text: session.toast)
            }
        } accessory: { _ in
            EmptyView()
        } banner: {
            EmptyView()
        } keys: { axis in
            KeyBar(axis: axis,
                   onBack: { session.handle(.back) },
                   onHome: { session.handle(.home) },
                   onSearch: { session.handle(.spotlight) },
                   onKeyboard: { session.handle(.text("")) }) {
                Button { immersive = true } label: {
                    Label("沉浸模式", systemImage: "arrow.up.left.and.arrow.down.right")
                }
                Button(role: .destructive, action: exit) { Label("退出演示", systemImage: "xmark.circle") }
            }
        } keyboard: {
            EmptyView()
        }
    }
}
