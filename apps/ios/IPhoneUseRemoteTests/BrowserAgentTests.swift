import XCTest
@testable import IPhoneUseRemote

@MainActor
private final class ScriptedBrowserReasoning: BrowserReasoning, BrowserCompletionChecking {
    var requested = false
    var delay = false
    var mode = "complete"
    var count = 0
    var requests: [String] = []

    func verifyCompletion(goal: String, page: BrowserPage, steps: [BrowserAgentStep], models: [ModelsPayload.Assist]) async throws -> Bool {
        if mode == "premature" { return page.text.contains("Updated") && page.elements.first(where: { $0.label == "Name" })?.value == "Phone Use" }
        return true
    }
    func decide(goal: String, page: BrowserPage?, steps: [BrowserAgentStep], models: [ModelsPayload.Assist]) async throws -> BrowserDecision {
        requested = true
        requests.append(goal)
        if delay { try? await Task.sleep(for: .milliseconds(500)) }
        count += 1
        guard let page else { throw BrowserToolError.invalidResult }
        if mode == "premature" {
            if count == 2 { return .init(action: .fill, explanation: "填写测试名称", element: try XCTUnwrap(page.elements.first { $0.label == "Name" }).id, text: "Phone Use") }
            if count == 3 { return .init(action: .click, explanation: "更新网页", element: try XCTUnwrap(page.elements.first { $0.label == "Continue" }).id) }
            return .init(action: .done, explanation: "模型声称完成")
        }
        if mode == "submit", !page.text.contains("Submitted") {
            return .init(action: .click, explanation: "提交测试表单", element: try XCTUnwrap(page.elements.first { $0.isSubmit }).id)
        }
        if count == 1 {
            return .init(action: .fill, explanation: "填写测试名称", element: try XCTUnwrap(page.elements.first { $0.label == "Name" }).id, text: "Phone Use")
        }
        if count == 2, mode == "complete" {
            return .init(action: .click, explanation: "更新网页", element: try XCTUnwrap(page.elements.first { $0.label == "Continue" }).id)
        }
        return .init(action: .done, explanation: "网页已显示更新结果")
    }
}

@MainActor
final class BrowserAgentTests: XCTestCase {
    private func fixture() async throws -> BrowserSession {
        let browser = BrowserSession()
        browser.webView.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        browser.hasPage = true
        browser.loading = true
        browser.webView.loadHTMLString("""
        <title>Ready</title><h1>Ready</h1><label>Name<input id="name"></label>
        <button onclick="document.querySelector('h1').textContent='Updated';document.title='Updated'">Continue</button>
        <form onsubmit="event.preventDefault();document.querySelector('h1').textContent='Submitted'"><button type="submit">Submit</button></form>
        """, baseURL: URL(string: "https://example.com"))
        try await browser.waitUntilReady()
        return browser
    }

