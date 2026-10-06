import Foundation
import Observation
import Security
import UIKit

/// App state: the saved connection, the session with the daemon, the phone's
/// status, and the live video stream.
@MainActor
@Observable
final class RemoteModel {
    enum Phase: Equatable {
        case setup            // no saved connection yet
        case connecting
        case connected
        case failed(String)
    }

    var address: String = UserDefaults.standard.string(forKey: "address") ?? ""
    var phase: Phase = .setup
    var status: PhoneStatus?
    var videoLive = false
    var videoMessage: String?
    var toast: String?
    var busy = false
    var lastFrameAt: Date?
    /// Wireframe shown over a picture the app blanked (see `captureRedacted`).
    var redactedImage: UIImage?
    private var redactedTask: Task<Void, Never>?

    private var client: DaemonClient?
    private var reader: H264StreamReader?
    private weak var video: VideoDisplayView?
    private var statusTask: Task<Void, Never>?
    private var pendingActions = 0
    private var reloginTask: Task<Bool, Never>?
    /// Opening the app is the request to drive the phone: an idle-released
    /// device is started at once, once per foreground, instead of waiting for
    /// a tap on 连接手机. Never for a phone handed back to its holder.
    private var autoWakeTried = false

    init() {
        #if DEBUG
        // UI checks on the simulator: `-address <url> -password <pw>`, or
        // `-pair <QR text>` standing in for a scan (the simulator has no camera).
        let defaults = UserDefaults.standard
        if let scanned = defaults.string(forKey: "pair"), let link = PairLink.parse(scanned) {
            phase = .connecting
            Task { await pair(link) }
            return
        }
        if let address = defaults.string(forKey: "address"),
           let password = defaults.string(forKey: "password"), !password.isEmpty {
            self.address = address
            phase = .connecting
            Task { await connect(password: password) }
            return
        }
        #endif
        if !address.isEmpty, deviceToken != nil || Keychain.password(for: address) != nil {
            phase = .connecting
            Task { await connect(password: nil) }
        }
    }

    /// A scanned pairing's token for the saved address, if any.
    private var deviceToken: String? { Keychain.password(for: Self.deviceAccount(address)) }

    private static func deviceAccount(_ address: String) -> String { "device:" + address }

    /// Connect with a typed password, or (nil) with what was saved: the
    /// paired device token first, then the password.
    func connect(password: String?) async {
        guard let base = DaemonClient.parse(address: address) else {
            phase = .failed(DaemonError.badAddress.localizedDescription)
            return
        }
        let token = password == nil ? deviceToken : nil
        let password = password ?? Keychain.password(for: address)
        guard token != nil || password?.isEmpty == false else {
            phase = .setup
            return
        }
        phase = .connecting
        let client = DaemonClient(base: base)
        do {
            if let token {
                try await client.renew(deviceToken: token)
            } else if let password {
                try await client.login(password: password)
                Keychain.save(password: password, for: address)
            }
            try await finishConnecting(client)
        } catch DaemonError.pairingRevoked where Keychain.password(for: address) != nil {
            // The pairing died (password changed?); fall back to the password.
            Keychain.delete(for: Self.deviceAccount(address))
            await connect(password: nil)
        } catch {
            if case DaemonError.pairingRevoked = error { Keychain.delete(for: Self.deviceAccount(address)) }
            phase = .failed(error.localizedDescription)
        }
    }

    /// Connect from a scanned QR code: no address or password to type.
    /// The current connection, address and saved credentials stay untouched
    /// until the new daemon has accepted the code and answered a status read.
    func pair(_ link: PairLink) async {
        let wasConnected = phase == .connected
        if !wasConnected { phase = .connecting }
        let client = DaemonClient(base: link.base)
        do {
            let token = try await client.pair(code: link.code)
            let newAddress = link.base.absoluteString
            try await finishConnecting(client, replacingWith: newAddress)
            Keychain.save(password: token, for: Self.deviceAccount(newAddress))
        } catch {
            if wasConnected {
                show("扫码连接失败：\(error.localizedDescription)")
            } else {
                phase = .failed(error.localizedDescription)
            }
        }
    }

    /// A pairing link opened from outside the app (the camera's landing page,
    /// or any other app). It waits for the person to confirm, so a stray link
    /// cannot silently move the app to another server.
    var pendingLink: PairLink?

    func handle(url: URL) {
        guard let link = PairLink.parse(url.absoluteString) else { return }
        pendingLink = link
    }

    func confirmPendingLink() {
        guard let link = pendingLink else { return }
        pendingLink = nil
        Task { await pair(link) }
    }

    /// Commit a verified client. With `replacingWith`, the old session is
    /// dropped and the address switched only now that the new one works.
    private func finishConnecting(_ client: DaemonClient, replacingWith newAddress: String? = nil) async throws {
        let status = try await client.status()
        if let newAddress {
            disconnect()
            address = newAddress
        }
        UserDefaults.standard.set(address, forKey: "address")
        self.client = client
        self.status = status
        phase = .connected
        startPolling()
        updateStream()
    }

