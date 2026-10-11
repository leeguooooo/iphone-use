import Foundation
import Combine
import UIKit
import UserNotifications

/// The Agent screen's state: which device carries out the goal, the task
/// being followed, recent tasks there, and the question a paused task asks.
///
/// On 本机 the agent works in other apps while this one sits in the
/// background. Continued processing keeps polling alive on iOS 26+; older
/// systems can suspend it. The daemon brings this app back to the
/// front for a confirmation and at the end (`X-Task-Return-Bundle`), so the
/// task being followed is remembered and picked up again on every return.
@MainActor
final class AgentController: ObservableObject {
    weak var app: AppModel?

    /// The device that gets the goal.
    @Published var targetID: UUID? {
        didSet {
            guard targetID != oldValue else { return }
            history = []
            details = [:]
            error = nil
            Task { await refreshHistory() }
        }
    }

    /// `GET /agent/tasks` on the target, newest first, without steps.
    @Published private(set) var history: [AgentTaskSnapshot] = []
    /// The task followed live, and the device it runs on.
    @Published private(set) var live: AgentTaskTracker?
    @Published private(set) var liveDeviceID: UUID?
    /// Steps of past tasks, read when the person opens one.
    @Published private(set) var details: [String: AgentTaskSnapshot] = [:]
    /// A goal sent and not yet acknowledged.
    @Published private(set) var pendingGoal: String?
    @Published var error: String?
    @Published private var drafts: [String: String]
    /// Each device keeps its own unsent goal, including across app launches.
    var draft: String {
        get { drafts[targetID?.uuidString ?? "unselected"] ?? "" }
        set {
            drafts[targetID?.uuidString ?? "unselected"] = newValue
            defaults.set(drafts, forKey: Self.draftsKey)
        }
    }
    /// Following the task gave up (the daemon stopped answering); it may
    /// still be running there.
    @Published private(set) var linkNote: String?
    /// The question a paused task asks; drives the confirmation sheet.
    @Published var confirmPrompt: ConfirmPrompt?
    @Published private(set) var answering = false
    @Published private(set) var stopping = false

    @Published var settings: ModelSettings {
        didSet {
            guard settings != oldValue else { return }
            ModelSettingsStore.save(settings, to: defaults)
        }
    }

    struct ConfirmPrompt: Identifiable, Equatable {
        let taskID: String
        let deviceID: UUID
        let goal: String
        let confirm: AgentConfirm
        var id: String { "\(taskID)|\(confirm.action)|\(confirm.target ?? "")" }
    }

    private let defaults: UserDefaults
    private var pollTask: Task<Void, Never>?
    private let liveStatus = AgentLiveStatus.shared
    private var stopBeforeAcknowledgement = false
    private var progressNeedsRecovery = false
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    /// The confirmation already announced (sheet or notification), so a
    /// poll that repeats it does not announce it again.
    private var announced: String?
    /// A question the person dismissed by answering: never shown again for
    /// the same task and step, even if a poll still reports it.
    private var answered: Set<String> = []

    private static let runningKey = "agent.running.v1"
    private static let draftsKey = "agent.drafts.v1"

    init(defaults: UserDefaults = .standard) {
        #if DEBUG
        let defaults = UserDefaults.standard.bool(forKey: "modelSettingsFixture")
            ? UserDefaults(suiteName: "model-settings-ui.\(UUID().uuidString)")! : defaults
        #endif
        self.defaults = defaults
        #if DEBUG
        // Isolate UI fixtures without deleting pairing or model credentials.
        if UserDefaults.standard.bool(forKey: "clearAgentTestState") {
            defaults.removeObject(forKey: Self.runningKey)
            defaults.removeObject(forKey: Self.draftsKey)
        }
        #endif
        drafts = defaults.dictionary(forKey: Self.draftsKey) as? [String: String] ?? [:]
        settings = ModelSettingsStore.load(from: defaults)
    }

    // MARK: target

    var target: DeviceSession? { targetID.flatMap { app?.session($0) } }

    /// The device on screen, else 本机 when it is paired, else the first one.
    func chooseDefaultTarget() {
        guard let app else { return }
        if let focused = app.focusedID {
            targetID = focused
        } else if let own = app.devices.first(where: \.isThisPhone) {
            targetID = own.id
        } else if targetID == nil || app.session(targetID!) == nil {
            targetID = app.devices.first?.id
        }
    }

