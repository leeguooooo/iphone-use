import Foundation

// Flows from the phone (docs/agent-task.md › "Flows from the phone"): record
// what the person does in the remote view and save it as a local flow, list
// the flows a daemon can run, run one now, and schedule it. Every device
// works the same way: 本机, a Mac's daemon, another phone's.

/// One flow a daemon can run (`GET /agent/flows`, as `flow list --json`).
struct FlowEntry: Decodable, Equatable, Identifiable, Sendable {
    var id: String
    var name: String
    var description: String?
    var steps: Int
    var inputs: [String]
    var app: String?
    var category: String?
    /// `read_only`, `navigation`, `side_effect` or `unknown`.
    var risk: String
    /// `official` (the registry) or `local` (saved on that daemon).
    var source: String
    var verified: Bool
    var exampleInputs: [String: String]
    var outputs: [String]

    enum CodingKeys: String, CodingKey {
        case id, name, description, steps, inputs, app, category, risk, source, verified, outputs
        case exampleInputs = "example_inputs"
    }

    init(id: String, name: String, risk: String = "unknown", source: String = "local", inputs: [String] = []) {
        self.id = id
        self.name = name
        self.steps = 0
        self.inputs = inputs
        self.risk = risk
        self.source = source
        self.verified = false
        self.exampleInputs = [:]
        self.outputs = []
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? id
        description = try c.decodeIfPresent(String.self, forKey: .description)
        steps = (try? c.decodeIfPresent(Int.self, forKey: .steps)) ?? 0
        inputs = (try? c.decodeIfPresent([String].self, forKey: .inputs)) ?? []
        app = try? c.decodeIfPresent(String.self, forKey: .app)
        category = try? c.decodeIfPresent(String.self, forKey: .category)
        risk = (try? c.decodeIfPresent(String.self, forKey: .risk)) ?? "unknown"
        source = (try? c.decodeIfPresent(String.self, forKey: .source)) ?? "official"
        verified = (try? c.decodeIfPresent(Bool.self, forKey: .verified)) ?? false
        exampleInputs = (try? c.decodeIfPresent([String: String].self, forKey: .exampleInputs)) ?? [:]
        outputs = (try? c.decodeIfPresent([String].self, forKey: .outputs)) ?? []
    }

    var isLocal: Bool { source == "local" }
    /// It sends, pays, publishes or deletes: running it needs the person's OK.
    var hasSideEffects: Bool { risk == "side_effect" }
}

/// A recording's (or the last task's) draft: the flow document as the daemon
/// wrote it, kept as JSON to send back unchanged, and what to check.
struct FlowDraft: Equatable, Identifiable, Sendable {
    var id: Int { flow?.hashValue ?? steps }

    /// `flow` from the draft, as JSON.
    var flow: Data?
    /// The app's bundle id (`flow.app`, else `app.bundle`).
    var bundle: String?
    /// The app's name as the phone shows it (`app.label`).
    var label: String?
    var name: String?
    var steps: Int
    var todo: [String]

    /// Read a `{"draft": …}` or bare draft object.
    init?(json: Any?) {
        guard var object = json as? [String: Any] else { return nil }
        if let inner = object["draft"] as? [String: Any] { object = inner } else if object["draft"] is NSNull {
            return nil
        }
        let flow = object["flow"] as? [String: Any]
        self.flow = flow.flatMap { try? JSONSerialization.data(withJSONObject: $0, options: [.sortedKeys]) }
        let app = object["app"] as? [String: Any]
        bundle = (flow?["app"] as? String) ?? (app?["bundle"] as? String)
        label = app?["label"] as? String
        name = flow?["name"] as? String
        steps = (flow?["steps"] as? [Any])?.count ?? (object["recorded_steps"] as? Int) ?? 0
        todo = (object["todo"] as? [String]) ?? []
    }

    init(flow: Data? = nil, bundle: String? = nil, label: String? = nil, name: String? = nil,
         steps: Int = 0, todo: [String] = []) {
        self.flow = flow
        self.bundle = bundle
        self.label = label
        self.name = name
        self.steps = steps
        self.todo = todo
    }
}

