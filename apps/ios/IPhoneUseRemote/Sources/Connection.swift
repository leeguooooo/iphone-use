import Foundation

// The connection lifecycle, as pure values: what went wrong when reaching a
// daemon, what a typed address means, how long to wait before trying again,
// and what the screen says about it. No UI and no networking here, so every
// rule is unit-tested.

// MARK: - What went wrong

/// Why a device is not connected, in terms of what the person can do about
/// it. Built from `DaemonError`; never shows a raw HTTP body.
enum ConnectProblem: Equatable, Sendable {
    /// How a request failed to reach the daemon.
    enum Reach: Equatable, Sendable {
        /// No answer in time: the Mac is asleep, off, or on another network.
        case timedOut
        /// The host answered but nothing listens on that port.
        case refused
        /// The name does not resolve.
        case noHost
        /// This iPhone has no network at all.
        case offline
        /// iOS refused: Local Network access is off for this app.
        case localNetworkDenied
        /// HTTPS failed (certificate, TLS).
        case tls
        case other
    }

    case badAddress
    /// Nothing saved to log in with (a scan that never completed).
    case noCredentials
    case wrongPassword
    /// Too many wrong passwords; the daemon refuses for 30 s.
    case lockedOut
    /// The scanned code was used already or is older than 5 minutes.
    case pairCodeExpired
    /// The saved pairing stopped working (the control password changed).
    case pairingRevoked
    /// The session ran out and could not be renewed with what is saved.
    case sessionExpired
    /// Something answered, but it is not iphone-use.
    case notIphoneUse
    /// The daemon predates scan-to-connect.
    case daemonTooOld
    /// The daemon answered with an error status.
    case server(Int)
    case unreachable(Reach)

    init(_ error: Error) {
        switch error {
        case let daemon as DaemonError:
            switch daemon {
            case .badAddress: self = .badAddress
            case .wrongPassword: self = .wrongPassword
            case .sessionExpired: self = .sessionExpired
            case .pairingCodeInvalid: self = .pairCodeExpired
            case .pairingRevoked: self = .pairingRevoked
            case .lockedOut: self = .lockedOut
            case .notIphoneUse: self = .notIphoneUse
            case .daemonTooOld: self = .daemonTooOld
            case let .http(code, _):
                self = code == 404 || (200..<400).contains(code) ? .notIphoneUse : .server(code)
            case .localNetworkDenied: self = .unreachable(.localNetworkDenied)
            case let .transport(code, _): self = .unreachable(Self.reach(code))
            case .unreachable: self = .unreachable(.other)
            }
        case let url as URLError:
            self = .unreachable(Self.reach(url.code))
        default:
            self = .unreachable(.other)
        }
    }

    static func reach(_ code: URLError.Code) -> Reach {
        switch code {
        case .timedOut, .networkConnectionLost: return .timedOut
        case .cannotConnectToHost: return .refused
        case .cannotFindHost, .dnsLookupFailed: return .noHost
        case .notConnectedToInternet, .dataNotAllowed, .internationalRoamingOff: return .offline
        case .secureConnectionFailed, .serverCertificateUntrusted, .serverCertificateHasBadDate,
             .serverCertificateNotYetValid, .serverCertificateHasUnknownRoot, .clientCertificateRejected,
             .appTransportSecurityRequiresSecureConnection:
            return .tls
        default: return .other
        }
    }

    /// Only the person can fix it: a password, a new scan, a new address.
    var needsLogin: Bool {
        switch self {
        case .noCredentials, .wrongPassword, .pairCodeExpired, .pairingRevoked, .sessionExpired: return true
        default: return false
        }
    }

    /// Worth trying again on its own (the Mac may wake, the network may come back).
    var retryable: Bool {
        switch self {
        case .unreachable, .server, .lockedOut: return true
        default: return false
        }
    }

    /// A fixed wait the daemon asked for.
    var retryAfter: TimeInterval? { self == .lockedOut ? 30 : nil }

    /// Title and explanation as one sentence (an inline error, a toast).
    var sentence: String { String(localized: "\(title)：\(message)") }