    /// The thread on the target, oldest first: past tasks (with their steps
    /// once read) and the live one.
    var thread: [AgentTaskSnapshot] {
        var items = history.reversed().map { item -> AgentTaskSnapshot in
            if let live, live.task.id == item.id, liveDeviceID == targetID { return live.task }
            return details[item.id] ?? item
        }
        if let live, liveDeviceID == targetID, !items.contains(where: { $0.id == live.task.id }) {
            items.append(live.task)
        }
        return items
    }

    /// A task is under way on the target (the composer becomes a Stop button).
    var busyOnTarget: Bool {
        guard let live, liveDeviceID == targetID else { return pendingGoal != nil }
        return !live.task.state.isTerminal || pendingGoal != nil
    }

    // MARK: running

    /// Hand `goal` to the target's daemon and follow it.
    @discardableResult
    func start(goal typed: String) async -> Bool {
        let goal = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !goal.isEmpty, !busyOnTarget else { return false }
        guard let session = target else {
            error = String(localized: "先选一台设备")
            return false
        }
        guard let client = session.client, session.phase == .connected else {
            error = String(localized: "「\(session.name)」现在没有连上：\(session.shortState)")
            return false
        }
        error = nil
        linkNote = nil
        pendingGoal = goal
        stopBeforeAcknowledgement = false
        liveStatus.begin(goal: goal, device: session.name) { [weak self] in await self?.stop() }
        askNotificationPermission()
        let request = AgentTaskRequest(goal: goal, models: settings.payload(key: ModelSettingsStore.secret))
        let bundle = session.record.isThisPhone ? Bundle.main.bundleIdentifier : nil
        var id: String?
        var accepted = true
        for attempt in 0..<2 {
            do {
                id = try await client.startTask(request, returnBundle: bundle)
                break
            } catch DaemonError.sessionExpired where attempt == 0 {
                // Turned away before anything started: renew and send once more.
                if await session.relogin() { continue }
                error = DaemonError.sessionExpired.localizedDescription
            } catch AgentTaskError.taskRunning(let running) {
                // Already one there (or this POST's own, whose answer was lost):
                // show that one instead.
                id = running
                accepted = false
                if running != live?.task.id { app?.show(String(localized: "那台手机上已经有一个任务在进行，正在显示它")) }
                break
            } catch {
                self.error = Self.describe(error, onThisPhone: session.record.isThisPhone)
            }
            break
        }
        guard let id else {
            pendingGoal = nil
            liveStatus.finish(success: false, dismissImmediately: true)
            return false
        }
        var first = AgentTaskSnapshot(id: id, goal: accepted ? goal : "", state: .running)
        progressNeedsRecovery = !accepted
        if !accepted {
            // The refused goal belongs in the draft, never on another task's activity.
            liveStatus.finish(success: false, dismissImmediately: true)
            if let snapshot = try? await client.task(id: id, since: 0) {
                first = snapshot
                progressNeedsRecovery = false
                liveStatus.recover(task: snapshot, device: session.name, deviceID: session.id) { [weak self] in await self?.stop() }
            }
        }
        pendingGoal = nil
        follow(AgentTaskTracker(first), on: session)
        if accepted && stopBeforeAcknowledgement { await stop() }
        return accepted
    }

    static func describe(_ error: Error, onThisPhone: Bool) -> String {
        if onThisPhone, case .unreachable = ConnectProblem(error),
           error is DaemonError || error is URLError {
            return String(localized: "本机执行服务未连接。请从 Mac 启动这台 iPhone 的执行服务，再重试。")
        }
        return error.localizedDescription
    }

    /// Follow `tracker` on `session` until it ends, remembering it so a
    /// relaunch picks it up again.
    private func follow(_ tracker: AgentTaskTracker, on session: DeviceSession) {
        guard let client = session.client else { return }
        pollTask?.cancel()
        live = tracker
        liveDeviceID = session.id
        liveStatus.bind(taskID: tracker.task.id, deviceID: session.id)
        linkNote = nil
        remember(task: tracker.task.id, device: session.id)
        holdBackgroundTime()
        let taskID = tracker.task.id
        pollTask = Task { [weak self, weak session] in
            let end = await AgentTaskPoller.follow(
                tracker, api: client,
                relogin: { await session?.relogin() ?? false },
                onUpdate: { self?.received($0) })
            self?.ended(end, taskID: taskID)
        }
    }

