import SwiftUI
import UIKit

// MARK: - Agent screen

/// A messaging thread with a daemon: the person's goals as bubbles, what the
/// agent did as a collapsible card of steps, and its summary as the reply.
/// The device that carries the goals out is picked in the title.
struct AgentView: View {
    @ObservedObject var app: AppModel
    @ObservedObject var agent: AgentController
    @State private var showSettings = false
    @State private var pairingThisPhone = false
    @State private var showFlows = false
    @State private var savingFlow = false
    @FocusState private var composing: Bool
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: Theme.Space.l) {
                        if let note = targetNote {
                            AgentNotice(text: note.text, symbol: note.symbol, action: note.action)
                        }
                        if agent.thread.isEmpty && agent.pendingGoal == nil {
                            AgentEmptyState(onPick: { agent.draft = $0; composing = true })
                        }
                        let thread = agent.thread
                        ForEach(thread) { task in
                            // Only the last task's trail is still on the daemon to save.
                            AgentTaskItem(task: task, agent: agent,
                                          onSaveFlow: task.id == thread.last?.id && task.state == .done
                                              ? { savingFlow = true } : nil)
                                .id(task.id)
                        }
                        if let pending = agent.pendingGoal {
                            GoalBubble(text: pending)
                            HStack(spacing: Theme.Space.s) {
                                ProgressView()
                                Text("正在交给 agent…").font(.footnote).foregroundStyle(.secondary)
                            }
                            .id("pending")
                        }
                        if let note = agent.linkNote {
                            AgentNotice(text: String(localized: "暂时跟不上这个任务（\(note)）。它可能还在手机上进行，回到这里会再看一次。"),
                                        symbol: "wifi.exclamationmark", action: nil)
                        }
                        if let error = agent.error {
                            Label(error, systemImage: "exclamationmark.triangle.fill")
                                .font(.footnote)
                                .foregroundStyle(Theme.down)
                                .accessibilityAddTraits(.isStaticText)
                        }
                        Color.clear.frame(height: 1).id("bottom")
                    }
                    .padding(.horizontal, Theme.Space.l)
                    .padding(.vertical, Theme.Space.m)
                }
                .appDismissKeyboardOnScroll()
                .refreshable { await agent.refreshHistory() }
                .appOnChange(of: agent.thread.last?.steps.count) { _, _ in scrollDown(proxy) }
                .appOnChange(of: agent.thread.count) { _, _ in scrollDown(proxy) }
                .appOnChange(of: agent.pendingGoal) { _, _ in scrollDown(proxy) }
                .onAppear { scrollDown(proxy, animated: false) }
            }
            .background(Theme.canvas.ignoresSafeArea())
            .safeAreaInset(edge: .bottom) { composer }
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("完成") { dismiss() }
                }
                ToolbarItem(placement: .principal) { targetPicker }
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        Button { showSettings = true } label: { Label("模型", systemImage: "slider.horizontal.3") }
                        Button { showFlows = true } label: { Label("Flow 与定时", systemImage: "alarm") }
                            .disabled(agent.target?.client == nil)
                    } label: {
                        Label("更多", systemImage: "ellipsis.circle")
                    }
                    .accessibilityIdentifier("agent-more")
                }
            }
            .sheet(isPresented: $showSettings) {
                AppNavigationStack { ModelSettingsView(agent: agent) }
            }
            .sheet(isPresented: $showFlows) {
                if let session = agent.target { FlowsHubView(session: session) }
            }
            .sheet(isPresented: $savingFlow) {
                if let session = agent.target { SaveFlowSheet(session: session, draft: nil) }
            }
            .sheet(isPresented: $pairingThisPhone) {
                PairThisPhoneSheet(app: app)
            }
            .sheet(item: $agent.confirmPrompt) { prompt in
                ConfirmStepSheet(prompt: prompt, answering: agent.answering, error: agent.error,
                                 onLater: { agent.confirmPrompt = nil }) { approve in
                    Task { await agent.answer(prompt, approve: approve) }
                }
            }
        }
        // Again once the target connects (at launch it is still connecting).
        .task(id: "\(agent.targetID?.uuidString ?? "")|\(agent.target?.phase == .connected)") {
            await agent.refreshHistory()
        }
    }

    // MARK: pieces

    private var targetPicker: some View {
        Menu {
            ForEach(app.devices) { record in
                Button {
                    agent.targetID = record.id
                } label: {
                    if record.id == agent.targetID {
                        Label(record.displayName, systemImage: "checkmark")
                    } else {
                        Label(record.displayName, systemImage: record.isThisPhone ? "iphone" : "desktopcomputer")
                    }
                }
            }
            if !app.devices.contains(where: \.isThisPhone) {
                Divider()
                Button { pairingThisPhone = true } label: {
                    Label("配对本机…", systemImage: "iphone.badge.plus")
                }
            }
        } label: {
            HStack(spacing: 6) {
                if let session = agent.target {
                    HealthDot(health: session.health)
                    Text(session.name).font(.headline).lineLimit(1)
                } else {
                    Text("选择设备").font(.headline)
                }
                Image(systemName: "chevron.down").font(.caption.weight(.bold)).foregroundStyle(.secondary)
            }
            .frame(maxWidth: 210)
            .foregroundStyle(.primary)
        }
        .accessibilityLabel(Text("执行任务的设备"))
        .accessibilityValue(Text(agent.target?.name ?? ""))
    }

    /// Something to fix before a goal can go out, above the thread.
    private var targetNote: (text: String, symbol: String, action: (title: String, run: () -> Void)?)? {
        guard let session = agent.target else {
            if app.devices.contains(where: \.isThisPhone) { return nil }
            return (String(localized: "还没有可用的设备。配对本机，让 agent 直接在这台 iPhone 上做事；或者在总览里添加一台 Mac 上的手机。"),
                    "iphone.badge.plus", (String(localized: "配对本机"), { pairingThisPhone = true }))
        }
        if session.phase != .connected {
            if session.record.isThisPhone, case .failed(let problem) = session.phase, problem.needsLogin {
                return (String(localized: "本机还没有有效配对。请扫描 Mac 上这台 iPhone 的「配对本机 Agent」二维码。"),
                        "iphone.badge.plus", (String(localized: "重新配对本机"), { pairingThisPhone = true }))
            }
            return (String(localized: "「\(session.name)」现在没有连上：\(session.shortState)"), "bolt.horizontal.circle",
                    (String(localized: "重试"), { Task { await session.connect(password: nil) } }))
        }
        if let status = session.status, status.ownedByOther, !(status.owner ?? "").hasPrefix("task-") {
            return (String(localized: "手机正被「\(status.owner ?? "?")」控制，任务会被拒绝"), "hand.raised", nil)
        }
        return nil
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: Theme.Space.s) {
            AppTextField("让 agent 做点什么…", text: $agent.draft, axis: .vertical)
                .appLineLimit(1...5)
                .focused($composing)
                .submitLabel(.send)
                .onSubmit(send)
                .padding(.horizontal, 14).padding(.vertical, 10)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous).strokeBorder(Theme.hairline))
                .accessibilityIdentifier("agent-goal")
            if agent.busyOnTarget, agent.pendingGoal == nil {
                Button {
                    Task { await agent.stop() }
                } label: {
                    Image(systemName: "stop.fill")
                        .font(.system(size: 15, weight: .bold))
                        .frame(width: 40, height: 40)
                        .background(Theme.down, in: Circle())
                        .foregroundStyle(.white)
                }
                .buttonStyle(PressableStyle())
                .disabled(agent.stopping)
                .accessibilityLabel(Text("停止"))
                .accessibilityIdentifier("agent-stop")
            } else {
                Button(action: send) {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 17, weight: .bold))
                        .frame(width: 40, height: 40)
                        .background(canSend ? AnyShapeStyle(Theme.accent) : AnyShapeStyle(Color.secondary.opacity(0.3)),
                                    in: Circle())
                        .foregroundStyle(.white)
                }
                .buttonStyle(PressableStyle())
                .disabled(!canSend)
                .accessibilityLabel(Text("发送"))
                .accessibilityIdentifier("agent-send")
            }
        }
        .padding(.horizontal, Theme.Space.l)
        .padding(.vertical, Theme.Space.s)
        .background(.bar)
    }

    private var canSend: Bool {
        !agent.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && agent.target != nil && !agent.busyOnTarget
    }

    private func send() {
        guard canSend else { return }
        let text = agent.draft
        let device = agent.targetID
        composing = false
        Task {
            // Keep the draft if the request is refused or the service is away.
            // Do not erase edits the person made while the request was in flight.
            if await agent.start(goal: text), agent.targetID == device, agent.draft == text { agent.draft = "" }
        }
    }

    private func scrollDown(_ proxy: ScrollViewProxy, animated: Bool = true) {
        if animated {
            withAnimation(.appSnappy) { proxy.scrollTo("bottom", anchor: .bottom) }
        } else {
            proxy.scrollTo("bottom", anchor: .bottom)
        }
    }
}

