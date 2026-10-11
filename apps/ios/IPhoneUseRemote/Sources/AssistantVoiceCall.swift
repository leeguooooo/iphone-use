import SwiftUI
import Combine

@MainActor
final class AssistantVoiceCall: ObservableObject {
    @Published private(set) var active = false
    @Published private(set) var connecting = false
    @Published private(set) var muted = false
    @Published private(set) var paused = false
    @Published private(set) var thinking = false
    @Published private(set) var speaking = false
    @Published private(set) var speakerEnabled = true
    @Published private(set) var status = ""
    @Published private(set) var transcript = ""
    @Published private(set) var reply = ""
    @Published private(set) var error: String?
    @Published private(set) var messages: [AssistantVoiceMessage] = []
    @Published private(set) var level: Float = 0
    @Published private(set) var startedAt: Date?
    @Published var showing = false

    private let audio: any AssistantVoiceAudio
    private let reasoning: any AssistantVoiceReasoning
    private var phone: (any AssistantPhoneControlling)?
    private var run: Task<Void, Never>?
    private var generation = UUID()
    private var callID = UUID()
    private var announced = Set<String>()
    private var pendingAnnouncements: [String] = []
    private var hearing = false
    private var latestPhone = AssistantPhoneState()
    private var submitting = false

    init(audio: any AssistantVoiceAudio = NativeAssistantVoiceAudio(), reasoning: any AssistantVoiceReasoning = AssistantConversationModel()) {
        self.audio = audio
        self.reasoning = reasoning
        audio.onPartial = { [weak self] in self?.heardPartial($0) }
        audio.onFinal = { [weak self] in self?.heardFinal($0) }
        audio.onError = { [weak self] in self?.audioFailed($0) }
        audio.onLevel = { [weak self] in self?.level = $0 }
        audio.onPlaybackEnd = { [weak self] in
            guard let self else { return }
            self.speaking = false
            self.refreshStatus()
            self.flushAnnouncements()
        }
        audio.onStatus = { [weak self] status, paused in
            guard let self, self.active else { return }
            self.paused = paused
            self.status = status
            if paused { self.invalidate(); self.hearing = false; self.speaking = false }
            else { self.refreshStatus(); self.flushAnnouncements() }
        }
    }

    func start(agent: AgentController, deviceID: UUID) async {
        await start(phone: AssistantPhoneControl(agent: agent, deviceID: deviceID))
    }

    /// Injectable boundaries let regression tests exercise late model replies,
    /// task replacements, disconnections, and audio interruptions without a mic.
    func start(phone: any AssistantPhoneControlling) async {
        guard !active, !connecting else { phone.disconnect(); return }
        guard phone.state.valid, phone.state.connected else {
            error = "本机执行服务未连接，请先完成本机配置。"
            phone.disconnect()
            return
        }
        connecting = true
        showing = true
        error = nil
        status = "正在接通"
        callID = UUID()
        let token = callID
        self.phone = phone
        do {
            try await audio.start()
            guard callID == token else { return }
            guard phone.state.valid else { end(); return }
            active = true
            connecting = false
            startedAt = Date()
            muted = false; paused = false; speakerEnabled = true
            transcript = ""; reply = ""; messages = []
            announced.removeAll(); pendingAnnouncements.removeAll()
            latestPhone = phone.state
            if let task = phone.state.task, task.state.isTerminal { announced.insert(Self.announcementKey(task)) }
            phone.onChange = { [weak self] in self?.updatePhone() }
            phone.maintainConnection(true)
            say("已接通。你可以直接说想做什么，也可以和我聊天。")
            updatePhone()
        } catch {
            guard callID == token else { return }
            end()
            self.error = error.localizedDescription
            status = "未接通"
        }
    }

    func end() {
        callID = UUID()
        invalidate()
        phone?.disconnect(); phone = nil
        active = false; connecting = false; paused = false; muted = false
        speaking = false; hearing = false; submitting = false
        pendingAnnouncements.removeAll()
        audio.end()
        status = "通话已结束"
        startedAt = nil
    }

    func reconnect() async {
        guard let phone else { return }
        // Preserve the same device binding, but abandon all old audio/model
        // callbacks. A new connection never replays an interrupted instruction.
        invalidate()
        active = false; connecting = true; paused = false
        audio.end()
        let token = UUID(); callID = token
        do {
            try await audio.start()
            guard callID == token else { return }
            guard phone.state.valid else { end(); return }
            active = true; connecting = false; muted = false; speaking = false
            error = nil
            startedAt = Date()
            say("语音已重新连接。")
            updatePhone()
        } catch {
            guard callID == token else { return }
            connecting = false; active = true; paused = true
            self.error = error.localizedDescription
            status = "连接失败，点击重连"
        }
    }

