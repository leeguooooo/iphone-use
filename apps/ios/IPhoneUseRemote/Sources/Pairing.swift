import AVFoundation
import SwiftUI
import UIKit

/// What a scan-to-connect QR code carries: the daemon's address and a
/// one-time code. Two spellings reach the app:
/// - `http://192.168.1.11:44321/pair?c=CODE`, the QR itself (scanned in-app);
/// - `iphoneuse://pair?u=http%3A%2F%2F…&c=CODE`, from the landing page the
///   system camera opens.
struct PairLink: Equatable {
    let base: URL
    let code: String

    static func parse(_ text: String) -> PairLink? {
        guard let components = URLComponents(string: text.trimmingCharacters(in: .whitespacesAndNewlines)),
              let scheme = components.scheme?.lowercased() else { return nil }
        let items = components.queryItems ?? []
        func value(_ name: String) -> String? {
            items.first { $0.name == name }?.value.flatMap { $0.isEmpty ? nil : $0 }
        }
        guard let code = value("c") else { return nil }
        switch scheme {
        case "iphoneuse":
            guard components.host == "pair", let u = value("u"),
                  let base = DaemonClient.parse(address: u) else { return nil }
            return PairLink(base: base, code: code)
        case "http", "https":
            guard components.path == "/pair", components.host != nil else { return nil }
            var root = components
            root.path = ""
            root.queryItems = nil
            root.fragment = nil
            guard let rootURL = root.url, let base = DaemonClient.parse(address: rootURL.absoluteString) else {
                return nil
            }
            return PairLink(base: base, code: code)
        default:
            return nil
        }
    }
}

/// Full-screen camera that reports the first pairing QR code it sees.
struct QRScannerView: UIViewControllerRepresentable {
    let onFound: (PairLink) -> Void
    let onFailure: (String) -> Void
    /// A QR code was seen that is not a pairing code.
    var onOther: (() -> Void)?

    func makeUIViewController(context: Context) -> ScannerController {
        let controller = ScannerController()
        controller.onFound = onFound
        controller.onFailure = onFailure
        controller.onOther = onOther
        return controller
    }

    func updateUIViewController(_ controller: ScannerController, context: Context) {}
}

final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    /// The one failure the person can fix in Settings; the sheet offers a
    /// button for it.
    static let noCameraPermission = String(localized: "没有相机权限：请在「设置 › Phone Use Remote」里打开相机")

    var onFound: ((PairLink) -> Void)?
    var onFailure: ((String) -> Void)?
    var onOther: (() -> Void)?

    private let session = AVCaptureSession()
    private var preview: AVCaptureVideoPreviewLayer?
    private var done = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            configure()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async {
                    granted ? self.configure() : self.fail(Self.noCameraPermission)
                }
            }
        default:
            fail(Self.noCameraPermission)
        }
    }

    private func configure() {
        guard let camera = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: camera),
              session.canAddInput(input) else {
            fail(String(localized: "这台设备没有可用的相机"))
            return
        }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else {
            fail(String(localized: "相机无法识别二维码"))
            return
        }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]
        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.videoGravity = .resizeAspectFill
        preview.frame = view.bounds
        view.layer.addSublayer(preview)
        self.preview = preview
        let session = self.session
        DispatchQueue.global(qos: .userInitiated).async { session.startRunning() }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        preview?.frame = view.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        let session = self.session
        DispatchQueue.global(qos: .userInitiated).async { session.stopRunning() }
    }

    // Delivered on the main queue (see `setMetadataObjectsDelegate`).
    nonisolated func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject],
                                    from connection: AVCaptureConnection) {
        let texts = objects.compactMap { ($0 as? AVMetadataMachineReadableCodeObject)?.stringValue }
        MainActor.assumeIsolated {
            guard !done else { return }
            guard let link = texts.lazy.compactMap(PairLink.parse).first else {
                if !texts.isEmpty { onOther?() }
                return
            }
            done = true
            UINotificationFeedbackGenerator().notificationOccurred(.success)
            onFound?(link)
        }
    }

    private func fail(_ message: String) {
        guard !done else { return }
        done = true
        onFailure?(message)
    }
}