    var title: String {
        switch self {
        case .badAddress: return String(localized: "地址格式不对")
        case .noCredentials: return String(localized: "还没登录")
        case .wrongPassword: return String(localized: "密码不对")
        case .lockedOut: return String(localized: "密码错误次数太多")
        case .pairCodeExpired: return String(localized: "二维码已过期")
        case .pairingRevoked, .sessionExpired: return String(localized: "需要重新登录")
        case .notIphoneUse: return String(localized: "这个地址不是 iphone-use")
        case .daemonTooOld: return String(localized: "Mac 端版本太旧")
        case .server: return String(localized: "Mac 上的服务出错了")
        case .unreachable(.localNetworkDenied): return String(localized: "没有本地网络权限")
        case .unreachable(.offline): return String(localized: "这台设备没有联网")
        case .unreachable: return String(localized: "连不上 Mac")
        }
    }

    var message: String {
        switch self {
        case .badAddress:
            return String(localized: "应该像 192.168.1.11:44321，或者 https://xxx.trycloudflare.com")
        case .noCredentials:
            return String(localized: "这台 Mac 已保存，但还没登录。输入控制密码，或在 Mac 上重新扫码。")
        case .wrongPassword:
            return String(localized: "控制密码在 Mac 上运行安装程序时打印过，也可以在 Mac 上运行 iphone-use status 查看。")
        case .lockedOut:
            return String(localized: "为保护手机，Mac 暂停接受密码 30 秒，之后会自动再试。")
        case .pairCodeExpired:
            return String(localized: "二维码只能用一次，5 分钟内有效。在 Mac 的 iphone-use 页面点「换一个」再扫，或者输入控制密码。")
        case .pairingRevoked:
            return String(localized: "配对已失效（Mac 上的控制密码可能改过）。重新扫码，或输入新密码。")
        case .sessionExpired:
            return String(localized: "登录过期了，保存的信息也登不上。重新扫码，或输入控制密码。")
        case .notIphoneUse:
            return String(localized: "这个地址有服务在应答，但不是 iphone-use。检查一下端口（默认 44321）。")
        case .daemonTooOld:
            return String(localized: "Mac 上的 iphone-use 不支持扫码连接，请先升级；也可以用控制密码连接。")
        case let .server(code):
            return String(localized: "服务返回了错误（\(code)），稍后会自动重试。")
        case .unreachable(.timedOut):
            return String(localized: "等不到回应：Mac 可能睡眠了、关机了，或者和这台设备不在同一个网络。")
        case .unreachable(.refused):
            return String(localized: "找到了这台 Mac，但 iphone-use 没在运行，或者端口不对。")
        case .unreachable(.noHost):
            return String(localized: "找不到这个地址。检查拼写；隧道地址可能已经变了。")
        case .unreachable(.offline):
            return String(localized: "打开 Wi‑Fi 或蜂窝数据后会自动重连。")
        case .unreachable(.localNetworkDenied):
            return String(localized: "在「设置 › 隐私与安全性 › 本地网络」里打开 Phone Use Remote，才能连到局域网里的 Mac。")
        case .unreachable(.tls):
            return String(localized: "HTTPS 连接失败（证书有问题）。局域网地址请用 http://。")
        case .unreachable(.other):
            return String(localized: "网络请求失败，稍后会自动重试。")
        }
    }
}

// MARK: - What the person typed

/// A typed or pasted address: a daemon address, or a whole pairing link
/// (someone pasted the QR's text, or the camera's landing page URL).
enum AddressInput: Equatable {
    case address(URL)
    case pair(PairLink)

    init?(_ text: String) {
        if let link = PairLink.parse(Self.clean(text)) {
            self = .pair(link)
        } else if let url = DaemonClient.parse(address: text) {
            self = .address(url)
        } else {
            return nil
        }
    }

    /// Undo what phones and input methods do to a pasted address: full-width
    /// punctuation, stray spaces, a trailing period.
    static func clean(_ text: String) -> String {
        var text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        for (wide, narrow) in [("：", ":"), ("。", "."), ("．", "."), ("／", "/"), ("　", "")] {
            text = text.replacingOccurrences(of: wide, with: narrow)
        }
        text = text.replacingOccurrences(of: " ", with: "")
        while text.hasSuffix(".") { text.removeLast() }
        return text
    }

    /// Whether `host` is a name on the local network or an IP address, as
    /// opposed to a public name (a tunnel), which is served over https.
    static func isLocal(host: String) -> Bool {
        let host = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if host == "localhost" || !host.contains(".") || host.contains(":") { return true }
        if host.split(separator: ".").allSatisfy({ Int($0) != nil }) { return true }
        return [".local", ".lan", ".home", ".internal", ".localdomain", ".home.arpa"].contains { host.hasSuffix($0) }
    }
}

