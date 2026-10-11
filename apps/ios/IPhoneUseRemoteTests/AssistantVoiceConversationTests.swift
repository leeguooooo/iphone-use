import XCTest
@testable import IPhoneUseRemote

@MainActor
private final class VoiceAudioFixture: AssistantVoiceAudio {
    var onPartial: ((String) -> Void)?
    var onFinal: ((String) -> Void)?
    var onStatus: ((String, Bool) -> Void)?
    var onError: ((String) -> Void)?
    var onPlaybackEnd: (() -> Void)?
    var onLevel: ((Float) -> Void)?
    var spoken: [String] = []
    var starts = 0, ends = 0, interrupts = 0
    var muted = false
    func start() async throws { starts += 1 }
    func end() { ends += 1 }
    func pause() { interrupts += 1 }
    func setMuted(_ value: Bool) { muted = value }
    func speak(_ text: String) { spoken.append(text) }
    func interrupt() { interrupts += 1 }
    func resume() throws {}
    func setSpeaker(_ value: Bool) throws {}
    func finish() { onPlaybackEnd?() }
    func say(_ text: String) { onPartial?(text); onFinal?(text) }
}

@MainActor
private final class VoicePhoneFixture: AssistantPhoneControlling {
    var state = AssistantPhoneState()
    var models: [ModelsPayload.Assist] = [.init(format: "openai", baseURL: "https://fixture.example", key: "synthetic", model: "fixture")]
    var onChange: (() -> Void)?
    var starts: [String] = []
    var stops = 0
    var completesStop = true
    var startError = false
    var disconnected = false
    var keepingConnection = false
    func start(goal: String) async -> Bool {
        starts.append(goal)
        if startError { state.error = "提交失败"; return false }
        state.busy = true
        state.task = .init(id: UUID().uuidString, goal: goal, state: .running)
        onChange?()
        return true
    }
    func stop() async {
        stops += 1
        if completesStop { state.busy = false; state.task?.state = .cancelled; onChange?() }
    }
    func maintainConnection(_ enabled: Bool) { keepingConnection = enabled }
    func disconnect() { keepingConnection = false; disconnected = true; onChange = nil }
    func update(_ body: (inout AssistantPhoneState) -> Void) { body(&state); onChange?() }
}

@MainActor
private final class VoiceModelFixture: AssistantVoiceReasoning {
    var requests: [[AssistantVoiceMessage]] = []
    var states: [AssistantPhoneState] = []
    var pending: [CheckedContinuation<AssistantVoiceDecision, any Error>] = []
    func respond(messages: [AssistantVoiceMessage], phone: AssistantPhoneState, models: [ModelsPayload.Assist]) async throws -> AssistantVoiceDecision {
        requests.append(messages); states.append(phone)
        // Deliberately ignore cancellation: generation checks must protect
        // actions even when an upstream service delivers a late response.
        return try await withCheckedThrowingContinuation { pending.append($0) }
    }
    func resolve(_ value: AssistantVoiceDecision) { pending.removeFirst().resume(returning: value) }
    func fail() { pending.removeFirst().resume(throwing: URLError(.timedOut)) }
}