    private func waitFor(_ predicate: () -> Bool) async throws {
        for _ in 0..<200 {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTFail("Browser task did not reach the expected state")
    }

    func testPrematureDoneIsRejectedUntilThePageActuallyMatches() async throws {
        let browser = try await fixture(), reasoning = ScriptedBrowserReasoning()
        reasoning.mode = "premature"
        let agent = BrowserAgent(browser: browser, reasoning: reasoning)
        agent.start("填写名称并更新网页", settings: ModelSettings())
        try await waitFor { agent.state == .done || agent.state == .failed }
        XCTAssertEqual(agent.state, .done, agent.message ?? "")
        XCTAssertEqual(reasoning.count, 4)
        XCTAssertTrue(agent.steps.first?.text.contains("核验未通过") == true)
        let value = try await browser.webView.evaluateJavaScript("document.querySelector('#name').value") as? String
        XCTAssertEqual(value, "Phone Use")
    }
    func testObserveFillClickAndFinish() async throws {
        let browser = try await fixture()
        let agent = BrowserAgent(browser: browser, reasoning: ScriptedBrowserReasoning())
        agent.start("更新网页", settings: ModelSettings())
        try await waitFor { agent.state == .done || agent.state == .failed }
        XCTAssertEqual(agent.state, .done, agent.message ?? "")
        XCTAssertEqual(agent.steps.count, 2)
        let text = try await browser.read()
        XCTAssertTrue(text.contains("Updated"))
    }

    func testSubmissionWaitsForApproval() async throws {
        let browser = try await fixture()
        let reasoning = ScriptedBrowserReasoning()
        reasoning.mode = "submit"
        let agent = BrowserAgent(browser: browser, reasoning: reasoning)
        agent.start("提交表单", settings: ModelSettings())
        try await waitFor { agent.state == .confirming || agent.state == .failed }
        XCTAssertEqual(agent.state, .confirming)
        let before = try await browser.read()
        XCTAssertTrue(before.contains("Ready"))
        agent.approve()
        try await waitFor { agent.state == .done || agent.state == .failed }
        XCTAssertEqual(agent.state, .done, agent.message ?? "")
        let after = try await browser.read()
        XCTAssertTrue(after.contains("Submitted"))
    }

    func testStopDiscardsAnInFlightModelAnswer() async throws {
        let browser = try await fixture()
        let reasoning = ScriptedBrowserReasoning()
        reasoning.delay = true
        let agent = BrowserAgent(browser: browser, reasoning: reasoning)
        agent.start("填写名称", settings: ModelSettings())
        try await waitFor { reasoning.requested }
        agent.stop()
        try await Task.sleep(for: .milliseconds(700))
        XCTAssertEqual(agent.state, .stopped)
        XCTAssertTrue(agent.steps.isEmpty)
        let value = try await browser.webView.evaluateJavaScript("document.querySelector('#name').value") as? String
        XCTAssertEqual(value, "")
    }

    func testChangedPageRequiresAFreshConfirmation() async throws {
        let browser = try await fixture()
        let reasoning = ScriptedBrowserReasoning()
        reasoning.mode = "submit"
        let agent = BrowserAgent(browser: browser, reasoning: reasoning)
        agent.start("提交测试表单", settings: ModelSettings())
        try await waitFor { agent.state == .confirming }
        _ = try await browser.webView.evaluateJavaScript("const b=document.querySelector('button[type=submit]'); b.replaceWith(b.cloneNode(true))")
        agent.approve()
        try await waitFor { agent.state == .confirming || agent.state == .failed }
        XCTAssertEqual(agent.state, .confirming)
        let text = try await browser.read()
        XCTAssertTrue(text.contains("Ready"), "Approval must not carry over to a replaced control")
        agent.stop()
    }

    func testModelReplyMustBeOneKnownAction() throws {
        XCTAssertThrowsError(try BrowserModelClient.parse("{\"action\":\"eval\",\"explanation\":\"unsafe\"}"))
        XCTAssertThrowsError(try BrowserModelClient.parse("prose {\"action\":\"done\",\"explanation\":\"done\"}"))
        let result = try BrowserModelClient.parse("```json\n{\"action\":\"wait\",\"explanation\":\"等待网页\"}\n```")
        XCTAssertEqual(result.action, .wait)
    }

    func testFollowUpKeepsThePreviousReplyAndCurrentRequest() async throws {
        let browser = try await fixture()
        let reasoning = ScriptedBrowserReasoning()
        let agent = BrowserAgent(browser: browser, reasoning: reasoning)
        agent.start("更新网页", settings: ModelSettings())
        try await waitFor { agent.state == .done || agent.state == .failed }
        XCTAssertEqual(agent.state, .done)
        agent.start("刚才填了什么？", settings: ModelSettings())
        try await waitFor { agent.state == .done || agent.state == .failed }
        XCTAssertEqual(agent.history.count, 1)
        XCTAssertEqual(agent.history.first?.goal, "更新网页")
        XCTAssertEqual(agent.history.first?.steps.count, 2)
        XCTAssertEqual(agent.history.first?.reply, "网页已显示更新结果")
        XCTAssertTrue(reasoning.requests.last?.contains("Current user request: 刚才填了什么？") == true)
        XCTAssertTrue(reasoning.requests.last?.contains("Previous reply: 网页已显示更新结果") == true)
    }

    func testLiveFreeModelCompletesTheBrowserTask() async throws {
        guard ProcessInfo.processInfo.environment["BROWSER_LIVE_MODEL"] == "1" else {
            throw XCTSkip("Set TEST_RUNNER_BROWSER_LIVE_MODEL=1 to test the live free model with a synthetic page")
        }
        let browser = try await fixture()
        let agent = BrowserAgent(browser: browser)
        agent.start("在 Name 输入框填入 Phone Use，然后点击 Continue。页面标题变成 Updated 后才完成。不要点击 Submit。", settings: ModelSettings())
        for _ in 0..<2400 {
            if agent.state != .running { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        defer { agent.stop() }
        print("Synthetic browser task steps:", agent.steps.map(\.text), "state:", agent.state)
        XCTAssertEqual(agent.state, .done, agent.message ?? "")
        let value = try await browser.webView.evaluateJavaScript("document.querySelector('#name').value") as? String
        XCTAssertEqual(value, "Phone Use")
        let text = try await browser.read()
        XCTAssertTrue(text.contains("Updated"))
    }
}