// MARK: - Retrying

/// How long to wait before the next automatic attempt.
enum RetryPolicy {
    static let steps: [TimeInterval] = [2, 4, 8, 15, 30]

    static func delay(attempt: Int, problem: ConnectProblem) -> TimeInterval {
        if let fixed = problem.retryAfter { return fixed }
        return steps[min(max(attempt, 0), steps.count - 1)]
    }
}

// MARK: - What the screen says

/// Everything that decides what a device's screen, tile and pill say.
struct ConnectionInputs: Equatable {
    enum Phase: Equatable {
        /// The person disconnected it.
        case idle
        case connecting
        case connected
        case failed(ConnectProblem)
    }
    var phase: Phase
    var status: PhoneStatus?
    var videoLive = false
    /// When the current attempt started (`connecting`).
    var connectingSince: Date?
    /// Status reads have been failing since then while connected.
    var linkDownSince: Date?
    var linkProblem: ConnectProblem?
    /// When the daemon was first seen starting the device service.
    var startingSince: Date?
    /// When the picture was first wanted and not there.
    var videoWaitSince: Date?
    /// When the next automatic attempt is due.
    var nextRetryAt: Date?
    /// A wake (`/agent/mode agent`) is in flight.
    var waking = false
    var now = Date()
}

/// The one description of a device's state that every surface uses (full
/// screen, tile, pill, VoiceOver), so they never disagree.
struct ConnectionPresentation: Equatable {
    enum Tone: Equatable { case ok, busy, attention, down }
    /// Where the explanation goes: over the picture (it cannot be driven),
    /// a strip above it (it can, with a caveat), or nowhere.
    enum Placement: Equatable { case cover, banner, none }
    enum Action: Equatable {
        /// Try connecting to the daemon again now.
        case retry
        /// Enter (or fix) the address and password.
        case login
        /// Scan a new QR code.
        case rescan
        /// Start the device runner (`/agent/mode agent`).
        case wakePhone
        /// Reconnect the video stream.
        case reloadVideo
        /// Open this app's page in Settings.
        case openSettings
    }

    var title: String
    var detail: String = ""
    /// A few words for a tile or a pill.
    var short: String
    var symbol: String
    var tone: Tone
    var placement: Placement
    var progress = false
    /// Seconds waited so far, shown next to a spinner.
    var elapsed: Int?
    /// Seconds until the next automatic attempt.
    var retryIn: Int?
    var primary: Action?
    var secondary: Action?

