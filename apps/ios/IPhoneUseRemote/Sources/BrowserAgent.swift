import Foundation
import Combine

struct BrowserDecision: Codable, Sendable {
    enum Action: String, Codable, Sendable { case open, click, fill, fillProfile, site, select, scroll, back, wait, done, handoff }
    let action: Action
    let explanation: String
    var element: Int?
    var text: String?
    var url: String?
    var requiresConfirmation: Bool?
    var siteSpec: String?
    var siteArguments: [String: JSONValue]?
    var profileField: String?
}

struct BrowserAgentStep: Identifiable, Codable, Sendable {
    let id = UUID()
    let text: String
    var action: String? = nil
    enum CodingKeys: String, CodingKey { case text, action }
}

struct BrowserConversationTurn: Identifiable {
    let id = UUID()
    let goal: String
    let reply: String?
    let steps: [BrowserAgentStep]
}

@MainActor
protocol BrowserCompletionChecking {
    func verifyCompletion(goal: String, page: BrowserPage, steps: [BrowserAgentStep], models: [ModelsPayload.Assist]) async throws -> Bool
}

@MainActor
protocol BrowserReasoning {
    func decide(goal: String, page: BrowserPage?, steps: [BrowserAgentStep], models: [ModelsPayload.Assist]) async throws -> BrowserDecision
}

@MainActor
final class BrowserAgent: ObservableObject {
    enum State: Equatable { case idle, running, confirming, paused, done, failed, stopped }
    @Published private(set) var state = State.idle
    @Published private(set) var goal = ""
    @Published private(set) var steps: [BrowserAgentStep] = []
    @Published private(set) var message: String?
    @Published private(set) var pending: BrowserDecision?
    @Published private(set) var history: [BrowserConversationTurn] = []
    @Published private var pendingPage: BrowserPage?
    @Published private var models: [ModelsPayload.Assist] = []
    @Published private var run: Task<Void, Never>?
    @Published private var generation = UUID()
    private let browser: BrowserSession
    private let reasoning: any BrowserReasoning
    private var pendingBinding: String?
    private weak var library: BrowserLibrary?
    var beforeAction: ((BrowserDecision, Bool) throws -> Void)?
    var afterAction: (([BrowserAgentStep]) throws -> Void)?
    var rejectedAction: (() -> Void)?

    init(browser: BrowserSession, reasoning: any BrowserReasoning = BrowserModelClient(), library: BrowserLibrary? = nil) {
        self.browser = browser
        self.reasoning = reasoning
        self.library = library
    }

    var confirmationDetails: String {
        guard let pending else { return "" }
        let account = library?.active.label ?? "当前账号"
        let host = pendingPage.flatMap { URL(string: $0.url) }?.host ?? browser.webView.url?.host ?? "网页"
        var details = "网站：\(host)\n账号：\(account)"
        if let field = pending.profileField, let value = library?.active.personal[field] {
            details += "\n填写资料：\(field) = \(value)"
        } else if let spec = pending.siteSpec {
            details += "\n网站操作：\(spec)"
            if let arguments = pending.siteArguments, let data = try? JSONEncoder().encode(arguments) { details += "\n参数：" + String(decoding: data, as: UTF8.self) }
        } else if let element = pendingPage?.elements.first(where: { $0.id == pending.element }) { details += "\n控件：\(element.label)" }
        return details
    }
    var active: Bool { state == .running || state == .confirming }

    func start(_ input: String, settings: ModelSettings, resumeSteps: [BrowserAgentStep] = []) {
        guard !active else { return }
        guard library?.pendingSite == nil else { message = "上次网站写入结果未知，请先在账号与网站资料中核对"; return }
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        if !goal.isEmpty && !(state == .failed && goal == text) {
            history.append(.init(goal: goal, reply: message, steps: steps))
            history = Array(history.suffix(20))
        }
        goal = text
        steps = resumeSteps
        message = nil
        pending = nil
        pendingPage = nil
        let configured = settings.payload(key: ModelSettingsStore.secret)?.assist ?? []
        guard !configured.isEmpty else {
            state = .failed
            message = String(localized: "先在设置里启用浏览器要使用的模型")
            return
        }
        models = configured
        launch()
    }

    func pause() {
        guard active else { return }
        invalidate()
        pending = nil
        pendingPage = nil
        state = .paused
        message = String(localized: "已暂停，可以手动操作网页。点继续后重新读取页面。")
    }

    func stop() {
        invalidate()
        pending = nil
        pendingPage = nil
        state = .stopped
        message = String(localized: "任务已停止")
    }

    func resume() {
        guard state == .paused, !models.isEmpty else { return }
        message = nil
        launch()
    }