/// What `GET /agent/flow/record` says.
struct RecordingState: Equatable, Sendable {
    var recording: Bool
    var steps: Int
}

/// A flow run's answer (`POST /agent/flow/run`).
struct FlowRunResult: Equatable, Sendable {
    var ok: Bool
    var outputs: [(key: String, value: String)]
    var error: String?
    var failedStep: Int?

    static func == (a: FlowRunResult, b: FlowRunResult) -> Bool {
        a.ok == b.ok && a.error == b.error && a.failedStep == b.failedStep
            && a.outputs.map(\.key) == b.outputs.map(\.key) && a.outputs.map(\.value) == b.outputs.map(\.value)
    }

    init(json: Any?) {
        let object = json as? [String: Any] ?? [:]
        let result = object["result"] as? [String: Any] ?? [:]
        ok = (object["ok"] as? Bool) ?? (result["ok"] as? Bool) ?? false
        let outputs = result["outputs"] as? [String: Any] ?? [:]
        self.outputs = outputs.keys.sorted().map { key in
            let value = outputs[key]
            if let text = value as? String { return (key, text) }
            if let number = value as? NSNumber { return (key, number.description) }
            if let value, let data = try? JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed]) {
                return (key, String(decoding: data, as: UTF8.self))
            }
            return (key, "")
        }
        error = (result["error"] as? String) ?? (object["message"] as? String) ?? (object["error"] as? String)
        failedStep = result["failed_step"] as? Int
    }
}

/// One schedule (`GET /agent/schedules`).
struct FlowSchedule: Decodable, Equatable, Identifiable, Sendable {
    var id: String
    var name: String?
    var cron: String
    var kind: String
    var target: String
    var inputs: [String: String]
    var confirmSideEffects: Bool
    var enabled: Bool
    var nextRunAt: Date?
    var lastRun: ScheduleRun?

    enum CodingKeys: String, CodingKey {
        case id, name, cron, kind, target, inputs, enabled
        case confirmSideEffects = "confirm_side_effects"
        case nextRunAt = "next_run_at"
        case lastRun = "last_run"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try? c.decodeIfPresent(String.self, forKey: .name)
        cron = (try? c.decodeIfPresent(String.self, forKey: .cron)) ?? ""
        kind = (try? c.decodeIfPresent(String.self, forKey: .kind)) ?? "flow"
        target = (try? c.decodeIfPresent(String.self, forKey: .target)) ?? ""
        inputs = (try? c.decodeIfPresent([String: String].self, forKey: .inputs)) ?? [:]
        confirmSideEffects = (try? c.decodeIfPresent(Bool.self, forKey: .confirmSideEffects)) ?? false
        enabled = (try? c.decodeIfPresent(Bool.self, forKey: .enabled)) ?? true
        nextRunAt = ((try? c.decodeIfPresent(Double.self, forKey: .nextRunAt)) ?? nil).map(Date.init(timeIntervalSince1970:))
        lastRun = (try? c.decodeIfPresent(ScheduleRun.self, forKey: .lastRun)) ?? nil
    }
}

/// One run of a schedule.
struct ScheduleRun: Decodable, Equatable, Identifiable, Sendable {
    var id: String
    var scheduleID: String
    var scheduledFor: Date?
    /// `pending`, `postponed`, `waiting_unlock`, `running`, `passed`,
    /// `failed`, `missed`, `skipped`.
    var state: String
    var manual: Bool
    var summary: String?
    var error: String?
    var reason: String?
    var durationMs: Int?

    enum CodingKeys: String, CodingKey {
        case id, state, manual, summary, error, reason
        case scheduleID = "schedule_id"
        case scheduledFor = "scheduled_for"
        case durationMs = "duration_ms"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        scheduleID = (try? c.decodeIfPresent(String.self, forKey: .scheduleID)) ?? ""
        scheduledFor = ((try? c.decodeIfPresent(Double.self, forKey: .scheduledFor)) ?? nil)
            .map(Date.init(timeIntervalSince1970:))
        state = (try? c.decodeIfPresent(String.self, forKey: .state)) ?? "pending"
        manual = (try? c.decodeIfPresent(Bool.self, forKey: .manual)) ?? false
        summary = try? c.decodeIfPresent(String.self, forKey: .summary)
        error = try? c.decodeIfPresent(String.self, forKey: .error)
        reason = try? c.decodeIfPresent(String.self, forKey: .reason)
        durationMs = try? c.decodeIfPresent(Int.self, forKey: .durationMs)
    }

