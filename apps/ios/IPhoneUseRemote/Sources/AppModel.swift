import Foundation
import Network
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
        didSet {
            guard focusedID != oldValue else { return }
            updateAutoWake()
            DeviceStore.saveFocused(focusedID, to: defaults)
        }
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

    /// The device whose address and password are being edited (a login
    /// prompt, or Settings › Edit).
    var editing: EditTarget?
    /// The scanner is open (from a device's "scan again" button).
    var scanning = false

    struct EditTarget: Identifiable, Equatable {
        let id: UUID
    }

    /// This iPhone has a network path at all.
    private(set) var online = true
    private var pathMonitor: NWPathMonitor?

    /// What became of adding or editing a device.
    enum AddOutcome: Equatable {
        /// Connected; the device is on screen.
        case connected(UUID)
        /// Saved, but not connected right now; the device retries on its own.
        case saved(UUID, ConnectProblem)
        /// Not saved: the message says why. `canSaveAnyway` offers to keep
        /// it regardless (something answered, but not the way expected).
        case rejected(String, canSaveAnyway: Bool)
    }

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
        // One device behaves as before the list existed: straight to its
        // screen; with several, the one last on screen comes back.
        focusedID = DeviceStore.loadFocused(from: defaults, in: devices)
        updateAutoWake()
        startNetworkMonitor()
        for session in sessions.values {
            Task { await session.connect(password: nil) }
        }
        #if DEBUG
        // UI checks on the simulator: `-address <url> -password <pw>`, or
        // `-pair <QR text>` standing in for a scan (the simulator has no camera).
        if defaults.bool(forKey: "grid") { focusedID = nil }
        if defaults.bool(forKey: "edit"), let id = focusedID { editing = .init(id: id) }
        if defaults.bool(forKey: "scan") { scanning = true }
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

    /// Pair from a scanned QR code. The scanned Mac is saved whatever
    /// happens: when the code cannot be traded now (the Mac is unreachable
    /// from here), the device keeps the code and tries it again for as long
    /// as it lives (5 min); after that it asks for the password or a new
    /// scan. Scanning a device already in the list refreshes its pairing
    /// instead of adding a twin.
    @discardableResult
    func pair(_ link: PairLink) async -> AddOutcome {
        adding = true
        addError = nil
        defer { adding = false }
        let address = link.base.absoluteString
        let client = DaemonClient(base: link.base)
        let (list, record) = DeviceStore.upsert(address, into: devices)
        let existing = sessions[record.id]
        let session = existing ?? DeviceSession(record: record)
        session.preferQuality = videoQuality
        if existing == nil {
            devices = list
            sessions[record.id] = session
            persist()
        }
        focusedID = record.id
        if session.phase != .connected { session.phase = .connecting }
        do {
            let token = try await client.pair(code: link.code)
            // Kept before anything else can fail: the code is spent now.
            Keychain.save(password: token, for: DeviceSession.deviceAccount(address))
            session.pendingPair = nil
            try await session.adopt(client)
            if existing != nil { show(String(localized: "已更新「\(session.name)」的配对")) }
            return .connected(record.id)
        } catch {
            let problem = ConnectProblem(error)
            if session.phase == .connected {
                // A working device stays as it was; only the new scan failed.
                show(problem.sentence)
                return .saved(record.id, problem)
            }
            if problem.retryable {
                session.pendingPair = .init(code: link.code, scannedAt: Date())
            }
            session.fail(problem)
            show(String(localized: "已保存这台 Mac，但现在没连上：\(problem.title)"))
            return .saved(record.id, problem)
        }
    }

    /// Add a device by address and control password. It is saved even when
    /// the Mac cannot be reached right now (it retries on its own); only a
    /// password the Mac refused, or an address that is not iphone-use, is
    /// sent back to the form. A pasted pairing link pairs instead.
    @discardableResult
    func add(address typed: String, password: String, saveAnyway: Bool = false) async -> AddOutcome {
        addError = nil
        guard let input = AddressInput(typed) else {
            let outcome = AddOutcome.rejected(ConnectProblem.badAddress.sentence,
                                              canSaveAnyway: false)
            addError = Self.message(outcome)
            return outcome
        }
        let base: URL
        switch input {
        case let .pair(link):
            return await pair(link)
        case let .address(url):
            base = url
        }
        adding = true
        defer { adding = false }
        let (list, record) = DeviceStore.upsert(base.absoluteString, into: devices)
        let existing = sessions[record.id]
        let session = existing ?? DeviceSession(record: record)
        session.preferQuality = videoQuality
        let keep = {
            if existing == nil {
                self.devices = list
                self.sessions[record.id] = session
                self.persist()
            }
            self.focusedID = record.id
        }
        if saveAnyway {
            DeviceSession.savePassword(password, for: record.address)
            keep()
            await session.connect(password: nil)
            return session.phase == .connected ? .connected(record.id) : .saved(record.id, Self.problem(of: session))
        }
        await session.connect(password: password)
        if Task.isCancelled {
            if existing == nil { session.disconnect() }
            return .rejected(String(localized: "已取消"), canSaveAnyway: false)
        }
        if session.phase == .connected {
            keep()
            return .connected(record.id)
        }
        let problem = Self.problem(of: session)
        switch problem {
        case .wrongPassword, .badAddress:
            let outcome = AddOutcome.rejected(problem.sentence, canSaveAnyway: false)
            if existing == nil { session.disconnect() } else { await session.connect(password: nil) }
            addError = Self.message(outcome)
            return outcome
        case .notIphoneUse:
            let outcome = AddOutcome.rejected(problem.sentence, canSaveAnyway: true)
            if existing == nil { session.disconnect() }
            addError = Self.message(outcome)
            return outcome
        default:
            // Unreachable, asleep, locked out, a server error: keep it, with
            // the typed password, and let it retry.
            DeviceSession.savePassword(password, for: record.address)
            keep()
            return .saved(record.id, problem)
        }
    }

    /// Change a saved device's name, address or password. A new address
    /// keeps the device's pairing and LAN routes; an empty password keeps the
    /// saved one. A password the Mac refuses is not saved.
    @discardableResult
    func update(_ id: UUID, name: String, address typed: String, password: String) async -> AddOutcome {
        guard let index = devices.firstIndex(where: { $0.id == id }), let session = sessions[id] else {
            return .rejected(String(localized: "这台设备已经不在列表里了"), canSaveAnyway: false)
        }
        guard let base = DaemonClient.parse(address: typed) else {
            return .rejected(ConnectProblem.badAddress.sentence, canSaveAnyway: false)
        }
        let address = base.absoluteString
        if let other = DeviceStore.conflict(address, in: devices, except: id) {
            return .rejected(String(localized: "这个地址已经保存为「\(other.name)」"), canSaveAnyway: false)
        }
        adding = true
        defer { adding = false }
        let old = devices[index].address
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !DeviceStore.same(old, address) {
            session.disconnect()
            DeviceSession.moveCredentials(from: old, to: address)
            devices[index].address = address
        }
        // An empty name, or the old default left as it was, follows the address.
        devices[index].name = trimmed.isEmpty || trimmed == DeviceStore.defaultName(for: old)
            ? DeviceStore.defaultName(for: address) : trimmed
        session.record = devices[index]
        persist()
        if password.isEmpty {
            await session.connect(password: nil)
        } else {
            await session.connect(password: password)
            let problem = Self.problem(of: session)
            if session.phase != .connected, problem == .wrongPassword {
                return .rejected(problem.sentence, canSaveAnyway: false)
            }
            if session.phase != .connected {
                DeviceSession.savePassword(password, for: address)
            }
        }
        focusedID = id
        return session.phase == .connected ? .connected(id) : .saved(id, Self.problem(of: session))
    }

    private static func problem(of session: DeviceSession) -> ConnectProblem {
        if case let .failed(problem) = session.phase { return problem }
        return .unreachable(.other)
    }

    static func message(_ outcome: AddOutcome) -> String? {
        switch outcome {
        case .connected: return nil
        case let .saved(_, problem): return problem.sentence
        case let .rejected(message, _): return message
        }
    }

    /// The name a pairing link's Mac is already saved under, if it is.
    func savedName(for link: PairLink) -> String? {
        DeviceStore.conflict(link.base.absoluteString, in: devices)?.name
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
            Task { await session.retryIfWaiting() }
        }
    }

    /// Pull to refresh: every waiting device tries again now, every
    /// connected one reads its status.
    func refreshAll() async {
        // All at once (one slow Mac must not hold up the others).
        let tasks = sessions.values.map { session in
            Task { @MainActor in
                if session.phase == .connected {
                    await session.refreshStatus()
                } else {
                    await session.retryIfWaiting()
                }
            }
        }
        for task in tasks { await task.value }
    }

    /// The network came back (or changed): devices waiting on it retry at once.
    private func startNetworkMonitor() {
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            let satisfied = path.status == .satisfied
            Task { @MainActor in
                guard let self else { return }
                let cameBack = satisfied && !self.online
                self.online = satisfied
                if cameBack {
                    for session in self.sessions.values { Task { await session.retryIfWaiting() } }
                }
            }
        }
        monitor.start(queue: .main)
        pathMonitor = monitor
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
