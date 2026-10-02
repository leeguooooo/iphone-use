import Foundation

/// What `/agent/status` says about the phone, reduced to what the remote needs.
struct PhoneStatus: Decodable, Equatable, Sendable {
    var deviceState: String
    var drivable: Bool
    var released: Bool
    var reconnecting: Bool
    var releasing: Bool
    var humanHandoff: Bool
    var locked: Bool?
    var hint: String
    var setupBlockedOn: String
    var recoveryOwner: String
    var version: String

    enum CodingKeys: String, CodingKey {
        case deviceState = "device_state"
        case drivable, released, reconnecting, releasing
        case humanHandoff = "human_handoff"
        case locked = "wda_locked"
        case hint
        case setupBlockedOn = "setup_blocked_on"
        case recoveryOwner = "recovery_owner"
        case version
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        deviceState = try c.decodeIfPresent(String.self, forKey: .deviceState) ?? "offline"
        drivable = try c.decodeIfPresent(Bool.self, forKey: .drivable) ?? false
        released = try c.decodeIfPresent(Bool.self, forKey: .released) ?? false
        reconnecting = try c.decodeIfPresent(Bool.self, forKey: .reconnecting) ?? false
        releasing = try c.decodeIfPresent(Bool.self, forKey: .releasing) ?? false
        humanHandoff = try c.decodeIfPresent(Bool.self, forKey: .humanHandoff) ?? false
        locked = try c.decodeIfPresent(Bool.self, forKey: .locked)
        hint = try c.decodeIfPresent(String.self, forKey: .hint) ?? ""
        setupBlockedOn = try c.decodeIfPresent(String.self, forKey: .setupBlockedOn) ?? ""
        recoveryOwner = try c.decodeIfPresent(String.self, forKey: .recoveryOwner) ?? ""
        version = try c.decodeIfPresent(String.self, forKey: .version) ?? ""
    }
}

/// One gesture or command for `POST /control`, in the shape the daemon's
/// browser path accepts. Coordinates are normalized to the phone screen.
enum PhoneAction: Sendable {
    case tap(x: Double, y: Double)
    case longPress(x: Double, y: Double, durationMs: Int)
    case swipe(x1: Double, y1: Double, x2: Double, y2: Double, durationMs: Int)
    case drag(x1: Double, y1: Double, x2: Double, y2: Double, holdMs: Int, durationMs: Int)
    case text(String)
    case home

    var json: [String: Any] {
        switch self {
        case let .tap(x, y):
            return ["type": "tap", "x": x, "y": y]
        case let .longPress(x, y, ms):
            return ["type": "longpress", "x": x, "y": y, "duration_ms": ms]
        case let .swipe(x1, y1, x2, y2, ms):
            return ["type": "swipe", "x1": x1, "y1": y1, "x2": x2, "y2": y2, "duration_ms": ms]
        case let .drag(x1, y1, x2, y2, hold, ms):
            return ["type": "drag", "x1": x1, "y1": y1, "x2": x2, "y2": y2,
                    "hold_ms": hold, "duration_ms": ms]
        case let .text(text):
            return ["type": "text", "text": text]
        case .home:
            return ["type": "shortcut", "name": "home"]
        }
    }
}

enum DaemonError: LocalizedError {
    case badAddress
    case wrongPassword
    case sessionExpired
    case lockedOut
    case http(Int, String)
    case unreachable(String)

    var errorDescription: String? {
        switch self {
        case .badAddress: return "地址格式不对，应该像 http://192.168.1.11:44321"
        case .wrongPassword: return "密码不对"
        case .sessionExpired: return "登录已过期"
        case .lockedOut: return "密码错误次数太多，30 秒后再试"
        case let .http(code, body): return "服务返回 \(code)：\(body.prefix(160))"
        case let .unreachable(why): return "连不上服务：\(why)"
        }
    }
}

/// Talks to the iphone-use daemon with the same session cookie a browser gets
/// from `/login`. The cookie lives in this client's own URLSession storage.
final class DaemonClient: @unchecked Sendable {
    let base: URL
    let session: URLSession
    /// `phone_session=…` from `/login`, sent by hand on every request (the
    /// video stream included) rather than trusting a cookie store.
    private(set) var cookie: String?

