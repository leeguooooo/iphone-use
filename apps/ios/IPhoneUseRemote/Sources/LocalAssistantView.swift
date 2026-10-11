import SwiftUI

/// This entry point can only submit goals to the paired loopback device.
struct LocalAssistantView: View {
    @ObservedObject var app: AppModel
    @ObservedObject var agent: AgentController
    @State private var pairing = false
    @State private var models = false
    @State private var flows = false
    @State private var savingFlow = false
    @State private var visible = false
    @State private var submitted = false
    @FocusState private var composing: Bool

    private var local: DeviceSession? {
        app.devices.first(where: \.isThisPhone).flatMap { app.session($0.id) }
    }
    private var localSelected: Bool { local != nil && agent.targetID == local?.id }
    private var canSend: Bool {
        localSelected && local?.phase == .connected && !agent.busyOnTarget &&
        !agent.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        AppNavigationStack {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 20) {
                        if let local {
                            if localSelected {
                                if agent.thread.isEmpty && agent.pendingGoal == nil {
                                    Text(navigationString("让助手在这台 iPhone 的 App 里完成任务。"))
                                        .foregroundStyle(.secondary).padding(.vertical, 12)
                                }
                                ForEach(agent.thread) { task in
                                    AgentTaskItem(task: task, agent: agent,
                                        onSaveFlow: task.id == agent.thread.last?.id && task.state == .done ? { savingFlow = true } : nil)
                                }
                                if agent.pendingGoal != nil { ProgressView() }
                                if let error = agent.error, submitted || agent.liveDeviceID == local.id { AgentBubble(text: error, failed: true) }
                                if let note = agent.linkNote, agent.liveDeviceID == local.id { AgentBubble(text: note) }
                                if local.phase != .connected {
                                    Label(local.shortState, systemImage: "wifi.exclamationmark").foregroundStyle(.secondary)
                                    Button(navigationString("重新连接本机")) { Task { await local.retryIfWaiting() } }
                                }
                            } else {
                                Button(navigationString("返回本机对话")) { selectLocal() }
                            }
                        } else {
                            VStack(alignment: .leading, spacing: 16) {
                                Image(systemName: "iphone").font(.system(size: 44)).foregroundStyle(Theme.accent)
                                Text(navigationString("让助手操作这台 iPhone")).font(.title2.bold())
                                Text(navigationString("先完成本机配置，再告诉助手你想在手机上做什么。远程手机在「远程控制」里连接。"))
                                    .foregroundStyle(.secondary)
                                Button(navigationString("配置本机助手")) { pairing = true }
                                    .buttonStyle(.borderedProminent).accessibilityIdentifier("pair-local-phone")
                            }.padding(.vertical, 28)
                        }
                        Color.clear.frame(height: 1).id("local-bottom")
                    }.padding(20)
                }
                .appOnChange(of: agent.thread.count) { _, _ in proxy.scrollTo("local-bottom", anchor: .bottom) }
                .appOnChange(of: agent.pendingGoal) { _, _ in proxy.scrollTo("local-bottom", anchor: .bottom) }
            }
            .safeAreaInset(edge: .bottom) { if localSelected { composer } }
            .navigationTitle(navigationString("本机助手"))
            .toolbar {
                ToolbarItem(placement: .navigationBarTrailing) {
                    Menu {
                        Button(navigationString("模型设置")) { models = true }
                        Button(navigationString("流程与定时")) { flows = true }.disabled(local?.client == nil)
                    } label: { Image(systemName: "ellipsis.circle") }
                        .accessibilityLabel(navigationString("更多"))
                }
            }
            .sheet(isPresented: $pairing) { PairThisPhoneSheet(app: app) }
            .sheet(isPresented: $models) { AppNavigationStack { ModelSettingsView(agent: agent) } }
            .sheet(isPresented: $flows) { if let local { FlowsHubView(session: local) } }
            .sheet(isPresented: $savingFlow) { if let local { SaveFlowSheet(session: local, draft: nil) } }
            .onAppear { visible = true; selectLocal() }
            .onDisappear { visible = false }
            .task(id: local?.id) {
                if app.voiceCall.active && !localSelected { app.voiceCall.end() }
                selectLocal(); await agent.refreshHistory()
            }
            .appOnChange(of: app.agentOpen) { _, open in if !open && visible { selectLocal() } }
        }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            Button {
                composing = false
                app.voiceCall.showing = true
                if !app.voiceCall.active, !app.voiceCall.connecting, let id = local?.id {
                    Task { await app.voiceCall.start(agent: agent, deviceID: id) }
                }
            } label: {
                Image(systemName: app.voiceCall.active ? "waveform.circle.fill" : "phone.circle.fill")
                    .font(.title).frame(width: 44, height: 44)
            }
            .disabled(!app.voiceCall.active && local?.phase != .connected)
            .accessibilityLabel("语音通话")
            .accessibilityIdentifier("local-voice-call")
            AppTextField(LocalizedStringKey(navigationString("告诉助手要做什么")), text: $agent.draft, axis: .vertical)
                .appLineLimit(1...5).focused($composing)
                .padding(12).background(Theme.surface, in: RoundedRectangle(cornerRadius: 22))
                .accessibilityIdentifier("local-agent-goal")
            if agent.pendingGoal != nil {
                ProgressView().frame(width: 44, height: 44)
            } else if agent.busyOnTarget, agent.liveDeviceID == local?.id {
                Button { Task { await agent.stop() } } label: { Image(systemName: "stop.circle.fill").font(.title) }
                    .disabled(agent.stopping).accessibilityLabel(navigationString("停止任务"))
            } else {
                Button(action: send) { Image(systemName: "arrow.up.circle.fill").font(.title).frame(width: 44, height: 44) }
                    .disabled(!canSend).accessibilityLabel(navigationString("发送消息"))
                    .accessibilityIdentifier("local-agent-send")
            }
        }.padding(16).background(.bar)
    }

    private func selectLocal() {
        guard !app.agentOpen else { return }
        if agent.targetID != local?.id { submitted = false; agent.targetID = local?.id }
    }

    private func send() {
        guard canSend, let id = local?.id else { return }
        let text = agent.draft
        composing = false
        Task {
            guard agent.targetID == id, app.session(id)?.record.isThisPhone == true else { return }
            submitted = true
            if await agent.start(goal: text), agent.targetID == id, agent.draft == text { agent.draft = "" }
        }
    }
}

func navigationString(_ key: String) -> String {
    NSLocalizedString(key, tableName: "Navigation", bundle: .main, value: key, comment: "")
}
