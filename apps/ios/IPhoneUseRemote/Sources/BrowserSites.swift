import Foundation
import WebKit
import CryptoKit

struct BrowserSiteAdapter: Codable, Identifiable, Sendable {
    var spec: String
    var meta: [String: JSONValue]
    var source: String
    var sha256: String
    var origin: String
    var metaJSON: String? = nil
    var pairedScope: String? = nil
    var id: String { spec }
    var name: String { meta["name"]?.string ?? spec }
    var description: String { meta["description"]?.string ?? spec }
    var readOnly: Bool {
        let verb = spec.split(separator: "/").last?.split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map(String.init) ?? ""
        return meta["readOnly"]?.bool == true && !["delete", "remove", "send", "post", "publish", "update", "upload", "buy", "pay", "create", "reply", "like", "follow", "subscribe"].contains(verb.lowercased())
    }
    var fingerprint: String {
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        let metadata: Data
        if let metaJSON, let data = metaJSON.data(using: .utf8), (try? JSONDecoder().decode([String: JSONValue].self, from: data)) == meta { metadata = data }
        else { metadata = (try? encoder.encode(meta)) ?? Data() }
        return SHA256.hash(data: metadata + Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
    }
    var arguments: [String: JSONValue] { meta["args"]?.object ?? [:] }
    var url: URL? {
        guard let domain = meta["domain"]?.string,
              let url = URL(string: domain.contains("://") ? domain : "https://" + domain), url.scheme == "https",
              let host = url.host, host.contains("."), url.user == nil, url.password == nil,
              url.query == nil, url.fragment == nil else { return nil }
        return url
    }
    var desktopOnly: Bool {
        pairedScope != nil || meta["runtime"]?.string == "opencli" || arguments.values.contains { $0.object?["type"]?.string == "file" } ||
        ["require(", "process.", "Buffer.", "chrome.", ".setOn("].contains(where: source.contains)
    }
    func validate(arguments supplied: [String: JSONValue]) throws {
        guard supplied.keys.allSatisfy({ arguments[$0] != nil }), supplied.count <= 30 else { throw BrowserToolError.actionFailed("网站操作参数不受支持") }
        for (name, definition) in arguments where definition.object?["type"]?.string == "file" && supplied[name] != nil {
            throw BrowserToolError.actionFailed("文件参数 \(name) 需要在电脑上选择，手机不会读取电脑文件路径")
        }
        for (name, definition) in arguments where definition.object?["required"]?.bool == true {
            guard let value = supplied[name], value != .null, value.string?.isEmpty != true else { throw BrowserToolError.actionFailed("请填写网站操作的必需参数：\(name)") }
        }
        guard (try JSONEncoder().encode(supplied)).count <= 32_000 else { throw BrowserToolError.invalidResult }
    }
}

struct BrowserSiteCatalog: Codable, Sendable {
    var chrome_use_sites: Int = 1
    var adapters: [BrowserSiteAdapter]
    init(adapters: [BrowserSiteAdapter]) { self.adapters = adapters }
    func validated() throws -> Self {
        guard chrome_use_sites == 1, adapters.count <= 2000 else { throw BrowserToolError.actionFailed("网站库格式不受支持") }
        var seen = Set<String>()
        for adapter in adapters {
            if let metaJSON = adapter.metaJSON {
                guard let data = metaJSON.data(using: .utf8), (try? JSONDecoder().decode([String: JSONValue].self, from: data)) == adapter.meta else { throw BrowserToolError.actionFailed("网站脚本元数据校验失败") }
            }
            let parts = adapter.spec.split(separator: "/")
            guard parts.count == 2, parts.allSatisfy({ !$0.isEmpty && !$0.contains("..") && $0.unicodeScalars.allSatisfy { CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_").contains($0) } }),
                  seen.insert(adapter.spec).inserted, adapter.url != nil, adapter.source.utf8.count <= 512_000,
                  adapter.source.trimmingCharacters(in: .whitespacesAndNewlines).hasPrefix("async function"),
                  adapter.sha256.lowercased() == SHA256.hash(data: Data(adapter.source.utf8)).map { String(format: "%02x", $0) }.joined() else {
                throw BrowserToolError.actionFailed("网站库来源、域名或内容校验失败")
            }
        }
        return self
    }
    func matching(_ url: URL?) -> [BrowserSiteAdapter] {
        guard let host = url?.host else { return [] }
        return adapters.filter { adapter in adapter.url?.host.map { host == $0 || host.hasSuffix("." + $0) } ?? false }
    }
}

struct BrowserSiteResult: Codable, Sendable {
    var result: JSONValue
    var progress: [String]
}

extension BrowserSession {
    func runSite(_ adapter: BrowserSiteAdapter, arguments: [String: JSONValue]) async throws -> BrowserSiteResult {
        guard !adapter.desktopOnly, let destination = adapter.url else { throw BrowserToolError.actionFailed("这个 chrome-use 网站操作需要电脑 Chrome，请选择电脑执行") }
        try adapter.validate(arguments: arguments)
        try await waitUntilReady()
        guard webView.url?.host == destination.host || (webView.url?.host?.hasSuffix("." + destination.host!) ?? false) else {
            throw BrowserToolError.actionFailed("请先打开该网站，再执行网站操作")
        }
        let script = """
        const execute = (\(adapter.source));
        const progress = [];
        const parameters = args;
        parameters.budgetMs = 20000;
        parameters.progress = value => { if (progress.length < 30) progress.push(String(value).slice(0, 300)); };
        const result = await Promise.race([execute(parameters), new Promise((_, reject) => setTimeout(() => reject(new Error('SITE_TIMEOUT: The operation may still be running; do not replay a write.')), 20000))]);
        const encoded = JSON.stringify({result: result === undefined ? null : result, progress});
        if (encoded.length > 200000) throw new Error('SITE_RESULT_TOO_LARGE: Inspect manually; do not replay a write.');
        return encoded;
        """
        let value = try await webView.callAsyncJavaScript(script, arguments: ["args": arguments.mapValues(\.any)], in: nil, contentWorld: .page)
        guard let text = value as? String, let data = text.data(using: .utf8) else { throw BrowserToolError.invalidResult }
        return try JSONDecoder().decode(BrowserSiteResult.self, from: data)
    }
}

/// Known credentials are removed before site results or observations reach a model.
enum BrowserRedactor {
    static func text(_ value: String, secrets: [String]) -> String {
        var value = value
        for secret in Set(secrets).sorted(by: { $0.count > $1.count }) where secret.count >= 2 {
            if secret.count >= 8 { value = value.replacingOccurrences(of: secret, with: "[已隐藏]") }
            else if let expression = try? NSRegularExpression(pattern: "(?<![\\p{L}\\p{N}_])" + NSRegularExpression.escapedPattern(for: secret) + "(?![\\p{L}\\p{N}_])") {
                value = expression.stringByReplacingMatches(in: value, range: NSRange(value.startIndex..., in: value), withTemplate: "[已隐藏]")
            }
        }
        return value
    }
    static func url(_ value: String) -> String {
        guard var components = URLComponents(string: value), components.scheme == "https" || components.scheme == "http" else { return value }
        let privateNames = Set(["token", "access_token", "refresh_token", "code", "auth", "authorization", "password", "key", "api_key", "session", "session_id", "ticket"])
        components.queryItems = components.queryItems?.map { item in
            privateNames.contains(item.name.lowercased()) ? URLQueryItem(name: item.name, value: "[hidden]") : item
        }
        if let fragment = components.fragment, fragment.contains("token=") || fragment.contains("code=") { components.fragment = "[hidden]" }
        components.user = nil; components.password = nil
        return components.string ?? value
    }
    static func value(_ value: JSONValue, secrets: [String]) -> JSONValue {
        switch value {
        case .object(let fields):
            return .object(fields.mapValues { self.value($0, secrets: secrets) }.filter { key, _ in
                !["cookie", "cookies", "password", "authorization", "access_token", "refresh_token", "token", "csrf", "csrf_token", "session", "session_id", "session_key", "api_key", "private_key", "credential"].contains(key.lowercased())
            })
        case .array(let values): return .array(values.map { self.value($0, secrets: secrets) })
        case .string(let string): return .string(secrets.contains(string) && !string.isEmpty ? "[已隐藏]" : text(url(string), secrets: secrets))
        default: return value
        }
    }
}
