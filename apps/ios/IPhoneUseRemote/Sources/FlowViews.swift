import Combine
import SwiftUI

// MARK: - Recording

/// A recording in the remote view: started and stopped on the device's
/// daemon, its step count read every 2 s while it runs, its draft kept for
/// the save sheet. The daemon owns the recording; this only mirrors it, so
/// a remote view opened later shows one still running.
@MainActor
final class FlowRecorder: ObservableObject {
    @Published private(set) var recording = false
    @Published private(set) var steps = 0
    @Published private(set) var busy = false
    /// Set when a recording stops with something in it: the save sheet.
    @Published var draft: FlowDraft?
    @Published var message: String?

    private weak var session: DeviceSession?
    private var pollTask: Task<Void, Never>?

    init(session: DeviceSession) {
        self.session = session
    }

    /// Read whether a recording already runs there (the view appeared).
    func sync() async {
        guard let client = session?.client, let state = try? await client.recordingState() else { return }
        apply(state)
    }

    func toggle() async {
        recording ? await stop() : await start()
    }

    func start() async {
        guard let client = session?.client, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            try await client.startRecording()
            apply(RecordingState(recording: true, steps: 0))
            session?.show(String(localized: "开始录制：接下来的操作会记成 flow"))
        } catch {
            session?.show(error.localizedDescription)
        }
    }

    func stop() async {
        guard let client = session?.client, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            let draft = try await client.stopRecording()
            apply(RecordingState(recording: false, steps: steps))
            if let draft, draft.flow != nil, draft.steps > 0 {
                self.draft = draft
            } else {
                session?.show(String(localized: "这次没有录到步骤"))
            }
        } catch {
            session?.show(error.localizedDescription)
        }
    }

    func leave() {
        pollTask?.cancel()
        pollTask = nil
    }

    private func apply(_ state: RecordingState) {
        recording = state.recording
        steps = state.steps
        if recording, pollTask == nil {
            pollTask = Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(2))
                    guard let self, let client = self.session?.client else { return }
                    if let state = try? await client.recordingState() {
                        self.recording = state.recording
                        self.steps = state.steps
                        if !state.recording { break }
                    }
                }
                self?.pollTask = nil
            }
        } else if !recording {
            pollTask?.cancel()
            pollTask = nil
        }
    }
}

/// The red pill over the remote view while recording: step count and Stop.
struct RecordingPill: View {
    @ObservedObject var recorder: FlowRecorder

    var body: some View {
        if recorder.recording {
            Button {
                Task { await recorder.stop() }
            } label: {
                HStack(spacing: 6) {
                    Circle().fill(Theme.down).frame(width: 8, height: 8).accessibilityHidden(true)
                    Text("录制中 · \(recorder.steps) 步").monospacedDigit()
                    Text("停止").fontWeight(.bold)
                }
                .font(.footnote.weight(.semibold))
                .padding(.horizontal, 12).padding(.vertical, 7)
                .glassBackground(Capsule())
            }
            .buttonStyle(PressableStyle())
            .foregroundStyle(.primary)
            .disabled(recorder.busy)
            .accessibilityIdentifier("flow-recording")
            .padding(.bottom, Theme.Space.s)
        }
    }
}

// MARK: - Saving