/// One task in the thread: the goal, the work, the answer.
struct AgentTaskItem: View {
    let task: AgentTaskSnapshot
    @ObservedObject var agent: AgentController
    /// Offered on the last finished task: save what it did as a flow.
    var onSaveFlow: (() -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            if !task.goal.isEmpty { GoalBubble(text: task.goal) }
            WorkCard(task: task, onOpen: { Task { await agent.loadDetail(task.id) } })
            if task.pendingConfirm != nil {
                Button { agent.reopenConfirm() } label: {
                    Label("这一步要你确认，点这里看", systemImage: "hand.raised.fill")
                        .font(.subheadline.weight(.semibold))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(Theme.Space.m)
                        .background(Theme.attention.opacity(0.16),
                                    in: RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous))
                        .foregroundStyle(Theme.attention)
                }
                .buttonStyle(PressableStyle())
            }
            if let reply = reply {
                AgentBubble(text: reply.text, failed: reply.failed)
            }
            if let onSaveFlow, task.flow == nil {
                Button(action: onSaveFlow) {
                    Label("保存为 flow", systemImage: "square.and.arrow.down")
                        .font(.footnote.weight(.semibold))
                }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("agent-save-flow")
            }
        }
    }

    /// The summary once there is one; else, for an end that is not `done`,
    /// the daemon's error.
    private var reply: (text: String, failed: Bool)? {
        if let summary = task.summary?.trimmingCharacters(in: .whitespacesAndNewlines), !summary.isEmpty {
            return (summary, false)
        }
        guard task.state.isTerminal else { return nil }
        if let error = task.error, !error.isEmpty { return (error, true) }
        return task.state == .done ? nil : (task.state.title, true)
    }
}