    private func received(_ tracker: AgentTaskTracker) {
        let previous = live?.task.state
        live = tracker
        let task = tracker.task
        if progressNeedsRecovery, !task.goal.isEmpty, let deviceID = liveDeviceID,
           let session = app?.session(deviceID) {
            progressNeedsRecovery = false
            liveStatus.recover(task: task, device: session.name, deviceID: deviceID) { [weak self] in await self?.stop() }
        }
        liveStatus.update(AgentProgress(task: task))
        if let confirm = task.pendingConfirm, let deviceID = liveDeviceID {
            let prompt = ConfirmPrompt(taskID: task.id, deviceID: deviceID, goal: task.goal, confirm: confirm)
            if !answered.contains(prompt.id), announced != prompt.id {
                announced = prompt.id
                confirmPrompt = prompt
                // Wherever the person is in the app, the question comes up.
                // The call sheet presents the same native confirmation UI.
                // Do not ask its covered parent to present a second sheet.
                if app?.voiceCall.showing != true { app?.agentOpen = true }
                notifyIfAway(title: String(localized: "Agent 等你确认"),
                             body: Self.describe(confirm), id: "agent-confirm-\(task.id)")
            }
        } else if confirmPrompt?.taskID == task.id {
            confirmPrompt = nil
        }
        if task.state.isTerminal, previous.map({ !$0.isTerminal }) ?? true {
            notifyIfAway(title: task.state.title, body: task.summary ?? task.error ?? task.goal,
                         id: "agent-end-\(task.id)")
        }
    }

    private func ended(_ end: AgentPollEnd, taskID: String) {
        guard live?.task.id == taskID else { return }
        switch end {
        case .finished:
            liveStatus.finish(success: live?.task.state == .done)
            forgetRunning()
            Task { await refreshHistory() }
        case .lostLink(let message):
            liveStatus.finish(success: false)
            linkNote = message
            // Its answer could not be sent now; the thread offers it again.
            if confirmPrompt?.taskID == taskID { confirmPrompt = nil }
        case .forgotten:
            forgetRunning()
            liveStatus.finish(success: false)
            if var task = live?.task {
                task.state = .unavailable
                task.error = String(localized: "服务已不保存这个任务，结果无法确认。检查手机后，可以发起新任务。")
                live = AgentTaskTracker(task)
            }
            linkNote = nil
            confirmPrompt = nil
        case .stopped:
            break
        }
        pollTask = nil
        releaseBackgroundTime()
    }

