import XCTest
import CryptoKit
import UIKit
@testable import IPhoneUseRemote

@MainActor
private final class FixturePlanner: AssistantReasoning {
    var calls = 0
    var phases: [AssistantPhase] = [.init(tool: .browser, goal: "Read fixture")]
    func plan(goal: String, phoneAvailable: Bool, sites: [BrowserSiteAdapter], models: [ModelsPayload.Assist]) async throws -> AssistantPlan { calls += 1; return AssistantPlan(phases: phases) }
    func summarize(goal: String, results: [String], models: [ModelsPayload.Assist]) async throws -> String { results.joined(separator: "; ") }
}
@MainActor
private final class FixtureReader: BrowserReasoning {
    var calls = 0
    var firstDecision: (() async -> Void)?
    func decide(goal: String, page: BrowserPage?, steps: [BrowserAgentStep], models: [ModelsPayload.Assist]) async throws -> BrowserDecision {
        calls += 1; if calls == 1 { await firstDecision?() }; return .init(action: .done, explanation: "Observed synthetic fixture")
    }
}

@MainActor
final class UnifiedAssistantTests: XCTestCase {
    private var files: [URL] = []
    private var suites: [String] = []
    override func tearDown() { for file in files { try? FileManager.default.removeItem(at: file) }; for suite in suites { UserDefaults.standard.removePersistentDomain(forName: suite) } }
    private func defaults() -> UserDefaults { let suite = "ipu-fixture-" + UUID().uuidString; suites.append(suite); return UserDefaults(suiteName: suite)! }
    private func file() -> URL { let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".enc"); files.append(file); return file }
    private func workspace() async throws -> (BrowserWorkspace, FixtureReader) {
        let reader = FixtureReader(), library = BrowserLibrary(file: file(), key: Data(repeating: 3, count: 32))
        let workspace = BrowserWorkspace(library: library, reasoning: reader)
        for _ in 0..<100 where workspace.switching { try await Task.sleep(for: .milliseconds(20)) }
        workspace.browser.loading = true; workspace.browser.hasPage = true
        workspace.browser.webView.loadHTMLString("<h1>Synthetic fixture</h1>", baseURL: URL(string: "https://example.com"))
        try await workspace.browser.waitUntilReady()
        return (workspace, reader)
    }
    private func wait(_ condition: () -> Bool) async throws {
        for _ in 0..<150 { if condition() { return }; try await Task.sleep(for: .milliseconds(50)) }
        XCTFail("Unified task did not settle")
    }
    func testSequentialPhasesAndEncryptedTaskRecovery() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner()
        planner.phases = [.init(tool: .browser, goal: "Read first"), .init(tool: .browser, goal: "Compare previous evidence")]
        let (workspace, reader) = try await workspace(), archive = file(), key = Data(repeating: 4, count: 32)
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: archive, key: key, reasoning: planner)
        XCTAssertTrue(assistant.start("Synthetic multi-tool conversation"))
        XCTAssertFalse(assistant.start("Must not overlap"))
        try await wait { assistant.task?.state == .done || assistant.task?.state == .failed }
        XCTAssertEqual(assistant.task?.state, .done, assistant.message ?? "")
        XCTAssertEqual(reader.calls, 2); XCTAssertEqual(planner.calls, 1)
        XCTAssertEqual(assistant.task?.phases.compactMap(\.result).count, 2)
        XCTAssertFalse(workspace.lockedForTask)
        XCTAssertFalse(String(decoding: try Data(contentsOf: archive), as: UTF8.self).contains("Observed synthetic"))
        let restored = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: archive, key: key, reasoning: planner)
        XCTAssertEqual(restored.task?.state, .done); XCTAssertEqual(planner.calls, 1)
    }
    func testForegroundWhilePausingDoesNotStrandBrowserTask() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner()
        let (workspace, reader) = try await workspace()
        reader.firstDecision = {
            NotificationCenter.default.post(name: UIApplication.willResignActiveNotification, object: nil)
            NotificationCenter.default.post(name: UIApplication.didBecomeActiveNotification, object: nil)
            try? await Task.sleep(for: .milliseconds(200))
        }
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults,
                                         file: file(), key: Data(repeating: 8, count: 32), reasoning: planner)
        assistant.mode = .browser
        XCTAssertTrue(assistant.start("Read synthetic fixture after returning to foreground"))
        try await wait { assistant.task?.state == .done || assistant.task?.state == .failed }
        XCTAssertEqual(assistant.task?.state, .done, assistant.message ?? "")
        XCTAssertEqual(reader.calls, 2)
        XCTAssertFalse(assistant.task?.automaticPause ?? true)
    }
    func testUnknownWriteRecoveryRequiresManualVerification() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner(), (workspace, reader) = try await workspace()
        let archive = file(), key = Data(repeating: 5, count: 32)
        var task = UnifiedTask(goal: "Synthetic uncertain write", mode: .automatic, profileID: workspace.library.activeID)
        task.phases = [.init(tool: .browser, goal: "Synthetic write")]; task.state = .running; task.inFlightRisky = true; task.inFlightAction = "click"
        struct Saved: Encodable { var task: UnifiedTask; var history: [UnifiedTask] = [] }
        let ciphertext = try AES.GCM.seal(JSONEncoder().encode(Saved(task: task)), using: SymmetricKey(data: key)).combined!
        try ciphertext.write(to: archive)
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: archive, key: key, reasoning: planner)
        XCTAssertEqual(assistant.task?.state, .needsReview)
        assistant.resume(); try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(reader.calls, 0); XCTAssertEqual(planner.calls, 0)
        XCTAssertFalse(assistant.start("Do not overwrite uncertain work"))
    }
    func testDesktopResumeRejectsAChangedComputerGrant() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner(), (workspace, reader) = try await workspace()
        let archive = file(), key = Data(repeating: 5, count: 32)
        let oldScope = String(repeating: "a", count: 24)
        let source = "async function(args) { return {}; }"
        let hash = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
        let adapter = BrowserSiteAdapter(spec: "example/read", meta: ["domain": .string("example.com"), "readOnly": .bool(true)], source: source, sha256: hash, origin: "synthetic", pairedScope: oldScope)
        try workspace.library.installCatalog(.init(adapters: [adapter]))
        try workspace.library.setChrome(.init(endpoint: URL(string: "http://127.0.0.1:1")!, scope: String(repeating: "b", count: 24), key: Data(repeating: 2, count: 32)))
        var task = UnifiedTask(goal: "Read scoped fixture", mode: .automatic, profileID: workspace.library.activeID)
        var phase = AssistantPhase(tool: .desktop, goal: "Read fixture", siteSpec: adapter.spec)
        phase.sourceHash = adapter.fingerprint; phase.desktopScope = oldScope; phase.desktopEndpoint = "http://127.0.0.1:1"
        task.phases = [phase]; task.state = .paused
        struct Saved: Encodable { var task: UnifiedTask; var history: [UnifiedTask] = [] }
        try AES.GCM.seal(JSONEncoder().encode(Saved(task: task)), using: SymmetricKey(data: key)).combined!.write(to: archive)
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: archive, key: key, reasoning: planner)
        assistant.resume(); try await wait { assistant.task?.state == .failed }
        XCTAssertEqual(assistant.task?.state, .failed); XCTAssertEqual(reader.calls, 0); XCTAssertEqual(planner.calls, 0)
        XCTAssertTrue(assistant.message?.contains("不执行电脑") == true)
    }
    func testStoppedUnknownWriteCannotResumeRemainingPhases() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner(), (workspace, reader) = try await workspace()
        let archive = file(), key = Data(repeating: 5, count: 32)
        var task = UnifiedTask(goal: "Synthetic stopped write", mode: .automatic, profileID: workspace.library.activeID)
        task.phases = [.init(tool: .browser, goal: "Synthetic write"), .init(tool: .browser, goal: "Must never run")]
        task.state = .needsReview; task.inFlightRisky = true; task.stopRequested = true
        struct Saved: Encodable { var task: UnifiedTask; var history: [UnifiedTask] = [] }
        try AES.GCM.seal(JSONEncoder().encode(Saved(task: task)), using: SymmetricKey(data: key)).combined!.write(to: archive)
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: archive, key: key, reasoning: planner)
        assistant.markVerifiedComplete(); assistant.resume()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(assistant.task?.state, .stopped); XCTAssertEqual(reader.calls, 0); XCTAssertEqual(planner.calls, 0)
    }
    func testUnavailableNativePhoneWaitsWithoutBrowserSubstitution() async throws {
        let defaults = defaults(), app = AppModel(defaults: defaults), planner = FixturePlanner(), (workspace, reader) = try await workspace()
        planner.phases = [.init(tool: .phone, goal: "Read private on-device data")]
        let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: file(), key: Data(repeating: 6, count: 32), reasoning: planner)
        XCTAssertTrue(assistant.start("Read private phone data"))
        try await wait { assistant.task?.state == .waitingForPhone || assistant.task?.state == .failed }
        XCTAssertEqual(assistant.task?.state, .waitingForPhone); XCTAssertEqual(reader.calls, 0)
        await assistant.stop()
    }
}