    static func make(_ i: ConnectionInputs) -> ConnectionPresentation {
        func seconds(since date: Date?) -> Int? {
            date.map { max(0, Int(i.now.timeIntervalSince($0))) }
        }
        let retryIn = i.nextRetryAt.map { max(0, Int(ceil($0.timeIntervalSince(i.now)))) }

        switch i.phase {
        case .idle:
            return .init(title: String(localized: "已断开"), detail: String(localized: "点下面的按钮重新连上这台 Mac。"),
                         short: String(localized: "已断开"), symbol: "pause.circle", tone: .down,
                         placement: .cover, primary: .retry)
        case .connecting:
            let waited = seconds(since: i.connectingSince) ?? 0
            return .init(title: String(localized: "正在连接 Mac…"),
                         detail: waited >= 6 ? String(localized: "比平时慢：Mac 可能在睡眠，或者网络不好。") : "",
                         short: String(localized: "正在连接…"), symbol: "hourglass", tone: .busy,
                         placement: .cover, progress: true, elapsed: waited >= 2 ? waited : nil)
        case let .failed(problem):
            var p = ConnectionPresentation(
                title: problem.title, detail: problem.message, short: problem.title,
                symbol: problem.needsLogin ? "key" : "wifi.exclamationmark",
                tone: problem.needsLogin ? .attention : .down, placement: .cover, retryIn: retryIn)
            switch problem {
            case .noCredentials, .wrongPassword, .sessionExpired, .pairingRevoked, .pairCodeExpired:
                p.primary = .login
                p.secondary = .rescan
                p.short = problem == .wrongPassword ? problem.title : String(localized: "需要登录")
            case .badAddress, .notIphoneUse:
                p.primary = .login
            case .daemonTooOld:
                p.primary = .login
            case .unreachable(.localNetworkDenied):
                p.primary = .openSettings
                p.secondary = .retry
            case .lockedOut:
                p.symbol = "lock.shield"
            default:
                p.primary = .retry
                p.secondary = .login
            }
            return p
        case .connected:
            break
        }

        if let down = i.linkDownSince {
            let reason = i.linkProblem.map(\.message) ?? ""
            return .init(title: String(localized: "和 Mac 的连接断了"),
                         detail: reason.isEmpty ? String(localized: "正在自动重连…") : reason,
                         short: String(localized: "重连中"), symbol: "wifi.exclamationmark", tone: .busy,
                         placement: .cover, progress: true, elapsed: seconds(since: down), primary: .retry)
        }
        guard let s = i.status else {
            return .init(title: String(localized: "正在连接 Mac…"), short: String(localized: "正在连接…"),
                         symbol: "hourglass", tone: .busy, placement: .cover, progress: true)
        }

        if s.ownedByOther {
            let who = s.owner ?? "?"
            return .init(title: String(localized: "正被「\(who)」使用"),
                         detail: String(localized: "另一个会话（AI 助手、网页或定时任务）正在操作这台手机。可以看画面，它停手 \(s.ownerLeaseRemainingSecs) 秒后就能操作。"),
                         short: String(localized: "被「\(who)」占用"), symbol: "person.badge.key", tone: .attention,
                         placement: .banner)
        }
        if s.humanHandoff {
            return .init(title: String(localized: "手机已交还"),
                         detail: String(localized: "手机在持有人手里，远程控制已停止。需要远程操作时点「连接手机」。"),
                         short: String(localized: "已交还"), symbol: "hand.raised", tone: .attention,
                         placement: .cover, progress: i.waking, primary: i.waking ? nil : .wakePhone)
        }
        if s.releasing {
            return .init(title: String(localized: "正在释放设备…"), detail: String(localized: "一会儿就好，之后可以再连接。"),
                         short: String(localized: "释放中"), symbol: "hourglass", tone: .busy,
                         placement: .cover, progress: true)
        }
        if !s.setupBlockedOn.isEmpty && !s.drivable {
            return blocked(s, retryIn: nil)
        }
        if s.reconnecting {
            let waited = seconds(since: i.startingSince)
            let slow = (waited ?? 0) >= 60
            let detail: String
            if slow {
                detail = String(localized: "比平时久。手机锁着的话请解锁一次；还不行可以在 Mac 上运行 iphone-use doctor。")
            } else if s.warming {
                detail = String(localized: "提前预热中，第一次大约 10–40 秒。")
            } else {
                detail = String(localized: "第一次启动大约要 10–40 秒。手机锁着的话请解锁一次。")
            }
            return .init(title: String(localized: "正在启动设备服务…"), detail: detail,
                         short: String(localized: "启动中"), symbol: "hourglass", tone: .busy,
                         placement: .cover, progress: true, elapsed: waited)
        }
        if s.released {
            if i.waking {
                return .init(title: String(localized: "正在唤醒手机…"), detail: String(localized: "手机锁着的话请解锁一次。"),
                             short: String(localized: "唤醒中"), symbol: "hourglass", tone: .busy,
                             placement: .cover, progress: true)
            }
            return .init(title: String(localized: "设备空闲中"),
                         detail: String(localized: "一段时间没人操作，连接已暂停，省电也不占手机。点「连接手机」继续（手机需解锁亮屏）。"),
                         short: String(localized: "空闲"), symbol: "moon.zzz", tone: .attention,
                         placement: .cover, primary: .wakePhone)
        }
        if !s.drivable, s.locked == true || s.deviceState == "locked" {
            return .init(title: String(localized: "手机锁屏了"),
                         detail: String(localized: "锁屏密码不能远程输入，请在手机上解锁。远程操作时可以把「自动锁定」调长一点。"),
                         short: String(localized: "锁屏"), symbol: "lock", tone: .attention, placement: .cover)
        }
        if !s.drivable, s.deviceState == "offline" || s.deviceState == "blocked" {
            let daemonCanFix = s.recoveryOwner.isEmpty || s.recoveryOwner == "daemon"
            return .init(title: String(localized: "连不上手机"),
                         detail: s.personHint.isEmpty ? String(localized: "Mac 上的设备服务没有运行。") : s.personHint,
                         short: String(localized: "手机离线"), symbol: "iphone.slash", tone: .down,
                         placement: .cover, progress: i.waking, primary: daemonCanFix && !i.waking ? .wakePhone : nil)
        }
        if !s.drivable {
            // `degraded` / `read_failing`: answers, but the last read failed;
            // the daemon recovers it on its own.
            return .init(title: String(localized: "手机反应慢，正在自动恢复"),
                         detail: s.personHint, short: String(localized: "恢复中"), symbol: "tortoise",
                         tone: .busy, placement: .banner)
        }
        if !i.videoLive {
            let waited = seconds(since: i.videoWaitSince) ?? 0
            if waited >= 15 {
                return .init(title: String(localized: "画面还没出来"),
                             detail: String(localized: "可以操作，但画面没有传过来。可能是网络慢，试试重新加载。"),
                             short: String(localized: "可操作 · 无画面"), symbol: "video.slash", tone: .busy,
                             placement: .cover, elapsed: waited, primary: .reloadVideo)
            }
            return .init(title: String(localized: "正在加载画面…"), short: String(localized: "加载画面"),
                         symbol: "hourglass", tone: .busy, placement: .cover, progress: true,
                         elapsed: waited >= 3 ? waited : nil)
        }
        return .init(title: String(localized: "可操作"), short: String(localized: "可操作"),
                     symbol: "checkmark.circle", tone: .ok, placement: .none)
    }