/// The scanner sheet: camera, a framing hint, and ways out when scanning
/// does not work (no camera, permission denied, the code will not read):
/// paste the pairing link, or type the address.
struct ScanSheet: View {
    /// Called with the scanned link once the camera has closed.
    let onFound: (PairLink) -> Void
    /// "Enter it by hand": called once the camera has closed.
    var onManual: (() -> Void)?
    @Environment(\.dismiss) private var dismiss
    @State private var failure: String?
    @State private var note: String?
    @State private var noteTask: Task<Void, Never>?
    @State private var slow = false

    var body: some View {
        ZStack {
            if let failure {
                VStack(spacing: 16) {
                    Image(systemName: "camera.fill").font(.largeTitle).foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    Text(failure).multilineTextAlignment(.center)
                    if failure == ScannerController.noCameraPermission {
                        Button("打开设置") {
                            if let url = URL(string: UIApplication.openSettingsURLString) {
                                UIApplication.shared.open(url)
                            }
                        }
                        .buttonStyle(.borderedProminent)
                    }
                    Text("也可以在 Mac 页面上复制配对链接再粘贴，或者手动输入地址和密码。")
                        .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                }
                .padding(32)
            } else {
                QRScannerView(
                    onFound: { link in
                        dismiss()
                        onFound(link)
                    },
                    onFailure: { failure = $0 },
                    onOther: { flash(String(localized: "这不是 iphone-use 的配对二维码。请扫 Mac 上 iphone-use 页面「扫码」按钮弹出的那个。")) })
                .ignoresSafeArea()
                .accessibilityLabel(Text("相机取景框"))
                RoundedRectangle(cornerRadius: 24)
                    .strokeBorder(.white.opacity(0.85), lineWidth: 3)
                    .frame(width: 250, height: 250)
                    .accessibilityHidden(true)
            }
            VStack {
                Text(note ?? (slow
                    ? String(localized: "扫不出来？二维码 5 分钟内有效，过期了在 Mac 上点「换一个」。也可以粘贴配对链接或手动输入。")
                    : String(localized: "扫描 Mac 上 iphone-use 页面里「扫码」按钮显示的二维码")))
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 16).padding(.vertical, 10)
                    .background(note == nil ? AnyShapeStyle(.ultraThinMaterial) : AnyShapeStyle(Color.orange.opacity(0.9)),
                                in: RoundedRectangle(cornerRadius: 14))
                    .padding(.top, 24)
                    .padding(.horizontal)
                    .accessibilityAddTraits(.updatesFrequently)
                Spacer()
                HStack(spacing: 12) {
                    Button {
                        paste()
                    } label: {
                        Label("粘贴链接", systemImage: "doc.on.clipboard")
                    }
                    if onManual != nil {
                        Button {
                            dismiss()
                            onManual?()
                        } label: {
                            Label("手动输入", systemImage: "keyboard")
                        }
                    }
                }
                .font(.subheadline.weight(.semibold))
                .buttonStyle(.bordered)
                .padding(.bottom, 8)
                Button("取消") { dismiss() }
                    .font(.headline)
                    .padding(.horizontal, 28).padding(.vertical, 12)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(.bottom, 32)
            }
        }
        .background(Color.black.ignoresSafeArea())
        .task {
            try? await Task.sleep(for: .seconds(20))
            slow = true
        }
    }

    /// A pairing link copied from the Mac page (or the QR's text).
    private func paste() {
        guard let text = UIPasteboard.general.string, let link = PairLink.parse(AddressInput.clean(text)) else {
            flash(String(localized: "剪贴板里没有配对链接。链接长这样：http://192.168.1.11:44321/pair?c=…"))
            return
        }
        dismiss()
        onFound(link)
    }

    private func flash(_ text: String) {
        guard note != text else { return }
        note = text
        noteTask?.cancel()
        noteTask = Task {
            try? await Task.sleep(for: .seconds(3))
            if !Task.isCancelled { note = nil }
        }
    }
}