    var stateTitle: String {
        switch state {
        case "pending": return String(localized: "等待中")
        case "postponed": return String(localized: "已推迟")
        case "waiting_unlock": return String(localized: "等解锁")
        case "running": return String(localized: "运行中")
        case "passed": return String(localized: "成功")
        case "failed": return String(localized: "失败")
        case "missed": return String(localized: "错过了")
        case "skipped": return String(localized: "跳过")
        default: return state
        }
    }

    var succeeded: Bool { state == "passed" }
    var failed: Bool { state == "failed" || state == "missed" }
}

/// A new schedule (`POST /agent/schedules`).
struct NewSchedule: Encodable, Equatable, Sendable {
    var cron: String
    var kind = "flow"
    var target: String
    var inputs: [String: String]
    var confirmSideEffects: Bool
    var name: String?

    enum CodingKeys: String, CodingKey {
        case cron, kind, target, inputs, name
        case confirmSideEffects = "confirm_side_effects"
    }
}

// MARK: - pure helpers

/// The simple schedules a person picks on a phone: a time of day, on every
/// day or on chosen weekdays, as a 5-field cron line (`0 7 * * 1-5` style,
/// weekdays listed: `0 7 * * 1,2,3,4,5`). 0 is Sunday, as in cron.
struct CronSpec: Equatable, Sendable {
    var hour: Int
    var minute: Int
    /// Empty or all seven: every day.
    var weekdays: Set<Int>

    init(hour: Int, minute: Int, weekdays: Set<Int> = []) {
        self.hour = min(max(hour, 0), 23)
        self.minute = min(max(minute, 0), 59)
        self.weekdays = weekdays.filter { (0...6).contains($0) }
    }

    var everyDay: Bool { weekdays.isEmpty || weekdays.count == 7 }

    var cron: String {
        let days = everyDay ? "*" : weekdays.sorted().map(String.init).joined(separator: ",")
        return "\(minute) \(hour) * * \(days)"
    }

    /// Read back a line this builder could have written (or `1-5` ranges,
    /// `7` for Sunday); nil for anything else (it is shown as written).
    init?(cron: String) {
        let fields = cron.split(separator: " ", omittingEmptySubsequences: true).map(String.init)
        guard fields.count == 5, fields[2] == "*", fields[3] == "*",
              let minute = Int(fields[0]), let hour = Int(fields[1]),
              (0...59).contains(minute), (0...23).contains(hour) else { return nil }
        var days = Set<Int>()
        if fields[4] != "*" {
            for part in fields[4].split(separator: ",") {
                let bounds = part.split(separator: "-").compactMap { Int($0) }
                guard bounds.count == (part.contains("-") ? 2 : 1),
                      bounds.allSatisfy({ (0...7).contains($0) }), bounds.first! <= bounds.last! else { return nil }
                for day in bounds.first!...bounds.last! { days.insert(day % 7) }
            }
        }
        self.init(hour: hour, minute: minute, weekdays: days)
    }

    /// `每天 07:00`, `周一、周三 07:00`, `工作日 07:00`.
    var summary: String {
        let time = String(format: "%02d:%02d", hour, minute)
        if everyDay { return String(localized: "每天 \(time)") }
        if weekdays == [1, 2, 3, 4, 5] { return String(localized: "工作日 \(time)") }
        if weekdays == [0, 6] { return String(localized: "周末 \(time)") }
        let names = Self.order.filter(weekdays.contains).map(Self.shortName)
        return "\(names.joined(separator: String(localized: "、"))) \(time)"
    }

    /// Monday first, as a week reads in China and most of the world.
    static let order = [1, 2, 3, 4, 5, 6, 0]

