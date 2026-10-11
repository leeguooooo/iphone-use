import Foundation
import CryptoKit
import WebKit
import Combine
import Security

enum JSONValue: Codable, Equatable, Sendable {
    case string(String), number(Double), bool(Bool), object([String: JSONValue]), array([JSONValue]), null
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([String: JSONValue].self) { self = .object(v) }
        else { self = .array(try c.decode([JSONValue].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
    var string: String? { if case .string(let v) = self { return v }; return nil }
    var bool: Bool? { if case .bool(let v) = self { return v }; return nil }
    var object: [String: JSONValue]? { if case .object(let v) = self { return v }; return nil }
    var array: [JSONValue]? { if case .array(let v) = self { return v }; return nil }
    var any: Any {
        switch self {
        case .string(let v): return v
        case .number(let v): return v
        case .bool(let v): return v
        case .object(let v): return v.mapValues(\.any)
        case .array(let v): return v.map(\.any)
        case .null: return NSNull()
        }
    }
}

struct BrowserCookie: Codable, Equatable, Sendable {
    var name: String
    var value: String
    var domain: String
    var path: String = "/"
    var secure: Bool = true
    var httpOnly: Bool = false
    var hostOnly: Bool?
    var expires: Double?
    var expirationDate: Double?
    var sameSite: String?
    var partitionKey: JSONValue?

    init(name: String, value: String, domain: String, path: String = "/", secure: Bool = true, httpOnly: Bool = false) {
        self.name = name; self.value = value; self.domain = domain; self.path = path; self.secure = secure; self.httpOnly = httpOnly
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name); value = try c.decode(String.self, forKey: .value)
        domain = try c.decode(String.self, forKey: .domain); path = try c.decodeIfPresent(String.self, forKey: .path) ?? "/"
        secure = try c.decodeIfPresent(Bool.self, forKey: .secure) ?? false
        httpOnly = try c.decodeIfPresent(Bool.self, forKey: .httpOnly) ?? false
        hostOnly = try c.decodeIfPresent(Bool.self, forKey: .hostOnly)
        expires = try c.decodeIfPresent(Double.self, forKey: .expires)
        expirationDate = try c.decodeIfPresent(Double.self, forKey: .expirationDate)
        sameSite = try c.decodeIfPresent(String.self, forKey: .sameSite)
        partitionKey = try c.decodeIfPresent(JSONValue.self, forKey: .partitionKey)
    }
    func cookie(allowedDomains: [String], now: Date = Date()) -> HTTPCookie? {
        let host = domain.trimmingCharacters(in: CharacterSet(charactersIn: ".")).lowercased()
        guard BrowserLibrary.domainAllowed(host, in: allowedDomains), partitionKey == nil || partitionKey == .null,
              !name.isEmpty, name.utf8.count <= 256, value.utf8.count <= 64_000, path.hasPrefix("/"),
              !name.contains(where: { $0.isWhitespace || "()<>@,;:\\\"/[]?={}".contains($0) }),
              !value.unicodeScalars.contains(where: { $0.value < 32 || $0.value == 127 }) else { return nil }
        var properties: [HTTPCookiePropertyKey: Any] = [.name: name, .value: value, .domain: hostOnly == true ? host : domain, .path: path]
        if secure { properties[.secure] = "TRUE" }
        if httpOnly { properties[HTTPCookiePropertyKey("HttpOnly")] = "TRUE" }
        if let expiry = expires ?? expirationDate, expiry > 0 {
            guard expiry > now.timeIntervalSince1970 else { return nil }
            properties[.expires] = Date(timeIntervalSince1970: expiry)
        }
        if let sameSite { properties[.sameSitePolicy] = sameSite.capitalized }
        return HTTPCookie(properties: properties)
    }
    init(_ cookie: HTTPCookie) {
        name = cookie.name; value = cookie.value; domain = cookie.domain; path = cookie.path
        secure = cookie.isSecure; httpOnly = cookie.isHTTPOnly; expires = cookie.expiresDate?.timeIntervalSince1970
        sameSite = cookie.properties?[.sameSitePolicy] as? String
    }
}

struct ImportedAccount: Codable, Sendable {
    var id: String
    var site: String
    var label: String?
    var cookies: [BrowserCookie]
    var local_storage: [String: JSONValue]?
}

struct BrowserLink: Codable, Equatable, Identifiable, Sendable {
    var id = UUID()
    var url: String
    var title: String
    var visitedAt: Date?
    enum CodingKeys: String, CodingKey { case url, title, visitedAt }
    init(url: String, title: String, visitedAt: Date? = nil) { self.url = url; self.title = title; self.visitedAt = visitedAt }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        url = try c.decode(String.self, forKey: .url); title = try c.decodeIfPresent(String.self, forKey: .title) ?? url
        visitedAt = try c.decodeIfPresent(Date.self, forKey: .visitedAt)
    }
}

struct BrowserImportedData: Codable, Sendable {
    var bookmarks: [BrowserLink]?
    var history: [BrowserLink]?
    var personal: [String: String]?
}

struct BrowserImportPayload: Codable, Sendable {
    var accounts: [ImportedAccount]
    var browser_data: BrowserImportedData?
    var site_catalog: BrowserSiteCatalog?
}

struct BrowserAccount: Codable, Identifiable, Sendable {
    var id: UUID
    var externalID: String?
    var label: String
    var domains: [String]
    var cookies: [BrowserCookie]
    var localStorage: [String: JSONValue]
    var bookmarks: [BrowserLink]
    var history: [BrowserLink]
    var personal: [String: String]
    var lastURL: String?
    var needsApply = true
    static let defaultID = UUID(uuidString: "7AD34BA5-9D8A-4A26-B123-70EA1F113CDF")!
    static var original: Self {
        var value = Self(id: defaultID, externalID: nil, label: "默认浏览器", domains: [], cookies: [], localStorage: [:], bookmarks: [], history: [], personal: [:])
        value.needsApply = false
        return value
    }
}

enum BrowserSessionCodec {
    struct Reply: Decodable { var ok: Bool; var error: String?; var payload: BrowserImportPayload? }
    static func open(_ data: Data, password: String) async throws -> BrowserImportPayload {
        guard data.count <= 8 * 1024 * 1024, let text = String(data: data, encoding: .utf8), !text.contains("\0"),
              !password.isEmpty, password.utf8.count <= 4096, !password.contains("\0") else { throw BrowserToolError.actionFailed("导入文件或密码格式无效") }
        let result: Data = try await Task.detached(priority: .userInitiated) {
            try text.withCString { input in
                try password.withCString { password in
                    guard let pointer = ipu_session_open(input, password) else { throw BrowserToolError.invalidResult }
                    defer { ipu_session_free(pointer) }
                    return Data(String(cString: pointer).utf8)
                }
            }
        }.value
        let reply = try JSONDecoder().decode(Reply.self, from: result)
        guard reply.ok, let payload = reply.payload else { throw BrowserToolError.actionFailed("密码不正确、文件已损坏，或导入格式不受支持") }
        return payload
    }
}

@MainActor
final class BrowserLibrary: ObservableObject {
    @Published private(set) var accounts: [BrowserAccount] = [.original]
    @Published private(set) var activeID = BrowserAccount.defaultID
    @Published private(set) var catalog = BrowserSiteCatalog(adapters: [])
    @Published private(set) var pendingSite: String?
    @Published private(set) var chrome: ChromeBinding?
    @Published var error: String?
    private let file: URL
    private let key: SymmetricKey
    private var writable = true
    private struct Archive: Codable { var accounts: [BrowserAccount]; var activeID: UUID; var catalog: BrowserSiteCatalog; var chrome: ChromeBinding? = nil; var pendingSite: String? = nil }

