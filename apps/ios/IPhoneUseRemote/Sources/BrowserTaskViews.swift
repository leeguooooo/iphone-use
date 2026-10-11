import SwiftUI
import Combine
import WebKit

@MainActor
final class BrowserWorkspace: NSObject, ObservableObject, WKHTTPCookieStoreObserver {
    @Published private(set) var browser: BrowserSession
    @Published private(set) var agent: BrowserAgent
    let library: BrowserLibrary
    @Published private(set) var switching = true
    @Published var lockedForTask = false
    private let reasoning: any BrowserReasoning
    private var visits: AnyCancellable?
    private var cookieCapture: Task<Void, Never>?
    init(library: BrowserLibrary? = nil, reasoning: any BrowserReasoning = BrowserModelClient()) {
        let library = library ?? BrowserLibrary()
        self.library = library; self.reasoning = reasoning
        if library.catalog.adapters.isEmpty {
            do {
                if let package = try PhoneSiteCatalogDownloader.bundled() { try library.installCatalog(package.catalog) }
            } catch { library.error = error.localizedDescription }
        }
        let browser = BrowserSession(dataStore: Self.store(for: library.activeID))
        self.browser = browser
        agent = BrowserAgent(browser: browser, reasoning: reasoning, library: library)
        super.init()
        bindVisits()
        Task { await restore(); switching = false }
    }
    private static func store(for id: UUID) -> WKWebsiteDataStore {
        if id == BrowserAccount.defaultID { return .default() }
        if #available(iOS 17, *), !AppCompatibility.forceLegacyUI { return WKWebsiteDataStore(forIdentifier: id) }
        return .nonPersistent()
    }
    private func bindVisits() {
        browser.webView.configuration.websiteDataStore.httpCookieStore.add(self)
        visits = browser.$loading.dropFirst().filter { !$0 }.sink { [weak self] _ in
            guard let self, let url = self.browser.webView.url?.absoluteString else { return }
            do { try self.library.recordVisit(url, title: self.browser.title) } catch { self.library.error = error.localizedDescription }
        }
    }
    func cookiesDidChange(in cookieStore: WKHTTPCookieStore) {
        cookieCapture?.cancel()
        cookieCapture = Task { [weak self] in
            do { try await Task.sleep(nanoseconds: 400_000_000) } catch { return }
            guard let self, !self.switching else { return }
            await self.capture()
        }
    }
    func switchAccount(_ id: UUID) async throws {
        guard !switching, !agent.active, !lockedForTask else { throw BrowserToolError.actionFailed("请先暂停或停止任务，再切换账号") }
        switching = true
        defer { switching = false }
        if !library.active.needsApply { await capture() }
        browser.webView.configuration.websiteDataStore.httpCookieStore.remove(self)
        agent.stop()
        try library.select(id)
        browser = BrowserSession(dataStore: Self.store(for: id))
        agent = BrowserAgent(browser: browser, reasoning: reasoning, library: library)
        bindVisits()
        await restore()
    }
    func removeAccount(_ id: UUID) async throws {
        guard id != BrowserAccount.defaultID, !switching, !agent.active, !lockedForTask else { throw BrowserToolError.actionFailed("请先停止任务，再移除账号") }
        if id == library.activeID { try await switchAccount(BrowserAccount.defaultID) }
        if #available(iOS 17, *), !AppCompatibility.forceLegacyUI {
            let store = WKWebsiteDataStore(forIdentifier: id)
            await withCheckedContinuation { continuation in
                store.removeData(ofTypes: WKWebsiteDataStore.allWebsiteDataTypes(), modifiedSince: .distantPast) { continuation.resume() }
            }
        }
        try library.remove(id)
    }
    func capture() async {
        let browser = self.browser
        let accountID = library.activeID
        guard !library.active.needsApply else { return }
        let cookies = await withCheckedContinuation { continuation in
            browser.webView.configuration.websiteDataStore.httpCookieStore.getAllCookies { continuation.resume(returning: $0) }
        }
        var storage: [String: JSONValue]?
        if let host = browser.webView.url?.host, host == library.active.domains.first {
            if let raw = try? await browser.webView.callAsyncJavaScript("return JSON.stringify(Object.fromEntries(Object.entries(localStorage)));", arguments: [:], in: nil, contentWorld: .page),
               let raw = raw as? String, raw.utf8.count <= 2_000_000, let data = raw.data(using: .utf8) {
                storage = try? JSONDecoder().decode([String: JSONValue].self, from: data)
            }
        }
        guard library.activeID == accountID, self.browser === browser, !library.active.needsApply else { return }
        do { try library.saveSnapshot(cookies: cookies, url: browser.webView.url?.absoluteString, localStorage: storage) }
        catch { library.error = error.localizedDescription }
    }
    func restore() async {
        let account = library.active
        let cookies = browser.webView.configuration.websiteDataStore.httpCookieStore
        // Named stores preserve full website data on iOS 17+. Older versions
        // rebuild an isolated ephemeral store from the encrypted account jar.
        let apply: Bool
        if #available(iOS 17, *), !AppCompatibility.forceLegacyUI { apply = account.needsApply } else { apply = account.id != BrowserAccount.defaultID || account.needsApply }
        if account.needsApply, account.id != BrowserAccount.defaultID {
            await withCheckedContinuation { continuation in
                browser.webView.configuration.websiteDataStore.removeData(ofTypes: WKWebsiteDataStore.allWebsiteDataTypes(), modifiedSince: .distantPast) { continuation.resume() }
            }
        }
        for item in apply ? account.cookies : [] {
            if let cookie = item.cookie(allowedDomains: account.domains) {
                await withCheckedContinuation { continuation in cookies.setCookie(cookie) { continuation.resume() } }
            }
        }
        if apply, let host = account.domains.first, !account.localStorage.isEmpty,
           let data = try? JSONEncoder().encode(account.localStorage), let json = String(data: data, encoding: .utf8),
           let originData = try? JSONEncoder().encode("https://" + host), let origin = String(data: originData, encoding: .utf8) {
            let script = "if (location.origin === " + origin + ") { const values = " + json + "; for (const [key,value] of Object.entries(values)) { if (!['__proto__','constructor','prototype'].includes(key)) localStorage.setItem(key, typeof value === 'string' ? value : JSON.stringify(value)); } }"
            browser.webView.configuration.userContentController.addUserScript(.init(source: script, injectionTime: .atDocumentStart, forMainFrameOnly: true))
        }
        do { try library.markApplied(account.id) } catch { library.error = error.localizedDescription }
        if let url = account.lastURL { browser.open(url) }
    }
}

