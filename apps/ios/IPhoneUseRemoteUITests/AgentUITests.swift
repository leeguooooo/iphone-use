import XCTest

/// A goal typed on the Agent screen, carried out by `Tools/mock_daemon.py`
/// (skipped unless it answers): the goal pays, so the task stops for the
/// person's OK, the confirmation sheet comes up, and after "Allow" the
/// summary arrives. `TEST_RUNNER_SHOT_DIR=<dir>` saves screenshots there.
@MainActor
final class AgentUITests: XCTestCase {
    private let mock = URL(string: ProcessInfo.processInfo.environment["MOCK_DAEMON"] ?? "http://127.0.0.1:47001")!
    private var app: XCUIApplication!
    private var conflictTaskID: String?

    override func setUp() async throws {
        var request = URLRequest(url: mock.appendingPathComponent( "mock/info"))
        request.timeoutInterval = 3
        guard (try? await URLSession.shared.data(for: request)) != nil else {
            throw XCTSkip("mock daemon not running at \(mock)")
        }
    }

    override func tearDown() async throws {
        if let id = conflictTaskID {
            _ = try? await post("agent/task/\(id)/cancel", body: [:])
            // Let the delayed fixture report cancellation and release background work.
            try? await Task.sleep(nanoseconds: 4_000_000_000)
        }
        app?.terminate()
    }

    func testGoalConfirmAndSummary() throws {
        app = XCUIApplication()
        // `-agent YES` opens the Agent screen once the mock is added (and so on screen).
        app.launchArguments = ["-address", mock.absoluteString, "-password", "mock", "-observeOnly", "YES",
                               "-agent", "YES", "-legacyUI", "YES"]
        app.launch()

        let field = app.textFields["agent-goal"].exists ? app.textFields["agent-goal"] : app.textViews["agent-goal"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "no goal field")
        // The target connects in the background; wait until a send is possible.
        field.tap()
        field.typeText("打开拼多多付款")
        let send = app.buttons["agent-send"]
        let enabled = NSPredicate(format: "isEnabled == true")
        expectation(for: enabled, evaluatedWith: send)
        waitForExpectations(timeout: 15)
        send.tap()
        shot("agent-running")

        let approve = app.buttons["agent-approve"]
        XCTAssertTrue(approve.waitForExistence(timeout: 20), "the confirmation sheet did not come up")
        shot("agent-confirm")
        approve.tap()

        let summary = app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "87%")).firstMatch
        XCTAssertTrue(summary.waitForExistence(timeout: 20), "no summary after allowing the step")
        XCTAssertTrue(app.buttons["agent-save-flow"].waitForExistence(timeout: 5), "no Save as flow on a done task")
        shot("agent-done")
    }

    func testRefusedTaskKeepsTheDraft() throws {
        app = XCUIApplication()
        app.launchArguments = ["-address", mock.absoluteString, "-password", "mock", "-observeOnly", "YES",
                               "-agent", "YES", "-legacyUI", "YES"]
        app.launch()
        let field = app.textFields["agent-goal"].exists ? app.textFields["agent-goal"] : app.textViews["agent-goal"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.tap()
        field.typeText("__mock_refuse_task__")
        let send = app.buttons["agent-send"]
        expectation(for: NSPredicate(format: "isEnabled == true"), evaluatedWith: send)
        waitForExpectations(timeout: 15)
        send.tap()
        XCTAssertTrue(app.staticTexts["Mock task service is unavailable"].waitForExistence(timeout: 10))
        XCTAssertEqual(field.value as? String, "__mock_refuse_task__")
        XCTAssertTrue(send.isEnabled, "The person must be able to retry their draft")
        shot("agent-refused-draft")
        app.terminate()
        app.launch()
        let restored = app.textFields["agent-goal"].exists ? app.textFields["agent-goal"] : app.textViews["agent-goal"]
        XCTAssertTrue(restored.waitForExistence(timeout: 10))
        XCTAssertEqual(restored.value as? String, "__mock_refuse_task__")
    }

    func testExistingTaskKeepsItsRealGoalAndTheNewDraft() async throws {
        let existing = "__mock_task_conflict_existing__"
        let data = try await post("agent/task", body: ["goal": existing])
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        conflictTaskID = try XCTUnwrap(object["id"] as? String)
        app = XCUIApplication()
        app.launchArguments = ["-address", mock.absoluteString, "-password", "mock", "-observeOnly", "YES",
                               "-agent", "YES", "-legacyUI", "YES", "-clearAgentTestState", "YES"]
        app.launch()
        let field = app.descendants(matching: .any).matching(identifier: "agent-goal").firstMatch
        guard field.waitForExistence(timeout: 10) else {
            XCTFail("Missing goal composer: \(app.debugDescription)")
            return
        }
        field.tap()
        field.typeText("新的未提交目标")
        let send = app.buttons["agent-send"]
        let enabled = expectation(for: NSPredicate(format: "isEnabled == true"), evaluatedWith: send)
        await fulfillment(of: [enabled], timeout: 15)
        send.tap()
        XCTAssertTrue(app.buttons["agent-stop"].waitForExistence(timeout: 15))
        XCTAssertTrue(app.staticTexts[existing].exists, "A rejected goal must never relabel the running task")
        XCTAssertFalse(app.staticTexts["新的未提交目标"].exists)
        XCTAssertEqual(field.value as? String, "新的未提交目标")
    }

    private func post(_ path: String, body: [String: String]) async throws -> Data {
        var request = URLRequest(url: mock.appendingPathComponent(path))
        request.httpMethod = "POST"
        request.setValue("phone_session=mock", forHTTPHeaderField: "Cookie")
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        return try await URLSession.shared.data(for: request).0
    }

    private func shot(_ name: String) {
        let screenshot = XCUIScreen.main.screenshot()
        let attachment = XCTAttachment(screenshot: screenshot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let dir = ProcessInfo.processInfo.environment["TEST_RUNNER_SHOT_DIR"] {
            try? screenshot.pngRepresentation.write(to: URL(fileURLWithPath: dir).appendingPathComponent( "\(name).png"))
        }
    }
}
