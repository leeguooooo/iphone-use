import Foundation

// Agent tasks (`docs/agent-task.md`): the person types a goal, a daemon
// carries it out on its phone step by step, and the app follows along. The
// same API serves the phone's own daemon (本机, `http://127.0.0.1:8787`), a
// Mac's and another phone's; to the app each is just a paired device.

/// Where a task stands (`status`). The two non-terminal ones keep the app
/// polling; everything else is the end.
enum AgentTaskState: Equatable, Sendable {
    case running
    /// Paused before a risky step until the person answers (`confirm`).
    case needsConfirm
    case done
    /// Declined, a 5-minute unanswered confirmation, or a screen the agent
    /// could not get past.
    case blocked
    case failed
    case cancelled
    /// Out of steps (`max_steps`) before the goal was reached.
    case maxSteps
    /// The daemon forgot the id; the app cannot confirm the operation's result.
    case unavailable
    /// A status this app does not know (a newer daemon): followed like a
    /// running task, since stopping early would hide how it ends.
    case other(String)

    init(_ raw: String) {
        switch raw {
        case "running": self = .running
        case "needs_confirm": self = .needsConfirm
        case "done": self = .done
        case "blocked": self = .blocked
        case "failed": self = .failed
        case "cancelled": self = .cancelled
        case "max_steps": self = .maxSteps
        case "unavailable": self = .unavailable
        default: self = .other(raw)
        }
    }

    var isTerminal: Bool {
        switch self {
        case .running, .needsConfirm, .other: return false
        case .done, .blocked, .failed, .cancelled, .maxSteps, .unavailable: return true
        }
    }

    /// The chip under a task, in the person's words.
    var title: String {
        switch self {
        case .running: return String(localized: "进行中")
        case .needsConfirm: return String(localized: "等你确认")
        case .done: return String(localized: "完成")
        case .blocked: return String(localized: "受阻")
        case .failed: return String(localized: "失败")
        case .cancelled: return String(localized: "已停止")
        case .maxSteps: return String(localized: "步数用完")
        case .unavailable: return String(localized: "结果未知")
        case .other(let raw): return raw == "paused" ? String(localized: "已暂停") : raw
        }
    }
}

/// One step the agent took (`steps[]`).
struct AgentStep: Decodable, Equatable, Identifiable, Sendable {
    var n: Int
    var action: String
    var target: String?
    var text: String?
    var app: String?
    var ok: Bool
    var error: String?
    var pageChanged: Bool?
    /// Who decided it: `jev`, `assist` or `flow`.
    var by: String?
    var ms: Int?

    var id: Int { n }

    enum CodingKeys: String, CodingKey {
        case n, action, target, text, app, ok, error, by, ms
        case pageChanged = "page_changed"
    }

    init(n: Int, action: String, target: String? = nil, text: String? = nil, app: String? = nil,
         ok: Bool = true, error: String? = nil, pageChanged: Bool? = nil, by: String? = nil, ms: Int? = nil) {
        self.n = n
        self.action = action
        self.target = target
        self.text = text
        self.app = app
        self.ok = ok
        self.error = error
        self.pageChanged = pageChanged
        self.by = by
        self.ms = ms
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        n = try c.decode(Int.self, forKey: .n)
        action = try c.decodeIfPresent(String.self, forKey: .action) ?? ""
        target = try c.decodeIfPresent(String.self, forKey: .target)
        text = try c.decodeIfPresent(String.self, forKey: .text)
        app = try c.decodeIfPresent(String.self, forKey: .app)
        error = try c.decodeIfPresent(String.self, forKey: .error)
        ok = try c.decodeIfPresent(Bool.self, forKey: .ok) ?? (error == nil)
        pageChanged = try c.decodeIfPresent(Bool.self, forKey: .pageChanged)
        by = try c.decodeIfPresent(String.self, forKey: .by)
        ms = try c.decodeIfPresent(Int.self, forKey: .ms)
    }
}

/// The step a task paused before (`confirm`), for the person to allow or not.
struct AgentConfirm: Decodable, Equatable, Sendable {
    var action: String
    var target: String?
    var reason: String?
    var app: String?

    init(action: String, target: String? = nil, reason: String? = nil, app: String? = nil) {
        self.action = action
        self.target = target
        self.reason = reason
        self.app = app
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        action = try c.decodeIfPresent(String.self, forKey: .action) ?? ""
        target = try c.decodeIfPresent(String.self, forKey: .target)
        reason = try c.decodeIfPresent(String.self, forKey: .reason)
        app = try c.decodeIfPresent(String.self, forKey: .app)
    }

