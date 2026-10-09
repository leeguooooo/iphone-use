import Foundation
import Network
import Observation
import Security
import UIKit

/// One paired daemon (one phone): its session, the phone's status, the live
/// video stream and the gestures sent to it. `AppModel` holds one per saved
/// device.
@MainActor
@Observable
final class DeviceSession: Identifiable {
    typealias Phase = ConnectionInputs.Phase

    /// Who is showing this phone's video. The focused screen asks for the
    /// person's chosen quality; a tile always streams in performance mode
    /// (half size, low bit rate), which keeps several decoders cheap.
    enum ViewerRole { case tile, full }

    var record: DeviceRecord
    nonisolated let id: UUID
    var address: String { record.address }
    var name: String { record.name }

    var phase: Phase = .idle {
        didSet {
            if phase == .connecting, oldValue != .connecting { connectingSince = Date() }
            if phase != .connecting { connectingSince = nil }
        }
    }
    var status: PhoneStatus? {
        didSet { trackStarting() }
    }
    /// When the current connection attempt started.
    private(set) var connectingSince: Date?
    /// Status reads have failed since then while connected: the Mac went to
    /// sleep, the network changed, the tunnel dropped. Polling continues and
    /// clears it on the first answer.
    private(set) var linkDownSince: Date?
    private(set) var linkProblem: ConnectProblem?
    private var pollFailures = 0
    /// When the daemon was first seen starting the device service.
    private(set) var startingSince: Date?
    /// When the picture was first wanted and missing.
    private(set) var videoWaitSince: Date?
    /// The next automatic connection attempt, while one is scheduled.
    private(set) var nextRetryAt: Date?
    private var retryAttempt = 0
    private var retryTask: Task<Void, Never>?
    /// A scanned code that could not be traded yet (the Mac was unreachable).
    /// Codes live 5 minutes on the daemon, so it is tried until then.
    var pendingPair: PendingPair?

    struct PendingPair: Equatable {
        let code: String
        let scannedAt: Date
        var fresh: Bool { Date().timeIntervalSince(scannedAt) < 290 }
    }
    var videoLive = false
    var videoMessage: String?
    var toast: String?
    var busy = false
    var lastFrameAt: Date?
    /// Wireframe shown over a picture the app blanked (see `captureRedacted`).
    var redactedImage: UIImage?
    /// Requests go straight to the Mac on the LAN rather than through the
    /// paired (tunnel) address.
    var onLAN = false
    /// The last gesture's result while sync is on, shown briefly on its tile.
    var delivery: DeliveryNote?

    struct DeliveryNote: Equatable {
        let outcome: DeliveryOutcome
        let seq: Int
    }

    /// The paired address is a public https tunnel, so the route is worth
    /// showing: 局域网 when direct, 外网 when through the tunnel.
    var routeLabel: String? {
        guard let client, phase == .connected else { return nil }
        if onLAN { return String(localized: "局域网") }
        return client.publicBase.scheme == "https" ? String(localized: "外网") : nil
    }

    /// The person's viewing choice for the focused screen (quality: native
    /// resolution, high frame rate). Tiles ignore it. A change that alters
    /// the running stream reconnects it, starting on a fresh keyframe.
    var preferQuality: Bool = false {
        didSet {
            guard preferQuality != oldValue, reader != nil, streamQuality != wantedQuality else { return }
            restartStream()
        }
    }

    /// The app is in the background: hold no stream, so the daemon sees no
    /// viewer and its idle release works as if the app were closed.
    var suspended = false {
        didSet {
            guard suspended != oldValue else { return }
            if suspended { cancelRetry() }
            updateStream()
        }
    }

    /// Opening a device is the request to drive it: an idle-released phone
    /// is started at once, once per foreground. `AppModel` allows it only
    /// for the device on screen (and sync members), never for every tile,
    /// so phones nobody looks at keep their idle release.
    var autoWakeAllowed = false {
        didSet { if autoWakeAllowed && !oldValue { autoWakeTried = false } }
    }

