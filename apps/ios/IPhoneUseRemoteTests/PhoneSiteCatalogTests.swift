import XCTest
@testable import IPhoneUseRemote

private final class PhoneSiteHTTPFixture: URLProtocol, @unchecked Sendable {
    static let corrupt = LockedState(initialState: false)
    static let commit = String(repeating: "a", count: 40)
    static func source(official: Bool) -> Data {
        Data("""
        /* @meta
        {"name":"fixture/read","domain":"example.com","readOnly":true,"description":"\(official ? "official" : "community")","args":{}}
        */
        async function(args) { return {value: "synthetic"}; }
        """.utf8)
    }
    override class func canInit(with request: URLRequest) -> Bool {
        ["api.github.com", "raw.githubusercontent.com"].contains(request.url?.host ?? "")
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        let url = request.url!, official = url.path.contains("leeguooooo")
        let source = Self.source(official: official)
        let data: Data
        if url.host == "raw.githubusercontent.com" {
            data = Self.corrupt.withLock { $0 } ? source + Data("modified".utf8) : source
        } else if url.path.contains("/commits") {
            data = try! JSONSerialization.data(withJSONObject: [["sha": Self.commit]])
        } else {
            data = try! JSONSerialization.data(withJSONObject: ["truncated": false, "tree": [
                ["path":"fixture/read.js","type":"blob","mode":"100644","size":source.count,"sha":PhoneSiteCatalogDownloader.blobHash(source)]
            ]])
        }
        let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: [:])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }
}

@MainActor
final class PhoneSiteCatalogTests: XCTestCase {
    private func downloader() -> PhoneSiteCatalogDownloader {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PhoneSiteHTTPFixture.self]
        return PhoneSiteCatalogDownloader(session: URLSession(configuration: configuration))
    }
    func testPhoneDownloadNeedsNoChromeMacOrProfileAndOfficialPackWins() async throws {
        PhoneSiteHTTPFixture.corrupt.withLock { $0 = false }
        let package = try await downloader().download()
        XCTAssertEqual(package.catalog.adapters.count, 1)
        XCTAssertEqual(package.catalog.adapters[0].description, "official")
        XCTAssertFalse(package.catalog.adapters[0].desktopOnly)
        XCTAssertEqual(package.revisions.count, 2)
        XCTAssertTrue(package.unsupported.isEmpty)
    }
    func testChangedBlobRejectsWholeUpdate() async {
        PhoneSiteHTTPFixture.corrupt.withLock { $0 = true }
        defer { PhoneSiteHTTPFixture.corrupt.withLock { $0 = false } }
        let value = downloader()
        do { _ = try await value.download(); XCTFail("Changed blob must not be installed") }
        catch { XCTAssertNotNil(value.error) }
        XCTAssertFalse(value.downloading)
    }
    func testNodeAndComputerFileCommandsAreNotPhoneCapabilities() throws {
        let prefix = "/* @meta {\"domain\":\"example.com\",\"args\":{}} */\n"
        for body in ["async function(args) { return require('fs'); }",
                     "async function(args) { return process.env; }",
                     "async function(args) { return Buffer.from('x'); }",
                     "async function(args) { return chrome.tabs; }"] {
            XCTAssertNil(try PhoneSiteCatalogDownloader.adapter(Data((prefix + body).utf8), spec: "fixture/read", origin: "synthetic"))
        }
        let file = "/* @meta {\"domain\":\"example.com\",\"args\":{\"video\":{\"type\":\"file\"}}} */\nasync function(args) { return {}; }"
        XCTAssertNil(try PhoneSiteCatalogDownloader.adapter(Data(file.utf8), spec: "fixture/upload", origin: "synthetic"))
    }
    func testTraversalNestedFilesAndTestsAreNotAdapters() {
        for path in ["../read.js", "site/../read.js", "site/test.test.js", "site/folder/read.js", "/site/read.js"] {
            XCTAssertNil(PhoneSiteCatalogDownloader.spec(path))
        }
        XCTAssertEqual(PhoneSiteCatalogDownloader.spec("hackernews/top.js"), "hackernews/top")
    }
    func testOfflineDefaultPackContainsOnlyValidatedPhoneAdapters() throws {
        let package = try XCTUnwrap(PhoneSiteCatalogDownloader.bundled())
        XCTAssertGreaterThan(package.catalog.adapters.count, 20)
        XCTAssertTrue(package.catalog.adapters.allSatisfy { !$0.desktopOnly })
        XCTAssertFalse(package.revisions.isEmpty)
    }
    func testCompatibleSiteRunsInsideThePhoneWebKitPage() async throws {
        let browser = BrowserSession()
        browser.loading = true
        browser.webView.loadHTMLString("<html><body><h1 id='value'>手机页面</h1></body></html>", baseURL: URL(string: "https://example.com"))
        try await browser.waitUntilReady()
        let script = "/* @meta {\"domain\":\"example.com\",\"readOnly\":true,\"args\":{\"label\":{\"required\":true}}} */\nasync function(args) { return {title: document.querySelector('#value').textContent, label: args.label}; }"
        let adapter = try XCTUnwrap(PhoneSiteCatalogDownloader.adapter(Data(script.utf8), spec: "fixture/read", origin: "synthetic"))
        let result = try await browser.runSite(adapter, arguments: ["label": .string("本机执行")])
        XCTAssertEqual(result.result.object?["title"]?.string, "手机页面")
        XCTAssertEqual(result.result.object?["label"]?.string, "本机执行")
    }
    func testActualPublicPacksDownloadOnPhoneWithoutDesktopTooling() async throws {
        guard ProcessInfo.processInfo.environment["PHONE_SITE_LIVE_TEST"] == "1" else { throw XCTSkip("Explicit opt-in required for GitHub download") }
        let package = try await PhoneSiteCatalogDownloader().download()
        XCTAssertGreaterThan(package.catalog.adapters.count, 50)
        XCTAssertTrue(package.catalog.adapters.allSatisfy { !$0.desktopOnly })
        print("PHONE_SITE_DOWNLOAD_VERIFIED supported=\(package.catalog.adapters.count) unsupported=\(package.unsupported.count)")
    }
}