    enum CodingKeys: String, CodingKey { case action, target, reason, app }
}

/// A task as `GET /agent/task/<id>` reports it (and, without steps, each
/// row of `GET /agent/tasks`). Every field but `id` may be missing or null.
struct AgentTaskSnapshot: Decodable, Equatable, Identifiable, Sendable {
    struct PlannedAction: Decodable, Equatable, Sendable {
        var action: String
        var target: String?
    }
    struct Flow: Decodable, Equatable, Sendable {
        var id: String
        var outcome: String?
    }

    var id: String
    var goal: String
    var state: AgentTaskState
    /// What is deciding right now: `flow`, `jev`, `assist`, `summary`.
    var phase: String?
    var plannedAction: PlannedAction?
    var flow: Flow?
    var steps: [AgentStep]
    var confirm: AgentConfirm?
    var summary: String?
    var error: String?
    var startedAtMs: Int
    var elapsedMs: Int
    var modelsUsed: [String: String]
    var clientRequestID: String?

    enum CodingKeys: String, CodingKey {
        case id, goal, status, phase, flow, steps, confirm, summary, error
        case plannedAction = "planned_action"
        case startedAtMs = "started_at_ms"
        case elapsedMs = "elapsed_ms"
        case modelsUsed = "models_used"
        case clientRequestID = "client_request_id"
    }

    init(id: String, goal: String, state: AgentTaskState, phase: String? = nil, flow: Flow? = nil,
         steps: [AgentStep] = [], confirm: AgentConfirm? = nil, summary: String? = nil,
         error: String? = nil, startedAtMs: Int = 0, elapsedMs: Int = 0, modelsUsed: [String: String] = [:]) {
        self.id = id
        self.goal = goal
        self.state = state
        self.phase = phase
        self.plannedAction = nil
        self.flow = flow
        self.steps = steps
        self.confirm = confirm
        self.summary = summary
        self.error = error
        self.startedAtMs = startedAtMs
        self.elapsedMs = elapsedMs
        self.modelsUsed = modelsUsed
        self.clientRequestID = nil
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        goal = try c.decodeIfPresent(String.self, forKey: .goal) ?? ""
        state = AgentTaskState(try c.decodeIfPresent(String.self, forKey: .status) ?? "running")
        phase = try c.decodeIfPresent(String.self, forKey: .phase)
        plannedAction = try? c.decodeIfPresent(PlannedAction.self, forKey: .plannedAction)
        flow = try? c.decodeIfPresent(Flow.self, forKey: .flow)
        steps = (try? c.decodeIfPresent([AgentStep].self, forKey: .steps)) ?? []
        confirm = try? c.decodeIfPresent(AgentConfirm.self, forKey: .confirm)
        summary = try c.decodeIfPresent(String.self, forKey: .summary)
        error = try c.decodeIfPresent(String.self, forKey: .error)
        startedAtMs = (try? c.decodeIfPresent(Int.self, forKey: .startedAtMs)) ?? 0
        elapsedMs = (try? c.decodeIfPresent(Int.self, forKey: .elapsedMs)) ?? 0
        modelsUsed = ((try? c.decodeIfPresent([String: String?].self, forKey: .modelsUsed)) ?? [:])
            .compactMapValues { $0 }
        clientRequestID = try c.decodeIfPresent(String.self, forKey: .clientRequestID)
    }

    /// The question to show while the task waits on the person; nil otherwise.
    var pendingConfirm: AgentConfirm? { state == .needsConfirm ? confirm : nil }
}

/// The body of `POST /agent/task`.
struct AgentTaskRequest: Encodable, Equatable, Sendable {
    var goal: String
    var app: String?
    var maxSteps: Int?
    var models: ModelsPayload?
    var clientRequestID: String?
    /// Always `risky` from the app: the person answers every risky step
    /// themselves (`never` is for Mac agents that accept every step).
    var confirm = "risky"

    enum CodingKeys: String, CodingKey {
        case goal, app, models, confirm
        case maxSteps = "max_steps"
        case clientRequestID = "client_request_id"
    }
}

enum AgentTaskError: LocalizedError, Equatable {
    /// One task at a time: this one (`id`) is already running there.
    case taskRunning(id: String)
    /// Another session holds the phone's owner lease.
    case phoneOwned(by: String?)
    /// The daemon has no `/agent/task` (it predates agent tasks).
    case unsupported
    /// No such task (the daemon restarted, or the id is too old).
    case notFound
    /// The daemon turned the request away; its own message.
    case refused(String)