    init(file: URL? = nil, key suppliedKey: Data? = nil) {
        self.file = file ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("browser-vault.v1.enc")
        let account = "browser.vault.key.v1"
        var keyData = suppliedKey ?? ModelSettingsStore.secret(account).flatMap { Data(base64Encoded: $0) }
        if keyData == nil, !FileManager.default.fileExists(atPath: self.file.path) {
            var bytes = [UInt8](repeating: 0, count: 32)
            if SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess {
                keyData = Data(bytes); ModelSettingsStore.setSecret(keyData!.base64EncodedString(), for: account)
                if ModelSettingsStore.secret(account) != keyData!.base64EncodedString() { keyData = nil }
            }
        }
        key = SymmetricKey(data: keyData ?? Data(repeating: 0, count: 32))
        guard let keyData, keyData.count == 32 else { writable = false; error = "浏览器资料密钥不可用，已有资料没有被覆盖"; return }
        if FileManager.default.fileExists(atPath: self.file.path) {
            do {
                let sealed = try AES.GCM.SealedBox(combined: Data(contentsOf: self.file))
                let saved = try JSONDecoder().decode(Archive.self, from: AES.GCM.open(sealed, using: key))
                accounts = saved.accounts; activeID = saved.activeID; catalog = saved.catalog; chrome = saved.chrome; pendingSite = saved.pendingSite
                if !accounts.contains(where: { $0.id == activeID }) { activeID = BrowserAccount.defaultID }
                if !accounts.contains(where: { $0.id == BrowserAccount.defaultID }) { accounts.insert(.original, at: 0) }
            } catch { writable = false; self.error = "浏览器资料无法解密，已有文件没有被覆盖" }
        }
    }