struct BrowserWorkspaceView: View {
    @ObservedObject var settings: AgentController
    @ObservedObject var workspace: BrowserWorkspace
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        BrowserView(session: workspace.browser, agent: workspace.agent, settings: settings, workspace: workspace)
            .appOnChange(of: scenePhase) { _, phase in
                if phase != .active { workspace.agent.pause(); Task { await workspace.capture() } }
            }
    }
}

struct BrowserTaskBar: View {
    @ObservedObject var agent: BrowserAgent
    @ObservedObject var settings: AgentController
    @Binding var expanded: Bool
    @AppStorage("browser.goalDraft.v1") private var draft = ""
    @State private var preparedDraft = false

    var body: some View {
        VStack(spacing: 10) {
            if !agent.goal.isEmpty {
                Button(action: openConversation) {
                    HStack(alignment: .top, spacing: 10) {
                        if agent.state == .running { ProgressView().padding(.top, 3) }
                        VStack(alignment: .leading, spacing: 3) {
                            Text(status).font(.caption).foregroundStyle(.secondary)
                            Text(agent.message ?? agent.pending?.explanation ?? agent.steps.last?.text ?? agent.goal)
                                .font(.subheadline).foregroundStyle(.primary).lineLimit(2)
                        }
                        Spacer(minLength: 0)
                        Image(systemName: "chevron.up").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(agent.message ?? status)
                .accessibilityHint(browserChatString("展开完整对话"))
                .accessibilityIdentifier(agent.message == nil ? "browser-task-progress" : "browser-task-result")
                if agent.active || agent.state == .paused { BrowserTaskActions(agent: agent, onInspect: openConversation) }
            }
            HStack(spacing: 10) {
                Button(action: openConversation) {
                    Image(systemName: "bubble.left.and.bubble.right")
                        .font(.title3).frame(width: 44, height: 44)
                }
                .accessibilityLabel(browserChatString("展开对话"))
                .accessibilityIdentifier("browser-open-chat")
                BrowserComposer(agent: agent, settings: settings, draft: $draft, expanded: false, onSend: openConversation)
            }
        }
        .padding(.horizontal, 16).padding(.vertical, 10)
        .background(.bar)
        .sheet(isPresented: $expanded) {
            BrowserTaskDetail(agent: agent, settings: settings, draft: $draft)
                .browserConversationPresentation()
        }
        .onAppear {
            #if DEBUG
            if !preparedDraft && UserDefaults.standard.bool(forKey: "clearBrowserDraft") { draft = "" }
            #endif
            preparedDraft = true
        }
        .appOnChange(of: agent.state) { _, state in if state == .confirming { openConversation() } }
    }

    private func openConversation() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        expanded = true
    }

    private var status: String {
        switch agent.state {
        case .running: return String(format: browserChatString("正在执行，第 %lld 步"), agent.steps.count + 1)
        case .confirming: return browserChatString("等待确认")
        case .paused: return browserChatString("已暂停")
        case .done: return browserChatString("已完成")
        case .failed: return browserChatString("需要重试")
        case .stopped: return browserChatString("已停止")
        case .idle: return browserChatString("浏览器助手")
        }
    }
}

private struct BrowserComposer: View {
    @ObservedObject var agent: BrowserAgent
    @ObservedObject var settings: AgentController
    @Binding var draft: String
    let expanded: Bool
    var onSend: () -> Void
    @FocusState private var composing: Bool

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            field
                .font(.body)
                .focused($composing)
                .accessibilityIdentifier(expanded ? "browser-chat-goal" : "browser-goal")
                .padding(.leading, 14).padding(.vertical, 11)
            Button(action: send) {
                Image(systemName: "arrow.up.circle.fill").font(.system(size: 28))
                    .frame(width: 44, height: 44)
            }
            .disabled(agent.active || draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            .accessibilityLabel(browserChatString("发送消息"))
            .accessibilityIdentifier(expanded ? "browser-chat-send" : "browser-start-task")
        }
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 22))
        .overlay(RoundedRectangle(cornerRadius: 22).strokeBorder(Theme.hairline))
    }

    @ViewBuilder private var field: some View {
        if expanded {
            if #available(iOS 16, *), !AppCompatibility.forceLegacyUI {
                TextField(browserChatString("继续提问或告诉我下一步"), text: $draft, axis: .vertical)
                    .lineLimit(1...5)
            } else {
                AppTextField(LocalizedStringKey(browserChatString("继续提问或告诉我下一步")), text: $draft, axis: .vertical)
                    .frame(height: 72)
            }
        } else {
            TextField(browserChatString("让助手操作网页"), text: $draft)
                .submitLabel(.send).onSubmit(send)
        }
    }

    private func send() {
        guard !agent.active else { return }
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        agent.start(text, settings: settings.settings)
        if agent.state == .running {
            draft = ""
            composing = false
        }
        onSend()
    }
}