    var errorDescription: String? {
        switch self {
        case .taskRunning: return String(localized: "那台手机上已经有一个任务在进行")
        case .phoneOwned(let owner): return String(localized: "手机正被「\(owner ?? "?")」控制，稍后再试")
        case .unsupported: return String(localized: "这台设备的 iphone-use 太旧，还不能接任务。请先升级。")
        case .notFound: return String(localized: "找不到这个任务了（服务可能重启过）")
        case .refused(let message): return message
        }
    }
}

/// What the app needs from a daemon to run and follow a task. `DaemonClient`
/// is the real one; tests stand in their own.
protocol AgentTaskAPI: Sendable {
    func startTask(_ request: AgentTaskRequest, returnBundle: String?) async throws -> String
    func task(id: String, since: Int) async throws -> AgentTaskSnapshot
    func answer(task id: String, approve: Bool) async throws
    func cancelTask(id: String) async throws
    func recentTasks() async throws -> [AgentTaskSnapshot]
    func pauseTask(id: String) async throws
    func resumeTask(id: String) async throws
}

extension AgentTaskAPI {
    func pauseTask(id: String) async throws { throw AgentTaskError.unsupported }
    func resumeTask(id: String) async throws { throw AgentTaskError.unsupported }
}

extension DaemonClient: AgentTaskAPI {
    /// `POST /agent/task` → the new task's id. Never repeated on another
    /// route: a lost answer may still have started it (a second POST would
    /// then be a 409 `task_running`, which the caller adopts).
    func startTask(_ request: AgentTaskRequest, returnBundle: String?) async throws -> String {
        var urlRequest = agentRequest("agent/task", method: "POST")
        urlRequest.httpBody = try JSONEncoder().encode(request)
        if let returnBundle { urlRequest.setValue(returnBundle, forHTTPHeaderField: "X-Task-Return-Bundle") }
        urlRequest.timeoutInterval = 20
        let (data, response) = try await send(urlRequest, repeatable: false)
        try Self.checkAgent(response, data)
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = json["id"] as? String else { throw DaemonError.notIphoneUse }
        return id
    }

    /// `GET /agent/task/<id>?since=<n>`: the task, with only the steps after `n`.
    func task(id: String, since: Int) async throws -> AgentTaskSnapshot {
        var urlRequest = request("agent/task/\(id)", query: [URLQueryItem(name: "since", value: String(since))])
        urlRequest.timeoutInterval = 10
        let (data, response) = try await send(urlRequest)
        try Self.checkAgent(response, data)
        do {
            return try JSONDecoder().decode(AgentTaskSnapshot.self, from: data)
        } catch {
            throw DaemonError.notIphoneUse
        }
    }

    /// `POST /agent/task/<id>/confirm`: the person's answer to `needs_confirm`.
    func answer(task id: String, approve: Bool) async throws {
        var urlRequest = agentRequest("agent/task/\(id)/confirm", method: "POST")
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: ["approve": approve])
        let (data, response) = try await send(urlRequest, repeatable: false)
        try Self.checkAgent(response, data)
    }

    /// `POST /agent/task/<id>/cancel`: stop after the current step.
    func cancelTask(id: String) async throws {
        let (data, response) = try await send(agentRequest("agent/task/\(id)/cancel", method: "POST"))
        try Self.checkAgent(response, data)
    }
    func pauseTask(id: String) async throws {
        let (data, response) = try await send(agentRequest("agent/task/\(id)/pause", method: "POST"), repeatable: false)
        try Self.checkAgent(response, data)
    }
    func resumeTask(id: String) async throws {
        let (data, response) = try await send(agentRequest("agent/task/\(id)/resume", method: "POST"), repeatable: false)
        try Self.checkAgent(response, data)
    }

    /// `GET /agent/tasks`: the last 20, newest first, without steps.
    func recentTasks() async throws -> [AgentTaskSnapshot] {
        let (data, response) = try await send(request("agent/tasks"))
        try Self.checkAgent(response, data)
        let json = try? JSONSerialization.jsonObject(with: data)
        // A bare list, or `{"tasks": [...]}`.
        let list = (json as? [[String: Any]]) ?? ((json as? [String: Any])?["tasks"] as? [[String: Any]]) ?? []
        return list.compactMap { item in
            guard let row = try? JSONSerialization.data(withJSONObject: item) else { return nil }
            return try? JSONDecoder().decode(AgentTaskSnapshot.self, from: row)
        }
    }

    /// A write to the agent API: JSON, with `X-Phone-Control: 1`.
    func agentRequest(_ path: String, method: String) -> URLRequest {
        var urlRequest = request(path)
        urlRequest.httpMethod = method
        urlRequest.setValue("application/json", forHTTPHeaderField: "Content-Type")
        urlRequest.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        return urlRequest
    }

    /// An agent endpoint's answer as an error, or nothing when it is 2xx.
    static func checkAgent(_ response: HTTPURLResponse, _ data: Data) throws {
        let status = response.statusCode
        if (200..<300).contains(status) { return }
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        let error = json["error"] as? String
        switch (status, error) {
        case (401, _): throw DaemonError.sessionExpired
        case (409, "task_running"):
            if let id = json["id"] as? String { throw AgentTaskError.taskRunning(id: id) }
        case (409, "phone_owned"): throw AgentTaskError.phoneOwned(by: json["owner"] as? String)
        case (404, "task_not_found"), (404, "not_found"): throw AgentTaskError.notFound
        case (404, nil), (405, _): throw AgentTaskError.unsupported
        case (400, _), (422, _):
            throw AgentTaskError.refused((json["message"] as? String) ?? error ?? "bad_request")
        default: break
        }
        // Flows and schedules answer `{"error": code, "message": …}`.
        if let message = (json["message"] as? String) ?? (json["hint"] as? String), !message.isEmpty {
            throw AgentTaskError.refused(message)
        }
        if status == 404 { throw AgentTaskError.notFound }
        throw DaemonError.http(status, String(decoding: data, as: UTF8.self))
    }
}

