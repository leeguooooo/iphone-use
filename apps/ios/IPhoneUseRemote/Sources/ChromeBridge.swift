import Foundation
import CryptoKit
import SwiftUI

struct ChromeBinding: Codable, Sendable {
    var endpoint: URL
    var scope: String
    var key: Data
    var profile: String?
    var domains: [String]?
    var writes: Bool?
    static func parse(_ link: String) throws -> Self {
        guard let url = URLComponents(string: link.trimmingCharacters(in: .whitespacesAndNewlines)), url.scheme == "phoneuse-chrome", url.host == "pair",
              let items = url.queryItems, items.count == 3, Set(items.map(\.name)) == Set(["u", "s", "k"]),
              let endpoint = items.first(where: { $0.name == "u" })?.value.flatMap({ URL(string: $0) }), ["http", "https"].contains(endpoint.scheme), endpoint.host != nil,
              endpoint.user == nil, endpoint.password == nil, endpoint.query == nil, endpoint.fragment == nil,
              let scope = items.first(where: { $0.name == "s" })?.value, scope.count == 24, scope.allSatisfy({ $0.isHexDigit }),
              let key = items.first(where: { $0.name == "k" })?.value.flatMap({ Data(base64Encoded: $0) }), key.count == 32 else {
            throw BrowserToolError.actionFailed("电脑配对链接无效")
        }
        return Self(endpoint: endpoint, scope: scope, key: key)
    }
    func request(_ operation: String, fields: [String: JSONValue] = [:], id: String = UUID().uuidString) async throws -> JSONValue {
        var body = fields; body["id"] = .string(id); body["operation"] = .string(operation); body["at"] = .number(Date().timeIntervalSince1970.rounded(.down))
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        let sealed = try AES.GCM.seal(encoder.encode(body), using: SymmetricKey(data: key), authenticating: Data("phone-use/chrome/request/\(scope)".utf8))
        var request = URLRequest(url: endpoint.appendingPathComponent("browser/chrome")); request.httpMethod = "POST"; request.timeoutInterval = 55
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try encoder.encode(["scope": JSONValue.string(scope), "sealed": .string(sealed.combined!.base64EncodedString())])
        // Ephemeral transport: independent grant key, no browser cookies or redirects.
        let config = URLSessionConfiguration.ephemeral; config.httpCookieStorage = nil; config.urlCache = nil
        let session = URLSession(configuration: config, delegate: ChromeNoRedirect(), delegateQueue: nil)
        defer { session.invalidateAndCancel() }
        let (data, response) = try await session.data(for: request)
        guard data.count < 4 * 1024 * 1024, (response as? HTTPURLResponse)?.statusCode == 200,
              let envelope = try JSONDecoder().decode(JSONValue.self, from: data).object,
              envelope["scope"]?.string == scope, envelope["id"]?.string == id,
              let ciphertext = envelope["sealed"]?.string.flatMap({ Data(base64Encoded: $0) }) else {
            throw BrowserToolError.actionFailed("电脑 Chrome 未返回可验证结果。写入操作请先到电脑核对，勿重复提交。")
        }
        let plain = try AES.GCM.open(AES.GCM.SealedBox(combined: ciphertext), using: SymmetricKey(data: key), authenticating: Data("phone-use/chrome/response/\(scope)/\(id)".utf8))
        return try JSONDecoder().decode(JSONValue.self, from: plain)
    }
    func run(_ adapter: BrowserSiteAdapter, arguments: [String: JSONValue], confirmed: Bool, id: String) async throws -> JSONValue {
        guard adapter.pairedScope == scope else { throw BrowserToolError.actionFailed("网站操作未绑定当前电脑授权，请重新核对电脑与网站库") }
        try adapter.validate(arguments: arguments)
        let result = try await request("run", fields: ["spec": .string(adapter.spec), "fingerprint": .string(adapter.fingerprint), "arguments": .object(arguments), "confirmed": .bool(confirmed)], id: id)
        guard result.object?["ok"]?.bool == true, let data = result.object?["data"] else {
            throw BrowserToolError.actionFailed(result.object?["error"]?.string == "operation_outcome_unknown" ? "电脑操作结果未知，请先到 Chrome 核对" : "电脑网站操作失败，请检查授权范围与脚本版本")
        }
        return data
    }
}

private final class ChromeNoRedirect: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}

struct ChromePairView: View {
    @ObservedObject var library: BrowserLibrary
    @State private var link = ""
    @State private var candidate: ChromeBinding?
    @State private var catalog: BrowserSiteCatalog?
    @State private var error: String?
    @State private var busy = false
    @State private var trusted = false
    var body: some View {
        Form {
            if let binding = library.chrome {
                Section("已连接电脑 Chrome") {
                    Text(binding.endpoint.host ?? "电脑")
                    Text("\(binding.profile ?? "") · \((binding.domains ?? []).joined(separator: ", "))")
                    Text(binding.writes == true ? "允许写入；每次操作仍需确认" : "只允许读取")
                    Button("断开电脑授权", role: .destructive) { try? library.setChrome(nil) }
                }
            }
            Section("从电脑配对") {
                Text("电脑运行 browser-use-bridge grant，指定 profile、网站和 Mac 服务地址；检查预览后加 --confirm，复制生成的私密链接。")
                SecureField("粘贴配对链接", text: $link).textInputAutocapitalization(.never).autocorrectionDisabled()
                Button(busy ? "正在核对…" : "核对电脑与授权范围") { inspect() }.disabled(busy || link.isEmpty)
            }
            if let candidate, let catalog {
                Section("确认授权") {
                    Text(candidate.endpoint.host ?? "电脑")
                    Text("\(candidate.profile ?? "") · \((candidate.domains ?? []).joined(separator: ", "))")
                    Text("\(catalog.adapters.count) 个已授权 chrome-use site 操作")
                    Text(candidate.writes == true ? "包含写入能力；执行前将逐次确认" : "仅读取")
                    Toggle("我确认这台电脑、账号和网站，并信任这些脚本", isOn: $trusted)
                    Button("保存连接与网站库") {
                        do { try library.installCatalog(catalog); try library.setChrome(candidate); link = ""; self.candidate = nil; self.catalog = nil }
                        catch { self.error = error.localizedDescription }
                    }.disabled(!trusted)
                }
            }
            if let error { Text(error).foregroundStyle(Theme.attention) }
        }.navigationTitle("电脑 Chrome")
    }
    private func inspect() {
        busy = true; error = nil; trusted = false; candidate = nil; catalog = nil
        Task {
            defer { busy = false }
            do {
                var binding = try ChromeBinding.parse(link)
                let result = try await binding.request("catalog")
                guard let object = result.object, object["ok"]?.bool == true, let raw = object["catalog"] else { throw BrowserToolError.invalidResult }
                var parsed = try JSONDecoder().decode(BrowserSiteCatalog.self, from: JSONEncoder().encode(raw)).validated()
                parsed.adapters = parsed.adapters.map { var adapter = $0; adapter.pairedScope = binding.scope; return adapter }
                binding.profile = object["profile"]?.string; binding.domains = object["domains"]?.array?.compactMap(\.string); binding.writes = object["writes"]?.bool
                candidate = binding; catalog = parsed
            } catch { self.error = error.localizedDescription }
        }
    }
}
