import CryptoKit
import Foundation
import os

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
    /// What a person should do now, in the device language (`next_step`);
    /// empty when nothing is needed or the daemon predates it.
    var nextStep: String
    var setupBlockedOn: String
    var recoveryOwner: String
    var version: String
    /// The app on screen hides it from capture; the picture is blank and
    /// `/agent/screenshot` answers with a wireframe of its accessibility tree.
    var captureRedacted: Bool
    /// Who holds the owner lease (`X-Phone-Owner`), while it is live.
    var owner: String?
    var ownerLeaseRemainingSecs: Int

    /// Another session (an agent, the web page, a schedule) holds this
    /// phone's lease: a gesture from here would be refused with 409.
    var ownedByOther: Bool {
        guard let owner, !owner.isEmpty, ownerLeaseRemainingSecs > 0 else { return false }
        return owner != DaemonClient.ownerName
    }

    enum CodingKeys: String, CodingKey {
        case deviceState = "device_state"
        case drivable, released, reconnecting, releasing
        case humanHandoff = "human_handoff"
        case locked = "wda_locked"
        case hint
        case nextStep = "next_step"
        case setupBlockedOn = "setup_blocked_on"
        case recoveryOwner = "recovery_owner"
        case version
        case captureRedacted = "capture_redacted"
        case owner
        case ownerLeaseRemainingSecs = "owner_lease_remaining_secs"
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
        let steps = try c.decodeIfPresent([String: String].self, forKey: .nextStep) ?? [:]
        let chinese = Locale.preferredLanguages.first?.hasPrefix("zh") ?? true
        nextStep = (chinese ? steps["zh"] : steps["en"]) ?? steps["zh"] ?? ""
        setupBlockedOn = try c.decodeIfPresent(String.self, forKey: .setupBlockedOn) ?? ""
        recoveryOwner = try c.decodeIfPresent(String.self, forKey: .recoveryOwner) ?? ""
        version = try c.decodeIfPresent(String.self, forKey: .version) ?? ""
        captureRedacted = try c.decodeIfPresent(Bool.self, forKey: .captureRedacted) ?? false
        owner = try c.decodeIfPresent(String.self, forKey: .owner)
        ownerLeaseRemainingSecs = try c.decodeIfPresent(Int.self, forKey: .ownerLeaseRemainingSecs) ?? 0
    }

    /// The line to show a person: the daemon's `next_step`, else (older
    /// daemons) its diagnostic `hint`.
    var personHint: String { nextStep.isEmpty ? hint : nextStep }
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
    case pairingCodeInvalid
    case pairingRevoked
    case lockedOut
    case http(Int, String)
    case unreachable(String)
    /// A transport error, with its code: whether a gesture may have left
    /// this device depends on it (see `DeliveryOutcome.classify(transport:)`).
    case transport(URLError.Code, String)

    var errorDescription: String? {
        switch self {
        case .badAddress: return String(localized: "地址格式不对，应该像 http://192.168.1.11:44321")
        case .wrongPassword: return String(localized: "密码不对")
        case .sessionExpired: return String(localized: "登录已过期")
        case .pairingCodeInvalid: return String(localized: "二维码已用过或已过期，请在 Mac 上点「换一个」再扫")
        case .pairingRevoked: return String(localized: "配对已失效（可能改过控制密码），请重新扫码")
        case .lockedOut: return String(localized: "密码错误次数太多，30 秒后再试")
        case let .http(code, body): return String(localized: "服务返回 \(code)：\(String(body.prefix(160)))")
        case let .unreachable(why), let .transport(_, why): return String(localized: "连不上服务：\(why)")
        }
    }
}

/// Talks to the iphone-use daemon with the same session cookie a browser gets
/// from `/login`. The cookie lives in this client's own URLSession storage.
///
/// Two routes reach the same daemon: the paired address (often a public
/// https tunnel) and the Mac's LAN addresses that `/pair` reports. The
/// session cookie is the daemon's own, so it is valid on both; requests go
/// to whichever route is active, and a LAN route that stops answering falls
/// back to the paired address.
final class DaemonClient: @unchecked Sendable {
    /// The owner lease this app takes on every daemon it drives. Each daemon
    /// keeps its own lease, so driving several phones holds one per phone.
    static let ownerName = "ios-remote"