    func resume() {
        do { try audio.resume(); paused = false; error = nil; refreshStatus(); flushAnnouncements() }
        catch { self.error = error.localizedDescription }
    }

    func toggleMute() {
        guard active else { return }
        muted.toggle()
        audio.setMuted(muted)
        if muted { invalidate(); hearing = false; transcript = "" }
        refreshStatus()
        flushAnnouncements()
    }

    func toggleSpeaker() {
        do { try audio.setSpeaker(!speakerEnabled); speakerEnabled.toggle() }
        catch { self.error = "音频输出切换失败，请检查耳机连接。" }
    }

    func interruptReply() {
        invalidate()
        audio.interrupt()
        speaking = false
        refreshStatus()
    }

    private func heardPartial(_ text: String) {
        guard active, !muted, !paused else { return }
        transcript = text
        if !hearing {
            hearing = true
            invalidate() // A late reply cannot launch a task after being cut off.
            audio.interrupt()
            speaking = false
        }
        status = "正在听你说"
    }

    private func heardFinal(_ input: String) {
        guard active, !muted, !paused else { return }
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { hearing = false; flushAnnouncements(); return }
        hearing = false
        transcript = text
        append(.user, text)
        invalidate()
        let token = generation, session = callID
        guard let phone, phone.state.valid else { end(); return }
        if Self.isStop(text) {
            run = Task { [weak self] in await self?.stopPhone(token: token, session: session) }
            return
        }
        if Self.isStatus(text) { say(phone.state.spokenStatus); return }
        error = nil
        thinking = true
        refreshStatus()
        let history = messages, evidence = phone.state, models = phone.models
        run = Task { [weak self] in
            guard let self else { return }
            do {
                let decision = try await self.reasoning.respond(messages: history, phone: evidence, models: models)
                guard self.current(token, session) else { return }
                try Task.checkCancellation()
                await self.apply(decision, observed: evidence, token: token, session: session)
            } catch {
                guard self.current(token, session), !Task.isCancelled else { return }
                self.thinking = false
                self.error = "对话模型暂时没有响应。可以重说，或检查模型设置；手机任务不受影响。"
                self.say(self.error!)
            }
        }
    }

