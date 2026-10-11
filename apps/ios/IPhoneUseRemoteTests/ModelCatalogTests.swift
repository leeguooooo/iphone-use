import XCTest
@testable import IPhoneUseRemote

private final class ZenCatalogFixture: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        guard request.url?.absoluteString == "https://opencode.ai/zen/v1/models",
              request.value(forHTTPHeaderField: "Authorization") == "Bearer public" else {
            client?.urlProtocol(self, didFailWithError: URLError(.userAuthenticationRequired)); return
        }
        let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(#"{"data":[{"id":"big-pickle"}]}"#.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
}

@MainActor
final class ModelCatalogTests: XCTestCase {
    func testLiveZenToolCheck() async throws {
        guard ProcessInfo.processInfo.environment["ZEN_LIVE"] == "1" else { throw XCTSkip("Set TEST_RUNNER_ZEN_LIVE=1 for a synthetic public Zen tool-call check") }
        try await ModelCatalog.check(ModelProvider(kind: .zen))
    }

    func testZenStreamReassemblesToolArgumentsAndRejectsTruncation() throws {
        let stream = #"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"browser_action","arguments":"{\"action\":"}}]}}]}"# + "\n" + #"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"done\",\"explanation\":\"OK\"}"}}]},"finish_reason":"tool_calls"}]}"# + "\ndata: [DONE]\n"
        let result = try ZenCompatibility.collapse(Data(stream.utf8))
        let message = (result["choices"] as? [[String: Any]])?.first?["message"] as? [String: Any]
        let call = (message?["tool_calls"] as? [[String: Any]])?.first?["function"] as? [String: String]
        XCTAssertEqual(call?["name"], "browser_action")
        XCTAssertEqual(try BrowserModelClient.parse(call!["arguments"]!).action, .done)
        XCTAssertThrowsError(try ZenCompatibility.collapse(Data(stream.replacingOccurrences(of: "data: [DONE]", with: "").utf8)))
        XCTAssertThrowsError(try ZenCompatibility.collapse(Data("data: {\"error\":{\"message\":\"failed\"}}\ndata: [DONE]\n".utf8)))
    }

    func testZenCompatibilityDoesNotApplyToOtherServicesOrPersonalKeys() {
        XCTAssertTrue(ZenCompatibility.matches("https://opencode.ai/zen/v1", key: "public"))
        XCTAssertFalse(ZenCompatibility.matches("https://other.example/v1", key: "public"))
        XCTAssertFalse(ZenCompatibility.matches("https://opencode.ai/zen/v1", key: "personal-key"))
    }
    func testZenDiscoverySendsPublicToThePresetEndpoint() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ZenCatalogFixture.self]
        let models = try await ModelCatalog.fetch(ModelProvider(kind: .zen), session: URLSession(configuration: configuration))
        XCTAssertEqual(models.map(\.id), ["big-pickle"])
    }
    func testZenListExcludesPaidAndUnsupportedProtocols() throws {
        let data = Data(#"{"data":[{"id":"big-pickle"},{"id":"space-bunny-free"},{"id":"jev-1.13-free"},{"id":"muse-spark-1.3-contributor-free"},{"id":"gpt-paid"},{"id":"old-free","deprecated":true},{"id":"space-bunny-free"}]}"#.utf8)
        XCTAssertEqual(try ModelCatalog.parse(data, kind: .zen).map(\.id), ["big-pickle", "space-bunny-free"])
    }

    func testFreeListRequiresToolsAndPrioritizesImages() throws {
        let data = Data(#"{"data":[{"id":"text:free","supported_parameters":["tools"]},{"id":"image:free","supported_parameters":["tools"],"architecture":{"input_modalities":["text","image"]}},{"id":"paid","supported_parameters":["tools"]},{"id":"safety:free"},{"id":"no-tools:free","supported_parameters":["temperature"]}]}"#.utf8)
        let models = try ModelCatalog.parse(data, kind: .free)
        XCTAssertEqual(models.map(\.id), ["image:free", "text:free"])
        XCTAssertTrue(models[0].readsImages)
    }

    func testInferenceRestrictionIsReportedEvenWhenListingSucceeded() throws {
        let data = Data(#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode"}}"#.utf8)
        let response = HTTPURLResponse(url: URL(string: "https://opencode.ai/zen/v1/chat/completions")!, statusCode: 403, httpVersion: nil, headerFields: nil)!
        XCTAssertThrowsError(try validateModelResponse(data, response: response)) { error in
            XCTAssertTrue(error.localizedDescription.contains("403"))
            XCTAssertTrue(error.localizedDescription.contains("Kilo"))
        }
    }
}
