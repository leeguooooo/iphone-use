import Foundation

/// One paired daemon. A Mac runs one daemon per phone, each on its own port,
/// so one record is one phone. Credentials stay in the Keychain keyed by
/// `address` (as they were before there was a list), so a record only
/// carries what the person sees and chooses.
struct DeviceRecord: Codable, Identifiable, Equatable, Sendable {
    var id: UUID
    /// The paired base URL, e.g. `http://192.168.1.11:44321`.
    var address: String
    /// The person's name for it; defaults to `host:port`.
    var name: String
    /// Shown as a live tile on the overview. A hidden device keeps no stream
    /// open, so its daemon may idle-release the phone.
    var showInGrid: Bool

    init(id: UUID = UUID(), address: String, name: String? = nil, showInGrid: Bool = true) {
        self.id = id
        self.address = address
        self.name = name ?? DeviceStore.defaultName(for: address)
        self.showInGrid = showInGrid
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        address = try c.decode(String.self, forKey: .address)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? DeviceStore.defaultName(for: address)
        showInGrid = try c.decodeIfPresent(Bool.self, forKey: .showInGrid) ?? true
    }
}

/// The saved device list, in `UserDefaults` under `devices.v1`.
enum DeviceStore {
    static let key = "devices.v1"
    /// Where the single-device app kept its one address.
    static let legacyAddressKey = "address"

    /// The saved list. The first time (no list saved yet) the single pairing
    /// from before the list existed becomes its first entry, keyed by the
    /// same address so its Keychain items keep working: no re-pair.
    static func load(from defaults: UserDefaults, hasCredentials: (String) -> Bool) -> [DeviceRecord] {
        if let data = defaults.data(forKey: key),
           let list = try? JSONDecoder().decode([DeviceRecord].self, from: data) {
            return list
        }
        var list: [DeviceRecord] = []
        let legacy = defaults.string(forKey: legacyAddressKey)?.trimmingCharacters(in: .whitespaces) ?? ""
        if !legacy.isEmpty, hasCredentials(legacy) {
            list.append(DeviceRecord(address: legacy))
        }
        save(list, to: defaults)
        return list
    }

    static func save(_ list: [DeviceRecord], to defaults: UserDefaults) {
        if let data = try? JSONEncoder().encode(list) {
            defaults.set(data, forKey: key)
        }
    }

    /// The list with `address` in it: the existing entry when that daemon is
    /// already there (scanning a device again refreshes its pairing rather
    /// than adding a twin), else a new one at the end.
    static func upsert(_ address: String, into list: [DeviceRecord]) -> (list: [DeviceRecord], record: DeviceRecord) {
        if let existing = list.first(where: { same($0.address, address) }) {
            return (list, existing)
        }
        let record = DeviceRecord(address: address)
        return (list + [record], record)
    }

    /// Where the device last shown full screen is remembered.
    static let focusedKey = "focused.v1"

    static func loadFocused(from defaults: UserDefaults, in list: [DeviceRecord]) -> UUID? {
        if list.count == 1 { return list[0].id }
        guard let text = defaults.string(forKey: focusedKey), let id = UUID(uuidString: text),
              list.contains(where: { $0.id == id }) else { return nil }
        return id
    }

    static func saveFocused(_ id: UUID?, to defaults: UserDefaults) {
        if let id { defaults.set(id.uuidString, forKey: focusedKey) } else { defaults.removeObject(forKey: focusedKey) }
    }

    /// The saved device that already uses `address`, other than `except`.
    static func conflict(_ address: String, in list: [DeviceRecord], except: UUID? = nil) -> DeviceRecord? {
        list.first { $0.id != except && same($0.address, address) }
    }

    /// Two spellings of one daemon address (trailing slash, case of the host).
    static func same(_ a: String, _ b: String) -> Bool {
        normalize(a) == normalize(b)
    }

    private static func normalize(_ address: String) -> String {
        var text = address.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        while text.hasSuffix("/") { text.removeLast() }
        return text
    }

    /// `host:port`, which tells two instances on one Mac apart.
    static func defaultName(for address: String) -> String {
        guard let url = URL(string: address), let host = url.host() else { return address }
        if let port = url.port { return "\(host):\(port)" }
        return host
    }
}