    var active: BrowserAccount { accounts.first { $0.id == activeID } ?? .original }
    nonisolated static func domainAllowed(_ host: String, in domains: [String]) -> Bool {
        let host = host.lowercased()
        return domains.contains { domain in host == domain || host.hasSuffix("." + domain) }
    }
    nonisolated static func domains(_ site: String) throws -> [String] {
        let domains = site.split(separator: ",").map { $0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() }
        guard !domains.isEmpty, domains.count <= 30, domains.allSatisfy({ domain in
            domain.count <= 253 && domain.contains(".") && !domain.contains("..") && !domain.hasPrefix(".") && !domain.hasSuffix(".") && domain.unicodeScalars.allSatisfy { CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789.-").contains($0) }
        }) else { throw BrowserToolError.actionFailed("导入网站范围无效") }
        var seen = Set<String>()
        return domains.filter { seen.insert($0).inserted }
    }
    nonisolated static func safeLinks(_ links: [BrowserLink], domains: [String]) -> [BrowserLink] {
        Array(links.filter { link in
            guard let url = URL(string: link.url), url.scheme == "https", let host = url.host,
                  url.user == nil, url.password == nil else { return false }
            return domains.isEmpty || domainAllowed(host, in: domains)
        }.prefix(1000))
    }

    func importPayload(_ payload: BrowserImportPayload, selected: Set<String>, includeCookies: Bool, includeStorage: Bool, includeBookmarks: Bool, includeHistory: Bool, includePersonal: Bool, trustSites: Bool) throws {
        guard writable else { throw BrowserToolError.actionFailed(error ?? "资料库不可写") }
        guard Set(payload.accounts.map(\.id)).count == payload.accounts.count, payload.accounts.allSatisfy({ !$0.id.isEmpty && $0.id.count <= 256 }) else { throw BrowserToolError.actionFailed("导入账号 ID 重复或无效") }
        var next = accounts
        for incoming in payload.accounts where selected.contains(incoming.id) {
            let domains = try Self.domains(incoming.site)
            guard incoming.cookies.count <= 10_000 else { throw BrowserToolError.invalidResult }
            var account = next.first { $0.externalID == incoming.id } ?? BrowserAccount(id: UUID(), externalID: incoming.id,
                label: incoming.label ?? incoming.id, domains: domains, cookies: [], localStorage: [:], bookmarks: [], history: [], personal: [:])
            account.label = incoming.label ?? incoming.id; account.domains = domains
            if includeCookies { account.cookies = incoming.cookies.filter { $0.cookie(allowedDomains: domains) != nil } }
            if includeStorage {
                account.localStorage = (incoming.local_storage ?? [:]).filter { $0.key.utf8.count <= 2000 && $0.key != "__proto__" && ((try? JSONEncoder().encode($0.value).count) ?? Int.max) <= 100_000 }
            }
            if includeCookies || includeStorage { account.needsApply = true }
            if includeBookmarks { account.bookmarks = Self.safeLinks(payload.browser_data?.bookmarks ?? [], domains: domains) }
            if includeHistory { account.history = Self.safeLinks(payload.browser_data?.history ?? [], domains: domains) }
            if includePersonal {
                let allowed = Set(["name", "email", "phone", "address", "city", "postalCode", "country"])
                account.personal = (payload.browser_data?.personal ?? [:]).filter { allowed.contains($0.key) && $0.value.count <= 2000 }
            }
            if let index = next.firstIndex(where: { $0.id == account.id }) { next[index] = account } else { next.append(account) }
        }
        guard next.count <= 101 else { throw BrowserToolError.actionFailed("最多保存 100 个导入账号") }
        let nextCatalog = trustSites ? try (payload.site_catalog ?? catalog).validated() : catalog
        try persist(Archive(accounts: next, activeID: activeID, catalog: nextCatalog, chrome: chrome, pendingSite: pendingSite))
        accounts = next; catalog = nextCatalog
    }

    func select(_ id: UUID) throws {
        guard accounts.contains(where: { $0.id == id }) else { throw BrowserToolError.invalidResult }
        try persist(Archive(accounts: accounts, activeID: id, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); activeID = id
    }
    func markApplied(_ id: UUID) throws {
        guard let index = accounts.firstIndex(where: { $0.id == id }) else { return }
        var next = accounts; next[index].needsApply = false
        try persist(Archive(accounts: next, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next
    }
    func installCatalog(_ value: BrowserSiteCatalog) throws {
        let value = try value.validated()
        try persist(Archive(accounts: accounts, activeID: activeID, catalog: value, chrome: chrome, pendingSite: pendingSite)); catalog = value
    }
    func saveSnapshot(cookies: [HTTPCookie], url: String?, localStorage: [String: JSONValue]?) throws {
        guard let index = accounts.firstIndex(where: { $0.id == activeID }) else { return }
        var next = accounts
        let scope = next[index].domains
        next[index].cookies = cookies.map(BrowserCookie.init).filter { scope.isEmpty || $0.cookie(allowedDomains: scope) != nil }
        next[index].lastURL = url
        if let localStorage { next[index].localStorage = localStorage }
        try persist(Archive(accounts: next, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next
    }
    func bookmark(_ url: String, title: String) throws {
        guard let index = accounts.firstIndex(where: { $0.id == activeID }), let link = Self.safeLinks([.init(url: url, title: title)], domains: []).first else { throw BrowserToolError.invalidResult }
        var next = accounts
        if !next[index].bookmarks.contains(where: { $0.url == url }) { next[index].bookmarks.append(link) }
        try persist(Archive(accounts: next, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next
    }
    func recordVisit(_ url: String, title: String) throws {
        guard let index = accounts.firstIndex(where: { $0.id == activeID }), Self.safeLinks([.init(url: url, title: title)], domains: []).count == 1 else { return }
        var next = accounts
        next[index].history.removeAll { $0.url == url }
        next[index].history.insert(.init(url: url, title: title, visitedAt: Date()), at: 0)
        next[index].history = Array(next[index].history.prefix(500))
        try persist(Archive(accounts: next, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next
    }
    func updatePersonal(_ fields: [String: String]) throws {
        guard let index = accounts.firstIndex(where: { $0.id == activeID }) else { return }
        let allowed = Set(["name", "email", "phone", "address", "city", "postalCode", "country"])
        var next = accounts
        next[index].personal = fields.filter { allowed.contains($0.key) && !$0.value.isEmpty && $0.value.count <= 2000 }
        try persist(Archive(accounts: next, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next
    }
    func remove(_ id: UUID) throws {
        guard id != BrowserAccount.defaultID else { return }
        let next = accounts.filter { $0.id != id }
        let selected = activeID == id ? BrowserAccount.defaultID : activeID
        try persist(Archive(accounts: next, activeID: selected, catalog: catalog, chrome: chrome, pendingSite: pendingSite)); accounts = next; activeID = selected
    }
    func recordSite(_ value: String?) throws {
        try persist(Archive(accounts: accounts, activeID: activeID, catalog: catalog, chrome: chrome, pendingSite: value)); pendingSite = value
    }
    func setChrome(_ value: ChromeBinding?) throws {
        try persist(Archive(accounts: accounts, activeID: activeID, catalog: catalog, chrome: value, pendingSite: pendingSite)); chrome = value
    }
    private func persist(_ archive: Archive) throws {
        guard writable else { throw BrowserToolError.actionFailed(error ?? "资料库不可写") }
        let data = try JSONEncoder().encode(archive)
        let sealed = try AES.GCM.seal(data, using: key)
        guard let combined = sealed.combined else { throw BrowserToolError.invalidResult }
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try combined.write(to: file, options: [.atomic, .completeFileProtectionUnlessOpen])
    }
}
