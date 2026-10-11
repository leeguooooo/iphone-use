import XCTest
@testable import IPhoneUseRemote

@MainActor
final class AgentStartFailureTests: XCTestCase {
    func testDraftSurvivesRelaunchAndStaysWithItsDevice() {
        let suite = "agent-drafts-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let first = UUID(), second = UUID()
        let agent = AgentController(defaults: defaults)
        agent.targetID = first
        agent.draft = "看看电量"
        agent.targetID = second
        XCTAssertEqual(agent.draft, "")
        agent.draft = "打开设置"
        let relaunched = AgentController(defaults: defaults)
        relaunched.targetID = first
        XCTAssertEqual(relaunched.draft, "看看电量")
        relaunched.targetID = second
        XCTAssertEqual(relaunched.draft, "打开设置")
        relaunched.forget(device: first)
        relaunched.targetID = first
        XCTAssertEqual(relaunched.draft, "")
    }
    func testMissingTargetDoesNotAcceptGoal() async {
        let defaults = UserDefaults(suiteName: "agent-start-\(UUID().uuidString)")!
        let agent = AgentController(defaults: defaults)
        let accepted = await agent.start(goal: "看看电量")
        XCTAssertFalse(accepted)
        XCTAssertNil(agent.pendingGoal)
        XCTAssertNil(agent.live)
        XCTAssertNotNil(agent.error)
    }

    func testThisPhoneTransportFailureExplainsTheLocalService() {
        let error = DaemonError.transport(.cannotConnectToHost, "private details")
        let message = AgentController.describe(error, onThisPhone: true)
        XCTAssertEqual(message, String(localized: "本机执行服务未连接。请从 Mac 启动这台 iPhone 的执行服务，再重试。"))
        XCTAssertFalse(message.contains("private details"))
        XCTAssertEqual(AgentController.describe(error, onThisPhone: false), error.localizedDescription)
    }

    func testLocalServiceFailureDoesNotSayTheMacIsUnreachable() {
        let local = DeviceSession(record: DeviceRecord(address: "http://127.0.0.1:8787"))
        local.phase = .failed(.unreachable(.refused))
        XCTAssertEqual(local.shortState, String(localized: "本机执行服务未连接"))
        let remote = DeviceSession(record: DeviceRecord(address: "http://192.168.1.20:44321"))
        remote.phase = .failed(.unreachable(.refused))
        XCTAssertEqual(remote.shortState, String(localized: "连不上 Mac"))
    }
}

final class AgentTaskDecodingTests: XCTestCase {
    private func decode(_ json: String) throws -> AgentTaskSnapshot {
        try JSONDecoder().decode(AgentTaskSnapshot.self, from: Data(json.utf8))
    }

    /// The example in docs/agent-task.md, field for field.
    func testDecodesTheDocumentedTask() throws {
        let task = try decode("""
        {"id": "t-1", "goal": "打开设置查看电池电量", "status": "needs_confirm", "phase": "jev",
         "flow": {"id": "settings/battery", "outcome": "ok"},
         "steps": [{"n": 1, "action": "CLICK", "target": "电池", "text": null, "app": "设置",
                    "ok": true, "error": null, "page_changed": true, "by": "jev", "ms": 640}],
         "confirm": {"action": "CLICK", "target": "确认支付", "reason": "pays ¥55.51", "app": "拼多多"},
         "summary": null, "error": null, "started_at_ms": 1000, "elapsed_ms": 45000,
         "models_used": {"jev": "jev-latest", "assist": "claude-sonnet-5-5"}}
        """)
        XCTAssertEqual(task.id, "t-1")
        XCTAssertEqual(task.state, .needsConfirm)
        XCTAssertEqual(task.phase, "jev")
        XCTAssertEqual(task.flow, .init(id: "settings/battery", outcome: "ok"))
        XCTAssertEqual(task.steps, [AgentStep(n: 1, action: "CLICK", target: "电池", app: "设置", ok: true,
                                              pageChanged: true, by: "jev", ms: 640)])
        XCTAssertEqual(task.confirm, AgentConfirm(action: "CLICK", target: "确认支付", reason: "pays ¥55.51", app: "拼多多"))
        XCTAssertEqual(task.pendingConfirm?.target, "确认支付")
        XCTAssertNil(task.summary)
        XCTAssertEqual(task.elapsedMs, 45000)
        XCTAssertEqual(task.modelsUsed["assist"], "claude-sonnet-5-5")
    }

