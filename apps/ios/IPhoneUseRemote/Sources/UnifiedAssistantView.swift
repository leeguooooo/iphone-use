import SwiftUI
import Combine

struct UnifiedAssistantView: View {
    @ObservedObject var app: AppModel
    @ObservedObject var workspace: BrowserWorkspace
    @StateObject private var assistant: UnifiedAssistant
    @State private var pairing = false
    @State private var models = false
    @State private var library = false
    @State private var flows = false
    @State private var verify = false
    @FocusState private var composing: Bool
    var openBrowser: () -> Void
    var openAssistant: () -> Void

    init(app: AppModel, workspace: BrowserWorkspace, openBrowser: @escaping () -> Void, openAssistant: @escaping () -> Void = {}) {
        self.app = app; self.workspace = workspace; self.openBrowser = openBrowser; self.openAssistant = openAssistant
        _assistant = StateObject(wrappedValue: UnifiedAssistant(app: app, workspace: workspace))
    }

    var body: some View {
        AppNavigationStack {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    if assistant.history.isEmpty && assistant.task == nil {
                        Text("告诉助手目标，它会选择手机 App、内置浏览器或 chrome-use 网站操作。")
                            .foregroundStyle(.secondary).padding(.vertical, 10)
                        if !assistant.phoneAvailable {
                            Text("浏览器已可使用。手机 App 操作需要先连接本机执行服务。")
                                .font(.footnote).foregroundStyle(.secondary)
                            Button("配置手机执行服务") { pairing = true }.accessibilityIdentifier("pair-local-phone")
                        }
                    }
                    ForEach(assistant.history.reversed()) { task in taskCard(task) }
                    if let task = assistant.task {
                        taskCard(task)
                        if let native = assistant.native, let prompt = native.pendingConfirm {
                            VStack(alignment: .leading, spacing: 10) {
                                Label("需要确认手机操作", systemImage: "hand.raised")
                                Text(AgentController.describe(prompt))
                                HStack {
                                    Button("拒绝") { assistant.answerPhone(approve: false) }
                                    Button("允许这一次") { assistant.answerPhone(approve: true) }
                                }
                            }.padding().background(Theme.surface, in: RoundedRectangle(cornerRadius: 16))
                        } else if workspace.agent.state == .confirming, let pending = workspace.agent.pending {
                            VStack(alignment: .leading, spacing: 10) {
                                Label("需要确认网页操作", systemImage: "hand.raised")
                                Text(pending.explanation)
                                Text(workspace.agent.confirmationDetails).font(.caption).privacySensitive()
                                Text("网站：\(workspace.browser.webView.url?.host ?? "")\n账号：\(workspace.library.active.label)")
                                    .font(.caption).foregroundStyle(.secondary)
                                if let field = pending.profileField { Text("资料字段：\(field)") }
                                HStack {
                                    Button("接管网页") { Task { await assistant.pause(); openBrowser() } }
                                    Button("允许这一次") { workspace.agent.approve() }
                                }
                            }.padding().background(Theme.surface, in: RoundedRectangle(cornerRadius: 16))
                        } else if task.state == .confirming, let phase = assistant.current, phase.tool == .site || phase.tool == .desktop {
                            VStack(alignment: .leading, spacing: 10) {
                                Label("需要确认网站操作", systemImage: "hand.raised")
                                Text(phase.siteSpec ?? phase.goal)
                                Text("账号：\(phase.tool == .desktop ? (workspace.library.chrome?.profile ?? "未连接电脑") : workspace.library.active.label)")
                                if let args = phase.arguments { Text(String(decoding: (try? JSONEncoder().encode(args)) ?? Data(), as: UTF8.self)).font(.caption) }
                                HStack {
                                    Button("停止任务") { Task { await assistant.stop() } }
                                    Button("允许这一次") { assistant.approveSite() }
                                }
                            }.padding().background(Theme.surface, in: RoundedRectangle(cornerRadius: 16))
                        }
                        if task.active {
                            HStack {
                                if [.paused, .waitingForPhone].contains(task.state) { Button("继续任务") { assistant.resume() }.accessibilityIdentifier("assistant-resume") }
                                else if task.state != .needsReview { Button("暂停并接管") { Task { await assistant.pause() } }.accessibilityIdentifier("assistant-pause") }
                                Button("停止") { Task { await assistant.stop() } }.accessibilityIdentifier("assistant-stop")
                                if task.state == .needsReview { Button("我已检查结果") { verify = true } }
                                if assistant.current?.tool == .phone { Button("核对手机任务") { assistant.inspectExistingPhoneTask() } }
                            }.font(.subheadline)
                        }
                    }
                    if let message = assistant.message { Text(message).foregroundStyle(Theme.attention).font(.footnote) }
                    if assistant.busy { ProgressView() }
                }.padding(20)
            }
            .safeAreaInset(edge: .bottom) {
                VStack(spacing: 8) {
                    HStack {
                        Picker("执行方式", selection: $assistant.mode) {
                            ForEach(AssistantMode.allCases, id: \.self) { Text($0.title).tag($0) }
                        }.pickerStyle(.menu).disabled(assistant.task?.active == true)
                        Spacer()
                        Text(workspace.library.active.label).font(.caption).foregroundStyle(.secondary)
                        Button { openBrowser() } label: { Image(systemName: "globe") }.accessibilityLabel("查看浏览器")
                    }
                    HStack(alignment: .bottom) {
                        AppTextField("告诉助手要做什么", text: $assistant.draft, axis: .vertical)
                            .appLineLimit(1...5).focused($composing)
                            .padding(12).background(Theme.surface, in: RoundedRectangle(cornerRadius: 20))
                            .accessibilityIdentifier("local-agent-goal")
                        Button {
                            composing = false; _ = assistant.start(assistant.draft)
                        } label: { Image(systemName: "arrow.up.circle.fill").font(.title).frame(width: 44, height: 44) }
                            .disabled(!assistant.canStart || assistant.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            .accessibilityLabel("发送消息").accessibilityIdentifier("local-agent-send")
                    }
                }.padding(16).background(.bar)
            }
            .onAppear { assistant.showBrowser = openBrowser; assistant.showAssistant = openAssistant }
            .navigationTitle("助手")
            .toolbar {
                ToolbarItem(placement: .navigationBarTrailing) {
                    Menu {
                        Button("模型设置") { models = true }
                        Button("账号与网站资料") { library = true }
                        Button("配置手机执行服务") { pairing = true }
                        Button("流程与定时") { flows = true }.disabled(assistant.local?.client == nil)
                        Button("语音通话") { Task { await app.voiceCall.start(phone: UnifiedVoiceControl(assistant: assistant)) } }
                    } label: { Image(systemName: "ellipsis.circle") }.accessibilityLabel("更多")
                }
            }
            .sheet(isPresented: $pairing) { PairThisPhoneSheet(app: app) }
            .sheet(isPresented: $models) { AppNavigationStack { ModelSettingsView(agent: app.agent) } }
            .sheet(isPresented: $library) { BrowserLibraryView(workspace: workspace, onBrowse: openBrowser) }
            .sheet(isPresented: $flows) { if let local = assistant.local { FlowsHubView(session: local) } }
            .confirmationDialog("确认你已检查上次操作结果", isPresented: $verify, titleVisibility: .visible) {
                Button(assistant.task?.stopRequested == true ? "我已核对，结束任务" : "本步骤已完成，继续后续步骤") { assistant.markVerifiedComplete() }
                Button("取消", role: .cancel) {}
            } message: { Text("只有实际检查手机或网页后，才能标记完成。未知结果的写入不会自动重试。") }
        }
    }
    private func taskCard(_ task: UnifiedTask) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(task.goal).font(.headline).privacySensitive()
            if !task.phases.isEmpty {
                ForEach(Array(task.phases.enumerated()), id: \.element.id) { index, phase in
                    HStack(alignment: .top) {
                        Image(systemName: phase.result != nil ? "checkmark.circle.fill" : (index == task.index ? "arrow.right.circle" : "circle"))
                        VStack(alignment: .leading) {
                            Text(phase.tool.title).font(.caption).foregroundStyle(.secondary)
                            Text(phase.goal).font(.subheadline)
                            if let result = phase.result { DisclosureGroup("结果") { Text(result).textSelection(.enabled).font(.footnote) } }
                        }
                    }
                }
            }
            if let reply = task.reply { Text(reply).textSelection(.enabled).accessibilityIdentifier("assistant-task-result") }
            Text(task.state.title).font(.caption).foregroundStyle(.secondary)
        }.padding(16).background(Theme.surface, in: RoundedRectangle(cornerRadius: 18))
    }
}