    static func shortName(_ day: Int) -> String {
        switch day {
        case 0: return String(localized: "周日")
        case 1: return String(localized: "周一")
        case 2: return String(localized: "周二")
        case 3: return String(localized: "周三")
        case 4: return String(localized: "周四")
        case 5: return String(localized: "周五")
        default: return String(localized: "周六")
        }
    }
}

/// Local flow ids (`<app>/<name>`, lowercase slugs) made from what the
/// person sees: the app's bundle id and the name they typed.
enum FlowID {
    /// `[a-z0-9][a-z0-9_-]*`, at most 64 long; nil when nothing is left.
    static func slug(_ text: String) -> String? {
        var out = ""
        var lastDash = false
        for scalar in text.lowercased().unicodeScalars {
            if scalar.isASCII, CharacterSet.alphanumerics.contains(scalar) || scalar == "_" {
                out.unicodeScalars.append(scalar)
                lastDash = false
            } else if !out.isEmpty, !lastDash {
                out.append("-")
                lastDash = true
            }
        }
        while out.hasSuffix("-") || out.hasSuffix("_") { out.removeLast() }
        if out.count > 64 { out = String(out.prefix(64)) }
        while out.hasSuffix("-") { out.removeLast() }
        return out.isEmpty ? nil : out
    }

    /// The app part: the last component of the bundle id
    /// (`com.apple.Preferences` → `preferences`), else the app's name,
    /// else `app`.
    static func appPart(bundle: String?, label: String? = nil) -> String {
        if let bundle, let last = bundle.split(separator: ".").last, let slug = slug(String(last)) { return slug }
        if let label, let slug = slug(label) { return slug }
        return "app"
    }

    /// `<app>/<name>`; a name with no Latin letters or digits (`查电量`)
    /// becomes `flow-<yyyyMMdd-HHmm>`.
    static func make(bundle: String?, label: String? = nil, name: String, now: Date = Date()) -> String {
        let app = appPart(bundle: bundle, label: label)
        if let name = slug(name) { return "\(app)/\(name)" }
        let format = DateFormatter()
        format.locale = Locale(identifier: "en_US_POSIX")
        format.dateFormat = "yyyyMMdd-HHmm"
        return "\(app)/flow-\(format.string(from: now))"
    }

    /// What the daemon accepts as a local flow id.
    static func isValid(_ id: String) -> Bool {
        let parts = id.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        return parts.allSatisfy { part in
            guard let first = part.unicodeScalars.first, part.count <= 64 else { return false }
            let lowerOrDigit = { (s: Unicode.Scalar) in ("a"..."z").contains(s) || ("0"..."9").contains(s) }
            return lowerOrDigit(first) && part.unicodeScalars.allSatisfy { lowerOrDigit($0) || $0 == "-" || $0 == "_" }
        }
    }
}

// MARK: - client

extension DaemonClient {
    /// `GET /agent/flows`.
    func flows() async throws -> [FlowEntry] {
        let (data, response) = try await send(request("agent/flows"))
        try Self.checkAgent(response, data)
        struct List: Decodable { var flows: [FlowEntry]? }
        return (try? JSONDecoder().decode(List.self, from: data))?.flows ?? []
    }