/// What became of one gesture sent to one phone, in the daemon's own terms
/// (`outcome` on `/control` answers; see #196/#200). Nothing is ever retried
/// automatically: `outcomeUnknown` may have landed, and repeating a tap is
/// worse than asking the person to look.
enum DeliveryOutcome: Equatable, Sendable {
    /// The daemon applied it.
    case ok
    /// It never reached the phone; `reason` is the daemon's error code or a
    /// local one (`not_connected`, `not_drivable`, `unreachable`, `unauthorized`).
    case notSent(reason: String)
    /// Another session holds this phone's owner lease (409 `phone_owned`);
    /// it was skipped.
    case owned(by: String?)
    /// It went out and no answer says whether it landed.
    case outcomeUnknown
    /// The daemon answered with a final error.
    case failed(reason: String)

    var delivered: Bool { self == .ok }

    /// Classify a `/control` answer. 2xx is applied; the body's `outcome`
    /// decides the rest; a 5xx without one (a tunnel's gateway timeout, say)
    /// may have reached the daemon, so it is unknown, never "not sent".
    static func classify(status: Int, body: Data) -> DeliveryOutcome {
        if (200..<300).contains(status) { return .ok }
        let json = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
        let error = json["error"] as? String
        if status == 409, error == "phone_owned" {
            return .owned(by: json["owner"] as? String)
        }
        if status == 401 { return .notSent(reason: "unauthorized") }
        switch json["outcome"] as? String {
        case "not_sent": return .notSent(reason: error ?? "not_sent")
        case "unknown": return .outcomeUnknown
        default: break
        }
        // Lifecycle refusals (released/reconnecting/releasing) and malformed
        // requests are turned away before anything is dispatched.
        if status == 503, let error, ["released", "reconnecting", "releasing", "wda_not_configured"].contains(error) {
            return .notSent(reason: error)
        }
        if status == 400 { return .notSent(reason: error ?? "bad_request") }
        if status >= 500 { return .outcomeUnknown }
        return .failed(reason: error ?? "http_\(status)")
    }

    /// Classify a transport failure. Errors raised before the request left
    /// this device mean it was not sent; any other (timeout, dropped
    /// connection) may have been delivered.
    static func classify(transport code: URLError.Code) -> DeliveryOutcome {
        switch code {
        case .cannotFindHost, .cannotConnectToHost, .dnsLookupFailed, .notConnectedToInternet,
             .badURL, .unsupportedURL, .appTransportSecurityRequiresSecureConnection,
             .secureConnectionFailed, .serverCertificateUntrusted, .serverCertificateHasBadDate,
             .serverCertificateNotYetValid, .serverCertificateHasUnknownRoot, .internationalRoamingOff,
             .dataNotAllowed:
            return .notSent(reason: "unreachable")
        default:
            return .outcomeUnknown
        }
    }

    /// Short label for a tile badge.
    var badge: String {
        switch self {
        case .ok: return String(localized: "已送达")
        case .notSent: return String(localized: "未发送")
        case .owned: return String(localized: "被占用")
        case .outcomeUnknown: return String(localized: "结果未知")
        case .failed: return String(localized: "失败")
        }
    }

    /// A sentence for a toast.
    var sentence: String {
        switch self {
        case .ok:
            return String(localized: "已送达")
        case .notSent(let reason):
            return String(localized: "没有发出（\(Self.reasonText(reason))），可以再操作一次")
        case .owned(let owner):
            return String(localized: "手机正被「\(owner ?? "?")」控制，已跳过")
        case .outcomeUnknown:
            return String(localized: "不确定是否送达，请先看画面再决定要不要重做")
        case .failed(let reason):
            return String(localized: "没有送达：\(Self.reasonText(reason))")
        }
    }

    /// The person-facing words for a reason code; unknown codes pass through.
    static func reasonText(_ reason: String) -> String {
        switch reason {
        case "unauthorized": return String(localized: "登录已过期")
        case "not_connected": return String(localized: "未连接")
        case "not_drivable": return String(localized: "手机暂时不能操作")
        case "unreachable": return String(localized: "连不上服务")
        case "released": return String(localized: "设备空闲中")
        case "reconnecting", "releasing": return String(localized: "正在连接手机")
        default: return reason
        }
    }
}
