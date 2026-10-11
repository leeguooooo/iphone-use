import XCTest
@testable import IPhoneUseRemote

@MainActor
final class BrowserTests: XCTestCase {
    func testAddressAndSearch() {
        XCTAssertEqual(BrowserSession.destination(" example.com ")?.absoluteString, "https://example.com")
        XCTAssertEqual(BrowserSession.destination("https://example.com/a?q=x")?.host, "example.com")
        let search = BrowserSession.destination("苹果 手机")!
        XCTAssertEqual(URLComponents(url: search, resolvingAgainstBaseURL: false)?.queryItems?.first?.value, "苹果 手机")
    }

    func testUnsupportedSchemesAndEmptyAddresses() {
        XCTAssertNil(BrowserSession.destination("file:///etc/passwd"))
        XCTAssertNil(BrowserSession.destination("javascript://alert(1)"))
        XCTAssertNil(BrowserSession.destination("iphone-use://pair"))
        XCTAssertNil(BrowserSession.destination("   "))
    }
    func testPageToolsAndStaleReferences() async throws {
        let browser = BrowserSession()
        browser.webView.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        browser.loading = true
        browser.webView.loadHTMLString("""
        <h1>Browser fixture</h1><label>Name<input id="name" placeholder="Your name"></label>
        <input type="password" aria-label="Password"><button onclick="document.querySelector('h1').textContent='Clicked'">Continue</button>
        <select aria-label="Language"><option value="en">English</option><option value="zh">Chinese</option></select>
        """, baseURL: URL(string: "https://example.com"))
        for _ in 0..<200 {
            if !browser.loading { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertFalse(browser.loading)
        let page = try await browser.readPage()
        XCTAssertTrue(page.text.contains("Browser fixture"))
        let field = try XCTUnwrap(page.elements.first { $0.label == "Name" })
        let password = try XCTUnwrap(page.elements.first { $0.label == "Password" })
        XCTAssertNil(password.value)
        let button = try XCTUnwrap(page.elements.first { $0.label == "Continue" })
        let dropdown = try XCTUnwrap(page.elements.first { $0.label == "Language" })
        let input = #"O'Reilly "quoted" <script>literal</script>"#
        try await browser.fill(element: field.id, token: page.token, text: input)
        let value = try await browser.webView.evaluateJavaScript("document.querySelector('#name').value") as? String
        XCTAssertEqual(value, input)
        try await browser.select(element: dropdown.id, token: page.token, value: "Chinese")
        let selected = try await browser.webView.evaluateJavaScript("document.querySelector('select').value") as? String
        XCTAssertEqual(selected, "zh")
        do {
            try await browser.fill(element: password.id, token: page.token, text: "synthetic")
            XCTFail("Password fields require the person")
        } catch {}
        try await browser.click(element: button.id, token: page.token)
        let clickedText = try await browser.read()
        XCTAssertTrue(clickedText.contains("Clicked"))
        _ = try await browser.readPage()
        do {
            try await browser.click(element: button.id, token: page.token)
            XCTFail("A new snapshot must invalidate old references")
        } catch {}
    }

    func testNativeJavaScriptConfirmation() async throws {
        let browser = BrowserSession()
        browser.webView.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        browser.loading = true
        browser.webView.loadHTMLString("<h1>Dialog fixture</h1>", baseURL: URL(string: "https://example.com"))
        try await browser.waitUntilReady()
        // WebKit dialogs need a visible view; an offscreen view can suppress
        // the callback on recent iOS, leaving this test waiting indefinitely.
        let window = UIWindow(frame: browser.webView.frame)
        let controller = UIViewController(); controller.view = browser.webView
        window.rootViewController = controller; window.makeKeyAndVisible()
        defer { window.isHidden = true; browser.answerDialog(false) }
        let result = Task { try await browser.webView.evaluateJavaScript("confirm('Approve fixture?')") as? Bool }
        for _ in 0..<100 {
            if browser.dialog != nil { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        guard browser.dialog != nil else { result.cancel(); XCTFail("WebKit did not present the fixture dialog"); return }
        XCTAssertEqual(browser.dialog?.message, "Approve fixture?")
        browser.answerDialog(true)
        let accepted = try await result.value
        XCTAssertEqual(accepted, true)
    }

    func testLivePublicPageLoads() async throws {
        guard ProcessInfo.processInfo.environment["BROWSER_LIVE_MODEL"] == "1" else {
            throw XCTSkip("Set TEST_RUNNER_BROWSER_LIVE_MODEL=1 for public website acceptance")
        }
        let browser = BrowserSession()
        browser.webView.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        browser.open("https://www.apple.com")
        try await browser.waitUntilReady()
        let page = try await browser.readPage()
        XCTAssertTrue(page.title.contains("Apple"))
        XCTAssertFalse(page.elements.isEmpty, "Public page: title=\(page.title), text=\(page.text.prefix(300))")
    }

}