    /// `POST /agent/flow/run`: runs it to the end before answering (tens of
    /// seconds), so never repeated.
    func runFlow(id: String, inputs: [String: String], confirm: Bool) async throws -> FlowRunResult {
        var urlRequest = agentRequest("agent/flow/run", method: "POST")
        var body: [String: Any] = ["id": id, "confirm": confirm]
        if !inputs.isEmpty { body["inputs"] = inputs }
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: body)
        urlRequest.timeoutInterval = 300
        let (data, response) = try await send(urlRequest, repeatable: false)
        if response.statusCode == 200 {
            return FlowRunResult(json: try? JSONSerialization.jsonObject(with: data))
        }
        try Self.checkAgent(response, data)
        return FlowRunResult(json: try? JSONSerialization.jsonObject(with: data))
    }

    /// `GET /agent/flow/record`.
    func recordingState() async throws -> RecordingState {
        let (data, response) = try await send(request("agent/flow/record"))
        try Self.checkAgent(response, data)
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        return RecordingState(recording: json["recording"] as? Bool ?? false, steps: json["steps"] as? Int ?? 0)
    }

    /// `POST /agent/flow/record {"action": "start"}`.
    func startRecording() async throws {
        var urlRequest = agentRequest("agent/flow/record", method: "POST")
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: ["action": "start"])
        let (data, response) = try await send(urlRequest)
        try Self.checkAgent(response, data)
    }

    /// `POST /agent/flow/record {"action": "stop"}` → the recording's draft
    /// (nil when nothing was recorded).
    func stopRecording() async throws -> FlowDraft? {
        var urlRequest = agentRequest("agent/flow/record", method: "POST")
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: ["action": "stop"])
        let (data, response) = try await send(urlRequest)
        try Self.checkAgent(response, data)
        return FlowDraft(json: try? JSONSerialization.jsonObject(with: data))
    }

    /// `GET /agent/flow/draft`: what the last task did, as a draft; nil when
    /// there is none.
    func lastDraft() async throws -> FlowDraft? {
        let (data, response) = try await send(request("agent/flow/draft"))
        if response.statusCode == 404 { return nil }
        try Self.checkAgent(response, data)
        return FlowDraft(json: try? JSONSerialization.jsonObject(with: data))
    }

    /// `POST /agent/flow/save`. Without `flow` the daemon saves the running
    /// or last recording, else what the last task did. Answers the saved id.
    @discardableResult
    func saveFlow(id: String, name: String, description: String?, flow: Data?) async throws -> String {
        var urlRequest = agentRequest("agent/flow/save", method: "POST")
        var body: [String: Any] = ["id": id, "name": name]
        if let description, !description.isEmpty { body["description"] = description }
        if let flow, let object = try? JSONSerialization.jsonObject(with: flow) { body["flow"] = object }
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (data, response) = try await send(urlRequest, repeatable: false)
        try Self.checkAgent(response, data)
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        return ((json?["saved"] as? [String: Any])?["id"] as? String) ?? id
    }

    /// `GET /agent/schedules`.
    func schedules() async throws -> [FlowSchedule] {
        let (data, response) = try await send(request("agent/schedules"))
        try Self.checkAgent(response, data)
        struct List: Decodable { var schedules: [FlowSchedule]? }
        return (try? JSONDecoder().decode(List.self, from: data))?.schedules ?? []
    }

    /// `POST /agent/schedules`.
    func createSchedule(_ schedule: NewSchedule) async throws {
        var urlRequest = agentRequest("agent/schedules", method: "POST")
        urlRequest.httpBody = try JSONEncoder().encode(schedule)
        let (data, response) = try await send(urlRequest, repeatable: false)
        try Self.checkAgent(response, data)
    }

    /// `PATCH /agent/schedules/<id> {"enabled": …}`.
    func setSchedule(_ id: String, enabled: Bool) async throws {
        var urlRequest = agentRequest("agent/schedules/\(id)", method: "PATCH")
        urlRequest.httpBody = try JSONSerialization.data(withJSONObject: ["enabled": enabled])
        let (data, response) = try await send(urlRequest)
        try Self.checkAgent(response, data)
    }

    /// `DELETE /agent/schedules/<id>`.
    func deleteSchedule(_ id: String) async throws {
        let (data, response) = try await send(agentRequest("agent/schedules/\(id)", method: "DELETE"))
        try Self.checkAgent(response, data)
    }

    /// `POST /agent/schedules/<id>/run`: run now (queued; see its runs).
    func runSchedule(_ id: String) async throws {
        let (data, response) = try await send(agentRequest("agent/schedules/\(id)/run", method: "POST"),
                                              repeatable: false)
        try Self.checkAgent(response, data)
    }

    /// `GET /agent/schedules/<id>/runs`, newest first.
    func scheduleRuns(_ id: String) async throws -> [ScheduleRun] {
        let (data, response) = try await send(request("agent/schedules/\(id)/runs"))
        try Self.checkAgent(response, data)
        struct List: Decodable { var runs: [ScheduleRun]? }
        return (try? JSONDecoder().decode(List.self, from: data))?.runs ?? []
    }
}