    /// Stop: the daemon stops after the current step; following goes on
    /// until it reports `cancelled`.
    func stop() async {
        if pendingGoal != nil { stopBeforeAcknowledgement = true; return }
        guard let live, let deviceID = liveDeviceID, let client = app?.session(deviceID)?.client else { return }
        stopping = true
        defer { stopping = false }
        do {
            try await client.cancelTask(id: live.task.id)
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// The person's answer to a paused task. Nothing ever answers for them.
    func answer(_ prompt: ConfirmPrompt, approve: Bool) async {
        guard let client = app?.session(prompt.deviceID)?.client else {
            error = String(localized: "设备没有连上，回答没有送出")
            return
        }
        answering = true
        defer { answering = false }
        do {
            try await client.answer(task: prompt.taskID, approve: approve)
            answered.insert(prompt.id)
            if confirmPrompt == prompt { confirmPrompt = nil }
            UNUserNotificationCenter.current().removeDeliveredNotifications(
                withIdentifiers: ["agent-confirm-\(prompt.taskID)"])
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// Bring the question of the live task back up (the person closed the screen).
    func reopenConfirm() {
        guard let live, let deviceID = liveDeviceID, let confirm = live.task.pendingConfirm else { return }
        confirmPrompt = ConfirmPrompt(taskID: live.task.id, deviceID: deviceID, goal: live.task.goal, confirm: confirm)
    }

    // MARK: history

    func refreshHistory() async {
        guard let session = target, let client = session.client else { return }
        let id = session.id
        do {
            let list = try await client.recentTasks()
            guard targetID == id else { return }
            history = list
            if confirmPrompt == nil { error = nil }
        } catch DaemonError.sessionExpired {
            if await session.relogin(), targetID == id, let list = try? await client.recentTasks() {
                history = list
                if confirmPrompt == nil { error = nil }
            }
        } catch {
            // No history (an older daemon) is not worth a message.
        }
    }

    /// Read a past task's steps when the person opens it.
    func loadDetail(_ id: String) async {
        guard details[id] == nil, live?.task.id != id, let client = target?.client else { return }
        if let task = try? await client.task(id: id, since: 0) { details[id] = task }
    }

    // MARK: coming back

    /// The app is in front again (or just launched): pick the remembered
    /// task up where it is, so a pending question shows at once.
    func resume() {
        guard pollTask == nil, let saved = defaults.string(forKey: Self.runningKey) else {
            if pollTask != nil {
                holdBackgroundTime()
                if pendingGoal == nil, let task = live?.task, let deviceID = liveDeviceID,
                   let session = app?.session(deviceID) {
                    liveStatus.recover(task: task, device: session.name, deviceID: deviceID) { [weak self] in await self?.stop() }
                }
            }
            return
        }
        let parts = saved.split(separator: "|", maxSplits: 1).map(String.init)
        guard parts.count == 2, let deviceID = UUID(uuidString: parts[0]) else {
            forgetRunning()
            return
        }
        let taskID = parts[1]
        pollTask = Task { [weak self] in
            // At launch the device is still connecting: wait for it a little.
            for _ in 0..<20 {
                guard let self else { return }
                if let session = self.app?.session(deviceID), session.client != nil, session.phase == .connected {
                    var seed = self.live?.task.id == taskID
                        ? self.live! : AgentTaskTracker(AgentTaskSnapshot(id: taskID, goal: "", state: .running))
                    if let snapshot = try? await session.client?.task(id: taskID, since: 0) {
                        seed = AgentTaskTracker(snapshot)
                        self.liveStatus.recover(task: snapshot, device: session.name, deviceID: session.id) { [weak self] in await self?.stop() }
                    }
                    guard !Task.isCancelled else { return }
                    self.pollTask = nil
                    if self.targetID == nil { self.targetID = deviceID }
                    self.follow(seed, on: session)
                    return
                }
                if self.app?.session(deviceID) == nil {
                    self.forgetRunning()
                    self.pollTask = nil
                    return
                }
                try? await Task.sleep(for: .seconds(1))
            }
            self?.pollTask = nil
        }
    }

    private func remember(task id: String, device: UUID) {
        defaults.set("\(device.uuidString)|\(id)", forKey: Self.runningKey)
    }

    private func forgetRunning() {
        defaults.removeObject(forKey: Self.runningKey)
    }

    /// A device was forgotten: drop what was followed there.
    func forget(device id: UUID) {
        drafts.removeValue(forKey: id.uuidString)
        defaults.set(drafts, forKey: Self.draftsKey)
        if liveDeviceID == id {
            pollTask?.cancel()
            pollTask = nil
            live = nil
            liveDeviceID = nil
            forgetRunning()
        }
        if targetID == id { targetID = nil }
    }

    // MARK: background and notifications

    /// A few more seconds of polling after the person leaves the app (iOS
    /// gives about 30 s), so a question or the end can still be announced.
    private func holdBackgroundTime() {
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "agent-task") { [weak self] in
            MainActor.assumeIsolated { self?.releaseBackgroundTime() }
        }
    }

    private func releaseBackgroundTime() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    /// Asked the first time a task starts, not before: a person who never
    /// uses the agent is never asked.
    private func askNotificationPermission() {
        Task {
            let center = UNUserNotificationCenter.current()
            let settings = await center.notificationSettings()
            if settings.authorizationStatus == .notDetermined {
                _ = try? await center.requestAuthorization(options: [.alert, .sound])
            }
        }
    }

    private func notifyIfAway(title: String, body: String, id: String) {
        guard UIApplication.shared.applicationState != .active else { return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        let request = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    /// `CLICK「确认支付」· 拼多多 — pays ¥55.51`
    static func describe(_ confirm: AgentConfirm) -> String {
        var text = confirm.action
        if let target = confirm.target, !target.isEmpty { text += "「\(target)」" }
        if let app = confirm.app, !app.isEmpty { text += " · \(app)" }
        if let reason = confirm.reason, !reason.isEmpty { text += " — \(reason)" }
        return text
    }
}
