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

    func makeUIViewController(context: Context) -> ScannerController {
        let controller = ScannerController()
        controller.onFound = onFound
        controller.onFailure = onFailure
        return controller
    }

    func updateUIViewController(_ controller: ScannerController, context: Context) {}
}

final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var onFound: ((PairLink) -> Void)?
    var onFailure: ((String) -> Void)?

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
                    granted ? self.configure() : self.fail("没有相机权限：请在「设置 › iPhone Use」里打开相机")
                }
            }
        default:
            fail("没有相机权限：请在「设置 › iPhone Use」里打开相机")
        }
    }

    private func configure() {
        guard let camera = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: camera),
              session.canAddInput(input) else {
            fail("这台设备没有可用的相机")
            return
        }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else {
            fail("相机无法识别二维码")
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
            guard !done, let link = texts.lazy.compactMap(PairLink.parse).first else { return }
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

/// The scanner sheet: camera, a framing hint and a cancel button.
struct ScanSheet: View {
    let model: RemoteModel
    @Environment(\.dismiss) private var dismiss
    @State private var failure: String?

    var body: some View {
        ZStack {
            if let failure {
                VStack(spacing: 16) {
                    Image(systemName: "camera.fill").font(.largeTitle).foregroundStyle(.secondary)
                    Text(failure).multilineTextAlignment(.center)
                    if failure.contains("设置") {
                        Button("打开设置") {
                            if let url = URL(string: UIApplication.openSettingsURLString) {
                                UIApplication.shared.open(url)
                            }
                        }
                        .buttonStyle(.borderedProminent)
                    }
                }
                .padding(32)
            } else {
                QRScannerView(
                    onFound: { link in
                        dismiss()
                        Task { await model.pair(link) }
                    },
                    onFailure: { failure = $0 })
                .ignoresSafeArea()
                RoundedRectangle(cornerRadius: 24)
                    .strokeBorder(.white.opacity(0.85), lineWidth: 3)
                    .frame(width: 250, height: 250)
            }
            VStack {
                Text("扫描 Mac 上 iphone-use 页面里「扫码」按钮显示的二维码")
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 16).padding(.vertical, 10)
                    .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
                    .padding(.top, 24)
                    .padding(.horizontal)
                Spacer()
                Button("取消") { dismiss() }
                    .font(.headline)
                    .padding(.horizontal, 28).padding(.vertical, 12)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(.bottom, 32)
            }
        }
        .background(Color.black.ignoresSafeArea())
    }
}