/// The person's goal, on the right like a sent message.
struct GoalBubble: View {
    let text: String

    var body: some View {
        HStack {
            Spacer(minLength: 48)
            Text(text)
                .padding(.horizontal, 14).padding(.vertical, 10)
                .background(Theme.accent, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                .foregroundStyle(.white)
                .textSelection(.enabled)
        }
    }
}

/// The agent's reply, on the left.
struct AgentBubble: View {
    let text: String
    var failed = false

    var body: some View {
        HStack {
            Text(text)
                .padding(.horizontal, 14).padding(.vertical, 10)
                .background(failed ? AnyShapeStyle(Theme.down.opacity(0.16)) : AnyShapeStyle(Theme.surface),
                            in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                .foregroundStyle(failed ? Theme.down : Color.primary)
                .textSelection(.enabled)
            Spacer(minLength: 48)
        }
    }
}

/// "Worked for 45s · 13 steps", opening to the steps themselves.
struct WorkCard: View {
    let task: AgentTaskSnapshot
    let onOpen: () -> Void
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button {
                withAnimation(.appSnappy) { open.toggle() }
                if open { onOpen() }
            } label: {
                HStack(spacing: Theme.Space.s) {
                    if task.state.isTerminal {
                        Image(systemName: "sparkles").foregroundStyle(Theme.accent).accessibilityHidden(true)
                    } else {
                        ProgressView().controlSize(.small)
                    }
                    Text(headline).font(.subheadline.weight(.medium)).foregroundStyle(.primary)
                    Spacer(minLength: Theme.Space.s)
                    AgentStateChip(state: task.state)
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.bold))
                        .foregroundStyle(.secondary)
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .accessibilityHidden(true)
                }
                .padding(Theme.Space.m)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityHint(Text(open ? "收起步骤" : "展开步骤"))
            if open {
                Divider().overlay(Theme.hairline)
                VStack(alignment: .leading, spacing: Theme.Space.s) {
                    if let flow = task.flow {
                        Label(String(localized: "用了保存的流程 \(flow.id)"), systemImage: "bolt.fill")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    if task.steps.isEmpty {
                        Text(task.state.isTerminal ? "没有步骤记录" : "还没有步骤")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    ForEach(task.steps) { step in
                        AgentStepRow(step: step)
                    }
                    if !task.modelsUsed.isEmpty {
                        Text(task.modelsUsed.sorted { $0.key < $1.key }.map { "\($0.key): \($0.value)" }
                            .joined(separator: " · "))
                            .font(.caption2).foregroundStyle(.tertiary)
                    }
                }
                .padding(Theme.Space.m)
                .transition(.opacity)
            }
        }
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous).strokeBorder(Theme.hairline))
    }

    private var headline: String {
        let time = AgentFormat.duration(ms: task.elapsedMs)
        let steps = task.steps.count
        if !task.state.isTerminal {
            if task.phase == "summary" { return String(localized: "正在写总结 · \(time)") }
            return steps > 0
                ? String(localized: "正在工作 · \(time) · 第 \(steps) 步")
                : String(localized: "正在工作 · \(time)")
        }
        return steps > 0
            ? String(localized: "工作了 \(time) · \(steps) 步")
            : String(localized: "工作了 \(time)")
    }
}

