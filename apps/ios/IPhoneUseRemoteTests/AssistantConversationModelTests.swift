import XCTest
@testable import IPhoneUseRemote

private final class VoiceModelHTTPFixture: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "voice-model.example" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        var data = request.httpBody ?? Data()
        if let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                if count <= 0 { break }
                data.append(contentsOf: buffer.prefix(count))
            }
        }
        let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        let model = body["model"] as? String ?? ""
        var status = 200
        let object: [String: Any]
        if request.url!.path.hasSuffix("/models") {
            object = ["data": [["id": "available:free", "supported_parameters": ["tools"]]]]
        } else if model == "retired:free" || model == "broken" {
            status = 503
            object = ["error": "synthetic service failure"]
        } else if request.url!.path.hasSuffix("/messages") {
            guard request.url?.path == "/v1/messages", request.value(forHTTPHeaderField: "x-api-key") == "fixture-key",
                  (body["tool_choice"] as? [String: Any])?["name"] as? String == "assistant_turn" else {
                client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse)); return
            }
            object = ["content": [["type": "tool_use", "name": "assistant_turn", "input": ["action": "reply", "reply": "你好，来自测试模型。"]]]]
        } else {
            guard ((body["tool_choice"] as? [String: Any])?["function"] as? [String: Any])?["name"] as? String == "assistant_turn" else {
                client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse)); return
            }
            object = ["choices": [["finish_reason": "tool_calls", "message": ["tool_calls": [["function": ["name": "assistant_turn", "arguments": "{\"action\":\"run\",\"reply\":\"准备打开设置\",\"goal\":\"打开设置\"}"]]]]]]]
        }
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: try! JSONSerialization.data(withJSONObject: object))
        client?.urlProtocolDidFinishLoading(self)
    }
}