// MARK: - following a task

/// The app's picture of one task, built from successive answers. `?since=n`
/// answers carry only new steps, so steps are merged by number; a full
/// answer (or one that repeats steps) replaces those it repeats.
struct AgentTaskTracker: Equatable, Sendable {
    private(set) var task: AgentTaskSnapshot

    init(_ task: AgentTaskSnapshot) {
        self.task = task
    }

    /// The last step seen, for the next `?since=`.
    var lastStep: Int { task.steps.last?.n ?? 0 }

    /// Fold in an answer. Returns whether to keep polling.
    @discardableResult
    mutating func apply(_ update: AgentTaskSnapshot) -> Bool {
        guard update.id == task.id else { return !task.state.isTerminal }
        var byNumber = Dictionary(task.steps.map { ($0.n, $0) }, uniquingKeysWith: { _, new in new })
        for step in update.steps { byNumber[step.n] = step }
        var merged = update
        merged.steps = byNumber.values.sorted { $0.n < $1.n }
        // An answer that lost a field it had (an older daemon writing
        // `summary` only at the very end) keeps what was known.
        if merged.goal.isEmpty { merged.goal = task.goal }
        task = merged
        return !task.state.isTerminal
    }
}

/// How following a task ended.
enum AgentPollEnd: Equatable, Sendable {
    /// It reached a terminal status.
    case finished
    /// The daemon stopped answering; polling gave up.
    case lostLink(String)
    case forgotten
    /// The caller stopped following (the screen closed, another task began).
    case stopped
}

/// The polling loop, kept apart from the screen so it can be tested: ask
/// for the task about once a second while it is running or waiting on the
/// person, and stop as soon as its status is terminal. A run of failed
/// reads ends it as `lostLink` (the task itself goes on on the daemon).
@MainActor
enum AgentTaskPoller {
    /// Consecutive failed reads before giving up (about 30 s at 1 s).
    static let maxFailures = 30

    static func follow(
        _ tracker: AgentTaskTracker,
        api: any AgentTaskAPI,
        interval: TimeInterval = .seconds(1),
        sleep: @escaping @Sendable (TimeInterval) async throws -> Void = { try await Task.sleep(for: $0) },
        relogin: @escaping @MainActor () async -> Bool = { false },
        onUpdate: @escaping @MainActor (AgentTaskTracker) -> Void
    ) async -> AgentPollEnd {
        var tracker = tracker
        var failures = 0
        var lastError = ""
        while true {
            if Task.isCancelled { return .stopped }
            do {
                let update = try await api.task(id: tracker.task.id, since: tracker.lastStep)
                if Task.isCancelled { return .stopped }
                failures = 0
                let more = tracker.apply(update)
                onUpdate(tracker)
                if !more { return .finished }
            } catch DaemonError.sessionExpired {
                failures += 1
                lastError = DaemonError.sessionExpired.localizedDescription
                if !(await relogin()) { return .lostLink(lastError) }
            } catch AgentTaskError.notFound {
                return .forgotten
            } catch {
                if Task.isCancelled { return .stopped }
                failures += 1
                lastError = error.localizedDescription
            }
            if failures >= maxFailures { return .lostLink(lastError) }
            do {
                try await sleep(interval)
            } catch {
                return .stopped
            }
        }
    }
}