    /// A setup blocker: something on the Mac or the iPhone a person has to
    /// clear. The daemon's `next_step` says exactly what; the title names it.
    private static func blocked(_ s: PhoneStatus, retryIn: Int?) -> ConnectionPresentation {
        let title: String
        var detail = s.personHint
        var symbol = "exclamationmark.triangle"
        switch s.setupBlockedOn {
        case "locked":
            title = String(localized: "请解锁手机")
            symbol = "lock"
        case "wifi_automation_refused":
            title = String(localized: "需要插一次线")
            if detail.isEmpty {
                detail = String(localized: "这台 iPhone 不允许通过 Wi‑Fi 启动设备服务。用 USB 线插到 Mac 上启动一次（保持解锁，约 20 秒），之后拔掉就可以一直用 Wi‑Fi；手机重启后要再插一次。")
            }
            symbol = "cable.connector"
        case "usb", "not_connected":
            title = String(localized: "iPhone 没连到 Mac")
            symbol = "cable.connector"
        case "trust":
            title = String(localized: "请在 iPhone 上点「信任」")
        case "ios_too_old":
            title = String(localized: "iPhone 系统太旧")
        case "xcode_too_old":
            title = String(localized: "Mac 上的 Xcode 太旧")
        case "warp", "proxy":
            title = String(localized: "代理挡住了连接")
        case "account":
            title = String(localized: "Xcode 没登录 Apple ID")
        case "automation_mode_disabled", "automation_not_allowed":
            title = String(localized: "需要允许 UI 自动化")
        case "ddi":
            title = String(localized: "Mac 正在准备设备")
            symbol = "hourglass"
        case "wda":
            title = String(localized: "设备服务启动失败")
        default:
            title = String(localized: "需要处理一下")
        }
        if detail.isEmpty { detail = String(localized: "按 Mac 上 iphone-use 页面的提示处理，处理好后会自动接着连接。") }
        // Most blockers clear on their own once the person acts (the daemon
        // keeps trying); a retry button there would only restart a doomed
        // attempt. A failed runner start, or a blocker this app does not
        // know, can be retried by hand.
        let daemonCanFix = s.recoveryOwner.isEmpty || s.recoveryOwner == "daemon"
        let known = ["locked", "wifi_automation_refused", "usb", "not_connected", "trust", "ios_too_old",
                     "xcode_too_old", "warp", "proxy", "account", "automation_mode_disabled",
                     "automation_not_allowed", "ddi"]
        return .init(title: title, detail: detail, short: title, symbol: symbol, tone: .attention,
                     placement: .cover, retryIn: retryIn,
                     primary: daemonCanFix && !known.contains(s.setupBlockedOn) ? .wakePhone : nil)
    }

    /// The words on a button for `action`.
    static func label(_ action: Action) -> String {
        switch action {
        case .retry: return String(localized: "立即重试")
        case .login: return String(localized: "输入地址和密码")
        case .rescan: return String(localized: "重新扫码")
        case .wakePhone: return String(localized: "连接手机")
        case .reloadVideo: return String(localized: "重新加载画面")
        case .openSettings: return String(localized: "打开设置")
        }
    }
}