/// One step: what, on what, who decided, and whether it worked.
struct AgentStepRow: View {
    let step: AgentStep

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            Image(systemName: step.ok ? "checkmark.circle.fill" : "xmark.octagon.fill")
                .foregroundStyle(step.ok ? Theme.ok : Theme.down)
                .imageScale(.small)
                .accessibilityLabel(Text(step.ok ? "成功" : "出错"))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.footnote.weight(.medium))
                if let error = step.error, !error.isEmpty {
                    Text(error).font(.caption).foregroundStyle(Theme.down)
                }
            }
            Spacer(minLength: Theme.Space.s)
            if let by = step.by, !by.isEmpty {
                Chip(text: by, color: by == "assist" ? Theme.attention : (by == "flow" ? Theme.ok : Theme.accent))
            }
            if let ms = step.ms {
                Text(AgentFormat.duration(ms: ms, precise: true)).font(.caption2.monospacedDigit()).foregroundStyle(.tertiary)
            }
        }
        .accessibilityElement(children: .combine)
    }

    /// `3. CLICK「电池」· 设置`, with typed text in quotes.
    private var title: String {
        var text = "\(step.n). \(step.action)"
        if let target = step.target, !target.isEmpty { text += "「\(target)」" }
        if let typed = step.text, !typed.isEmpty { text += " “\(typed)”" }
        if let app = step.app, !app.isEmpty { text += " · \(app)" }
        return text
    }
}

/// A task's status as a chip: running, waiting on the person, the end.
struct AgentStateChip: View {
    let state: AgentTaskState

    var body: some View {
        Chip(text: state.title, symbol: symbol, color: color)
    }

    private var color: Color {
        switch state {
        case .done: return Theme.ok
        case .running, .other: return Theme.accent
        case .needsConfirm, .maxSteps, .unavailable: return Theme.attention
        case .blocked, .failed: return Theme.down
        case .cancelled: return .secondary
        }
    }

    private var symbol: String? {
        switch state {
        case .done: return "checkmark"
        case .needsConfirm: return "hand.raised.fill"
        case .blocked: return "nosign"
        case .failed: return "exclamationmark.triangle.fill"
        case .cancelled: return "stop.fill"
        case .maxSteps: return "flag.checkered"
        case .unavailable: return "questionmark.circle"
        case .running, .other: return nil
        }
    }
}