    init(base: URL) {
        self.base = base
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.httpCookieAcceptPolicy = .never
        config.timeoutIntervalForRequest = 8
        config.waitsForConnectivity = false
        // Each gesture is one small request; keep connections warm.
        config.httpMaximumConnectionsPerHost = 4
        session = URLSession(configuration: config)
    }

    static func parse(address: String) -> URL? {
        var text = address.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty { return nil }
        if !text.contains("://") { text = "http://" + text }
        guard var components = URLComponents(string: text), components.host != nil else { return nil }
        if components.port == nil { components.port = 44321 }
        components.path = ""
        return components.url
    }

    func login(password: String) async throws {
        var request = URLRequest(url: base.appending(path: "login"))
        request.httpMethod = "POST"
        request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        request.setValue(base.absoluteString.trimmingCharacters(in: CharacterSet(charactersIn: "/")),
                         forHTTPHeaderField: "Origin")
        var form = URLComponents()
        form.queryItems = [URLQueryItem(name: "password", value: password)]
        request.httpBody = form.percentEncodedQuery?.data(using: .utf8)
        let (data, response) = try await send(request, followRedirects: false)
        switch response.statusCode {
        case 303, 302, 200:
            let header = response.value(forHTTPHeaderField: "Set-Cookie") ?? ""
            guard let pair = header.split(separator: ";").first.map(String.init),
                  pair.hasPrefix("phone_session=") else {
                throw DaemonError.http(response.statusCode, "登录没有返回会话")
            }
            cookie = pair
        case 401:
            throw DaemonError.wrongPassword
        case 429:
            throw DaemonError.lockedOut
        default:
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
    }

    func status() async throws -> PhoneStatus {
        let (data, response) = try await send(URLRequest(url: base.appending(path: "agent/status")))
        guard response.statusCode == 200 else {
            if response.statusCode == 401 { throw DaemonError.sessionExpired }
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
        return try JSONDecoder().decode(PhoneStatus.self, from: data)
    }

    /// Send one action. The daemon drops it if it cannot start within the TTL,
    /// so a gesture never lands seconds late on a screen that moved on.
    @discardableResult
    func control(_ action: PhoneAction) async throws -> [String: Any] {
        var body = action.json
        body["issued_at_ms"] = Int(Date().timeIntervalSince1970 * 1000)
        body["ttl_ms"] = 2500
        var request = URLRequest(url: base.appending(path: "control"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.setValue("ios-remote", forHTTPHeaderField: "X-Phone-Owner")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        request.timeoutInterval = 15
        let (data, response) = try await send(request)
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        if response.statusCode == 401 { throw DaemonError.sessionExpired }
        guard (200..<300).contains(response.statusCode) else {
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
        return json
    }

    /// `agent` brings WDA up for remote control; `human` gives the phone back
    /// to the person holding it.
    func setMode(_ mode: String) async throws {
        var request = URLRequest(url: base.appending(path: "agent/mode"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.setValue("ios-remote", forHTTPHeaderField: "X-Phone-Owner")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["mode": mode])
        request.timeoutInterval = 60
        let (data, response) = try await send(request)
        if response.statusCode == 401 { throw DaemonError.sessionExpired }
        guard (200..<300).contains(response.statusCode) else {
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
    }

    /// A request for `path` with the session cookie attached.
    func request(_ path: String) -> URLRequest {
        var request = URLRequest(url: base.appending(path: path))
        if let cookie { request.setValue(cookie, forHTTPHeaderField: "Cookie") }
        return request
    }

    private func send(_ request: URLRequest, followRedirects: Bool = true) async throws -> (Data, HTTPURLResponse) {
        var request = request
        if let cookie, request.value(forHTTPHeaderField: "Cookie") == nil {
            request.setValue(cookie, forHTTPHeaderField: "Cookie")
        }
        do {
            let delegate = followRedirects ? nil : NoRedirect()
            let (data, response) = try await session.data(for: request, delegate: delegate)
            guard let http = response as? HTTPURLResponse else {
                throw DaemonError.unreachable("not HTTP")
            }
            return (data, http)
        } catch let error as DaemonError {
            throw error
        } catch {
            throw DaemonError.unreachable(error.localizedDescription)
        }
    }
}

/// Keeps `/login`'s 303 visible so its Set-Cookie is read and the status checked.
private final class NoRedirect: NSObject, URLSessionTaskDelegate {
    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest) async -> URLRequest? {
        nil
    }
}
