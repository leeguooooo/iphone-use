import XCTest

/// The toast after an action never covers the phone's picture: portrait,
/// landscape and immersive, against `Tools/mock_daemon.py` (skipped unless
/// it answers). The mock says another agent holds the phone, so a tap on
/// the picture is refused with a toast.
///
/// `TEST_RUNNER_SHOT_DIR=<dir>` saves a screenshot of each toast there.
@MainActor
final class ToastPlacementUITests: XCTestCase {
    private let mock = URL(string: ProcessInfo.processInfo.environment["MOCK_DAEMON"] ?? "http://127.0.0.1:47001")!
    private var app: XCUIApplication!
    private var pictureSize: CGSize = .zero

    override func setUp() async throws {
        guard let info = try? JSONSerialization.jsonObject(with: try await request("mock/info")) as? [String: Any],
              let width = info["width"] as? Double, let height = info["height"] as? Double, width > 0 else {
            throw XCTSkip("mock daemon not running at \(mock)")
        }
        pictureSize = CGSize(width: width, height: height)
        _ = try await request("mock/scenario", method: "POST", body: #"{"name":"live"}"#)
    }

    override func tearDown() async throws {
        app?.terminate()
        XCUIDevice.shared.orientation = .portrait
        _ = try? await request("mock/scenario", method: "POST", body: #"{"name":"live"}"#)
    }

    private func request(_ path: String, method: String = "GET", body: String? = nil) async throws -> Data {
        var request = URLRequest(url: mock.appending(path: path))
        request.httpMethod = method
        request.httpBody = body?.data(using: .utf8)
        request.timeoutInterval = 3
        return try await URLSession.shared.data(for: request).0
    }

    /// Launch, turn, make the phone refuse, tap the picture, and check
    /// where the toast landed.
    private func checkToast(orientation: UIDeviceOrientation, immersive: Bool, shot: String) async throws {
        XCUIDevice.shared.orientation = orientation
        app = XCUIApplication()
        app.launchArguments = ["-address", mock.absoluteString, "-password", "mock"]
            + (immersive ? ["-immersive", "YES"] : [])
        app.launch()
        let screen = app.otherElements["remote-screen"]
        XCTAssertTrue(screen.waitForExistence(timeout: 15), "the remote screen did not appear")
        try await Task.sleep(for: .seconds(2))
        _ = try await request("mock/scenario", method: "POST", body: #"{"name":"owned"}"#)

        let toast = app.descendants(matching: .any)["toast"].firstMatch
        let deadline = Date().addingTimeInterval(20)
        while !toast.exists && Date() < deadline {
            // The refusal needs the status to say the phone is held; tap
            // until it does.
            screen.tap()
            _ = toast.waitForExistence(timeout: 2)
        }
        XCTAssertTrue(toast.exists, "no toast after tapping a phone another agent holds")
        try await Task.sleep(for: .milliseconds(400))   // let it settle where it goes

        let frame = screen.frame
        let scale = min(frame.width / pictureSize.width, frame.height / pictureSize.height)
        let size = CGSize(width: pictureSize.width * scale, height: pictureSize.height * scale)
        let picture = CGRect(x: frame.midX - size.width / 2, y: frame.midY - size.height / 2,
                             width: size.width, height: size.height)
        let placed = toast.frame
        if let dir = ProcessInfo.processInfo.environment["SHOT_DIR"] {
            try? XCUIScreen.main.screenshot().pngRepresentation
                .write(to: URL(fileURLWithPath: dir).appending(path: shot + ".png"))
        }
        XCTAssertGreaterThan(placed.width, 0)
        XCTAssertFalse(placed.intersects(picture.insetBy(dx: 1, dy: 1)),
                       "the toast \(placed) covers the picture \(picture)")
        XCTAssertTrue(app.windows.firstMatch.frame.contains(placed), "the toast \(placed) is off screen")
    }

    func testToastKeepsOffThePictureInPortrait() async throws {
        try await checkToast(orientation: .portrait, immersive: false, shot: "toast-portrait")
    }

    func testToastKeepsOffThePictureInLandscape() async throws {
        try await checkToast(orientation: .landscapeLeft, immersive: false, shot: "toast-landscape")
    }

    func testToastKeepsOffThePictureWhenImmersive() async throws {
        try await checkToast(orientation: .portrait, immersive: true, shot: "toast-immersive")
    }

    func testToastKeepsOffThePictureWhenImmersiveInLandscape() async throws {
        try await checkToast(orientation: .landscapeLeft, immersive: true, shot: "toast-immersive-landscape")
    }
}
