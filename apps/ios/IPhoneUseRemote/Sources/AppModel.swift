import Foundation
import Observation

/// The app: the saved devices (one session each), which one is on screen,
/// sync mode, pairing, and the demo.
@MainActor
@Observable
final class AppModel {
    private(set) var devices: [DeviceRecord] = []
    private(set) var sessions: [UUID: DeviceSession] = [:]

    /// The device shown full screen; nil shows the overview grid.
    var focusedID: UUID? {
        didSet { if focusedID != oldValue { updateAutoWake() } }
    }

    /// Sync ("同步"): every gesture on the lead goes to the lead and each
    /// follower at once. Nil when off.
    private(set) var sync: SyncGroup?

    struct SyncGroup: Equatable {
        var lead: UUID
        var followers: [UUID]
        var members: [UUID] { [lead] + followers.filter { $0 != lead } }
    }

    /// Adding a device (first launch, or from the device list).
    var adding = false
    var addError: String?
    var toast: String?

    /// The recorded demo, while someone is trying the app without a Mac.
    var demo: DemoSession?

    /// The person's viewing choice for the focused screen, remembered on this
    /// device. Tiles always stream in performance mode.
    var videoQuality: Bool = UserDefaults.standard.bool(forKey: "videoQuality") {
        didSet {
            guard videoQuality != oldValue else { return }
            UserDefaults.standard.set(videoQuality, forKey: "videoQuality")
            for session in sessions.values { session.preferQuality = videoQuality }
        }
    }