private struct BrowserTaskActions: View {
    @ObservedObject var agent: BrowserAgent
    var onInspect: () -> Void
    var onTakeOver: () -> Void = {}
    var body: some View {
        HStack(spacing: 20) {
            if agent.state == .paused {
                Button(browserChatString("继续执行")) { agent.resume() }.accessibilityIdentifier("browser-resume")
            } else if agent.state == .confirming {
                Button(browserChatString("查看并确认"), action: onInspect)
            } else if agent.state == .running {
                Button(browserChatString("接手网页")) { agent.pause(); onTakeOver() }.accessibilityIdentifier("browser-pause")
            }
            Spacer()
            Button(browserChatString("停止"), role: .destructive) { agent.stop() }.accessibilityIdentifier("browser-stop")
        }.font(.subheadline)
    }
}

struct BrowserTaskDetail: View {
    @ObservedObject var agent: BrowserAgent
    @ObservedObject var settings: AgentController
    @Binding var draft: String
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 20) {
                        if agent.goal.isEmpty {
                            VStack(alignment: .leading, spacing: 10) {
                                Text(browserChatString("有什么需要我帮忙？")).font(.title2.bold())
                                Text(browserChatString("告诉我想在网页上做什么。你可以随时查看网页、接手操作，再回到这里继续。"))
                                    .foregroundStyle(.secondary)
                            }.padding(.vertical, 20)
                        }
                        ForEach(agent.history) { turn in
                            conversation(goal: turn.goal, reply: turn.reply, steps: turn.steps)
                        }
                        if !agent.goal.isEmpty {
                            conversation(goal: agent.goal, reply: agent.message, steps: agent.steps)
                        }
                        if agent.state == .running {
                            HStack(spacing: 10) { ProgressView(); Text(browserChatString("正在操作网页…")).foregroundStyle(.secondary) }
                                .font(.subheadline)
                        }
                        if let pending = agent.pending {
                            VStack(alignment: .leading, spacing: 12) {
                                Label(browserChatString("需要你的确认"), systemImage: "hand.raised").font(.headline)
                                Text(pending.explanation)
                    Text(agent.confirmationDetails).font(.caption).privacySensitive().textSelection(.enabled)
                                Button(browserChatString("允许这一步")) { agent.approve() }
                                    .buttonStyle(.borderedProminent).accessibilityIdentifier("browser-approve")
                                Button(browserChatString("我来操作网页")) { agent.pause(); dismiss() }
                            }
                            .padding(16).frame(maxWidth: .infinity, alignment: .leading)
                            .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
                        }
                        if agent.active || agent.state == .paused {
                            BrowserTaskActions(agent: agent, onInspect: {}, onTakeOver: { dismiss() }).padding(.top, 4)
                        }
                        Color.clear.frame(height: 1).id("latest")
                    }.padding(20)
                }
                .onAppear { scrollToLatest(proxy) }
                .appOnChange(of: agent.goal) { _, _ in scrollToLatest(proxy) }
                .appOnChange(of: agent.message) { _, _ in scrollToLatest(proxy) }
                .appOnChange(of: agent.pending?.explanation) { _, _ in scrollToLatest(proxy) }
            }
            .safeAreaInset(edge: .bottom) {
                VStack(alignment: .leading, spacing: 8) {
                    BrowserComposer(agent: agent, settings: settings, draft: $draft, expanded: true, onSend: {})
                    Text(browserChatString("当前网页与对话会发送给设置中启用的模型。"))
                        .font(.caption2).foregroundStyle(.secondary)
                }.padding(.horizontal, 16).padding(.vertical, 10).background(.bar)
            }
            .navigationTitle(browserChatString("浏览器助手")).navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button(browserChatString("查看网页")) { dismiss() }.accessibilityIdentifier("browser-close-chat")
                }
            }
        }
    }

    @ViewBuilder private func conversation(goal: String, reply: String?, steps: [BrowserAgentStep]) -> some View {
        HStack {
            Spacer(minLength: 32)
            Text(goal).textSelection(.enabled).padding(12)
                .background(Theme.accent.opacity(0.12), in: RoundedRectangle(cornerRadius: 16))
                .accessibilityIdentifier("browser-user-message")
        }
        if !steps.isEmpty {
            DisclosureGroup {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(steps) { step in Label(step.text, systemImage: "checkmark").font(.subheadline) }
                }.padding(.top, 8)
            } label: {
                Text(String(format: browserChatString("执行步骤（%lld）"), steps.count)).font(.caption).foregroundStyle(.secondary)
            }
        }
        if let reply {
            Text(reply).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("browser-assistant-message")
        }
    }

    private func scrollToLatest(_ proxy: ScrollViewProxy) {
        DispatchQueue.main.async { proxy.scrollTo("latest", anchor: .bottom) }
    }
}

private extension View {
    @ViewBuilder func browserConversationPresentation() -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI {
            presentationDetents([.medium, .large]).presentationDragIndicator(.visible)
        } else { self }
    }
}

private func browserChatString(_ key: String) -> String {
    NSLocalizedString(key, tableName: "BrowserChat", bundle: .main, value: key, comment: "")
}