@MainActor
final class AssistantConversationModelTests: XCTestCase {
    private func client() -> AssistantConversationModel {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [VoiceModelHTTPFixture.self]
        return AssistantConversationModel(session: URLSession(configuration: configuration))
    }
    func testOpenAICompatibleNativeToolContract() async throws {
        let response = try await client().respond(messages: [.init(role: .user, text: "打开设置")], phone: .init(), models: [
            .init(format: "openai", baseURL: "https://voice-model.example/v1", key: "fixture-key", model: "fixture")
        ])
        XCTAssertEqual(response.action, .run)
        XCTAssertEqual(response.goal, "打开设置")
    }
    func testAnthropicNativeToolContract() async throws {
        let response = try await client().respond(messages: [.init(role: .user, text: "你好")], phone: .init(), models: [
            .init(format: "anthropic", baseURL: "https://voice-model.example/v1", key: "fixture-key", model: "fixture")
        ])
        XCTAssertEqual(response.action, .reply)
    }
    func testAnthropicBaseURLWithTrailingSlashDoesNotDuplicateVersion() async throws {
        let response = try await client().respond(messages: [.init(role: .user, text: "你好")], phone: .init(), models: [
            .init(format: "anthropic", baseURL: "https://voice-model.example/v1/", key: "fixture-key", model: "fixture")
        ])
        XCTAssertEqual(response.action, .reply)
    }
    func testRetiredFreeModelFallsBackToDiscoveredToolCapableModel() async throws {
        let response = try await client().respond(messages: [.init(role: .user, text: "打开设置")], phone: .init(), models: [
            .init(format: "openai", baseURL: "https://voice-model.example/v1", key: "", model: "retired:free")
        ])
        XCTAssertEqual(response.goal, "打开设置")
    }
    func testProviderFailureFallsBackInConfiguredOrder() async throws {
        let response = try await client().respond(messages: [.init(role: .user, text: "打开设置")], phone: .init(), models: [
            .init(format: "openai", baseURL: "https://voice-model.example/v1", key: "", model: "broken"),
            .init(format: "openai", baseURL: "https://voice-model.example/v1", key: "", model: "fixture")
        ])
        XCTAssertEqual(response.action, .run)
    }
    func testMissingGoalUnknownActionAndEmptyReplyAreRejected() {
        for value in [#"{"action":"run","reply":"打开"}"#, #"{"action":"approve","reply":"已批准"}"#,
                      #"{"action":"replace","reply":"换任务","goal":" "}"#, #"{"action":"reply","reply":" "}"#] {
            XCTAssertThrowsError(try AssistantVoiceDecision.parse(Data(value.utf8)))
        }
    }
    func testMultipleToolsTruncatedReplyAndWrongToolAreRejected() {
        let call: [String: Any] = ["function": ["name": "assistant_turn", "arguments": "{\"action\":\"reply\",\"reply\":\"你好\"}"]]
        XCTAssertThrowsError(try AssistantConversationModel.decode(["choices": [["message": ["tool_calls": [call, call]]]]], anthropic: false))
        XCTAssertThrowsError(try AssistantConversationModel.decode(["choices": [["finish_reason": "length", "message": ["tool_calls": [call]]]]], anthropic: false))
        XCTAssertThrowsError(try AssistantConversationModel.decode(["content": [["type": "tool_use", "name": "approve", "input": ["action": "reply", "reply": "你好"]]]], anthropic: true))
    }
    /// What free models actually send back (probed on Kilo's free pool).
    func testFreeModelShapesAreReadNotRejected() throws {
        func openAI(_ message: [String: Any]) throws -> AssistantVoiceDecision {
            try AssistantConversationModel.decode(["choices": [["finish_reason": "stop", "message": message]]], anthropic: false, fallbackGoal: "打开设置看 iOS 版本")
        }
        func call(_ args: String) -> [String: Any] { ["tool_calls": [["function": ["name": "assistant_turn", "arguments": args]]]] }
        // run without a goal: the person's words are the goal
        let noGoal = try openAI(call("{\"action\": \"run\", \"reply\": \"好的\"}"))
        XCTAssertEqual(noGoal.goal, "打开设置看 iOS 版本")
        // arguments fenced and wrapped in words
        XCTAssertEqual(try openAI(call("好的：\n```json\n{\"action\":\"reply\",\"reply\":\"你好\"}\n```")).reply, "你好")
        // the call written into the text
        let written = try openAI(["content": "<tool_call>{\"name\":\"assistant_turn\",\"arguments\":{\"action\":\"run\",\"reply\":\"打开\",\"goal\":\"打开设置\"}}</tool_call>"])
        XCTAssertEqual(written.action, .run); XCTAssertEqual(written.goal, "打开设置")
        // plain words, no call: a spoken reply
        let plain = try openAI(["content": "我不能直接打开设置。"])
        XCTAssertEqual(plain.action, .reply); XCTAssertEqual(plain.reply, "我不能直接打开设置。")
        // a call naming another tool is still refused
        XCTAssertThrowsError(try openAI(["content": "<tool_call>{\"name\":\"approve\",\"arguments\":{}}</tool_call>"]))
    }
    func testRemoteHTTPAndCredentialsInModelURLAreRejected() {
        XCTAssertThrowsError(try AssistantConversationModel.endpoint("http://voice-model.example/v1", suffix: "messages"))
        XCTAssertThrowsError(try AssistantConversationModel.endpoint("https://user:secret@voice-model.example/v1", suffix: "messages"))
        XCTAssertNoThrow(try AssistantConversationModel.endpoint("http://127.0.0.1:9000/v1", suffix: "messages"))
    }
    func testRealFreeProviderWithSyntheticConversationOnly() async throws {
        guard ProcessInfo.processInfo.environment["VOICE_LIVE_MODEL_TEST"] == "1" else { throw XCTSkip("Explicit opt-in required for live inference") }
        let response = try await AssistantConversationModel().respond(messages: [.init(role: .user, text: "这是自动化验收，只聊天，不操作手机。请回答你好。")], phone: .init(), models: [
            .init(format: "openai", baseURL: ModelProvider.Kind.free.defaultBaseURL, key: "", model: "free")
        ])
        XCTAssertEqual(response.action, .reply)
        XCTAssertFalse(response.reply.isEmpty)
    }
}