    /// A pairing link opened from outside the app (the camera's landing page,
    /// or any other app). It waits for the person to confirm, so a stray link
    /// cannot silently add a server.
    var pendingLink: PairLink?

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        #if DEBUG
        // Screenshots: `-demo YES [-demoScreen <id>]` opens the demo directly.
        if defaults.bool(forKey: "demo") {
            demo = DemoSession(startingAt: defaults.string(forKey: "demoScreen"))
            return
        }
        #endif
        devices = DeviceStore.load(from: defaults, hasCredentials: DeviceSession.hasCredentials)
        for record in devices { makeSession(record) }
        // One device behaves as before the list existed: straight to its screen.
        if devices.count == 1 { focusedID = devices[0].id }
        updateAutoWake()
        for session in sessions.values {
            Task { await session.connect(password: nil) }
        }
        #if DEBUG
        // UI checks on the simulator: `-address <url> -password <pw>`, or
        // `-pair <QR text>` standing in for a scan (the simulator has no camera).
        if let scanned = defaults.string(forKey: "pair"), let link = PairLink.parse(scanned) {
            Task { await pair(link) }
        } else if let address = defaults.string(forKey: "address"),
                  let password = defaults.string(forKey: "password"), !password.isEmpty {
            Task { await add(address: address, password: password) }
        }
        #endif
    }

    var focused: DeviceSession? { focusedID.flatMap { sessions[$0] } }

    func session(_ id: UUID) -> DeviceSession? { sessions[id] }

    /// Devices shown as tiles, in list order.
    var gridSessions: [DeviceSession] {
        devices.filter(\.showInGrid).compactMap { sessions[$0.id] }
    }

    func startDemo() {
        demo = DemoSession()
        if demo == nil { addError = String(localized: "演示内容缺失，请重新安装 App") }
    }

    @discardableResult
    private func makeSession(_ record: DeviceRecord) -> DeviceSession {
        let session = DeviceSession(record: record)
        session.preferQuality = videoQuality
        sessions[record.id] = session
        return session
    }

    private func persist() {
        DeviceStore.save(devices, to: defaults)
    }

    // MARK: adding and removing

    /// Pair from a scanned QR code. Nothing is saved until the daemon has
    /// accepted the code and answered a status read. Scanning a device
    /// already in the list refreshes its pairing instead of adding a twin.
    func pair(_ link: PairLink) async {
        adding = true
        addError = nil
        defer { adding = false }
        let address = link.base.absoluteString
        let client = DaemonClient(base: link.base)
        let (list, record) = DeviceStore.upsert(address, into: devices)
        let existing = sessions[record.id]
        let session = existing ?? DeviceSession(record: record)
        session.preferQuality = videoQuality
        do {
            let token = try await client.pair(code: link.code)
            try await session.adopt(client)
            Keychain.save(password: token, for: DeviceSession.deviceAccount(address))
        } catch {
            let message = String(localized: "扫码连接失败：\(error.localizedDescription)")
            addError = message
            show(message)
            return
        }
        if existing == nil {
            devices = list
            sessions[record.id] = session
            persist()
        }
        focusedID = record.id
    }

    /// Add (or re-login) a device by address and control password.
    func add(address typed: String, password: String) async {
        guard let base = DaemonClient.parse(address: typed) else {
            addError = DaemonError.badAddress.localizedDescription
            return
        }
        adding = true
        addError = nil
        defer { adding = false }
        let (list, record) = DeviceStore.upsert(base.absoluteString, into: devices)
        let existing = sessions[record.id]
        let session = existing ?? DeviceSession(record: record)
        session.preferQuality = videoQuality
        await session.connect(password: password)
        guard session.phase == .connected else {
            if case let .failed(why) = session.phase { addError = why }
            return
        }
        if existing == nil {
            devices = list
            sessions[record.id] = session
            persist()
        }
        focusedID = record.id
    }

    func handle(url: URL) {
        guard let link = PairLink.parse(url.absoluteString) else { return }
        pendingLink = link
    }

    func confirmPendingLink() {
        guard let link = pendingLink else { return }
        pendingLink = nil
        Task { await pair(link) }
    }

    func rename(_ id: UUID, to name: String) {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let index = devices.firstIndex(where: { $0.id == id }) else { return }
        devices[index].name = trimmed.isEmpty ? DeviceStore.defaultName(for: devices[index].address) : trimmed
        sessions[id]?.record = devices[index]
        persist()
    }

    func setShowInGrid(_ id: UUID, _ shown: Bool) {
        guard let index = devices.firstIndex(where: { $0.id == id }) else { return }
        devices[index].showInGrid = shown
        sessions[id]?.record = devices[index]
        persist()
    }

    func move(from source: IndexSet, to destination: Int) {
        devices.move(fromOffsets: source, toOffset: destination)
        persist()
    }

    /// Forget a device: its credentials, its session and its place in sync.
    func remove(_ id: UUID) {
        sessions[id]?.forget()
        sessions[id] = nil
        devices.removeAll { $0.id == id }
        if focusedID == id { focusedID = nil }
        if var group = sync {
            group.followers.removeAll { $0 == id }
            sync = group.lead == id ? nil : group
        }
        persist()
        updateAutoWake()
    }

    // MARK: app lifecycle

    func becameActive() {
        for session in sessions.values {
            session.suspended = false
            session.becameActive()
            // A device whose session died while away is retried on return.
            if case .failed = session.phase {
                Task { await session.connect(password: nil) }
            }
        }
    }

    /// Background: every stream closes, so no daemon counts this app as a
    /// viewer and idle release works as if the app were closed.
    func enteredBackground() {
        for session in sessions.values { session.suspended = true }
    }

    // MARK: sync

    /// Start sync with `lead` driving `followers`. Each member that was
    /// idle-released is woken once, since driving it is the point.
    func startSync(lead: UUID, followers: [UUID]) {
        let group = SyncGroup(lead: lead, followers: followers.filter { $0 != lead && sessions[$0] != nil })
        guard sessions[lead] != nil, !group.followers.isEmpty else { return }
        sync = group
        focusedID = lead
        updateAutoWake()
    }

    func stopSync() {
        sync = nil
        focusedID = nil
        updateAutoWake()
    }

    /// Only the device on screen (and, in sync, every member) may be woken
    /// automatically; other tiles leave their phones to idle release.
    private func updateAutoWake() {
        let members = Set(sync?.members ?? [])
        for (id, session) in sessions {
            session.autoWakeAllowed = id == focusedID || members.contains(id)
        }
    }

    /// One gesture from the screen of `origin`. Outside sync, or from a
    /// device that is not the lead, it drives that device alone; from the
    /// lead it goes to every member at once.
    func dispatch(_ action: PhoneAction, from origin: DeviceSession) {
        guard let group = sync, group.lead == origin.id else {
            origin.send(action)
            return
        }
        let members = group.members.compactMap { sessions[$0] }
        Task { await fanOut(to: members) { client in await client.deliver(action) } }
    }

    /// Run `operation` on every session at once and show each result on its
    /// tile. Members that cannot take it (not connected, not drivable, held
    /// by another owner) are skipped and marked without sending anything.
    /// Nothing is retried: `outcomeUnknown` may have landed.
    ///
    /// Phase 2 (step sync) plugs in here: an operation that runs a saved flow
    /// on each daemon and reports per-device progress (see the PR's design note).
    func fanOut(to members: [DeviceSession],
                _ operation: @escaping @Sendable (DaemonClient) async -> DeliveryOutcome) async {
        var running: [(DeviceSession, DaemonClient)] = []
        for session in members {
            if let refusal = session.preflight() {
                session.note(refusal)
            } else if let client = session.client {
                running.append((session, client))
            }
        }
        let byID = Dictionary(uniqueKeysWithValues: running.map { ($0.0.id, $0.0) })
        await withTaskGroup(of: (UUID, DeliveryOutcome).self) { group in
            for (session, client) in running {
                let id = session.id
                group.addTask { (id, await operation(client)) }
            }
            for await (id, outcome) in group {
                guard let session = byID[id] else { continue }
                session.note(outcome)
                Task { await session.settle(outcome, announce: false) }
            }
        }
    }

    /// Wake every sync member that is idle-released or handed back.
    func connectAll(_ ids: [UUID]) {
        for id in ids {
            guard let session = sessions[id], let status = session.status,
                  status.released || status.humanHandoff else { continue }
            session.connectPhone()
        }
    }

    func show(_ message: String) {
        toast = message
        Task {
            try? await Task.sleep(for: .seconds(3))
            if toast == message { toast = nil }
        }
    }
}
