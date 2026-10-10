import XCTest

/// Drives the remote screen in the Simulator against `Tools/mock_daemon.py`
/// and checks what reached the "phone": that a touch lands where it was
/// made on the picture, also zoomed in, that zooming itself sends nothing,
/// and that the keyboard and keys send what they say. Skipped unless the
/// mock answers (see apps/ios/README.md).
@MainActor
final class RemoteGestureUITests: XCTestCase {
    private let mock = URL(string: ProcessInfo.processInfo.environment["MOCK_DAEMON"] ?? "http://127.0.0.1:47001")!
    private var app: XCUIApplication!
    private var screen: XCUIElement!
    /// Where the picture sits inside the screen element (aspect fit).
    private var picture: CGRect = .zero

    override func setUp() async throws {
        guard let info = try? await json(get: "mock/info") as? [String: Any],
              let width = info["width"] as? Double, let height = info["height"] as? Double, width > 0 else {
            throw XCTSkip("mock daemon not running at \(mock)")
        }
        _ = try await request("mock/scenario", method: "POST", body: #"{"name":"live"}"#)
        app = XCUIApplication()
        app.launchArguments = ["-address", mock.absoluteString, "-password", "mock"]
        app.launch()
        screen = app.otherElements["remote-screen"]
        XCTAssertTrue(screen.waitForExistence(timeout: 15), "the remote screen did not appear")
        // Let the first frame size the picture, then start counting.
        try await Task.sleep(for: .seconds(2))
        _ = try await request("mock/actions", method: "DELETE")
        let frame = screen.frame
        let scale = min(frame.width / width, frame.height / height)
        let size = CGSize(width: width * scale, height: height * scale)
        picture = CGRect(x: (frame.width - size.width) / 2, y: (frame.height - size.height) / 2,
                         width: size.width, height: size.height)
    }

    override func tearDown() async throws {
        app?.terminate()
    }

    // MARK: helpers

    private func request(_ path: String, method: String = "GET", body: String? = nil) async throws -> Data {
        var request = URLRequest(url: mock.appending(path: path))
        request.httpMethod = method
        request.httpBody = body?.data(using: .utf8)
        request.timeoutInterval = 3
        return try await URLSession.shared.data(for: request).0
    }

    private func json(get path: String) async throws -> Any {
        try JSONSerialization.jsonObject(with: try await request(path))
    }

    /// `/control` actions the mock received, waiting up to `timeout` for `count`.
    private func actions(count: Int, timeout: TimeInterval = 5) async throws -> [[String: Any]] {
        let deadline = Date().addingTimeInterval(timeout)
        var list: [[String: Any]] = []
        repeat {
            list = (try await json(get: "mock/actions") as? [[String: Any]] ?? [])
                .filter { $0["path"] as? String == "/control" }
            if list.count >= count { return list }
            try await Task.sleep(for: .milliseconds(200))
        } while Date() < deadline
        return list
    }

    /// The screen element's coordinate at a point of the picture (0…1).
    private func at(_ x: CGFloat, _ y: CGFloat) -> XCUICoordinate {
        screen.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: picture.minX + x * picture.width, dy: picture.minY + y * picture.height))
    }

    private func number(_ action: [String: Any], _ key: String) -> Double {
        (action[key] as? Double) ?? Double(action[key] as? Int ?? -1)
    }

    // MARK: tests

    func testTapLandsWhereThePictureWasTouched() async throws {
        at(0.5, 0.5).tap()
        at(0.2, 0.8).tap()
        let sent = try await actions(count: 2)
        XCTAssertEqual(sent.count, 2)
        XCTAssertEqual(sent[0]["type"] as? String, "tap")
        XCTAssertEqual(number(sent[0], "x"), 0.5, accuracy: 0.02)
        XCTAssertEqual(number(sent[0], "y"), 0.5, accuracy: 0.02)
        XCTAssertEqual(number(sent[1], "x"), 0.2, accuracy: 0.02)
        XCTAssertEqual(number(sent[1], "y"), 0.8, accuracy: 0.02)
    }

    func testHoldIsALongPress() async throws {
        at(0.4, 0.3).press(forDuration: 1.0)
        let sent = try await actions(count: 1)
        XCTAssertEqual(sent.first?["type"] as? String, "longpress")
        XCTAssertGreaterThanOrEqual(number(sent[0], "duration_ms"), 600)
        XCTAssertEqual(number(sent[0], "x"), 0.4, accuracy: 0.02)
        XCTAssertEqual(number(sent[0], "y"), 0.3, accuracy: 0.02)
    }

    func testSlowDragIsASwipeThatEndsUnderTheFinger() async throws {
        at(0.5, 0.7).press(forDuration: 0.05, thenDragTo: at(0.5, 0.4), withVelocity: 300, thenHoldForDuration: 0.3)
        let sent = try await actions(count: 1)
        XCTAssertEqual(sent.first?["type"] as? String, "swipe")
        XCTAssertEqual(number(sent[0], "y1"), 0.7, accuracy: 0.03)
        XCTAssertEqual(number(sent[0], "y2"), 0.4, accuracy: 0.03)
    }

    func testFlickCarriesFurtherThanTheFinger() async throws {
        at(0.5, 0.75).press(forDuration: 0.02, thenDragTo: at(0.5, 0.55), withVelocity: 3000, thenHoldForDuration: 0)
        let sent = try await actions(count: 1)
        XCTAssertEqual(sent.first?["type"] as? String, "swipe")
        XCTAssertLessThan(number(sent[0], "y2"), 0.53, "a flick should travel past where the finger lifted")
        XCTAssertLessThanOrEqual(number(sent[0], "duration_ms"), 300)
    }

    func testPinchZoomsWithoutTouchingThePhoneAndTapsStillLand() async throws {
        screen.pinch(withScale: 2.5, velocity: 2)
        try await Task.sleep(for: .seconds(1))
        let afterPinch = try await actions(count: 1, timeout: 0.3)
        XCTAssertTrue(afterPinch.isEmpty, "zooming must not send anything to the phone: \(afterPinch)")
        let chip = app.buttons["zoom-chip"]
        XCTAssertTrue(chip.waitForExistence(timeout: 3), "the zoom chip should appear")
        let scale = Double(chip.value as? String ?? "") ?? 0
        XCTAssertGreaterThan(scale, 1.2)
        // A tap a quarter of the picture right of center lands 0.25/scale
        // right of the phone's center (the pinch was centered).
        let frame = screen.frame
        screen.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: frame.width / 2 + picture.width * 0.25, dy: frame.height / 2)).tap()
        let sent = try await actions(count: 1)
        XCTAssertEqual(sent.first?["type"] as? String, "tap")
        XCTAssertEqual(number(sent[0], "x"), 0.5 + 0.25 / scale, accuracy: 0.04)
        // The chip zooms back out.
        chip.tap()
        XCTAssertTrue(chip.waitForNonExistence(timeout: 3))
    }

    func testKeyboardTypesLiveAndSendsKeys() async throws {
        app.buttons["key-keyboard"].tap()
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText("hi")
        app.buttons["delete.left"].tap()
        field.typeText("\n")
        let sent = try await actions(count: 3)
        let text = sent.filter { $0["type"] as? String == "text" }.compactMap { $0["text"] as? String }.joined()
        XCTAssertEqual(text, "hi")
        let keys = sent.filter { $0["type"] as? String == "key" }.compactMap { $0["name"] as? String }
        XCTAssertEqual(keys, ["backspace", "return"])
    }

    func testKeysSendBackHomeAndSearch() async throws {
        for key in ["key-chevron.backward", "key-house", "key-magnifyingglass"] {
            app.buttons[key].tap()
        }
        let sent = try await actions(count: 3)
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["back", "shortcut", "shortcut"])
        XCTAssertEqual(sent.dropFirst().map { $0["name"] as? String }, ["home", "spotlight"])
    }
}