    /// The paired address: what credentials are saved under, and the route
    /// that always works.
    let publicBase: URL
    let session: URLSession
    /// `phone_session=…` from `/login`, sent by hand on every request (the
    /// video stream included) rather than trusting a cookie store.
    private(set) var cookie: String?
    /// Called (off the main actor) when a failed LAN request moved the
    /// client back to the paired address.
    var onFallback: (@Sendable () -> Void)?

    private struct Routes {
        var active: URL
        var lan: [URL]
        var lanKey: Data?
    }
    private let routes: OSAllocatedUnfairLock<Routes>
    /// Short-fused session for route probes: a LAN address that does not
    /// answer within this is not worth using.
    private let probeSession: URLSession

    /// Where requests go now.
    var base: URL { routes.withLock { $0.active } }
    /// True while requests go straight to the Mac on the LAN.
    var onLAN: Bool { base != publicBase }
    /// The daemon's LAN addresses (`lan_urls`), best first; empty from older daemons.
    var lanCandidates: [URL] { routes.withLock { $0.lan } }
    /// The key a LAN address must prove it holds (`lan_key`, from `/pair`)
    /// before it is sent any credential; nil from older daemons.
    var lanKey: Data? { routes.withLock { $0.lanKey } }

    init(base: URL, lanCandidates: [URL] = [], lanKey: Data? = nil) {
        self.publicBase = base
        routes = OSAllocatedUnfairLock(initialState: Routes(active: base, lan: lanCandidates, lanKey: lanKey))
        let probe = URLSessionConfiguration.ephemeral
        probe.httpShouldSetCookies = false
        probe.httpCookieAcceptPolicy = .never
        probe.timeoutIntervalForRequest = 0.6
        probe.timeoutIntervalForResource = 1
        probe.waitsForConnectivity = false
        probeSession = URLSession(configuration: probe)
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
        // A bare host means the daemon's own port on the LAN. An https address
        // is a tunnel (e.g. Cloudflare) that serves on 443, so keep its default.
        if components.port == nil, components.scheme?.lowercased() != "https" {
            components.port = 44321
        }
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
            try takeSessionCookie(from: response)
        case 401:
            throw DaemonError.wrongPassword
        case 429:
            throw DaemonError.lockedOut
        default:
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
    }

    /// Trade a scanned one-time code for a session. Returns the long-lived
    /// device token that renews the session later without the password.
    func pair(code: String) async throws -> String {
        let (data, response) = try await postPair(["code": code])
        try takeSessionCookie(from: response)
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let token = json["device_token"] as? String else {
            throw DaemonError.http(response.statusCode, String(localized: "配对没有返回设备令牌"))
        }
        return token
    }

    /// Fresh session from a paired device's token.
    func renew(deviceToken: String) async throws {
        let (_, response) = try await postPair(["device_token": deviceToken])
        try takeSessionCookie(from: response)
    }