    private func apply(_ decision: AssistantVoiceDecision, observed: AssistantPhoneState, token: UUID, session: UUID) async {
        guard current(token, session), let phone, phone.state.valid else { return }
        thinking = false
        switch decision.action {
        case .reply: say(decision.reply)
        case .status: say(phone.state.spokenStatus)
        case .stop:
            guard phone.state.task?.id == observed.task?.id, phone.state.busy == observed.busy else {
                say("手机任务状态已变化，请重新说明要停止哪个任务。")
                return
            }
            await stopPhone(token: token, session: session)
        case .run, .replace:
            guard phone.state.connected, phone.state.linkNote == nil else { say(phone.state.spokenStatus); return }
            // A changed task invalidates permission to replace the task seen by
            // the model. Never stop a different task that appeared meanwhile.
            if decision.action == .replace && phone.state.busy {
                guard phone.state.task?.id == observed.task?.id, observed.busy else {
                    say("手机任务状态已变化。请重新说明要停止哪个任务、改做什么。")
                    return
                }
                await phone.stop()
                guard current(token, session) else { return }
                if let error = phone.state.error { say(error); return }
                status = "等待旧任务停止"
                let oldTaskID = observed.task?.id
                for _ in 0..<60 {
                    guard current(token, session), phone.state.valid else { return }
                    if !phone.state.connected || phone.state.linkNote != nil { say(phone.state.spokenStatus); return }
                    if let oldTaskID, let actualID = phone.state.task?.id, actualID != oldTaskID {
                        say("手机上出现了另一个任务，新的指令没有启动。请重新说明要做什么。")
                        return
                    }
                    if !phone.state.busy { break }
                    try? await Task.sleep(for: .milliseconds(250))
                }
                guard current(token, session) else { return }
                guard !phone.state.busy else { say("旧任务还没有确认停止，新的任务没有启动。请查看手机后再试。"); return }
            }
            guard !phone.state.busy, !submitting else {
                say("当前任务还在执行。如果要改做新任务，请告诉我停止当前任务并说明新目标。")
                return
            }
            guard let goal = decision.goal, !goal.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                say("请具体告诉我你想在手机上做什么。")
                return
            }
            submitting = true
            status = "正在提交任务"
            // Keep the POST alive after a voice interruption; cancelling a
            // request whose response is pending makes acceptance ambiguous.
            let submission = Task { await phone.start(goal: goal) }
            let accepted = await submission.value
            // This request may already be accepted if the user hangs up or
            // speaks over it. Observe via the phone controller, never retry it.
            guard callID == session else { return }
            submitting = false
            guard current(token, session) else { return }
            if !accepted { say(phone.state.error ?? "任务没有被执行服务接受，请查看本机连接。") }
            else if phone.state.task?.state.isTerminal == true || phone.state.task?.pendingConfirm != nil { updatePhone(); flushAnnouncements() }
            else { say("收到，开始操作：" + String(goal.prefix(120))) }
        }
        refreshStatus()
        flushAnnouncements()
    }

    func stopTask() {
        guard active else { return }
        invalidate(); audio.interrupt(); speaking = false; hearing = false
        let token = generation, session = callID
        run = Task { [weak self] in await self?.stopPhone(token: token, session: session) }
    }

    private func stopPhone(token: UUID, session: UUID) async {
        guard let phone, phone.state.valid else { end(); return }
        guard phone.state.busy else { say("当前没有正在执行的任务。"); return }
        guard phone.state.connected, phone.state.linkNote == nil else { say("停止请求没有送达。" + phone.state.spokenStatus); return }
        await phone.stop()
        guard current(token, session) else { return }
        say(phone.state.error ?? "已请求停止，等待执行服务确认。")
    }

    private func updatePhone() {
        guard active, let phone else { return }
        let state = phone.state
        guard state.valid else { end(); error = "本机设备已切换，语音通话已结束。"; return }
        defer { latestPhone = state }
        if let note = state.linkNote, note != latestPhone.linkNote {
            enqueue("与执行服务的连接中断。任务可能仍在执行，请查看手机。")
        } else if !state.connected, latestPhone.connected {
            enqueue("本机执行服务已断开。你仍可以和我说话，手机操作需要恢复连接。")
        } else if state.connected, !latestPhone.connected { enqueue("本机执行服务已重新连接。") }
        guard let task = state.task else { return }
        let key = Self.announcementKey(task)
        if task.pendingConfirm != nil || task.state.isTerminal {
            if announced.insert(key).inserted { enqueue(state.spokenStatus) }
        }
    }

    private func enqueue(_ text: String) {
        pendingAnnouncements.append(text)
        pendingAnnouncements = Array(pendingAnnouncements.suffix(4))
        flushAnnouncements()
    }
    private func flushAnnouncements() {
        guard active, !paused, !hearing, !thinking, !speaking, !submitting, !pendingAnnouncements.isEmpty else { return }
        say(pendingAnnouncements.removeFirst())
    }
    private func say(_ text: String) {
        guard active else { return }
        reply = text
        append(.assistant, text)
        speaking = !paused
        if !paused { audio.speak(text) }
        refreshStatus()
    }
    private func append(_ role: AssistantVoiceMessage.Role, _ text: String) {
        messages.append(.init(role: role, text: text))
        messages = Array(messages.suffix(80))
    }
    private func audioFailed(_ message: String) {
        guard active else { return }
        invalidate(); audio.pause(); speaking = false; hearing = false; paused = true
        error = message
        status = "语音连接暂停"
    }
    private func refreshStatus() {
        guard active, !paused else { return }
        if muted { status = "麦克风已静音" }
        else if hearing { status = "正在听你说" }
        else if thinking { status = "正在思考，可以打断" }
        else if speaking { status = "助手正在说话，可以打断" }
        else if phone?.state.busy == true { status = "正在操作手机，也在听你说" }
        else { status = "正在听" }
    }
    private func invalidate() { generation = UUID(); run?.cancel(); run = nil; thinking = false }
    private func current(_ token: UUID, _ session: UUID) -> Bool { active && !paused && generation == token && callID == session }

    nonisolated static func isStop(_ text: String) -> Bool {
        ["停止", "停下", "停止任务", "停止操作", "取消任务", "别操作了", "先停一下"].contains(AssistantVoiceEchoGate.normalized(text))
    }
    nonisolated static func isStatus(_ text: String) -> Bool {
        ["现在怎么样了", "做到哪了", "做完了吗", "任务进度", "现在什么进度"].contains(AssistantVoiceEchoGate.normalized(text))
    }
    nonisolated static func announcementKey(_ task: AgentTaskSnapshot) -> String {
        "\(task.id)|\(String(describing: task.state))|\(String(describing: task.confirm))"
    }
}

