import Foundation
import Observation
import Security

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

    private var client: DaemonClient?
    private var reader: H264StreamReader?
    private weak var video: VideoDisplayView?
    private var statusTask: Task<Void, Never>?
    private var pendingActions = 0
    private var reloginTask: Task<Bool, Never>?

    init() {
        #if DEBUG
        // UI checks on the simulator: `-address <url> -password <pw>`.
        let defaults = UserDefaults.standard
        if let address = defaults.string(forKey: "address"),
           let password = defaults.string(forKey: "password"), !password.isEmpty {
            self.address = address
            phase = .connecting
            Task { await connect(password: password) }
            return
        }
        #endif
        if !address.isEmpty, Keychain.password(for: address) != nil {
            phase = .connecting
            Task { await connect(password: nil) }
        }
    }

    var savedPassword: String? { Keychain.password(for: address) }

    func connect(password: String?) async {
        guard let base = DaemonClient.parse(address: address) else {
            phase = .failed(DaemonError.badAddress.localizedDescription)
            return
        }
        guard let password = password ?? Keychain.password(for: address), !password.isEmpty else {
            phase = .setup
            return
        }
        phase = .connecting
        let client = DaemonClient(base: base)
        do {
            try await client.login(password: password)
            let status = try await client.status()
            UserDefaults.standard.set(address, forKey: "address")
            Keychain.save(password: password, for: address)
            self.client = client
            self.status = status
            phase = .connected
            startPolling()
            updateStream()
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    func disconnect() {
        statusTask?.cancel()
        reader?.stop()
        reader = nil
        client = nil
        status = nil
        videoLive = false
        phase = .setup
    }

    func forget() {
        Keychain.delete(for: address)
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

    private func startPolling() {
        statusTask?.cancel()
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, let client = self.client else { return }
                do {
                    self.status = try await client.status()
                    self.updateStream()
                } catch DaemonError.sessionExpired {
                    _ = await self.relogin()
                } catch {}
                try? await Task.sleep(for: .seconds(self.status?.reconnecting == true ? 1 : 2))
            }
        }
    }

    /// The daemon's session cookie expires (8 h by default). Log in again
    /// with the saved password; every caller shares one attempt.
    private func relogin() async -> Bool {
        if let running = reloginTask { return await running.value }
        let task = Task { @MainActor [weak self] () -> Bool in
            guard let self, let client = self.client,
                  let password = Keychain.password(for: self.address) else { return false }
            do {
                try await client.login(password: password)
                // The stream still carries the old cookie: restart it.
                self.reader?.stop()
                self.reader = nil
                self.updateStream()
                return true
            } catch {
                self.phase = .failed("登录已过期，请重新输入密码（\(error.localizedDescription)）")
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
        return status.hint.isEmpty ? "手机暂时不能操作" : status.hint
    }
}

/// The control password lives in the Keychain, keyed by server address.
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