@MainActor
final class AssistantVoiceConversationTests: XCTestCase {
    private func ready(_ condition: @escaping () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<100 {
            if condition() { return }
            try? await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("Expected asynchronous state did not arrive", file: file, line: line)
    }
    private func rig() async -> (AssistantVoiceCall, VoiceAudioFixture, VoicePhoneFixture, VoiceModelFixture) {
        let audio = VoiceAudioFixture(), phone = VoicePhoneFixture(), model = VoiceModelFixture()
        let call = AssistantVoiceCall(audio: audio, reasoning: model)
        await call.start(phone: phone)
        audio.finish()
        return (call, audio, phone, model)
    }

    func testCallOwnsConnectionUpkeepAndReleasesItOnHangup() async {
        let (call, _, phone, _) = await rig()
        XCTAssertTrue(phone.keepingConnection)
        call.end()
        XCTAssertFalse(phone.keepingConnection)
    }

    func testDisconnectedStopReportsUnsentRequest() async {
        let (call, audio, phone, _) = await rig()
        phone.update { $0.busy = true; $0.connected = false; $0.task = .init(id: "old", goal: "测试", state: .running) }
        audio.say("停止")
        await ready { call.reply.contains("没有送达") }
        XCTAssertEqual(phone.stops, 0)
        call.end()
    }

    func testReplacementAbortsIfAnotherTaskAppearsWhileAwaitingCancellation() async {
        let (call, audio, phone, model) = await rig()
        phone.completesStop = false
        phone.update { $0.busy = true; $0.task = .init(id: "old", goal: "旧任务", state: .running) }
        audio.say("停止当前任务，改为打开设置")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .replace, reply: "准备换任务", goal: "打开设置"))
        await ready { phone.stops == 1 }
        phone.update { $0.busy = false; $0.task = .init(id: "another", goal: "其他任务", state: .done) }
        await ready { call.reply.contains("另一个任务") }
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertEqual(phone.stops, 1)
        call.end()
    }

    func testOrdinaryConversationNeverBecomesAPhoneTaskAndRetainsContext() async {
        let (call, audio, phone, model) = await rig()
        audio.say("你有什么功能？")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .reply, reply: "我可以和你聊天，也能按要求操作这台手机。"))
        await ready { call.reply.contains("聊天") }
        XCTAssertTrue(phone.starts.isEmpty)
        audio.finish(); audio.say("那帮我打开设置查看电池")
        await ready { model.pending.count == 1 }
        XCTAssertTrue(model.requests.last!.contains { $0.text == "你有什么功能？" })
        model.resolve(.init(action: .run, reply: "准备查看", goal: "打开设置查看电池电量"))
        await ready { phone.starts.count == 1 }
        XCTAssertEqual(phone.starts, ["打开设置查看电池电量"])
        call.end()
    }

    func testSpeakingOverModelDropsLateTaskDecision() async {
        let (call, audio, phone, model) = await rig()
        audio.say("打开设置")
        await ready { model.pending.count == 1 }
        audio.onPartial?("等一下")
        model.resolve(.init(action: .run, reply: "准备打开", goal: "打开设置"))
        await Task.yield()
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertFalse(call.thinking)
        XCTAssertEqual(call.status, "正在听你说")
        call.end()
    }

    func testHangupRejectsLateModelResponseAndKeepsExistingPhoneTask() async {
        let (call, audio, phone, model) = await rig()
        phone.update { $0.busy = true; $0.task = .init(id: "existing", goal: "测试任务", state: .running) }
        audio.say("换成打开设置")
        await ready { model.pending.count == 1 }
        call.end()
        model.resolve(.init(action: .replace, reply: "准备换任务", goal: "打开设置"))
        await Task.yield()
        XCTAssertEqual(phone.stops, 0)
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertTrue(phone.state.busy)
        XCTAssertTrue(phone.disconnected)
    }

    func testStopWorksWithoutModelAndDoesNotRepeatOnTaskUpdates() async {
        let (call, audio, phone, model) = await rig()
        phone.update { $0.busy = true; $0.task = .init(id: "one", goal: "测试", state: .running) }
        audio.say("停止！")
        await ready { phone.stops == 1 }
        XCTAssertTrue(model.requests.isEmpty)
        phone.onChange?(); phone.onChange?()
        audio.finish()
        XCTAssertEqual(phone.stops, 1)
        XCTAssertTrue(phone.starts.isEmpty)
        call.end()
    }

    func testTaskProgressAnswersFromCurrentEvidenceInsteadOfModelClaims() async {
        let (call, audio, phone, model) = await rig()
        phone.update { $0.busy = true; $0.task = .init(id: "one", goal: "打开设置", state: .running, phase: "assist") }
        audio.say("做到哪了？")
        XCTAssertTrue(call.reply.contains("打开设置"))
        XCTAssertFalse(call.reply.contains("任务完成"))
        XCTAssertTrue(model.requests.isEmpty)
        audio.finish()
        audio.say("刚才那个任务结果如何")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .status, reply: "全部完成了"))
        await ready { !call.thinking }
        XCTAssertFalse(call.reply.contains("全部完成了"))
        call.end()
    }

    func testExplicitReplacementWaitsForCancellationBeforeStarting() async {
        let (call, audio, phone, model) = await rig()
        phone.completesStop = false
        phone.update { $0.busy = true; $0.task = .init(id: "old", goal: "旧任务", state: .running) }
        audio.say("停止当前任务，改为打开设置")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .replace, reply: "改做设置", goal: "打开设置"))
        await ready { phone.stops == 1 }
        XCTAssertTrue(phone.starts.isEmpty)
        phone.update { $0.busy = false; $0.task?.state = .cancelled }
        await ready { phone.starts.count == 1 }
        XCTAssertEqual(phone.starts, ["打开设置"])
        call.end()
    }

    func testReplacementCannotStopTaskThatChangedDuringReasoning() async {
        let (call, audio, phone, model) = await rig()
        phone.update { $0.busy = true; $0.task = .init(id: "old", goal: "旧任务", state: .running) }
        audio.say("停止当前任务，改为打开设置")
        await ready { model.pending.count == 1 }
        phone.update { $0.task = .init(id: "another", goal: "另一个任务", state: .running) }
        model.resolve(.init(action: .replace, reply: "改做设置", goal: "打开设置"))
        await ready { call.reply.contains("状态已变化") }
        XCTAssertEqual(phone.stops, 0)
        XCTAssertTrue(phone.starts.isEmpty)
        call.end()
    }

    func testConfirmationCannotBeApprovedByVoiceModel() async {
        let (call, audio, phone, model) = await rig()
        phone.update { $0.busy = true; $0.task = .init(id: "one", goal: "测试", state: .needsConfirm,
                                                    confirm: .init(action: "CLICK", target: "发送")) }
        audio.finish()
        audio.say("确认执行")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .run, reply: "我已确认", goal: "发送"))
        await ready { !call.thinking }
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertEqual(phone.stops, 0)
        XCTAssertNotEqual(call.reply, "我已确认")
        call.end()
    }

    func testModelFailurePreservesCallAndDoesNotSubmitUtteranceAsFallback() async {
        let (call, audio, phone, model) = await rig()
        audio.say("你好")
        await ready { model.pending.count == 1 }
        model.fail()
        await ready { call.error != nil }
        XCTAssertTrue(call.active)
        XCTAssertTrue(phone.starts.isEmpty)
        audio.finish()
        audio.say("再试一次")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .reply, reply: "你好。"))
        await ready { call.reply == "你好。" }
        call.end()
    }

    func testAudioInterruptionDropsPendingInstructionAndCanResume() async {
        let (call, audio, phone, model) = await rig()
        audio.say("打开设置")
        await ready { model.pending.count == 1 }
        audio.onStatus?("系统暂停", true)
        model.resolve(.init(action: .run, reply: "准备打开", goal: "打开设置"))
        await Task.yield()
        XCTAssertTrue(call.active)
        XCTAssertTrue(call.paused)
        XCTAssertTrue(phone.starts.isEmpty)
        audio.onStatus?("正在听", false)
        XCTAssertFalse(call.paused)
        call.end()
    }

    func testDisconnectBlocksTaskButLeavesConversationAvailable() async {
        let (call, audio, phone, model) = await rig()
        audio.say("打开设置")
        await ready { model.pending.count == 1 }
        phone.update { $0.connected = false }
        model.resolve(.init(action: .run, reply: "准备打开", goal: "打开设置"))
        await ready { !call.thinking }
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertTrue(call.active)
        call.end()
    }

    func testTargetChangeEndsCallAndInvalidatesLateReply() async {
        let (call, audio, phone, model) = await rig()
        audio.say("打开设置")
        await ready { model.pending.count == 1 }
        phone.update { $0.valid = false }
        XCTAssertFalse(call.active)
        model.resolve(.init(action: .run, reply: "准备打开", goal: "打开设置"))
        await Task.yield()
        XCTAssertTrue(phone.starts.isEmpty)
    }

    func testCompletionAnnouncementWaitsUntilUserHasFinishedSpeaking() async {
        let (call, audio, phone, model) = await rig()
        audio.onPartial?("你能")
        let count = audio.spoken.count
        phone.update { $0.task = .init(id: "one", goal: "测试", state: .done, summary: "电量87%") }
        XCTAssertEqual(audio.spoken.count, count)
        audio.onFinal?("你能讲个笑话吗")
        await ready { model.pending.count == 1 }
        model.resolve(.init(action: .reply, reply: "一个简短笑话。"))
        await ready { call.reply.contains("笑话") }
        audio.finish()
        XCTAssertTrue(call.reply.contains("87%"))
        let finished = audio.spoken.count
        phone.onChange?(); audio.finish()
        XCTAssertEqual(audio.spoken.count, finished)
        call.end()
    }

    func testMuteCancelsPendingDecisionAndDoesNotAcceptNewSpeech() async {
        let (call, audio, phone, model) = await rig()
        audio.say("打开设置")
        await ready { model.pending.count == 1 }
        call.toggleMute()
        model.resolve(.init(action: .run, reply: "准备打开", goal: "打开设置"))
        await Task.yield()
        audio.say("打开Safari")
        XCTAssertEqual(model.requests.count, 1)
        XCTAssertTrue(phone.starts.isEmpty)
        XCTAssertTrue(audio.muted)
        call.end()
    }
}

final class AssistantVoiceEchoTests: XCTestCase {
    func testFullPlaybackEchoIsFilteredButShortStopCommandsArePreserved() {
        XCTAssertTrue(AssistantVoiceEchoGate.isEcho("收到，开始操作：打开设置。", playback: "收到，开始操作：打开设置。"))
        XCTAssertFalse(AssistantVoiceEchoGate.isEcho("停止", playback: "可以说停止任务。"))
        XCTAssertFalse(AssistantVoiceEchoGate.isEcho("停止任务", playback: "可以说停止任务。"))
        XCTAssertFalse(AssistantVoiceEchoGate.isEcho("改为打开浏览器", playback: "收到，开始操作：打开设置。"))
    }
}