    private func postPair(_ body: [String: String]) async throws -> (Data, HTTPURLResponse) {
        var request = URLRequest(url: base.appending(path: "pair"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (data, response) = try await send(request)
        switch response.statusCode {
        case 200:
            if let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let urls = json["lan_urls"] as? [String] {
                let lan = urls.compactMap(DaemonClient.parse(address:))
                let key = (json["lan_key"] as? String).flatMap(Data.init(base64URL:))
                routes.withLock {
                    $0.lan = lan
                    $0.lanKey = key
                }
            }
            return (data, response)
        case 401:
            throw body["code"] != nil ? DaemonError.pairingCodeInvalid : DaemonError.pairingRevoked
        case 429:
            throw DaemonError.lockedOut
        case 404:
            throw DaemonError.http(404, String(localized: "Mac 上的 iphone-use 版本太旧，不支持扫码连接，请先升级"))
        default:
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
    }

    private func takeSessionCookie(from response: HTTPURLResponse) throws {
        let header = response.value(forHTTPHeaderField: "Set-Cookie") ?? ""
        guard let pair = header.split(separator: ";").first.map(String.init),
              pair.hasPrefix("phone_session=") else {
            throw DaemonError.http(response.statusCode, String(localized: "登录没有返回会话"))
        }
        cookie = pair
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
    ///
    /// Answers what became of it instead of throwing, so a caller driving
    /// several phones can report each one. Never retried here: a gesture
    /// that failed on the LAN is not resent on the paired route either.
    func deliver(_ action: PhoneAction) async -> DeliveryOutcome {
        var body = action.json
        body["issued_at_ms"] = Int(Date().timeIntervalSince1970 * 1000)
        body["ttl_ms"] = 2500
        var request = URLRequest(url: base.appending(path: "control"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.setValue(Self.ownerName, forHTTPHeaderField: "X-Phone-Owner")
        guard let encoded = try? JSONSerialization.data(withJSONObject: body) else {
            return .notSent(reason: "invalid_action")
        }
        request.httpBody = encoded
        request.timeoutInterval = 15
        do {
            let (data, response) = try await send(request)
            return DeliveryOutcome.classify(status: response.statusCode, body: data)
        } catch DaemonError.transport(let code, _) {
            return DeliveryOutcome.classify(transport: code)
        } catch {
            return .outcomeUnknown
        }
    }

    /// Ask the live stream for a keyframe (the decoder lost its place).
    func requestKeyframe() async {
        var request = URLRequest(url: base.appending(path: "agent/h264/keyframe"))
        request.httpMethod = "POST"
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.timeoutInterval = 5
        _ = try? await send(request)
    }

    /// `agent` brings WDA up for remote control; `human` gives the phone back
    /// to the person holding it.
    func setMode(_ mode: String) async throws {
        var request = URLRequest(url: base.appending(path: "agent/mode"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("1", forHTTPHeaderField: "X-Phone-Control")
        request.setValue(Self.ownerName, forHTTPHeaderField: "X-Phone-Owner")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["mode": mode])
        request.timeoutInterval = 60
        let (data, response) = try await send(request)
        if response.statusCode == 401 { throw DaemonError.sessionExpired }
        guard (200..<300).contains(response.statusCode) else {
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
    }

    /// The current screen as `/agent/screenshot` answers it, and whether that
    /// answer is the wireframe of a screen hidden from capture.
    func screenshot() async throws -> (Data, redacted: Bool) {
        let (data, response) = try await send(request("agent/screenshot"))
        if response.statusCode == 401 { throw DaemonError.sessionExpired }
        guard response.statusCode == 200 else {
            throw DaemonError.http(response.statusCode, String(decoding: data, as: UTF8.self))
        }
        return (data, response.value(forHTTPHeaderField: "X-Capture-Redacted") == "1")
    }

    // MARK: routes

    /// The first LAN address that proves, within the probe timeout, that it
    /// is the daemon this app paired with, or nil. The probe carries no
    /// credential: whoever else holds that IP on another network learns
    /// nothing, and is never sent the session.
    func probeLAN() async -> URL? {
        let candidates = lanCandidates.filter { $0 != publicBase }
        guard !candidates.isEmpty, let lanKey else { return nil }
        let session = probeSession
        return await withTaskGroup(of: URL?.self) { group in
            for url in candidates {
                group.addTask {
                    let nonce = Data((0..<24).map { _ in UInt8.random(in: 0...255) }).base64URL
                    var components = URLComponents(url: url.appending(path: "pair/probe"),
                                                   resolvingAgainstBaseURL: false)
                    components?.queryItems = [URLQueryItem(name: "n", value: nonce)]
                    guard let probeURL = components?.url,
                          let answer = try? await session.data(from: probeURL),
                          (answer.1 as? HTTPURLResponse)?.statusCode == 200,
                          let json = try? JSONSerialization.jsonObject(with: answer.0) as? [String: Any],
                          let proof = json["proof"] as? String else { return nil }
                    let expected = HMAC<SHA256>.authenticationCode(
                        for: Data(nonce.utf8), using: SymmetricKey(data: lanKey))
                    return Data(expected).base64URL == proof ? url : nil
                }
            }
            for await hit in group {
                if let hit {
                    group.cancelAll()
                    return hit
                }
            }
            return nil
        }
    }

    /// Send requests to `url` (a probed LAN address) or, with nil, to the
    /// paired address. Returns whether the route changed.
    @discardableResult
    func use(_ url: URL?) -> Bool {
        let target = url ?? publicBase
        return routes.withLock { routes in
            guard routes.active != target else { return false }
            routes.active = target
            return true
        }
    }

    /// `request` aimed at the paired address instead of `from`.
    private func onPublic(_ request: URLRequest, from: URL) -> URLRequest? {
        guard let url = request.url, url.absoluteString.hasPrefix(from.absoluteString) else { return nil }
        var moved = request
        moved.url = URL(string: publicBase.absoluteString + url.absoluteString.dropFirst(from.absoluteString.count))
        if moved.value(forHTTPHeaderField: "Origin") != nil {
            moved.setValue(publicBase.absoluteString.trimmingCharacters(in: CharacterSet(charactersIn: "/")),
                           forHTTPHeaderField: "Origin")
        }
        return moved.url == nil ? nil : moved
    }

    /// A request for `path` with the session cookie attached.
    func request(_ path: String) -> URLRequest {
        var request = URLRequest(url: base.appending(path: path))
        if let cookie { request.setValue(cookie, forHTTPHeaderField: "Cookie") }
        return request
    }

    func request(_ path: String, query: [URLQueryItem]) -> URLRequest {
        var request = request(path)
        if let url = request.url, var components = URLComponents(url: url, resolvingAgainstBaseURL: false) {
            components.queryItems = query
            request.url = components.url ?? url
        }
        return request
    }

    /// Send `request`. When it went to a LAN address and never got an
    /// answer, the client falls back to the paired address; a request that is
    /// safe to repeat (anything but a gesture or a mode change) is retried
    /// there at once, a gesture is not, since it may have landed.
    private func send(_ request: URLRequest, followRedirects: Bool = true) async throws -> (Data, HTTPURLResponse) {
        var request = request
        if let cookie, request.value(forHTTPHeaderField: "Cookie") == nil {
            request.setValue(cookie, forHTTPHeaderField: "Cookie")
        }
        do {
            return try await transmit(request, followRedirects: followRedirects)
        } catch let error as URLError where error.code != .cancelled {
            let lan = ([base] + lanCandidates).first {
                $0 != publicBase && request.url?.absoluteString.hasPrefix($0.absoluteString + "/") == true
            }
            guard let lan, lan != publicBase else {
                throw DaemonError.transport(error.code, error.localizedDescription)
            }
            if routes.withLock({ routes -> Bool in
                guard routes.active == lan else { return false }
                routes.active = publicBase
                return true
            }) {
                onFallback?()
            }
            guard request.value(forHTTPHeaderField: "X-Phone-Owner") == nil,
                  let retry = onPublic(request, from: lan) else {
                throw DaemonError.transport(error.code, error.localizedDescription)
            }
            do {
                return try await transmit(retry, followRedirects: followRedirects)
            } catch let error as DaemonError {
                throw error
            } catch let error as URLError {
                throw DaemonError.transport(error.code, error.localizedDescription)
            } catch {
                throw DaemonError.unreachable(error.localizedDescription)
            }
        } catch let error as DaemonError {
            throw error
        } catch {
            throw DaemonError.unreachable(error.localizedDescription)
        }
    }

    private func transmit(_ request: URLRequest, followRedirects: Bool) async throws -> (Data, HTTPURLResponse) {
        let delegate = followRedirects ? nil : NoRedirect()
        let (data, response) = try await session.data(for: request, delegate: delegate)
        guard let http = response as? HTTPURLResponse else {
            throw DaemonError.unreachable("not HTTP")
        }
        return (data, http)
    }
}

extension Data {
    /// base64url without padding, as the daemon writes it.
    init?(base64URL text: String) {
        var plain = text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        plain += String(repeating: "=", count: (4 - plain.count % 4) % 4)
        self.init(base64Encoded: plain)
    }

    var base64URL: String {
        base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
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