/// A line above the thread: something to fix first, with its button.
struct AgentNotice: View {
    let text: String
    let symbol: String
    let action: (title: String, run: () -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            Label(text, systemImage: symbol).font(.footnote)
            if let action {
                Button(action.title, action: action.run).font(.footnote.weight(.semibold))
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(Theme.Space.m)
        .background(Theme.attention.opacity(0.12), in: RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous))
    }
}

/// Before the first goal: what this is, with a few to try.
struct AgentEmptyState: View {
    let onPick: (String) -> Void

    private let examples: [String] = [
        String(localized: "打开设置看看电池电量"),
        String(localized: "看看今天的步数"),
        String(localized: "把手机调成勿扰模式"),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Image(systemName: "sparkles")
                .font(.title2.weight(.semibold))
                .frame(width: 48, height: 48)
                .background(Theme.accent.opacity(0.16), in: Circle())
                .foregroundStyle(Theme.accent)
                .accessibilityHidden(true)
            Text("把要做的事说给 agent").font(.title3.weight(.semibold))
            Text("它会在选中的手机上一步步去做，做完告诉你结果。付款、发消息、删除这类步骤会先停下来问你。")
                .font(.subheadline).foregroundStyle(.secondary)
            ForEach(examples, id: \.self) { example in
                Button { onPick(example) } label: {
                    Text(example)
                        .font(.subheadline)
                        .padding(.horizontal, 12).padding(.vertical, 8)
                        .background(Theme.surface, in: Capsule())
                        .overlay(Capsule().strokeBorder(Theme.hairline))
                }
                .buttonStyle(PressableStyle())
                .foregroundStyle(.primary)
            }
        }
        .padding(.top, Theme.Space.xl)
    }
}

enum AgentFormat {
    /// `45s`, `2m 5s` (`45秒`, `2分5秒`), in the device language.
    static func duration(ms: Int, precise: Bool = false) -> String {
        if precise, ms < 10_000 {
            return String(format: "%.1f", Double(ms) / 1_000) + String(localized: "秒")
        }
        let formatter = DateComponentsFormatter()
        formatter.allowedUnits = [.hour, .minute, .second]
        formatter.unitsStyle = .abbreviated
        return formatter.string(from: max(0, Double(ms) / 1_000)) ?? "0"
    }
}

// MARK: - Confirmation

/// A paused task's question, as big as it deserves: what the agent wants to
/// do, where and why, and two answers. Nothing answers it but the person.
struct ConfirmStepSheet: View {
    let prompt: AgentController.ConfirmPrompt
    let answering: Bool
    /// Why the last answer did not go out.
    var error: String?
    /// Close without answering (the thread offers it again); never an answer.
    var onLater: () -> Void = {}
    let onAnswer: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.l) {
            HStack(spacing: Theme.Space.m) {
                Image(systemName: "hand.raised.fill")
                    .font(.title2)
                    .frame(width: 52, height: 52)
                    .background(Theme.attention.opacity(0.18), in: Circle())
                    .foregroundStyle(Theme.attention)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text("这一步要你同意").font(.title3.weight(.bold))
                    Text(prompt.goal).font(.footnote).foregroundStyle(.secondary).lineLimit(2)
                }
            }
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                row("操作", prompt.confirm.action)
                if let target = prompt.confirm.target, !target.isEmpty { row("对象", target) }
                if let app = prompt.confirm.app, !app.isEmpty { row("App", app) }
                if let reason = prompt.confirm.reason, !reason.isEmpty { row("原因", reason) }
            }
            .padding(Theme.Space.m)
            .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous))
            Text("允许只执行这一步一次。5 分钟内不回答，任务会停下。")
                .font(.footnote).foregroundStyle(.secondary)
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.footnote).foregroundStyle(Theme.down)
            }
            Spacer(minLength: 0)
            Button { onAnswer(true) } label: { Text("允许这一步") }
                .buttonStyle(ProminentButtonStyle())
                .accessibilityIdentifier("agent-approve")
            Button { onAnswer(false) } label: { Text("不允许") }
                .buttonStyle(ProminentButtonStyle(filled: false))
                .accessibilityIdentifier("agent-decline")
            Button("稍后再说", action: onLater)
                .font(.subheadline)
                .frame(maxWidth: .infinity)
                .accessibilityHint(Text("关掉这一页，不回答；任务继续等你"))
        }
        .disabled(answering)
        .overlay { if answering { ProgressView() } }
        .padding(Theme.Space.xl)
        .appMediumLargeSheet()
        .interactiveDismissDisabled()
        .background(Theme.canvas.ignoresSafeArea())
    }

    private func row(_ label: LocalizedStringKey, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).font(.subheadline).foregroundStyle(.secondary).frame(width: 56, alignment: .leading)
            Text(value).font(.subheadline.weight(.semibold)).textSelection(.enabled)
        }
    }
}