@MainActor
private final class UnifiedVoiceControl: AssistantPhoneControlling {
    private weak var assistant: UnifiedAssistant?
    private var observation: AnyCancellable?
    var onChange: (() -> Void)?
    init(assistant: UnifiedAssistant) {
        self.assistant = assistant
        observation = assistant.objectWillChange.sink { [weak self] in Task { @MainActor in self?.onChange?() } }
    }
    var state: AssistantPhoneState {
        guard let assistant else { return .init(valid: false, connected: false) }
        let snapshot = (assistant.current?.tool == .phone ? assistant.native : nil) ?? assistant.task.map {
            AgentTaskSnapshot(id: $0.id.uuidString, goal: $0.goal,
                state: $0.state == .done ? .done : ($0.active ? .running : .cancelled), phase: "observing", summary: $0.reply)
        }
        let status = assistant.task.map { task in
            "任务\(task.state.title)。当前工具：\(assistant.current?.tool.title ?? "核验结果")。下一步：\(assistant.next)。" + (assistant.message ?? task.reply ?? "")
        } ?? "当前没有正在执行的任务。"
        return .init(valid: true, connected: true, busy: assistant.task?.active == true, unifiedTools: true, assistantStatus: status,
            task: snapshot, linkNote: assistant.message, error: assistant.message)
    }
    var models: [ModelsPayload.Assist] { assistant?.models ?? [] }
    func start(goal: String) async -> Bool { assistant?.start(goal) ?? false }
    func stop() async { await assistant?.stop() }
    func disconnect() { onChange = nil; observation = nil }
}