/// Name a recording (or what the last task did) and save it as a local flow
/// on that daemon. The id is made from the app and the name, and can be edited.
struct SaveFlowSheet: View {
    @ObservedObject var session: DeviceSession
    /// A stopped recording's draft; nil saves the daemon's last trail (a
    /// finished task), read here only to fill in the app and the checklist.
    let draft: FlowDraft?
    var onSaved: (String) -> Void = { _ in }
    @State private var name = ""
    @State private var detail = ""
    @State private var id = ""
    @State private var idEdited = false
    @State private var shown: FlowDraft?
    @State private var loading = false
    @State private var saving = false
    @State private var error: String?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            Form {
                Section {
                    TextField("名称，例如 查电量", text: $name)
                    AppTextField("说明（可选）", text: $detail, axis: .vertical)
                        .appLineLimit(1...4)
                } footer: {
                    if let shown {
                        Text("\(shown.label ?? shown.bundle ?? String(localized: "未知 App")) · \(shown.steps) 步")
                    }
                }
                Section {
                    TextField("app/name", text: Binding(get: { id }, set: { id = $0; idEdited = true }))
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .font(.body.monospaced())
                } header: {
                    Text("ID")
                } footer: {
                    Text(FlowID.isValid(id)
                         ? String(localized: "定时任务和 agent 用这个 ID 找到它。")
                         : String(localized: "ID 是「app/名字」：小写字母、数字、- 或 _。"))
                }
                if let todo = shown?.todo, !todo.isEmpty {
                    Section("保存后可以再检查") {
                        ForEach(todo, id: \.self) { item in
                            Label(item, systemImage: "checklist").font(.footnote)
                        }
                    }
                }
                if let error {
                    Section { Text(error).foregroundStyle(Theme.down).font(.footnote) }
                } else if loading {
                    Section { ProgressView("读取流程步骤…") }
                } else if shown?.flow == nil {
                    Section { Text("这次没有可保存的步骤。先录制操作，或完成一个包含手机操作的任务。")
                        .font(.footnote).foregroundStyle(.secondary) }
                }
            }
            .navigationTitle("保存为 flow")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("保存") { Task { await save() } }
                        .disabled(loading || shown?.flow == nil || saving || !FlowID.isValid(id) || name.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .disabled(saving)
            .appOnChange(of: name) { _, _ in deriveID() }
            .task {
                loading = true
                defer { loading = false }
                do {
                    if let draft { shown = draft }
                    else { shown = try await session.client?.lastDraft() }
                } catch {
                    self.error = error.localizedDescription
                }
                deriveID()
            }
        }
    }

    private func deriveID() {
        guard !idEdited else { return }
        id = FlowID.make(bundle: shown?.bundle, label: shown?.label, name: name)
    }

    private func save() async {
        guard let client = session.client else {
            error = String(localized: "设备没有连上")
            return
        }
        saving = true
        defer { saving = false }
        do {
            let saved = try await client.saveFlow(id: id, name: name.trimmingCharacters(in: .whitespaces),
                                                  description: detail.trimmingCharacters(in: .whitespacesAndNewlines),
                                                  flow: shown?.flow)
            session.show(String(localized: "已保存 flow \(saved)"))
            onSaved(saved)
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

// MARK: - Flows and schedules

/// A device's flows (registry and its own) and its schedules.
struct FlowsHubView: View {
    @ObservedObject var session: DeviceSession
    @Environment(\.scenePhase) private var scenePhase
    enum Page: Hashable { case flows, schedules }
    @State var page: Page = .flows
    @State private var flows: [FlowEntry] = []
    @State private var schedules: [FlowSchedule] = []
    @State private var loading = false
    @State private var error: String?
    @State private var newSchedule: FlowEntry?
    @State private var creating = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        AppNavigationStack {
            List {
                Section {
                    Picker("", selection: $page) {
                        Text("Flow").tag(Page.flows)
                        Text("定时").tag(Page.schedules)
                    }
                    .pickerStyle(.segmented)
                    .listRowBackground(Color.clear)
                    .listRowInsets(EdgeInsets())
                }
                if let error {
                    Section { Text(error).font(.footnote).foregroundStyle(Theme.down) }
                }
                switch page {
                case .flows: flowSections
                case .schedules: scheduleSections
                }
            }
            .overlay { if loading && flows.isEmpty && schedules.isEmpty { ProgressView() } }
            .refreshable { await load() }
            .navigationTitle(session.name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("完成") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    if page == .schedules {
                        Button { creating = true } label: { Label("新建定时", systemImage: "plus") }.disabled(flows.isEmpty)
                    }
                }
            }
            .sheet(item: $newSchedule) { flow in
                ScheduleEditor(session: session, flows: flows, initial: flow) { Task { await load() } }
            }
            .sheet(isPresented: $creating) {
                ScheduleEditor(session: session, flows: flows, initial: nil) { Task { await load() } }
            }
            .task { await load() }
            .appOnChange(of: scenePhase) { _, phase in
                if phase == .active { Task { await load() } }
            }
        }
    }

    @ViewBuilder private var flowSections: some View {
        let local = flows.filter(\.isLocal)
        let official = flows.filter { !$0.isLocal }
        if flows.isEmpty, !loading {
            Section {
                Text("这台设备还没有 flow。在遥控画面里点「录制」，或者让 agent 做完一件事后「保存为 flow」。")
                    .font(.footnote).foregroundStyle(.secondary)
            }
        }
        if !local.isEmpty {
            Section("我保存的") {
                ForEach(local) { flow in row(flow) }
            }
        }
        if !official.isEmpty {
            Section("流程库") {
                ForEach(official) { flow in row(flow) }
            }
        }
    }

    private func row(_ flow: FlowEntry) -> some View {
        NavigationLink {
            FlowDetailView(session: session, flow: flow) { newSchedule = flow }
        } label: {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(flow.name).font(.body.weight(.medium)).lineLimit(1)
                    if flow.isLocal { Chip(text: String(localized: "本地"), color: Theme.accent) }
                    if flow.hasSideEffects { Chip(text: String(localized: "有副作用"), color: Theme.attention) }
                }
                Text(flow.id).font(.caption.monospaced()).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private var scheduleSections: some View {
        if schedules.isEmpty, !loading {
            Section {
                Text("还没有定时任务。点右上角的 +，选一个 flow 和时间。")
                    .font(.footnote).foregroundStyle(.secondary)
            }
        }
        ForEach(schedules) { schedule in
            Section {
                NavigationLink {
                    ScheduleDetailView(session: session, schedule: schedule) { Task { await load() } }
                } label: {
                    ScheduleRow(schedule: schedule, flowName: flows.first { $0.id == schedule.target }?.name)
                }
                Toggle("启用", isOn: Binding(get: { schedule.enabled }, set: { value in
                    Task { await setEnabled(schedule, value) }
                }))
            }
        }
    }

    private func setEnabled(_ schedule: FlowSchedule, _ enabled: Bool) async {
        do {
            try await session.client?.setSchedule(schedule.id, enabled: enabled)
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func load() async {
        guard let client = session.client else {
            error = String(localized: "「\(session.name)」现在没有连上：\(session.shortState)")
            return
        }
        loading = true
        defer { loading = false }
        error = nil
        async let flowList = client.flows()
        async let scheduleList = client.schedules()
        do { flows = try await flowList } catch { self.error = error.localizedDescription }
        do { schedules = try await scheduleList } catch {
            if self.error == nil { self.error = error.localizedDescription }
        }
    }
}

/// Name, time and the last run of a schedule.
struct ScheduleRow: View {
    let schedule: FlowSchedule
    let flowName: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(schedule.name?.isEmpty == false ? schedule.name! : (flowName ?? schedule.target))
                .font(.body.weight(.medium))
            Text(CronSpec(cron: schedule.cron)?.summary ?? schedule.cron)
                .font(.subheadline).foregroundStyle(schedule.enabled ? Theme.accent : .secondary)
            if let run = schedule.lastRun {
                RunLine(run: run)
            } else if let next = schedule.nextRunAt, schedule.enabled {
                Text("下次 \(next.formatted(date: .abbreviated, time: .shortened))")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}

/// One run: state, when, and what it said.
struct RunLine: View {
    let run: ScheduleRun

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Chip(text: run.stateTitle, color: run.succeeded ? Theme.ok : (run.failed ? Theme.down : Theme.busy))
                if let at = run.scheduledFor {
                    Text(at.formatted(date: .abbreviated, time: .shortened)).font(.caption).foregroundStyle(.secondary)
                }
                if run.manual { Text("手动").font(.caption2).foregroundStyle(.tertiary) }
            }
            if let text = run.summary ?? run.error ?? run.reason, !text.isEmpty {
                Text(text).font(.caption).foregroundStyle(run.failed ? Theme.down : .secondary).lineLimit(3)
            }
        }
    }
}

/// One flow: what it does, its inputs, and 运行 / 定时.
struct FlowDetailView: View {
    @ObservedObject var session: DeviceSession
    let flow: FlowEntry
    let onSchedule: () -> Void
    @State private var inputs: [String: String] = [:]
    @State private var running = false
    @State private var confirmRun = false
    @State private var result: FlowRunResult?
    @State private var error: String?
    @State private var startedAt: Date?

    var body: some View {
        Form {
            Section {
                VStack(alignment: .leading, spacing: 6) {
                    Text(flow.name).font(.headline)
                    Text(flow.id).font(.caption.monospaced()).foregroundStyle(.secondary)
                    if let description = flow.description, !description.isEmpty {
                        Text(description).font(.subheadline)
                    }
                    HStack(spacing: 6) {
                        Chip(text: flow.isLocal ? String(localized: "本地") : String(localized: "流程库"))
                        Chip(text: riskTitle, color: flow.hasSideEffects ? Theme.attention : .secondary)
                        if flow.steps > 0 { Chip(text: String(localized: "\(flow.steps) 步")) }
                    }
                }
            }
            if !flow.inputs.isEmpty {
                Section("输入") {
                    ForEach(flow.inputs, id: \.self) { name in
                        TextField(name, text: Binding(get: { inputs[name] ?? "" }, set: { inputs[name] = $0 }),
                                  prompt: Text(flow.exampleInputs[name] ?? name))
                    }
                }
            }
            Section {
                Button {
                    if flow.hasSideEffects { confirmRun = true } else { Task { await run(confirm: false) } }
                } label: {
                    if running, let startedAt {
                        HStack {
                            ProgressView()
                            TimelineView(.periodic(from: startedAt, by: 1)) { context in
                                Text("运行中 \(Int(context.date.timeIntervalSince(startedAt))) 秒…").monospacedDigit()
                            }
                        }
                    } else {
                        Label("运行", systemImage: "play.fill")
                    }
                }
                .disabled(running)
                Button(action: onSchedule) { Label("定时", systemImage: "alarm") }
            } footer: {
                Text("运行会在「\(session.name)」上一步步执行，通常要几十秒。")
            }
            if let result {
                Section("结果") {
                    Label(result.ok ? String(localized: "成功") : String(localized: "没有完成"),
                          systemImage: result.ok ? "checkmark.circle.fill" : "xmark.octagon.fill")
                        .foregroundStyle(result.ok ? Theme.ok : Theme.down)
                    ForEach(result.outputs, id: \.key) { output in
                        AppLabeledContent(output.key, value: output.value)
                    }
                    if let error = result.error {
                        Text(result.failedStep.map { String(localized: "第 \($0) 步：\(error)") } ?? error)
                            .font(.footnote).foregroundStyle(Theme.down)
                    }
                }
            }
            if let error {
                Section { Text(error).font(.footnote).foregroundStyle(Theme.down) }
            }
        }
        .navigationTitle(flow.name)
        .navigationBarTitleDisplayMode(.inline)
        .confirmationDialog("这个 flow 会发送、付款或删除东西", isPresented: $confirmRun, titleVisibility: .visible) {
            Button("仍然运行", role: .destructive) { Task { await run(confirm: true) } }
        } message: {
            Text("只在你清楚它要做什么时才运行。")
        }
        .onAppear {
            for name in flow.inputs where inputs[name] == nil { inputs[name] = "" }
        }
    }

    private var riskTitle: String {
        switch flow.risk {
        case "read_only": return String(localized: "只读")
        case "navigation": return String(localized: "只浏览")
        case "side_effect": return String(localized: "有副作用")
        default: return String(localized: "风险未标注")
        }
    }

    private func run(confirm: Bool) async {
        guard let client = session.client else {
            error = String(localized: "设备没有连上")
            return
        }
        running = true
        startedAt = Date()
        error = nil
        result = nil
        defer { running = false }
        do {
            result = try await client.runFlow(id: flow.id, inputs: inputs.filter { !$0.value.isEmpty }, confirm: confirm)
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// A schedule: its runs, run now, delete.
struct ScheduleDetailView: View {
    @ObservedObject var session: DeviceSession
    let schedule: FlowSchedule
    @Environment(\.scenePhase) private var scenePhase
    @State private var refreshedSchedule: FlowSchedule?
    private var current: FlowSchedule { refreshedSchedule ?? schedule }
    let onChange: () -> Void
    @State private var runs: [ScheduleRun] = []
    @State private var error: String?
    @State private var working = false
    @State private var confirmDelete = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        List {
            Section {
                AppLabeledContent("Flow", value: schedule.target)
                AppLabeledContent("时间", value: CronSpec(cron: schedule.cron)?.summary ?? schedule.cron)
                if let next = current.nextRunAt, current.enabled {
                    AppLabeledContent("下次", value: next.formatted(date: .abbreviated, time: .shortened))
                }
                ForEach(schedule.inputs.sorted { $0.key < $1.key }, id: \.key) { item in
                    AppLabeledContent(item.key, value: item.value)
                }
                if schedule.confirmSideEffects {
                    Label("允许它发送、付款或删除", systemImage: "exclamationmark.triangle.fill")
                        .font(.footnote).foregroundStyle(Theme.attention)
                }
            }
            Section {
                Button { Task { await runNow() } } label: { Label("立即运行", systemImage: "play.fill") }
                    .disabled(working)
                Button(role: .destructive) { confirmDelete = true } label: { Label("删除", systemImage: "trash") }
                    .disabled(working)
            }
            Section("最近运行") {
                if runs.isEmpty {
                    Text("还没有运行过").font(.footnote).foregroundStyle(.secondary)
                }
                ForEach(runs) { RunLine(run: $0) }
            }
            if let error {
                Section { Text(error).font(.footnote).foregroundStyle(Theme.down) }
            }
        }
        .navigationTitle(schedule.name?.isEmpty == false ? schedule.name! : schedule.target)
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await loadRuns() }
        .task { await loadRuns() }
        .appOnChange(of: scenePhase) { _, phase in
            if phase == .active { Task { await loadRuns() } }
        }
        .confirmationDialog("删除这个定时任务？", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("删除", role: .destructive) { Task { await delete() } }
        }
    }

    private func loadRuns() async {
        guard let client = session.client else { return }
        do {
            async let history = client.scheduleRuns(schedule.id)
            async let list = client.schedules()
            runs = try await history
            refreshedSchedule = try await list.first { $0.id == schedule.id }
            error = nil
        } catch { self.error = error.localizedDescription }
    }

    private func runNow() async {
        guard let client = session.client else { return }
        working = true
        defer { working = false }
        do {
            try await client.runSchedule(schedule.id)
            session.show(String(localized: "已开始运行"))
            try? await Task.sleep(for: .seconds(1))
            await loadRuns()
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func delete() async {
        guard let client = session.client else { return }
        working = true
        defer { working = false }
        do {
            try await client.deleteSchedule(schedule.id)
            onChange()
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// A new schedule: which flow, when, its inputs, and (for a flow with side
/// effects) the person's explicit OK.
struct ScheduleEditor: View {
    @ObservedObject var session: DeviceSession
    let flows: [FlowEntry]
    let initial: FlowEntry?
    let onCreated: () -> Void
    @State private var flowID = ""
    @State private var time = Calendar.current.date(bySettingHour: 7, minute: 0, second: 0, of: Date()) ?? Date()
    @State private var weekdays: Set<Int> = []
    @State private var inputs: [String: String] = [:]
    @State private var confirmSideEffects = false
    @State private var name = ""
    @State private var saving = false
    @State private var error: String?
    @Environment(\.dismiss) private var dismiss

    private var flow: FlowEntry? { flows.first { $0.id == flowID } }

    private var spec: CronSpec {
        let parts = Calendar.current.dateComponents([.hour, .minute], from: time)
        return CronSpec(hour: parts.hour ?? 7, minute: parts.minute ?? 0, weekdays: weekdays)
    }

    var body: some View {
        AppNavigationStack {
            Form {
                Section("Flow") {
                    Picker("Flow", selection: $flowID) {
                        Text("选择").tag("")
                        ForEach(flows) { flow in Text(flow.name).tag(flow.id) }
                    }
                    .appFlowPickerStyle()
                    if let flow {
                        Text(flow.id).font(.caption.monospaced()).foregroundStyle(.secondary)
                    }
                }
                Section {
                    DatePicker("时间", selection: $time, displayedComponents: .hourAndMinute)
                    HStack(spacing: 6) {
                        ForEach(CronSpec.order, id: \.self) { day in
                            let on = weekdays.contains(day)
                            Button {
                                if on { weekdays.remove(day) } else { weekdays.insert(day) }
                            } label: {
                                Text(CronSpec.shortName(day).suffix(1))
                                    .font(.subheadline.weight(.semibold))
                                    .frame(width: 34, height: 34)
                                    .background(on ? AnyShapeStyle(Theme.accent) : AnyShapeStyle(Theme.hairline), in: Circle())
                                    .foregroundStyle(on ? Color.white : Color.primary)
                            }
                            .buttonStyle(.plain)
                            .accessibilityLabel(Text(CronSpec.shortName(day)))
                            .accessibilityAddTraits(on ? .isSelected : [])
                        }
                    }
                } header: {
                    Text("什么时候")
                } footer: {
                    Text("\(spec.summary)（\(spec.cron)）。不选星期就是每天。按那台设备的时区。")
                }
                if let flow, !flow.inputs.isEmpty {
                    Section("输入") {
                        ForEach(flow.inputs, id: \.self) { key in
                            TextField(key, text: Binding(get: { inputs[key] ?? "" }, set: { inputs[key] = $0 }),
                                      prompt: Text(flow.exampleInputs[key] ?? key))
                        }
                    }
                }
                if let flow, flow.hasSideEffects {
                    Section {
                        Toggle("允许它发送、付款或删除", isOn: $confirmSideEffects)
                    } footer: {
                        Label("这个 flow 有副作用：到点时没有人会再问你，它会直接执行。不打开这个开关，它会在那一步停下、不执行。",
                              systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(Theme.attention)
                    }
                }
                Section("名称（可选）") {
                    TextField(flow?.name ?? "", text: $name)
                }
                if let error {
                    Section { Text(error).font(.footnote).foregroundStyle(Theme.down) }
                }
            }
            .navigationTitle("新建定时")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("创建") { Task { await create() } }.disabled(flow == nil || saving)
                }
            }
            .disabled(saving)
            .onAppear { if flowID.isEmpty, let initial { flowID = initial.id } }
            .appOnChange(of: flowID) { _, _ in confirmSideEffects = false }
        }
    }

    private func create() async {
        guard let flow, let client = session.client else { return }
        saving = true
        defer { saving = false }
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        let schedule = NewSchedule(cron: spec.cron, target: flow.id,
                                   inputs: inputs.filter { flow.inputs.contains($0.key) && !$0.value.isEmpty },
                                   confirmSideEffects: flow.hasSideEffects && confirmSideEffects,
                                   name: trimmed.isEmpty ? nil : trimmed)
        do {
            try await client.createSchedule(schedule)
            onCreated()
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }
}