// MARK: - Pairing 本机

/// How to pair this iPhone's own daemon: the Mac page shows a QR code for it
/// (`iphoneuse://pair?u=http://127.0.0.1:8787&c=…`), scanned like any other.
struct PairThisPhoneSheet: View {
    @ObservedObject var app: AppModel
    @State private var scanning = false
    @State private var note: String?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            VStack(alignment: .leading, spacing: Theme.Space.l) {
                Image(systemName: "iphone.badge.plus")
                    .font(.title)
                    .frame(width: 56, height: 56)
                    .background(Theme.accent.opacity(0.16), in: Circle())
                    .foregroundStyle(Theme.accent)
                    .accessibilityHidden(true)
                Text("让 agent 在这台 iPhone 上做事").font(.title3.weight(.bold))
                Text("本机的 iphone-use 服务跑在这台手机自己的设备进程里。在 Mac 上装好之后，打开 iphone-use 页面，点「本机配对」显示二维码，用这里扫一下（或者复制那个链接再粘贴）。")
                    .font(.subheadline).foregroundStyle(.secondary)
                if let note {
                    Text(note).font(.footnote).foregroundStyle(Theme.attention)
                }
                Spacer()
                Button { scanning = true } label: { Label("扫码配对", systemImage: "qrcode.viewfinder") }
                    .buttonStyle(ProminentButtonStyle())
                Button(action: paste) { Label("粘贴配对链接", systemImage: "doc.on.clipboard") }
                    .buttonStyle(ProminentButtonStyle(filled: false))
            }
            .padding(Theme.Space.xl)
            .background(Theme.canvas.ignoresSafeArea())
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
            }
            .fullScreenCover(isPresented: $scanning) {
                ScanSheet(onFound: use)
            }
        }
    }

    private func paste() {
        guard let text = UIPasteboard.general.string, let link = PairLink.parse(AddressInput.clean(text)) else {
            note = String(localized: "剪贴板里没有配对链接。")
            return
        }
        use(link)
    }

    private func use(_ link: PairLink) {
        guard DeviceStore.isThisPhone(link.base.absoluteString) else {
            note = String(localized: "这个二维码是另一台设备的，不是本机。要添加它，请回到总览扫码。")
            return
        }
        dismiss()
        Task { await app.pair(link) }
    }
}

// MARK: - Model settings