    func approve() {
        guard state == .confirming, let action = pending, let page = pendingPage else { return }
        guard pendingBinding == binding(for: action) else {
            pending = nil; pendingPage = nil; state = .paused; message = "账号、资料或网站脚本已变化，请重新确认"; return
        }
        pending = nil
        pendingPage = nil
        launch(approved: action, page: page)
    }

    private func invalidate() { generation = UUID(); run?.cancel(); run = nil }

    private func launch(approved: BrowserDecision? = nil, page: BrowserPage? = nil) {
        invalidate()
        let id = generation
        state = .running
        browser.webView.endEditing(true)
        run = Task { [weak self] in await self?.execute(id: id, approved: approved, page: page) }
    }

    private func execute(id: UUID, approved: BrowserDecision?, page: BrowserPage?) async {
        do {
            try check(id)
            // The task composer dismisses its keyboard as this run starts.
            // Observe after the viewport has settled, including on resume.
            try await Task.sleep(for: .milliseconds(400))
            try check(id)
            if let approved {
                do { try await apply(approved, page: page) }
                catch BrowserToolError.stalePage { rejectedAction?(); steps.append(.init(text: String(localized: "页面已变化，原操作没有执行。重新读取。"))) }
            }
            var staleRetries = 0
            for _ in 0..<30 {
                try check(id)
                try await browser.waitUntilReady()
                try check(id)
                let page = browser.hasPage ? try await browser.readPage() : nil
                if let client = reasoning as? BrowserModelClient, let library {
                    client.setContext(sites: library.catalog.matching(browser.webView.url).filter { !$0.desktopOnly },
                        personalFields: Array(library.active.personal.keys).sorted(),
                        secrets: library.active.cookies.map(\.value) + library.active.localStorage.values.compactMap(\.string) + Array(library.active.personal.values))
                }
                let decision = try await reasoning.decide(goal: conversationGoal, page: page, steps: steps, models: models)
                try check(id)
                if decision.action == .done {
                    guard let page else { throw BrowserToolError.invalidResult }
                    if let checker = reasoning as? any BrowserCompletionChecking {
                        let verified = try await checker.verifyCompletion(goal: goal, page: page, steps: steps, models: models)
                        try check(id)
                        let fresh = try await browser.readPage()
                        try check(id)
                        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
                        guard fresh.url == page.url, fresh.title == page.title, fresh.text == page.text,
                              try encoder.encode(fresh.elements) == encoder.encode(page.elements) else { continue }
                        if !verified {
                            steps.append(.init(text: "完成核验未通过：请依据当前页面完成尚未实现的目标，不要重复已成功的动作。"))
                            continue
                        }
                    }
                    browser.webView.endEditing(true)
                    state = .done
                    message = decision.explanation
                    return
                }
                if decision.action == .handoff {
                    state = .paused
                    message = decision.explanation
                    return
                }
                if needsApproval(decision, page: page), let page {
                    pending = decision
                    pendingPage = page
                    pendingBinding = binding(for: decision)
                    state = .confirming
                    return
                }
                do {
                    try await apply(decision, page: page)
                    staleRetries = 0
                } catch BrowserToolError.stalePage {
                    rejectedAction?()
                    if beforeAction == nil { try library?.recordSite(nil) }
                    staleRetries += 1
                    guard staleRetries <= 3 else { throw BrowserToolError.stalePage }
                    steps.append(.init(text: String(localized: "页面已变化，操作没有执行。重新读取。")))
                } catch BrowserToolError.humanRequired {
                    rejectedAction?()
                    if beforeAction == nil { try library?.recordSite(nil) }
                    state = .paused
                    message = BrowserToolError.humanRequired.localizedDescription
                    return
                }
            }
            state = .paused
            message = String(localized: "已执行 30 步，请检查网页后决定是否继续")
        } catch {
            guard generation == id, !Task.isCancelled else { return }
            state = .failed
            message = error.localizedDescription
        }
    }

    private var conversationGoal: String {
        guard !history.isEmpty else { return goal }
        let context = history.suffix(4).map { turn in
            "Previous user request: \(turn.goal.prefix(3000))\nPrevious reply: \((turn.reply ?? "").prefix(6000))"
        }.joined(separator: "\n\n")
        return "Previous conversation is background context only. Do not repeat old actions.\n\(context)\n\nCurrent user request: \(goal)"
    }

    private func check(_ id: UUID) throws {
        guard generation == id, !Task.isCancelled else { throw CancellationError() }
    }