    private var redactedTask: Task<Void, Never>?
    private(set) var client: DaemonClient?
    private var reader: H264StreamReader?
    /// The mode the running reader asked for.
    private var streamQuality = false
    private weak var video: VideoDisplayView?
    private var role: ViewerRole = .tile
    private var statusTask: Task<Void, Never>?
    private var reloginTask: Task<Bool, Never>?
    private var autoWakeTried = false
    private var deliverySeq = 0
    /// Network changes (Wi-Fi joined or left) re-check the LAN route.
    private var pathMonitor: NWPathMonitor?
    private var probeTask: Task<Void, Never>?
    private var lastProbe: Date?
    /// While on the paired route, look for the LAN again this often.
    private static let reprobeInterval: TimeInterval = 30

    init(record: DeviceRecord) {
        self.record = record
        self.id = record.id
    }

    /// Saved credentials exist for this address: a paired device token or a
    /// password.
    static func hasCredentials(_ address: String) -> Bool {
        Keychain.password(for: deviceAccount(address)) != nil || Keychain.password(for: address) != nil
    }

    /// A scanned pairing's token for this address, if any.
    private var deviceToken: String? { Keychain.password(for: Self.deviceAccount(address)) }

    static func deviceAccount(_ address: String) -> String { "device:" + address }

    /// The daemon's LAN addresses last reported for `address` (`lan_urls`),
    /// kept for a password login, which does not report them.
    private static func savedLANCandidates(_ address: String) -> [URL] {
        (UserDefaults.standard.stringArray(forKey: lanURLsKey(address)) ?? [])
            .compactMap(DaemonClient.parse(address:))
    }

    private static func lanURLsKey(_ address: String) -> String { "lanURLs:" + address }

    /// Saves the client's LAN addresses and the key that checks them (a
    /// secret, so in the Keychain) for `address`.
    private static func saveLANRoutes(of client: DaemonClient, for address: String) {
        UserDefaults.standard.set(client.lanCandidates.map(\.absoluteString), forKey: lanURLsKey(address))
        if let key = client.lanKey {
            Keychain.save(password: key.base64URL, for: lanKeyAccount(address))
        }
    }

    private static func lanKeyAccount(_ address: String) -> String { "lan:" + address }

    /// The device moved to a new address (the person edited it): carry its
    /// password, pairing and LAN routes over, so nothing has to be re-entered.
    static func moveCredentials(from old: String, to new: String) {
        guard old != new else { return }
        let accounts: [(String) -> String] = [{ $0 }, deviceAccount, lanKeyAccount]
        for account in accounts {
            if let secret = Keychain.password(for: account(old)) {
                Keychain.save(password: secret, for: account(new))
                Keychain.delete(for: account(old))
            }
        }
        let defaults = UserDefaults.standard
        if let urls = defaults.stringArray(forKey: lanURLsKey(old)) {
            defaults.set(urls, forKey: lanURLsKey(new))
            defaults.removeObject(forKey: lanURLsKey(old))
        }
    }

    static func savePassword(_ password: String, for address: String) {
        Keychain.save(password: password, for: address)
    }

    private static func savedLANKey(_ address: String) -> Data? {
        Keychain.password(for: lanKeyAccount(address)).flatMap(Data.init(base64URL:))
    }

    /// Connect with a typed password, or (nil) with what was saved: the
    /// paired device token first, then the password.
    ///
    /// A failure the Mac may recover from on its own (asleep, unreachable,
    /// lockout) schedules the next attempt with backoff; one only the person
    /// can fix (wrong password, expired pairing) waits for them. The device
    /// stays saved either way.
    func connect(password typed: String?) async {
        // One attempt at a time; a typed password always gets its own.
        if typed == nil, phase == .connecting { return }
        cancelRetry()
        guard let base = DaemonClient.parse(address: address) else {
            phase = .failed(.badAddress)
            return
        }
        let token = typed == nil ? deviceToken : nil
        let password = typed ?? Keychain.password(for: address)
        let code = typed == nil && token == nil ? pendingPair.flatMap { $0.fresh ? $0.code : nil } : nil
        guard token != nil || code != nil || password?.isEmpty == false else {
            pendingPair = nil
            phase = .failed(.noCredentials)
            return
        }
        phase = .connecting
        let client = DaemonClient(base: base, lanCandidates: Self.savedLANCandidates(address),
                                  lanKey: Self.savedLANKey(address))
        do {
            if let token {
                try await client.renew(deviceToken: token)
            } else if let code {
                let token = try await client.pair(code: code)
                Keychain.save(password: token, for: Self.deviceAccount(address))
                pendingPair = nil
            } else if let password {
                try await client.login(password: password)
                Keychain.save(password: password, for: address)
            }
            try await adopt(client)
        } catch DaemonError.pairingRevoked where Keychain.password(for: address) != nil {
            // The pairing died (password changed?); fall back to the password.
            Keychain.delete(for: Self.deviceAccount(address))
            phase = .idle
            await connect(password: nil)
        } catch {
            fail(ConnectProblem(error))
        }
    }