/// The models a task may use: the assist list (tried in order), Jev, and
/// the text model. Keys go to the Keychain as they are typed.
struct ModelSettingsView: View {
    @ObservedObject var agent: AgentController
    @State private var jevKey = ""
    @State private var textKey = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        Form {
            Section {
                Toggle("用这里的模型", isOn: $agent.settings.sendModels)
            } footer: {
                Text("关掉后不发送模型设置，由执行任务的设备用它自己配置的模型（例如 Mac 上的环境变量）。")
            }
            if agent.settings.sendModels {
                Section {
                    ForEach($agent.settings.assist) { $provider in
                        NavigationLink {
                            ProviderEditView(provider: $provider)
                        } label: {
                            ProviderRow(provider: provider)
                        }
                    }
                    .onMove { agent.settings.assist.move(fromOffsets: $0, toOffset: $1) }
                    .onDelete(perform: remove)
                    Menu {
                        ForEach(ModelProvider.Kind.allCases, id: \.self) { kind in
                            Button { agent.settings.assist.append(ModelProvider(kind: kind)) } label: {
                                Text(kind.title)
                            }
                        }
                    } label: {
                        Label("添加模型", systemImage: "plus")
                    }
                } header: {
                    Text("助手模型")
                } footer: {
                    Text("按顺序尝试，第一个能回答的就用它。按住右边拖动可以调整顺序。助手模型会看到屏幕截图。")
                }
                Section {
                    SecureField("TypeSafe 密钥", text: $jevKey)
                        .textContentType(.password)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .appOnChange(of: jevKey) { _, value in
                            ModelSettingsStore.setSecret(value, for: ModelSettings.jevKeyAccount)
                        }
                    TextField("模型", text: $agent.settings.jevModel)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                } header: {
                    Text("Jev（可选）")
                } footer: {
                    Text("有密钥时由 Jev 快速决定每一步，助手模型只在它卡住时接手；没有就全部交给助手模型。")
                }
                Section {
                    TextField("地址", text: $agent.settings.textBaseURL)
                        .keyboardType(.URL)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    SecureField("密钥", text: $textKey)
                        .textContentType(.password)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .appOnChange(of: textKey) { _, value in
                            ModelSettingsStore.setSecret(value, for: ModelSettings.textKeyAccount)
                        }
                    TextField("模型", text: $agent.settings.textModel)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                } header: {
                    Text("文字模型（可选）")
                } footer: {
                    Text("输入文字时用。地址是 OpenRouter 又没填密钥时，会用列表里 OpenRouter 的密钥。")
                }
            }
        }
        .navigationTitle("模型")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) { Button("完成") { dismiss() } }
            ToolbarItem(placement: .navigationBarLeading) {
                if agent.settings.sendModels { EditButton() }
            }
        }
        .onAppear {
            jevKey = ModelSettingsStore.secret(ModelSettings.jevKeyAccount) ?? ""
            textKey = ModelSettingsStore.secret(ModelSettings.textKeyAccount) ?? ""
        }
    }

    private func remove(_ offsets: IndexSet) {
        for index in offsets {
            Keychain.delete(for: agent.settings.assist[index].keyAccount)
        }
        agent.settings.assist.remove(atOffsets: offsets)
    }
}

/// A list row: name, model, and whether it will be sent.
struct ProviderRow: View {
    let provider: ModelProvider

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(provider.name).font(.body.weight(.medium))
                if !provider.enabled {
                    Chip(text: String(localized: "已关闭"))
                } else if missing {
                    Chip(text: String(localized: "缺密钥"), color: Theme.attention)
                } else if provider.isFree {
                    Chip(text: String(localized: "免费"), color: Theme.ok)
                }
            }
            Text(provider.model == "free" ? String(localized: "自动选择免费模型") : (provider.model.isEmpty ? String(localized: "未填模型") : provider.model))
                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
        }
    }

    private var missing: Bool {
        provider.needsKey && (ModelSettingsStore.secret(provider.keyAccount) ?? "").isEmpty
    }
}

/// One assist model: a preset keeps its address; custom ones choose it.
struct ProviderEditView: View {
    @Binding var provider: ModelProvider
    @State private var key = ""
    @State private var checking = false
    @State private var checkResult: String?
    @State private var checkPassed = false
    @State private var checkTask: Task<Void, Never>?

