import Foundation

/// Will this phone get stuck at the lock screen? The daemon's `lock_readiness`
/// (passcode set, Auto-Lock setting, keep-awake, verdict and a one-line hint),
/// so an owner with many phones sees which ones need Auto-Lock set to Never.
struct LockReadiness: Decodable, Equatable, Sendable {
    enum Verdict: String, Sendable {
        /// Auto-Lock is Never: the phone does not lock on its own.
        case ready
        /// It locks when idle and has a passcode: a person must unlock it.
        case needsPerson = "will_lock_needs_person"
        /// It locks when idle but has no passcode: the daemon unlocks it.
        case autoUnlocks = "will_lock_auto_unlocks"
        case unknown
    }

    enum AutoLock: Equatable, Sendable {
        case never
        case seconds(Int)
    }

    var verdict: Verdict
    var passcodeProtected: Bool?
    var autoLock: AutoLock?
    var keepAwakeActive: Bool
    /// The daemon's hint in the device language (zh, else en).
    var hint: String

    enum CodingKeys: String, CodingKey {
        case verdict
        case passcodeProtected = "passcode_protected"
        case autoLock = "auto_lock_secs"
        case keepAwake = "keep_awake"
        case hint
    }

    private struct KeepAwake: Decodable {
        var active: Bool?
    }

    init(verdict: Verdict, passcodeProtected: Bool? = nil, autoLock: AutoLock? = nil,
         keepAwakeActive: Bool = false, hint: String = "") {
        self.verdict = verdict
        self.passcodeProtected = passcodeProtected
        self.autoLock = autoLock
        self.keepAwakeActive = keepAwakeActive
        self.hint = hint
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decodeIfPresent(String.self, forKey: .verdict) ?? ""
        verdict = Verdict(rawValue: raw) ?? .unknown
        passcodeProtected = try c.decodeIfPresent(Bool.self, forKey: .passcodeProtected)
        if let text = try? c.decodeIfPresent(String.self, forKey: .autoLock), text == "never" {
            autoLock = .never
        } else if let secs = try? c.decodeIfPresent(Int.self, forKey: .autoLock), secs > 0 {
            autoLock = .seconds(secs)
        } else {
            autoLock = nil
        }
        keepAwakeActive = (try? c.decodeIfPresent(KeepAwake.self, forKey: .keepAwake))?.active ?? false
        let hints = try? c.decodeIfPresent([String: String].self, forKey: .hint)
        let chinese = Locale.preferredLanguages.first?.hasPrefix("zh") ?? true
        hint = (chinese ? hints?["zh"] : hints?["en"]) ?? hints?["zh"] ?? ""
    }

    /// The badge on a tile and a list row; nil when there is nothing to fix
    /// (Auto-Lock is Never) or nothing known yet.
    var badge: LockBadge? {
        switch verdict {
        case .needsPerson: return .needsPerson
        case .autoUnlocks: return .autoUnlocks
        case .ready, .unknown: return nil
        }
    }

    /// "永不" / "30 秒" / "2 分钟", or nil when unknown.
    var autoLockText: String? {
        switch autoLock {
        case .never: return String(localized: "永不")
        case let .seconds(secs) where secs >= 60 && secs % 60 == 0:
            return String(localized: "\(secs / 60) 分钟")
        case let .seconds(secs): return String(localized: "\(secs) 秒")
        case nil: return nil
        }
    }

    var passcodeText: String {
        switch passcodeProtected {
        case true?: return String(localized: "已设置")
        case false?: return String(localized: "未设置")
        case nil: return String(localized: "未知")
        }
    }
}

/// How a phone that locks on its own is marked.
enum LockBadge: Equatable, Sendable {
    case needsPerson
    case autoUnlocks

    /// A few words for the badge itself.
    var title: String {
        switch self {
        case .needsPerson: return String(localized: "会锁屏")
        case .autoUnlocks: return String(localized: "可自动解锁")
        }
    }

    /// What VoiceOver says.
    var sentence: String {
        switch self {
        case .needsPerson: return String(localized: "闲置时会锁屏，需要有人解锁")
        case .autoUnlocks: return String(localized: "闲置时会锁屏，会自动解锁")
        }
    }

    var symbol: String {
        switch self {
        case .needsPerson: return "lock.trianglebadge.exclamationmark"
        case .autoUnlocks: return "lock.open"
        }
    }

    /// Needs a person: worth the owner's attention.
    var urgent: Bool { self == .needsPerson }
}

extension PhoneStatus {
    /// The lock badge for this phone, if it will lock on its own.
    var lockBadge: LockBadge? { lockReadiness?.badge }
}