    func disconnect() {
        statusTask?.cancel()
        redactedTask?.cancel()
        redactedTask = nil
        redactedImage = nil
        reader?.stop()
        reader = nil
        client = nil
        status = nil
        videoLive = false
        phase = .setup
    }

    func forget() {
        Keychain.delete(for: address)
        Keychain.delete(for: Self.deviceAccount(address))
        disconnect()
    }

    // MARK: video

    func attach(video: VideoDisplayView) {
        self.video = video
        updateStream()
    }

    func frameArrived() {
        lastFrameAt = Date()
        if !videoLive { videoLive = true }
    }

    /// Stream only while the phone can show a picture; a parked phone has none.
    private func updateStream() {
        guard let client, let status, video != nil else { return }
        let wanted = !status.released && !status.releasing && !status.reconnecting
            && status.deviceState != "offline" && status.deviceState != "blocked"
        if wanted, reader == nil {
            let reader = H264StreamReader(
                request: client.request("agent/h264"),
                onMessage: { [weak self] message in self?.video?.enqueue(message) },
                onState: { [weak self] ok, why in
                    self?.videoMessage = why
                    if !ok { self?.videoLive = false }
                    if why?.contains("401") == true {
                        Task { _ = await self?.relogin() }
                    }
                })
            self.reader = reader
            video?.reset()
            reader.start()
        } else if !wanted, let reader {
            reader.stop()
            self.reader = nil
            videoLive = false
        }
    }

    /// The app came back to the foreground: allow one more automatic start,
    /// decided by the next fresh status read (the cached one may predate a
    /// hand-back made while the app was in the background).
    func becameActive() {
        autoWakeTried = false
    }

    private func maybeAutoWake(_ status: PhoneStatus) {
        guard !autoWakeTried, !busy, status.released, !status.releasing,
              !status.humanHandoff, status.recoveryOwner.isEmpty || status.recoveryOwner == "daemon"
        else { return }
        autoWakeTried = true
        connectPhone()
    }

    /// While the app on screen hides it from capture, refresh the wireframe
    /// every 1.5 s; drop it as soon as the picture is real again.
    private func updateRedactedOverlay() {
        let wanted = status?.captureRedacted == true && client != nil
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
                    self.status = status
                    self.updateStream()
                    self.maybeAutoWake(status)
                    self.updateRedactedOverlay()
                } catch DaemonError.sessionExpired {
                    _ = await self.relogin()
                } catch {}
                try? await Task.sleep(for: .seconds(self.status?.reconnecting == true ? 1 : 2))
            }
        }
    }

    /// The daemon's session cookie expires (8 h by default). Renew it with
    /// the paired device token, or log in again with the saved password;
    /// every caller shares one attempt.
    private func relogin() async -> Bool {
        if let running = reloginTask { return await running.value }
        let task = Task { @MainActor [weak self] () -> Bool in
            guard let self, let client = self.client else { return false }
            let token = self.deviceToken
            let password = Keychain.password(for: self.address)
            guard token != nil || password != nil else { return false }
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
                // The stream still carries the old cookie: restart it.
                self.reader?.stop()
                self.reader = nil
                self.updateStream()
                return true
            } catch {
                if case DaemonError.pairingRevoked = error {
                    Keychain.delete(for: Self.deviceAccount(self.address))
                }
                self.phase = .failed("登录已过期，请重新扫码或输入密码（\(error.localizedDescription)）")
                self.statusTask?.cancel()
                self.reader?.stop()
                self.reader = nil
                return false
            }
        }
        reloginTask = task
        let ok = await task.value
        reloginTask = nil
        return ok
    }

    // MARK: control

    func send(_ action: PhoneAction) {
        guard let client else { return }
        guard status?.drivable == true else {
            show(hintForUndrivable())
            return
        }
        pendingActions += 1
        Task {
            defer { pendingActions -= 1 }
            do {
                try await client.control(action)
            } catch DaemonError.sessionExpired {
                // Do not replay the gesture: the screen may have moved on.
                if await relogin() { show("登录已刷新，请再操作一次") }
            } catch {
                show("没有送达：\(error.localizedDescription)")
                if let status = try? await client.status() {
                    self.status = status
                    updateStream()
                }
            }
        }
    }

    /// Bring WDA up so the phone can be driven again.
    func connectPhone() {
        setMode("agent", success: "正在连接手机…手机锁着的话请解锁一次")
    }

    /// Stop remote control and leave the phone to whoever holds it.
    func handBack() {
        setMode("human", success: "已交还，远程控制已停止")
    }

    private func setMode(_ mode: String, success: String) {
        guard let client, !busy else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                try await client.setMode(mode)
                show(success)
                if let status = try? await client.status() {
                    self.status = status
                    updateStream()
                }
            } catch {
                show(error.localizedDescription)
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

    private func hintForUndrivable() -> String {
        guard let status else { return "还没连上服务" }
        if status.humanHandoff { return "手机已交还，先点「连接手机」" }
        if status.released { return "设备空闲中，先点「连接手机」" }
        if status.reconnecting { return "正在连接手机，请稍等" }
        if status.locked == true || status.deviceState == "locked" { return "手机锁屏了，请在手机上解锁" }
        return status.personHint.isEmpty ? "手机暂时不能操作" : status.personHint
    }
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