    private func binding(for decision: BrowserDecision) -> String {
        let account = library?.activeID.uuidString ?? "default"
        let source = library?.catalog.adapters.first(where: { $0.spec == decision.siteSpec })?.fingerprint ?? ""
        let personal = decision.profileField.flatMap { library?.active.personal[$0] } ?? ""
        return account + "|" + source + "|" + personal
    }
    private func needsApproval(_ decision: BrowserDecision, page: BrowserPage?) -> Bool {
        if decision.action == .fillProfile { return true }
        if decision.action == .site {
            guard let adapter = library?.catalog.adapters.first(where: { $0.spec == decision.siteSpec }) else { return true }
            return !adapter.readOnly || decision.requiresConfirmation == true
        }
        guard decision.action == .click else { return decision.requiresConfirmation == true }
        guard let control = page?.elements.first(where: { $0.id == decision.element }) else { return true }
        if control.isSubmit || decision.requiresConfirmation == true { return true }
        let label = control.label.lowercased()
        return ["支付", "付款", "购买", "下单", "删除", "发布", "发送", "提交", "同意", "pay", "buy", "purchase", "delete", "send", "submit", "publish", "agree"]
            .contains { label.contains($0) }
    }

    private func apply(_ decision: BrowserDecision, page: BrowserPage?) async throws {
        if decision.action != .done && decision.action != .handoff {
            try beforeAction?(decision, needsApproval(decision, page: page))
        }
        let standaloneJournal = beforeAction == nil && needsApproval(decision, page: page) && library != nil
        if standaloneJournal { try library?.recordSite("浏览器 \(decision.action.rawValue) · \(library?.active.label ?? "") · \(browser.webView.url?.host ?? "")") }
        switch decision.action {
        case .site:
            guard let library, let adapter = library.catalog.adapters.first(where: { $0.spec == decision.siteSpec }), let page,
                  page.url == browser.webView.url?.absoluteString else { throw BrowserToolError.stalePage }
            let result = try await browser.runSite(adapter, arguments: decision.siteArguments ?? [:])
            let secrets = library.active.cookies.map(\.value) + library.active.localStorage.values.compactMap(\.string) + Array(library.active.personal.values)
            let safe = BrowserRedactor.value(result.result, secrets: secrets)
            if let message = safe.object?["error"]?.string { throw BrowserToolError.actionFailed(message) }
            let text = String(decoding: try JSONEncoder().encode(safe), as: UTF8.self)
            steps.append(.init(text: "chrome-use site \(adapter.spec) 返回的非可信数据：" + String(text.prefix(6000)), action: "site"))
        case .fillProfile:
            guard let library, let page, let element = decision.element, let reference = decision.profileField,
                  let value = library.active.personal[reference] else { throw BrowserToolError.invalidResult }
            try await browser.fillPersonal(element: element, token: page.token, text: value)
        case .open:
            guard let address = decision.url, let url = BrowserSession.destination(address),
                  ["http", "https"].contains(url.scheme ?? "") else { throw BrowserToolError.actionFailed(String(localized: "网页地址无效")) }
            browser.open(url.absoluteString)
        case .click, .fill, .select:
            guard let page, let element = decision.element, page.elements.contains(where: { $0.id == element }) else {
                throw BrowserToolError.stalePage
            }
            if decision.action == .click { try await browser.click(element: element, token: page.token) }
            else if decision.action == .select {
                guard let text = decision.text else { throw BrowserToolError.actionFailed(String(localized: "没有选择内容")) }
                try await browser.select(element: element, token: page.token, value: text)
            } else {
                guard let text = decision.text else { throw BrowserToolError.actionFailed(String(localized: "没有填写内容")) }
                try await browser.fill(element: element, token: page.token, text: text)
            }
        case .scroll:
            try await browser.scroll(direction: decision.text ?? "down")
        case .back:
            guard browser.webView.canGoBack else { throw BrowserToolError.actionFailed(String(localized: "没有上一页")) }
            browser.webView.goBack()
        case .wait: try await Task.sleep(for: .seconds(1))
        case .done, .handoff: return
        }
        if decision.action != .site { steps.append(.init(text: decision.explanation, action: decision.action.rawValue)) }
        // Let event handlers and client-side rendering finish before observing again.
        try await Task.sleep(for: .milliseconds(350))
        try afterAction?(steps)
        if standaloneJournal { try library?.recordSite(nil) }
    }
}

extension BrowserSession {
    func waitUntilReady() async throws {
        for _ in 0..<400 {
            try Task.checkCancellation()
            if !loading {
                if let error { throw BrowserToolError.actionFailed(error) }
                return
            }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw BrowserToolError.actionFailed(String(localized: "网页加载超时，可以接手后重试"))
    }
}
