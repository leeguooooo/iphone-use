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
        case .longPress, .drag, .text:
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

/// A recorded screen drawn the same way the live video is: aspect-fit, with
/// touches mapped onto the picture.
final class DemoImageView: UIImageView, ScreenDisplay {
    /// The recorded buttons that do something, outlined so the demo shows
    /// where to tap.
    var targets: [[Double]] = [] { didSet { setNeedsLayout() } }
    private let outlines = CAShapeLayer()

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentMode = .scaleAspectFit
        backgroundColor = .black
        outlines.fillColor = UIColor.systemBlue.withAlphaComponent(0.12).cgColor
        outlines.strokeColor = UIColor.systemBlue.withAlphaComponent(0.85).cgColor
        outlines.lineWidth = 2
        outlines.lineDashPattern = [6, 4]
        layer.addSublayer(outlines)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    var contentRect: CGRect {
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

    func makeUIView(context: Context) -> RemoteScreenUIView {
        let image = DemoImageView(frame: .zero)
        image.image = session.image
        image.targets = session.screen.taps.map(\.rect)
        let view = RemoteScreenUIView(display: image)
        view.onAction = { action in session.handle(action) }
        return view
    }

    func updateUIView(_ uiView: RemoteScreenUIView, context: Context) {
        guard let image = uiView.display as? DemoImageView, image.image !== session.image else { return }
        image.targets = session.screen.taps.map(\.rect)
        UIView.transition(with: image, duration: 0.25, options: .transitionCrossDissolve) {
            image.image = session.image
        }
    }
}

struct DemoView: View {
    let session: DemoSession
    let exit: () -> Void

    var body: some View {
        VStack(spacing: 6) {
            HStack(spacing: 6) {
                Circle().fill(Color.orange).frame(width: 8, height: 8)
                Text("演示 · 录制的画面").font(.caption.monospaced())
            }
            .padding(.horizontal, 10).padding(.vertical, 5)
            .background(.ultraThinMaterial, in: Capsule())
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal)
            // Above the picture, not over it: the recorded back button lives
            // at the top of the screen.
            Text("这是演示：画面是事先录好的，不会连接或操作任何手机。")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: .infinity)
                .padding(.horizontal)
            ZStack {
                DemoScreen(session: session)
                VStack {
                    Spacer()
                    if let toast = session.toast {
                        Text(toast)
                            .font(.callout)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 14).padding(.vertical, 8)
                            .background(.ultraThinMaterial, in: Capsule())
                            .padding(.bottom, 10)
                            .transition(.opacity)
                    }
                }
                .padding(.horizontal)
                .allowsHitTesting(false)
            }
            HStack {
                ToolButton(title: "主屏幕", symbol: "house") { session.handle(.home) }
                ToolButton(title: "键盘", symbol: "keyboard") {
                    session.show(String(localized: "连上自己的 Mac 后，可以在这里给手机输入文字"))
                }
                ToolButton(title: "退出演示", symbol: "xmark.circle", action: exit)
            }
            .padding(.horizontal, 8).padding(.vertical, 6)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 18))
            .padding(.horizontal)
            .padding(.bottom, 6)
        }
        .background(Color.black.ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: session.toast)
    }
}