    /// Record a failed attempt and decide what happens next.
    func fail(_ problem: ConnectProblem) {
        switch problem {
        case .pairingRevoked: Keychain.delete(for: Self.deviceAccount(address))
        case .pairCodeExpired: pendingPair = nil
        default: break
        }
        phase = .failed(problem)
        if problem.retryable { scheduleRetry(problem) }
    }

    /// Try again later, sooner at first. Paused while the app is in the
    /// background; coming back (or the network returning) retries at once.
    private func scheduleRetry(_ problem: ConnectProblem) {
        guard !suspended else { return }
        let delay = RetryPolicy.delay(attempt: retryAttempt, problem: problem)
        retryAttempt += 1
        nextRetryAt = Date().addingTimeInterval(delay)
        retryTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, let self else { return }
            self.retryTask = nil
            self.nextRetryAt = nil
            await self.connect(password: nil)
        }
    }

    private func cancelRetry() {
        retryTask?.cancel()
        retryTask = nil
        nextRetryAt = nil
    }

    /// Reconnect now if the device is waiting on something that may have
    /// changed (foreground, network back, pull to refresh). A device that
    /// needs a password is left alone.
    func retryIfWaiting() async {
        switch phase {
        case let .failed(problem) where !problem.needsLogin || pendingPair?.fresh == true:
            retryAttempt = 0
            await connect(password: nil)
        case .connected where linkDownSince != nil:
            await refreshStatus()
        default:
            break
        }
    }

    /// Commit a client that has a session (a login, a renewal or a fresh
    /// pairing). An existing connection is dropped only once the new one has
    /// answered a status read.
    func adopt(_ client: DaemonClient) async throws {
        // Look for the LAN while the paired address answers the status read,
        // so connecting off the LAN costs no extra round trip.
        async let lan = client.probeLAN()
        let status = try await client.status()
        client.use(await lan)
        if self.client != nil { teardown() }
        Self.saveLANRoutes(of: client, for: address)
        client.onFallback = { [weak self] in
            Task { @MainActor in self?.routeChanged() }
        }
        self.client = client
        self.status = status
        lastProbe = Date()
        onLAN = client.onLAN
        retryAttempt = 0
        cancelRetry()
        linkUp()
        phase = .connected
        startPolling()
        startPathMonitor()
        updateStream()
    }

    // MARK: route

    /// Look for the LAN again (foreground, network change, periodically
    /// while on the tunnel) and switch routes if the answer changed.
    func reprobe() {
        guard let client, probeTask == nil, !client.lanCandidates.isEmpty else { return }
        probeTask = Task { [weak self] in
            let hit = await client.probeLAN()
            guard let self, !Task.isCancelled else { return }
            self.probeTask = nil
            self.lastProbe = Date()
            guard self.client === client else { return }
            if client.use(hit) { self.routeChanged() }
        }
    }

    /// The route moved: requests already follow `client.base`; the stream
    /// holds its URL, so it reconnects on the new one.
    private func routeChanged() {
        guard let client else { return }
        onLAN = client.onLAN
        if reader != nil { restartStream() }
    }

    private func startPathMonitor() {
        guard pathMonitor == nil else { return }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            guard path.status == .satisfied else { return }
            Task { @MainActor in self?.reprobe() }
        }
        monitor.start(queue: .main)
        pathMonitor = monitor
    }

    private func teardown() {
        statusTask?.cancel()
        statusTask = nil
        probeTask?.cancel()
        probeTask = nil
        onLAN = false
        redactedTask?.cancel()
        redactedTask = nil
        redactedImage = nil
        reader?.stop()
        reader = nil
        client = nil
        status = nil
        videoLive = false
    }

    func disconnect() {
        cancelRetry()
        teardown()
        pathMonitor?.cancel()
        pathMonitor = nil
        linkUp()
        phase = .idle
    }

    /// Drop every saved credential for this device.
    func forget() {
        pendingPair = nil
        Keychain.delete(for: address)
        Keychain.delete(for: Self.deviceAccount(address))
        Keychain.delete(for: Self.lanKeyAccount(address))
        UserDefaults.standard.removeObject(forKey: Self.lanURLsKey(address))
        disconnect()
    }

    // MARK: video

    /// Show the stream in `video`. One view at a time: the newest attached
    /// one gets the frames (focusing a tile hands its stream to the full
    /// screen, or reconnects it when the quality differs).
    func attach(video: VideoDisplayView, role: ViewerRole) {
        let moved = self.video !== video
        self.video = video
        self.role = role
        if reader != nil, streamQuality != wantedQuality {
            restartStream()
            return
        }
        if moved, reader != nil {
            video.reset()
            requestKeyframe()
        }
        updateStream()
    }

    /// `video` is gone (tile scrolled away, screen closed). With no view left
    /// the stream stops: the daemon no longer counts this app as a viewer.
    func detach(video: VideoDisplayView) {
        guard self.video === video else { return }
        self.video = nil
        updateStream()
    }

    private var wantedQuality: Bool { role == .full && preferQuality }

    func requestKeyframe() {
        guard let client else { return }
        Task { await client.requestKeyframe() }
    }

    func frameArrived() {
        lastFrameAt = Date()
        if !videoLive { videoLive = true }
        videoWaitSince = nil
    }

    /// Reconnect the picture by hand (it never arrived).
    func reloadVideo() {
        videoWaitSince = Date()
        restartStream()
    }

    /// Stream only while someone shows the picture and the phone can show
    /// one; a parked phone has none.
    private func updateStream() {
        let showable = status.map {
            !$0.released && !$0.releasing && !$0.reconnecting
                && $0.deviceState != "offline" && $0.deviceState != "blocked"
        } ?? false
        let wanted = client != nil && video != nil && !suspended && showable
        if wanted, reader == nil, let client {
            let quality = wantedQuality
            let reader = H264StreamReader(
                request: client.request(
                    "agent/h264",
                    query: [URLQueryItem(name: "mode", value: quality ? "quality" : "performance")]),
                onMessage: { [weak self] message in self?.video?.enqueue(message) },
                onState: { [weak self] ok, why in
                    self?.videoMessage = why
                    if !ok { self?.videoLive = false }
                    // A dropped stream on the LAN may mean the LAN is gone:
                    // check it, and fall back to the paired address if so.
                    if !ok, self?.onLAN == true { self?.reprobe() }
                    if why?.contains("401") == true {
                        Task { _ = await self?.relogin() }
                    }
                })
            self.reader = reader
            streamQuality = quality
            if videoWaitSince == nil { videoWaitSince = Date() }
            video?.reset()
            reader.start()
        } else if !wanted, let reader {
            reader.stop()
            self.reader = nil
            videoLive = false
            videoWaitSince = nil
        }
    }

    private func restartStream() {
        reader?.stop()
        reader = nil
        videoLive = false
        updateStream()
    }

    /// The app came back to the foreground: allow one more automatic start,
    /// decided by the next fresh status read (the cached one may predate a
    /// hand-back made while the app was in the background).
    func becameActive() {
        autoWakeTried = false
        reprobe()
    }

    private func maybeAutoWake(_ status: PhoneStatus) {
        guard autoWakeAllowed, !autoWakeTried, !busy, status.released, !status.releasing,
              !status.humanHandoff, status.recoveryOwner.isEmpty || status.recoveryOwner == "daemon"
        else { return }
        autoWakeTried = true
        connectPhone()
    }

    /// While the app on screen hides it from capture, refresh the wireframe
    /// every 1.5 s; drop it as soon as the picture is real again. Only for
    /// the full screen: a tile shows the blank picture and its pill says why.
    private func updateRedactedOverlay() {
        let wanted = status?.captureRedacted == true && client != nil && role == .full && video != nil
        if wanted, redactedTask == nil {
            redactedTask = Task { [weak self] in
                while !Task.isCancelled {
                    guard let self, let client = self.client else { return }
                    if let (data, redacted) = try? await client.screenshot() {
                        self.redactedImage = redacted ? UIImage(data: data) : nil
                    }
                    try? await Task.sleep(for: .milliseconds(1500))
                }
            }
        } else if !wanted, let task = redactedTask {
            task.cancel()
            redactedTask = nil
            redactedImage = nil
        }
    }

    private func startPolling() {
        statusTask?.cancel()
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, let client = self.client else { return }
                do {
                    let status = try await client.status()
                    guard self.client === client else { return }
                    self.linkUp()
                    self.status = status
                    self.updateStream()
                    self.maybeAutoWake(status)
                    self.updateRedactedOverlay()
                    if !self.onLAN, Date().timeIntervalSince(self.lastProbe ?? .distantPast) > Self.reprobeInterval {
                        self.reprobe()
                    }
                } catch DaemonError.sessionExpired {
                    _ = await self.relogin()
                } catch {
                    guard self.client === client else { return }
                    self.linkFailed(ConnectProblem(error))
                }
                let pause: Double = self.linkDownSince != nil ? 3 : (self.status?.reconnecting == true ? 1 : 2)
                try? await Task.sleep(for: .seconds(pause))
            }
        }
    }

    /// A status read failed. One miss is noise (a slow tunnel); two in a row
    /// mean the Mac is gone for now, which the screen says instead of
    /// showing a frozen picture as if it were live.
    private func linkFailed(_ problem: ConnectProblem) {
        pollFailures += 1
        linkProblem = problem
        if problem == .notIphoneUse || problem.needsLogin {
            fail(problem)
            teardown()
            return
        }
        if pollFailures >= 2, linkDownSince == nil {
            linkDownSince = Date()
            reprobe()
        }
    }

    private func linkUp() {
        pollFailures = 0
        linkDownSince = nil
        linkProblem = nil
    }

    /// Note when the daemon started bringing the device service up, so the
    /// wait can be shown in seconds.
    private func trackStarting() {
        let starting = status.map { $0.reconnecting || $0.releasing } ?? false
        if starting, startingSince == nil { startingSince = Date() }
        if !starting { startingSince = nil }
    }

    /// The daemon's session cookie expires (8 h by default). Renew it with
    /// the paired device token, or log in again with the saved password;
    /// every caller shares one attempt.
    func relogin() async -> Bool {
        if let running = reloginTask { return await running.value }
        let task = Task { @MainActor [weak self] () -> Bool in
            guard let self, let client = self.client else { return false }
            let token = self.deviceToken
            let password = Keychain.password(for: self.address)
            guard token != nil || password != nil else {
                self.fail(.sessionExpired)
                self.teardown()
                return false
            }
            do {
                if let token {
                    do {
                        try await client.renew(deviceToken: token)
                    } catch DaemonError.pairingRevoked where password != nil {
                        // The pairing died (password changed?); the saved password may still work.
                        Keychain.delete(for: Self.deviceAccount(self.address))
                        try await client.login(password: password!)
                    }
                } else if let password {
                    try await client.login(password: password)
                }
                Self.saveLANRoutes(of: client, for: self.address)
                // The stream still carries the old cookie: restart it.
                self.reader?.stop()
                self.reader = nil
                self.updateStream()
                return true
            } catch {
                let problem = ConnectProblem(error)
                // Not reaching the Mac is not a login problem: keep the
                // session and let polling show the link as down.
                guard problem.needsLogin || problem == .notIphoneUse else {
                    self.linkFailed(problem)
                    return false
                }
                self.fail(problem)
                self.teardown()
                return false
            }
        }
        reloginTask = task
        let ok = await task.value
        reloginTask = nil
        return ok
    }

    // MARK: control

    /// Drive this phone alone: one gesture, its result as a toast on failure.
    func send(_ action: PhoneAction) {
        if let refusal = preflight() {
            if case .owned = refusal { show(refusal.sentence) } else { show(hintForUndrivable()) }
            return
        }
        guard let client else { return }
        Task {
            let outcome = await client.deliver(action)
            await settle(outcome, announce: true)
        }
    }

    /// Why a gesture cannot go to this phone right now, decided locally
    /// before anything is sent; nil when it can.
    func preflight() -> DeliveryOutcome? {
        guard client != nil, phase == .connected, let status else { return .notSent(reason: "not_connected") }
        if status.ownedByOther { return .owned(by: status.owner) }
        if !status.drivable { return .notSent(reason: "not_drivable") }
        return nil
    }

    /// Follow up on a gesture's outcome. Nothing is replayed: an expired
    /// session is renewed for the next gesture; any other failure refreshes
    /// the status so the screen shows why.
    func settle(_ outcome: DeliveryOutcome, announce: Bool) async {
        switch outcome {
        case .ok:
            return
        case .notSent(reason: "unauthorized"):
            if await relogin(), announce { show(String(localized: "登录已刷新，请再操作一次")) }
        default:
            if announce { show(outcome.sentence) }
            await refreshStatus()
        }
    }

    /// Show a gesture's result on this device's tile for a moment.
    func note(_ outcome: DeliveryOutcome) {
        deliverySeq += 1
        let note = DeliveryNote(outcome: outcome, seq: deliverySeq)
        delivery = note
        Task {
            try? await Task.sleep(for: .seconds(outcome == .ok ? 1.5 : 4))
            if delivery == note { delivery = nil }
        }
    }

    func refreshStatus() async {
        guard let client else { return }
        do {
            let status = try await client.status()
            guard self.client === client else { return }
            linkUp()
            self.status = status
            updateStream()
        } catch DaemonError.sessionExpired {
            _ = await relogin()
        } catch {
            guard self.client === client else { return }
            linkFailed(ConnectProblem(error))
        }
    }

    /// Bring the device runner up so the phone can be driven again.
    func connectPhone() {
        setMode("agent", success: String(localized: "正在连接手机…手机锁着的话请解锁一次"))
    }

    /// Stop remote control and leave the phone to whoever holds it.
    func handBack() {
        setMode("human", success: String(localized: "已交还，远程控制已停止"))
    }

    private func setMode(_ mode: String, success: String) {
        guard let client, !busy else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                try await client.setMode(mode)
                show(success)
                await refreshStatus()
            } catch DaemonError.sessionExpired {
                if await relogin() { show(String(localized: "登录已刷新，请再点一次")) }
            } catch {
                show(ConnectProblem(error).title)
            }
        }
    }

    func show(_ message: String) {
        toast = message
        Task {
            try? await Task.sleep(for: .seconds(3))
            if toast == message { toast = nil }
        }
    }

    func hintForUndrivable() -> String {
        let p = presentation()
        return p.detail.isEmpty ? p.title : String(localized: "\(p.title)：\(p.detail)")
    }

    func inputs(now: Date = Date()) -> ConnectionInputs {
        ConnectionInputs(phase: phase, status: status, videoLive: videoLive || video == nil,
                         connectingSince: connectingSince, linkDownSince: linkDownSince,
                         linkProblem: linkProblem, startingSince: startingSince,
                         videoWaitSince: videoWaitSince, nextRetryAt: nextRetryAt, waking: busy, now: now)
    }

    /// What every surface says about this device (see `ConnectionPresentation`).
    func presentation(now: Date = Date()) -> ConnectionPresentation {
        ConnectionPresentation.make(inputs(now: now))
    }

    /// One short state for a pill or a tile.
    var shortState: String { presentation().short }

    var health: Health { presentation().tone }

    typealias Health = ConnectionPresentation.Tone
}

/// The control password lives in the Keychain, keyed by server address; a
/// paired device token under `device:<address>`.
enum Keychain {
    private static let service = "com.leeguoo.iphone-use.remote"

    static func password(for account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func save(password: String, for account: String) {
        delete(for: account)
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
            kSecValueData as String: Data(password.utf8),
        ]
        SecItemAdd(item as CFDictionary, nil)
    }

    static func delete(for account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