struct AssistantVoiceCallView: View {
    @ObservedObject var call: AssistantVoiceCall
    @ObservedObject var agent: AgentController
    @State private var models = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            VStack(spacing: 16) {
                VStack(spacing: 8) {
                    Image(systemName: call.paused ? "phone.badge.waveform" : "waveform.circle.fill")
                        .font(.system(size: 64)).foregroundStyle(Theme.accent)
                        .scaleEffect(1 + CGFloat(call.level) * 0.12)
                        .animation(.easeOut(duration: 0.12), value: call.level)
                    Text(call.status).font(.headline).accessibilityIdentifier("voice-call-status")
                    if let date = call.startedAt { Text(date, style: .timer).font(.caption.monospacedDigit()).foregroundStyle(.secondary) }
                }.padding(.top)
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 16) {
                            ForEach(call.messages) { message in
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(message.role == .user ? "你" : "助手").font(.caption).foregroundStyle(.secondary)
                                    Text(message.text).textSelection(.enabled)
                                }.frame(maxWidth: .infinity, alignment: message.role == .user ? .trailing : .leading)
                                    .padding(12).background(Theme.surface, in: RoundedRectangle(cornerRadius: 16))
                            }
                            if !call.transcript.isEmpty {
                                Text(call.transcript).foregroundStyle(.secondary).accessibilityIdentifier("voice-call-transcript")
                            }
                            Color.clear.frame(height: 1).id("voice-bottom")
                        }.padding(.horizontal)
                    }
                    .appOnChange(of: call.messages.count) { _, _ in proxy.scrollTo("voice-bottom", anchor: .bottom) }
                    .appOnChange(of: call.transcript) { _, _ in proxy.scrollTo("voice-bottom", anchor: .bottom) }
                }
                if let error = call.error { Text(error).font(.footnote).foregroundStyle(.red).padding(.horizontal) }
                if let task = agent.live?.task, task.pendingConfirm != nil {
                    Button("查看待确认操作") { agent.reopenConfirm() }.buttonStyle(.borderedProminent)
                }
                if call.paused {
                    if call.error == nil { Button("继续通话") { call.resume() }.buttonStyle(.borderedProminent) }
                    else { Button("重新连接语音") { Task { await call.reconnect() } }.buttonStyle(.borderedProminent) }
                } else if !call.active && !call.connecting {
                    Button("重新接通") {
                        if let id = agent.targetID { Task { await call.start(agent: agent, deviceID: id) } }
                    }.buttonStyle(.borderedProminent).disabled(agent.target?.record.isThisPhone != true)
                }
                HStack(spacing: 8) {
                    control(call.muted ? "取消静音" : "静音", call.muted ? "mic.slash.fill" : "mic.fill", action: call.toggleMute)
                    control(call.speakerEnabled ? "扬声器" : "听筒", call.speakerEnabled ? "speaker.wave.2.fill" : "ear", action: call.toggleSpeaker)
                    control("停止任务", "stop.fill", action: call.stopTask)
                    Button { call.end() } label: {
                        VStack(spacing: 8) { Image(systemName: "phone.down.fill").font(.title2); Text("挂断").font(.caption) }
                            .foregroundStyle(.white).padding(12).frame(maxWidth: .infinity).background(.red, in: RoundedRectangle(cornerRadius: 18))
                    }.accessibilityIdentifier("voice-call-hangup")
                }.padding(.horizontal)
                if call.speaking || call.thinking { Button("打断回复") { call.interruptReply() }.accessibilityIdentifier("voice-call-interrupt") }
                Text("挂断只结束语音。停止手机操作请说“停止”。需要确认的操作会在屏幕上询问你。")
                    .font(.caption).foregroundStyle(.secondary).padding(.horizontal).padding(.bottom, 8)
            }
            .navigationTitle("语音通话")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) { Button("模型设置") { models = true } }
                ToolbarItem(placement: .navigationBarTrailing) { Button("收起") { dismiss() } }
            }
            .sheet(isPresented: $models) { AppNavigationStack { ModelSettingsView(agent: agent) } }
            .sheet(item: $agent.confirmPrompt) { prompt in
                ConfirmStepSheet(prompt: prompt, answering: agent.answering, error: agent.error,
                                 onLater: { agent.confirmPrompt = nil }) { approve in
                    Task { await agent.answer(prompt, approve: approve) }
                }
            }
        }
    }
    private func control(_ text: String, _ symbol: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            VStack(spacing: 8) { Image(systemName: symbol).font(.title2); Text(text).font(.caption).lineLimit(1).minimumScaleFactor(0.65) }
                .padding(12).frame(maxWidth: .infinity).background(Theme.surface, in: RoundedRectangle(cornerRadius: 18))
        }.disabled(!call.active || call.paused)
    }
}

struct AssistantVoiceCallBanner: View {
    @ObservedObject var call: AssistantVoiceCall
    var body: some View {
        if call.active || call.connecting {
            HStack {
                Button { call.showing = true } label: { Label(call.status, systemImage: "waveform").font(.footnote).lineLimit(1) }
                Spacer()
                Button { call.end() } label: { Image(systemName: "phone.down.fill").foregroundStyle(.red) }.accessibilityLabel("挂断语音通话")
            }.padding(12).background(.bar).accessibilityIdentifier("voice-call-banner")
        }
    }
}
