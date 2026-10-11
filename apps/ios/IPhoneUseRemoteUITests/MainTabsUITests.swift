import XCTest

@MainActor
final class MainTabsUITests: XCTestCase {
    private let fixture = ProcessInfo.processInfo.environment["BROWSER_FIXTURE"] ?? "http://127.0.0.1:47009/browser_fixture.html"

    override func setUp() async throws {
        var request = URLRequest(url: URL(string: fixture)!)
        request.timeoutInterval = 3
        guard let (data, _) = try? await URLSession.shared.data(for: request),
              String(data: data, encoding: .utf8)?.contains("Browser fixture") == true else {
            throw XCTSkip("Run python3 -m http.server 47009 --bind 127.0.0.1 --directory apps/ios/Tools")
        }
    }

    func testTabsAndBrowserNavigation() {
        let app = XCUIApplication()
        app.launchArguments = ["-tab", "browser", "-clearBrowserDraft", "YES", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        defer { app.terminate() }

        for title in ["助手", "浏览器", "远程控制", "任务", "设置"] {
            XCTAssertTrue(app.tabBars.buttons[title].waitForExistence(timeout: 10))
        }
        let field = app.textFields["browser-address"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        capture("browser-start")
        field.tap()
        field.typeText(fixture + "\n")
        XCTAssertTrue(app.webViews.firstMatch.waitForExistence(timeout: 15))
        XCTAssertTrue(app.webViews.staticTexts["Browser fixture"].waitForExistence(timeout: 30))
        capture("browser-loaded")
        app.buttons["browser-read"].tap()
        XCTAssertTrue(app.navigationBars["页面内容"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "Browser fixture")).firstMatch.exists)
        app.buttons["完成"].tap()
        app.tabBars.buttons["设置"].tap()
        XCTAssertTrue(app.staticTexts["模型与服务"].waitForExistence(timeout: 5))
        capture("settings")
        app.tabBars.buttons["任务"].tap()
        XCTAssertTrue(app.buttons["new-task"].waitForExistence(timeout: 5))
        capture("tasks")
        app.tabBars.buttons["浏览器"].tap()
        XCTAssertTrue(app.webViews.staticTexts["Browser fixture"].exists, "Switching tabs must preserve the page")
    }

    func testLegacyBrowserPromptAcceptsTextAndCancel() {
        let app = XCUIApplication()
        app.launchArguments = ["-tab", "browser", "-legacyUI", "YES", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        defer { app.terminate() }
        let address = app.textFields["browser-address"]
        XCTAssertTrue(address.waitForExistence(timeout: 10))
        address.tap()
        address.typeText(fixture + "\n")
        let prompt = app.webViews.buttons["Prompt fixture"]
        XCTAssertTrue(prompt.waitForExistence(timeout: 20))
        prompt.tap()
        let input = app.textFields["browser-dialog-input"]
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        input.tap()
        input.typeText(" fixture")
        app.buttons["确认"].tap()
        XCTAssertTrue(app.webViews.staticTexts["initial fixture"].waitForExistence(timeout: 10))
        prompt.tap()
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        app.buttons["取消"].tap()
        XCTAssertTrue(app.webViews.staticTexts["Prompt cancelled"].waitForExistence(timeout: 10))
    }

    func testLiveBrowserTaskFromTheComposer() throws {
        guard ProcessInfo.processInfo.environment["BROWSER_LIVE_MODEL"] == "1" else {
            throw XCTSkip("Set TEST_RUNNER_BROWSER_LIVE_MODEL=1 for the live browser task")
        }
        let app = XCUIApplication()
        app.launchArguments = ["-tab", "browser", "-clearBrowserDraft", "YES", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        defer { app.terminate() }
        let address = app.textFields["browser-address"]
        XCTAssertTrue(address.waitForExistence(timeout: 10))
        address.tap()
        address.typeText(fixture + "\n")
        XCTAssertTrue(app.webViews.staticTexts["Browser fixture"].waitForExistence(timeout: 20))
        let goal = app.textFields["browser-goal"].exists ? app.textFields["browser-goal"] : app.textViews["browser-goal"]
        goal.tap()
        goal.typeText("在 Name 中填写 Phone Use，点击 Continue，看到 Form updated 才完成。")
        let start = app.buttons["browser-start-task"]
        guard start.isEnabled else { XCTFail("The browser task composer must enable Send after typing a goal"); return }
        start.tap()
        XCTAssertTrue(app.buttons["browser-close-chat"].waitForExistence(timeout: 10))
        let composer = app.textFields["browser-chat-goal"].exists ? app.textFields["browser-chat-goal"] : app.textViews["browser-chat-goal"]
        XCTAssertFalse((composer.value as? String ?? "").contains("在 Name"), "Accepted messages must clear the composer")
        capture("browser-conversation-running")
        app.buttons["browser-close-chat"].tap()
        capture("browser-task-running")
        let updated = app.webViews.staticTexts["Form updated"]
        let deadline = Date().addingTimeInterval(120)
        while Date() < deadline, !updated.exists {
            if app.buttons["browser-approve"].exists {
                capture("browser-task-confirm")
                app.buttons["browser-approve"].tap()
                if app.buttons["browser-close-chat"].exists { app.buttons["browser-close-chat"].tap() }
            }
            if app.buttons["browser-task-result"].exists { break }
            _ = updated.waitForExistence(timeout: 1)
        }
        XCTAssertTrue(updated.exists)
        let finishDeadline = Date().addingTimeInterval(90)
        while Date() < finishDeadline, !app.buttons["browser-task-result"].exists {
            if app.buttons["browser-approve"].exists {
                app.buttons["browser-approve"].tap()
                if app.buttons["browser-close-chat"].exists { app.buttons["browser-close-chat"].tap() }
            }
            _ = app.buttons["browser-task-result"].waitForExistence(timeout: 1)
        }
        XCTAssertTrue(app.buttons["browser-task-result"].exists)
        XCTAssertTrue(app.tabBars.buttons["助手"].isHittable, "Autofill must not leave the web keyboard toolbar over the tab bar")
        capture("browser-task-completed")
        app.buttons["browser-open-chat"].tap()
        XCTAssertTrue(app.staticTexts["browser-assistant-message"].waitForExistence(timeout: 10))
        capture("browser-conversation-result")
        let followUp = app.textFields["browser-chat-goal"].exists ? app.textFields["browser-chat-goal"] : app.textViews["browser-chat-goal"]
        followUp.tap()
        followUp.typeText("刚才 Name 填了什么？只回复答案，不要再点击。")
        app.buttons["browser-chat-send"].tap()
        let replyDeadline = Date().addingTimeInterval(120)
        while Date() < replyDeadline, app.staticTexts.matching(identifier: "browser-assistant-message").count < 2 {
            _ = app.staticTexts.matching(identifier: "browser-assistant-message").element(boundBy: 1).waitForExistence(timeout: 1)
        }
        XCTAssertEqual(app.staticTexts.matching(identifier: "browser-user-message").count, 2)
        XCTAssertEqual(app.staticTexts.matching(identifier: "browser-assistant-message").count, 2)
        XCTAssertTrue(app.staticTexts.matching(identifier: "browser-assistant-message").element(boundBy: 1).label.contains("Phone Use"))
        capture("browser-conversation-follow-up")
    }

    func testConversationKeepsTheDraftWhenExpandedAndCollapsed() {
        let app = XCUIApplication()
        app.launchArguments = ["-tab", "browser", "-clearBrowserDraft", "YES", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        defer { app.terminate() }
        let dock = app.textFields["browser-goal"]
        XCTAssertTrue(dock.waitForExistence(timeout: 10))
        dock.tap()
        dock.typeText("帮我整理这个网页")
        app.buttons["browser-open-chat"].tap()
        XCTAssertTrue(app.buttons["browser-close-chat"].waitForExistence(timeout: 10))
        let editor = app.textFields["browser-chat-goal"].exists ? app.textFields["browser-chat-goal"] : app.textViews["browser-chat-goal"]
        XCTAssertEqual(editor.value as? String, "帮我整理这个网页")
        capture("browser-conversation-draft")
        app.buttons["browser-close-chat"].tap()
        XCTAssertEqual(dock.value as? String, "帮我整理这个网页")
        app.tabBars.buttons["助手"].tap()
        XCTAssertTrue(app.buttons["pair-local-phone"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.textFields["local-agent-goal"].exists)
        capture("local-assistant")
        app.tabBars.buttons["远程控制"].tap()
        XCTAssertTrue(app.buttons["扫码连接"].waitForExistence(timeout: 5))
        capture("remote-control")
    }

    private func capture(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