    func testEveryStatus() throws {
        let expected: [String: AgentTaskState] = [
            "running": .running, "needs_confirm": .needsConfirm, "done": .done, "blocked": .blocked,
            "failed": .failed, "cancelled": .cancelled, "max_steps": .maxSteps,
        ]
        for (raw, state) in expected {
            let task = try decode(#"{"id": "t", "status": "\#(raw)"}"#)
            XCTAssertEqual(task.state, state, raw)
            XCTAssertEqual(task.state.isTerminal, !["running", "needs_confirm"].contains(raw), raw)
        }
    }

    /// A status from a newer daemon is followed, not taken for the end.
    func testUnknownStatusKeepsFollowing() throws {
        let task = try decode(#"{"id": "t", "status": "paused_for_unlock"}"#)
        XCTAssertEqual(task.state, .other("paused_for_unlock"))
        XCTAssertFalse(task.state.isTerminal)
        XCTAssertEqual(task.state.title, "paused_for_unlock")
    }

    /// Nulls and missing fields everywhere (a `/agent/tasks` row has no steps).
    func testNullAndMissingFields() throws {
        let task = try decode("""
        {"id": "t-2", "goal": null, "status": "done", "phase": null, "flow": null, "confirm": null,
         "summary": "电池电量 87%。", "error": null, "started_at_ms": null, "elapsed_ms": null,
         "models_used": {"jev": null, "assist": "x"}}
        """)
        XCTAssertEqual(task.goal, "")
        XCTAssertEqual(task.state, .done)
        XCTAssertNil(task.flow)
        XCTAssertNil(task.confirm)
        XCTAssertNil(task.pendingConfirm)
        XCTAssertEqual(task.steps, [])
        XCTAssertEqual(task.elapsedMs, 0)
        XCTAssertEqual(task.modelsUsed, ["assist": "x"])
        let bare = try decode(#"{"id": "t-3"}"#)
        XCTAssertEqual(bare.state, .running)
    }

    /// A confirm left over on a task that moved on is not a question.
    func testConfirmOnlyCountsWhileWaiting() throws {
        let task = try decode(#"{"id": "t", "status": "blocked", "confirm": {"action": "CLICK"}}"#)
        XCTAssertNil(task.pendingConfirm)
    }

    func testStepWithoutOkIsJudgedByError() throws {
        let step = try JSONDecoder().decode(AgentStep.self, from: Data(#"{"n": 2, "action": "TYPE", "error": "no_field"}"#.utf8))
        XCTAssertFalse(step.ok)
        XCTAssertNil(step.target)
    }

    func testRequestEncoding() throws {
        let request = AgentTaskRequest(goal: "看电量", maxSteps: 20, models: nil)
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any]
        XCTAssertEqual(json?["goal"] as? String, "看电量")
        XCTAssertEqual(json?["max_steps"] as? Int, 20)
        XCTAssertEqual(json?["confirm"] as? String, "risky")
        XCTAssertNil(json?["models"])
        XCTAssertNil(json?["app"])
    }

    func testRefusalsMapToErrors() {
        func check(_ status: Int, _ body: String) -> Error? {
            let response = HTTPURLResponse(url: URL(string: "http://x")!, statusCode: status, httpVersion: nil, headerFields: nil)!
            do { try DaemonClient.checkAgent(response, Data(body.utf8)); return nil } catch { return error }
        }
        XCTAssertNil(check(202, #"{"id": "t", "status": "running"}"#))
        XCTAssertEqual(check(409, #"{"error": "task_running", "id": "t-9"}"#) as? AgentTaskError, .taskRunning(id: "t-9"))
        XCTAssertEqual(check(409, #"{"error": "phone_owned", "owner": "claude"}"#) as? AgentTaskError, .phoneOwned(by: "claude"))
        XCTAssertEqual(check(404, "{}") as? AgentTaskError, .unsupported)
        XCTAssertEqual(check(422, #"{"error": "invalid_flow", "message": "bad step 3"}"#) as? AgentTaskError, .refused("bad step 3"))
        if case .sessionExpired? = check(401, "") as? DaemonError {} else { XCTFail("401 is an expired session") }
    }
}

final class AgentTaskTrackerTests: XCTestCase {
    private func step(_ n: Int, ok: Bool = true) -> AgentStep {
        AgentStep(n: n, action: "CLICK", target: "t\(n)", ok: ok, by: "jev")
    }

    func testMergesSinceAnswersByStepNumber() {
        var tracker = AgentTaskTracker(.init(id: "t", goal: "g", state: .running))
        XCTAssertEqual(tracker.lastStep, 0)
        XCTAssertTrue(tracker.apply(.init(id: "t", goal: "g", state: .running, steps: [step(1), step(2)])))
        XCTAssertEqual(tracker.lastStep, 2)
        // `?since=2` answers only step 3.
        XCTAssertTrue(tracker.apply(.init(id: "t", goal: "g", state: .running, steps: [step(3)])))
        XCTAssertEqual(tracker.task.steps.map(\.n), [1, 2, 3])
        // A repeated step replaces the one it repeats.
        tracker.apply(.init(id: "t", goal: "g", state: .running, steps: [step(3, ok: false)]))
        XCTAssertEqual(tracker.task.steps.map(\.n), [1, 2, 3])
        XCTAssertFalse(tracker.task.steps[2].ok)
    }

    func testStopsOnTerminalAndKeepsGoal() {
        var tracker = AgentTaskTracker(.init(id: "t", goal: "看电量", state: .running))
        XCTAssertTrue(tracker.apply(.init(id: "t", goal: "", state: .needsConfirm)))
        XCTAssertEqual(tracker.task.goal, "看电量")
        XCTAssertFalse(tracker.apply(.init(id: "t", goal: "看电量", state: .done, summary: "87%")))
        XCTAssertEqual(tracker.task.summary, "87%")
    }

    func testIgnoresAnotherTasksAnswer() {
        var tracker = AgentTaskTracker(.init(id: "t", goal: "g", state: .running))
        XCTAssertTrue(tracker.apply(.init(id: "other", goal: "x", state: .done)))
        XCTAssertEqual(tracker.task.id, "t")
        XCTAssertEqual(tracker.task.state, .running)
    }
}

/// A daemon that answers a script, one entry per read.
private actor ScriptedAPI: AgentTaskAPI {
    enum Answer {
        case task(AgentTaskSnapshot)
        case fail(Error)
    }

    private var script: [Answer]
    private(set) var sinces: [Int] = []

    init(_ script: [Answer]) {
        self.script = script
    }

    func task(id: String, since: Int) async throws -> AgentTaskSnapshot {
        sinces.append(since)
        guard !script.isEmpty else { throw DaemonError.unreachable("script ended") }
        switch script.removeFirst() {
        case .task(let task): return task
        case .fail(let error): throw error
        }
    }

    func startTask(_ request: AgentTaskRequest, returnBundle: String?) async throws -> String { "t" }
    func answer(task id: String, approve: Bool) async throws {}
    func cancelTask(id: String) async throws {}
    func recentTasks() async throws -> [AgentTaskSnapshot] { [] }
}

@MainActor
final class AgentTaskPollerTests: XCTestCase {
    private func task(_ state: AgentTaskState, steps: [Int] = []) -> AgentTaskSnapshot {
        .init(id: "t", goal: "g", state: state, steps: steps.map { AgentStep(n: $0, action: "CLICK") })
    }

    private let start = AgentTaskTracker(.init(id: "t", goal: "g", state: .running))

    func testPollsWithSinceUntilTerminal() async {
        let api = ScriptedAPI([.task(task(.running, steps: [1, 2])), .task(task(.needsConfirm, steps: [3])),
                               .task(task(.running)), .task(task(.done, steps: [4])),
                               .task(task(.done))])
        var updates: [AgentTaskTracker] = []
        let end = await AgentTaskPoller.follow(start, api: api, sleep: { _ in }, onUpdate: {
            updates.append($0)
        })
        XCTAssertEqual(end, .finished)
        // Stops at the first terminal answer: the fifth entry is never read.
        let sinces = await api.sinces
        XCTAssertEqual(sinces, [0, 2, 3, 3])
        XCTAssertEqual(updates.count, 4)
        XCTAssertEqual(updates[1].task.state, .needsConfirm)
        XCTAssertEqual(updates.last?.task.steps.map(\.n), [1, 2, 3, 4])
    }

    func testSleepsAboutOneSecondBetweenReads() async {
        let api = ScriptedAPI([.task(task(.running)), .task(task(.done))])
        let slept = SleepLog()
        _ = await AgentTaskPoller.follow(start, api: api, sleep: { await slept.add($0) }, onUpdate: { _ in })
        let intervals = await slept.intervals
        XCTAssertEqual(intervals, [.seconds(1)])
    }

    func testGivesUpAfterARunOfFailures() async {
        let failures = Array(repeating: ScriptedAPI.Answer.fail(DaemonError.unreachable("down")),
                             count: AgentTaskPoller.maxFailures + 5)
        let api = ScriptedAPI(failures)
        let end = await AgentTaskPoller.follow(start, api: api, sleep: { _ in }, onUpdate: { _ in })
        guard case .lostLink = end else { return XCTFail("expected lostLink, got \(end)") }
        let reads = await api.sinces.count
        XCTAssertEqual(reads, AgentTaskPoller.maxFailures)
    }

    func testAFailureInBetweenDoesNotStopIt() async {
        let api = ScriptedAPI([.task(task(.running, steps: [1])), .fail(DaemonError.unreachable("blip")),
                               .task(task(.cancelled))])
        let end = await AgentTaskPoller.follow(start, api: api, sleep: { _ in }, onUpdate: { _ in })
        XCTAssertEqual(end, .finished)
    }

    func testRenewsAnExpiredSessionAndGoesOn() async {
        let api = ScriptedAPI([.fail(DaemonError.sessionExpired), .task(task(.done))])
        var renewed = 0
        let end = await AgentTaskPoller.follow(start, api: api, sleep: { _ in },
                                               relogin: { renewed += 1; return true }, onUpdate: { _ in })
        XCTAssertEqual(end, .finished)
        XCTAssertEqual(renewed, 1)
    }

    func testAForgottenTaskEndsFollowing() async {
        let api = ScriptedAPI([.fail(AgentTaskError.notFound)])
        let end = await AgentTaskPoller.follow(start, api: api, sleep: { _ in }, onUpdate: { _ in })
        XCTAssertEqual(end, .forgotten)
    }

    func testCancellingStops() async {
        let api = ScriptedAPI(Array(repeating: .task(task(.running)), count: 100))
        let follow = Task { @MainActor in
            await AgentTaskPoller.follow(self.start, api: api, sleep: { _ in try await Task.sleep(for: .milliseconds(5)) },
                                         onUpdate: { _ in })
        }
        try? await Task.sleep(for: .milliseconds(30))
        follow.cancel()
        let end = await follow.value
        XCTAssertEqual(end, .stopped)
    }
}

private actor SleepLog {
    private(set) var intervals: [TimeInterval] = []
    func add(_ duration: TimeInterval) { intervals.append(duration) }
}

final class ModelsPayloadTests: XCTestCase {
    private func json(_ payload: ModelsPayload?) throws -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return String(decoding: try encoder.encode(payload), as: UTF8.self)
    }

    /// Out of the box: public Zen, followed by Kilo without a key.
    func testDefaultIsTheFreeTier() throws {
        let payload = ModelSettings().payload { _ in nil }
        XCTAssertEqual(try json(payload),
                       #"{"assist":[{"base_url":"https://opencode.ai/zen/v1","format":"openai","key":"public","model":"big-pickle"},{"base_url":"https://api.kilo.ai/api/openrouter","format":"openai","key":"","model":"free"}]}"#)
    }

    /// Everything set: the shape of docs/agent-task.md › Models, assist in the person's order.
    func testFullShapeMatchesTheContract() throws {
        var settings = ModelSettings()
        let anthropic = ModelProvider(kind: .anthropic)
        let free = ModelProvider(kind: .free, model: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free")
        settings.assist = [anthropic, free]
        let keys = [anthropic.keyAccount: "sk-ant", ModelSettings.jevKeyAccount: "ts-key",
                    ModelSettings.textKeyAccount: " or-key "]
        let payload = settings.payload { keys[$0] }
        XCTAssertEqual(try json(payload), """
        {"assist":[{"base_url":"https://api.anthropic.com","format":"anthropic","key":"sk-ant","model":"claude-sonnet-5-5"},\
        {"base_url":"https://api.kilo.ai/api/openrouter","format":"openai","key":"","model":"nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free"}],\
        "jev":{"base_url":"https://api.typesafe.ai/v1","key":"ts-key","model":"jev-latest"},\
        "text":{"base_url":"https://openrouter.ai/api/v1","key":"or-key","model":"inception/mercury-2.5"}}
        """)
    }

    func testProvidersWithoutAKeyOrSwitchedOffAreLeftOut() throws {
        var settings = ModelSettings()
        var off = ModelProvider(kind: .free)
        off.enabled = false
        let noKey = ModelProvider(kind: .openRouter)
        var noModel = ModelProvider(kind: .custom, baseURL: "https://llm.example/v1")
        noModel.model = " "
        let custom = ModelProvider(kind: .custom, baseURL: "https://llm.example/v1", model: "m1")
        settings.assist = [off, noKey, noModel, custom]
        let payload = settings.payload { $0 == custom.keyAccount || $0 == noModel.keyAccount ? "k" : nil }
        XCTAssertEqual(payload?.assist, [.init(format: "openai", baseURL: "https://llm.example/v1", key: "k", model: "m1")])
        XCTAssertNil(payload?.jev)
        XCTAssertNil(payload?.text)
    }

    /// Nothing usable, or sending turned off: no `models`, so the daemon
    /// uses its own (a Mac's environment).
    func testNothingUsableSendsNoModels() {
        var settings = ModelSettings()
        settings.assist = [ModelProvider(kind: .openRouter)]
        XCTAssertNil(settings.payload { _ in nil })
        var off = ModelSettings()
        off.sendModels = false
        XCTAssertNil(off.payload { _ in "key" })
    }

    func testTextModelBorrowsTheOpenRouterKey() {
        var settings = ModelSettings()
        let openRouter = ModelProvider(kind: .openRouter)
        settings.assist = [openRouter]
        let payload = settings.payload { $0 == openRouter.keyAccount ? "or" : nil }
        XCTAssertEqual(payload?.text, .init(baseURL: "https://openrouter.ai/api/v1", key: "or", model: "inception/mercury-2.5"))
        // Another text endpoint does not get OpenRouter's key.
        settings.textBaseURL = "https://other.example/v1"
        XCTAssertNil(settings.payload { $0 == openRouter.keyAccount ? "or" : nil }?.text)
    }

    func testSettingsRoundTripWithoutKeys() throws {
        let suite = "test.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        // The defaults are saved on first load: the same list comes back.
        let first = ModelSettingsStore.load(from: defaults)
        XCTAssertEqual(ModelSettingsStore.load(from: defaults), first)
        XCTAssertEqual(first.assist.map(\.kind), [.zen, .free])
        var settings = first
        settings.assist.append(ModelProvider(kind: .anthropic))
        settings.jevModel = "jev-2"
        ModelSettingsStore.save(settings, to: defaults)
        XCTAssertEqual(ModelSettingsStore.load(from: defaults), settings)
        let stored = String(decoding: defaults.data(forKey: ModelSettingsStore.key) ?? Data(), as: UTF8.self)
        XCTAssertFalse(stored.contains("key\""), "no key field is ever written to UserDefaults")
    }

    func testZenUsesPublicWithoutReadingAPersonalKey() throws {
        var settings = ModelSettings()
        settings.assist = [ModelProvider(kind: .zen)]
        let payload = settings.payload { account in
            XCTAssertNotEqual(account, settings.assist[0].keyAccount)
            return nil
        }
        XCTAssertEqual(try json(payload), #"{"assist":[{"base_url":"https://opencode.ai/zen/v1","format":"openai","key":"public","model":"big-pickle"}]}"#)
        XCTAssertFalse(settings.assist[0].needsKey)
    }

    func testUpgradeAddsZenOnceAndPreservesDeletion() throws {
        let suite = "test.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        var old = ModelSettings()
        old.assist = [ModelProvider(kind: .free)]
        ModelSettingsStore.save(old, to: defaults)
        let upgraded = ModelSettingsStore.load(from: defaults)
        XCTAssertEqual(upgraded.assist[1], old.assist[0])
        XCTAssertEqual(upgraded.assist.first?.kind, .zen)
        XCTAssertEqual(upgraded.assist.first?.enabled, true)
        ModelSettingsStore.save(old, to: defaults)
        XCTAssertEqual(ModelSettingsStore.load(from: defaults), old)
    }

    func testUpgradeLeavesCustomProviderOrderUntouched() {
        let suite = "test.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        var chosen = ModelSettings()
        chosen.assist = [ModelProvider(kind: .anthropic), ModelProvider(kind: .custom, baseURL: "https://custom.example/v1", model: "chosen")]
        ModelSettingsStore.save(chosen, to: defaults)
        XCTAssertEqual(ModelSettingsStore.load(from: defaults), chosen)
    }
}

final class ThisPhoneTests: XCTestCase {
    func testThisPhoneRecord() {
        let link = PairLink.parse("iphoneuse://pair?u=http%3A%2F%2F127.0.0.1%3A8787&c=abc")
        XCTAssertEqual(link?.base.absoluteString, "http://127.0.0.1:8787")
        let record = DeviceRecord(address: link!.base.absoluteString)
        XCTAssertTrue(record.isThisPhone)
        XCTAssertFalse(record.showInGrid, "a tile of its own screen is not shown")
        XCTAssertEqual(record.displayName, String(localized: "本机"))
        let mac = DeviceRecord(address: "http://127.0.0.1:47001")
        XCTAssertFalse(mac.isThisPhone)
        XCTAssertTrue(mac.showInGrid)
    }
}
