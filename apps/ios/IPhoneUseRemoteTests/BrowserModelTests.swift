import XCTest
@testable import IPhoneUseRemote

private final class BrowserModelFixtureProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "model.example" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        let path = request.url!.path
        var status = 200
        let response: [String: Any]
        if path.hasSuffix("/models") {
            response = ["data": [["id": "fixture:free", "supported_parameters": ["tools", "tool_choice"]]]]
        } else {
            var bodyData = request.httpBody ?? Data()
            if let stream = request.httpBodyStream {
                stream.open()
                defer { stream.close() }
                var buffer = [UInt8](repeating: 0, count: 4096)
                while stream.hasBytesAvailable {
                    let count = stream.read(&buffer, maxLength: buffer.count)
                    if count <= 0 { break }
                    bodyData.append(contentsOf: buffer.prefix(count))
                }
            }
            let body = (try? JSONSerialization.jsonObject(with: bodyData)) as? [String: Any] ?? [:]
            if body["model"] as? String == "retired:free" {
                status = 404
                response = ["error": "Model retired"]
            } else if path.hasSuffix("/messages") {
                let choice = body["tool_choice"] as? [String: Any]
                guard choice?["name"] as? String == "browser_action", request.value(forHTTPHeaderField: "x-api-key") == "fixture-key" else {
                    client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse)); return
                }
                response = ["content": [["type": "tool_use", "name": "browser_action", "input": ["action": "wait", "explanation": "等待网页"]]]]
            } else {
                let choice = body["tool_choice"] as? [String: Any]
                guard (choice?["function"] as? [String: Any])?["name"] as? String == "browser_action" else {
                    client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse)); return
                }
                response = ["choices": [["message": ["tool_calls": [["function": ["name": "browser_action", "arguments": "{\"action\":\"wait\",\"explanation\":\"等待网页\"}"]]]]]]]
            }
        }
        let http = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: http, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: try! JSONSerialization.data(withJSONObject: response))
        client?.urlProtocolDidFinishLoading(self)
    }
}

@MainActor
final class BrowserModelTests: XCTestCase {
    private func modelClient() -> BrowserModelClient {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [BrowserModelFixtureProtocol.self]
        return BrowserModelClient(session: URLSession(configuration: configuration))
    }

    func testRetiredFreeModelFallsBackThroughNativeTools() async throws {
        let provider = ModelsPayload.Assist(format: "openai", baseURL: "https://model.example/v1", key: "", model: "retired:free")
        let decision = try await modelClient().decide(goal: "测试", page: nil, steps: [], models: [provider])
        XCTAssertEqual(decision.action, .wait)
    }

    func testAnthropicNativeToolResponse() async throws {
        let provider = ModelsPayload.Assist(format: "anthropic", baseURL: "https://model.example/v1", key: "fixture-key", model: "fixture")
        let decision = try await modelClient().decide(goal: "测试", page: nil, steps: [], models: [provider])
        XCTAssertEqual(decision.action, .wait)
        XCTAssertEqual(decision.explanation, "等待网页")
    }
}