    var body: some View {
        Form {
            Section {
                Toggle("使用", isOn: $provider.enabled)
                TextField("名称", text: $provider.name)
            } footer: {
                Text(provider.kind.detail)
            }
            Section("接口") {
                if provider.editableEndpoint {
                    Picker("格式", selection: $provider.format) {
                        Text("OpenAI").tag(ModelProvider.Format.openai)
                        Text("Anthropic").tag(ModelProvider.Format.anthropic)
                    }
                    TextField("地址", text: $provider.baseURL, prompt: Text(verbatim: "https://…/v1"))
                        .keyboardType(.URL)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                } else {
                    AppLabeledContent("地址", value: provider.baseURL)
                }
                if provider.needsKey {
                    SecureField("密钥", text: $key)
                        .textContentType(.password)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .appOnChange(of: key) { _, value in
                            ModelSettingsStore.setSecret(value, for: provider.keyAccount)
                        }
                } else {
                    AppLabeledContent("密钥", value: provider.kind == .zen ? "public" : String(localized: "无需密钥"))
                }
            }
            Section {
                NavigationLink {
                    ModelPickerView(provider: $provider)
                } label: {
                    Label("从服务选择模型", systemImage: "list.bullet")
                }
                TextField("模型", text: $provider.model)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
            } header: {
                Text("模型")
            } footer: {
                if provider.kind == .free || provider.kind == .openRouter {
                    Text("填 free 会自动挑一个能用的免费模型（能看截图的优先），不能用就换下一个；也可以填某个以 :free 结尾的模型。")
                }
            }
            Section {
                Button {
                    checking = true
                    checkResult = nil
                    let tested = provider
                    let testedKey = key
                    checkTask = Task { @MainActor in
                        do {
                            try await ModelCatalog.check(tested)
                            guard provider == tested, key == testedKey else { checking = false; return }
                            checkPassed = true
                            checkResult = String(localized: "连接正常，模型已返回有效工具调用")
                        } catch {
                            guard !Task.isCancelled else { checking = false; return }
                            guard provider == tested, key == testedKey else { checking = false; return }
                            checkPassed = false
                            checkResult = error.localizedDescription
                        }
                        checking = false
                    }
                } label: {
                    HStack {
                        Text("检测连接")
                        Spacer()
                        if checking { ProgressView() }
                    }
                }
                .disabled(checking || provider.model.isEmpty || provider.baseURL.isEmpty)
                if let checkResult {
                    Text(checkResult).font(.footnote).foregroundStyle(checkPassed ? Theme.ok : Theme.attention)
                }
            } footer: {
                Text("只发送一条测试请求，不包含手机画面。模型列表能读取不代表任务能执行；收费模型会产生少量费用。")
            }
        }
        .navigationTitle(provider.name)
        .navigationBarTitleDisplayMode(.inline)
        .onAppear { key = ModelSettingsStore.secret(provider.keyAccount) ?? "" }
        .onDisappear { checkTask?.cancel(); checking = false }
        .appOnChange(of: provider) { _, _ in checkResult = nil }
        .appOnChange(of: key) { _, _ in checkResult = nil }
    }
}

struct ModelPickerView: View {
    @Binding var provider: ModelProvider
    @State private var models: [CatalogModel] = []
    @State private var loading = false
    @State private var error: String?
    @State private var search = ""
    @Environment(\.dismiss) private var dismiss

    private var filtered: [CatalogModel] {
        models.filter { search.isEmpty || $0.name.localizedCaseInsensitiveContains(search) || $0.id.localizedCaseInsensitiveContains(search) }
    }

    var body: some View {
        List {
            if provider.kind == .free || provider.kind == .openRouter {
                Button {
                    provider.model = "free"
                    dismiss()
                } label: {
                    HStack {
                        Text("自动选择免费模型")
                        Spacer()
                        if provider.model == "free" { Image(systemName: "checkmark") }
                    }
                }
            }
            if loading { ProgressView("正在读取模型") }
            if let error { Text(error).font(.footnote).foregroundStyle(Theme.attention) }
            ForEach(filtered) { model in
                Button {
                    provider.model = model.id
                    dismiss()
                } label: {
                    HStack {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(model.name)
                            if model.name != model.id { Text(model.id).font(.caption).foregroundStyle(.secondary) }
                            if model.readsImages { Text("支持截图").font(.caption).foregroundStyle(.secondary) }
                        }
                        Spacer()
                        if provider.model == model.id { Image(systemName: "checkmark") }
                    }
                }
            }
        }
        .navigationTitle("选择模型")
        .searchable(text: $search, placement: .navigationBarDrawer(displayMode: .always), prompt: "搜索模型")
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button("刷新") { Task { await refresh() } }.disabled(loading)
            }
        }
        .task { await refresh() }
    }

    @MainActor private func refresh() async {
        loading = true
        error = nil
        defer { loading = false }
        do {
            models = try await ModelCatalog.fetch(provider)
        } catch is CancellationError {
        } catch {
            self.error = error.localizedDescription
        }
    }
}
