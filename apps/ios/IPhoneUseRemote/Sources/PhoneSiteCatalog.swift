import Foundation
import SwiftUI
import CryptoKit

struct PhoneSitePackage: Codable, Sendable {
    let catalog: BrowserSiteCatalog
    let unsupported: [String]
    let revisions: [String: String]
}

/// Phone-only reuse of chrome-use's public site packs. No CLI, desktop relay,
/// profile export or Mac login is involved. Updates are pinned to commit/blob
/// hashes and installed atomically after all downloads have succeeded.
@MainActor
final class PhoneSiteCatalogDownloader: ObservableObject {
    @Published private(set) var downloading = false
    @Published private(set) var downloaded = 0
    @Published private(set) var total = 0
    @Published private(set) var error: String?
    private let session: URLSession
    static let repositories = ["epiral/bb-sites", "leeguooooo/chrome-use-sites"]
    struct Tree: Decodable, Sendable {
        let tree: [Entry]
        let truncated: Bool?
    }
    struct Entry: Decodable, Sendable {
        let path: String
        let type: String
        let mode: String?
        let sha: String
        let size: Int?
    }
    init(session: URLSession? = nil) {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.httpMaximumConnectionsPerHost = 6
        self.session = session ?? URLSession(configuration: configuration, delegate: BrowserModelRedirectGuard(), delegateQueue: nil)
    }
    deinit { session.invalidateAndCancel() }

    func download() async throws -> PhoneSitePackage {
        guard !downloading else { throw BrowserToolError.actionFailed("网站库正在更新") }
        downloading = true; downloaded = 0; total = 0; error = nil
        defer { downloading = false }
        do {
            var collected: [String: BrowserSiteAdapter] = [:]
            var unsupported = Set<String>()
            var revisions: [String: String] = [:]
            // The official pack wins over the community pack on a shared spec,
            // including when its new implementation is no longer portable.
            for repository in Self.repositories {
                try Task.checkCancellation()
                let commitData = try await get("https://api.github.com/repos/\(repository)/commits?per_page=1", limit: 300_000)
                struct Commit: Decodable { let sha: String }
                guard let commit = try JSONDecoder().decode([Commit].self, from: commitData).first?.sha,
                      Self.validHash(commit) else { throw BrowserToolError.invalidResult }
                revisions[repository] = commit
                let treeData = try await get("https://api.github.com/repos/\(repository)/git/trees/\(commit)?recursive=1", limit: 2_000_000)
                let tree = try JSONDecoder().decode(Tree.self, from: treeData)
                guard tree.truncated != true else { throw BrowserToolError.actionFailed("网站库目录不完整，保留现有版本") }
                let files = tree.tree.filter { entry in
                    entry.type == "blob" && entry.mode != "120000" && Self.spec(entry.path) != nil &&
                    (entry.size ?? 0) <= 512_000 && Self.validHash(entry.sha)
                }
                guard files.count <= 1000 else { throw BrowserToolError.invalidResult }
                total += files.count
                let values = try await filesFrom(repository, commit: commit, entries: files)
                for (spec, adapter) in values {
                    collected.removeValue(forKey: spec)
                    unsupported.remove(spec)
                    if let adapter { collected[spec] = adapter }
                    else { unsupported.insert(spec) }
                }
            }
            guard !collected.isEmpty else { throw BrowserToolError.actionFailed("没有下载到可在手机执行的网站操作，保留现有版本") }
            let catalog = try BrowserSiteCatalog(adapters: collected.values.sorted { $0.spec < $1.spec }).validated()
            return PhoneSitePackage(catalog: catalog, unsupported: unsupported.sorted(), revisions: revisions)
        } catch {
            self.error = "网站库更新失败，现有版本保持不变。" + error.localizedDescription
            throw error
        }
    }

    private func filesFrom(_ repository: String, commit: String, entries: [Entry]) async throws -> [(String, BrowserSiteAdapter?)] {
        try await withThrowingTaskGroup(of: (String, BrowserSiteAdapter?).self) { group in
            var next = entries.makeIterator()
            for _ in 0..<6 {
                if let entry = next.next() { group.addTask { try await self.file(repository, commit: commit, entry: entry) } }
            }
            var result: [(String, BrowserSiteAdapter?)] = []
            while let value = try await group.next() {
                result.append(value); downloaded += 1
                if let entry = next.next() { group.addTask { try await self.file(repository, commit: commit, entry: entry) } }
            }
            return result
        }
    }
    private func file(_ repository: String, commit: String, entry: Entry) async throws -> (String, BrowserSiteAdapter?) {
        let data = try await get("https://raw.githubusercontent.com/\(repository)/\(commit)/\(entry.path)", limit: 512_000)
        guard Self.blobHash(data) == entry.sha else { throw BrowserToolError.actionFailed("网站操作内容与公开版本不一致，更新已取消") }
        let spec = Self.spec(entry.path)!
        return (spec, try Self.adapter(data, spec: spec, origin: "https://github.com/\(repository)/blob/\(commit)/\(entry.path)"))
    }
    private func get(_ address: String, limit: Int) async throws -> Data {
        try Task.checkCancellation()
        let url = URL(string: address)!
        var request = URLRequest(url: url)
        request.timeoutInterval = 25
        request.setValue("Phone-Use-Remote-Site-Catalog", forHTTPHeaderField: "User-Agent")
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse, http.url?.scheme == "https", http.url?.host == url.host else { throw BrowserToolError.invalidResult }
        if http.statusCode == 403 || http.statusCode == 429 { throw BrowserToolError.actionFailed("公开网站库暂时限流，请稍后更新") }
        guard http.statusCode == 200, data.count <= limit else { throw BrowserToolError.actionFailed("公开网站库下载不完整") }
        try Task.checkCancellation()
        return data
    }
    nonisolated static func validHash(_ value: String) -> Bool {
        value.count == 40 && value.allSatisfy { $0.isHexDigit && !$0.isUppercase }
    }
    nonisolated static func spec(_ path: String) -> String? {
        guard path.hasSuffix(".js") else { return nil }
        let value = String(path.dropLast(3))
        let parts = value.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, parts.allSatisfy({ !$0.isEmpty && $0.utf8.allSatisfy { (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || $0 == 45 || $0 == 95 } }) else { return nil }
        return value
    }
    nonisolated static func blobHash(_ data: Data) -> String {
        var blob = Data("blob \(data.count)\0".utf8); blob.append(data)
        return Insecure.SHA1.hash(data: blob).map { String(format: "%02x", $0) }.joined()
    }
    nonisolated static func adapter(_ data: Data, spec: String, origin: String) throws -> BrowserSiteAdapter? {
        guard data.count <= 512_000, let text = String(data: data, encoding: .utf8),
              let start = text.range(of: "/* @meta"), let end = text.range(of: "*/", range: start.upperBound..<text.endIndex) else { return nil }
        let metaData = Data(text[start.upperBound..<end.lowerBound].trimmingCharacters(in: .whitespacesAndNewlines).utf8)
        guard let meta = try? JSONDecoder().decode([String: JSONValue].self, from: metaData) else { return nil }
        let source = text[end.upperBound...].trimmingCharacters(in: .whitespacesAndNewlines)
        let adapter = BrowserSiteAdapter(spec: spec, meta: meta, source: source,
            sha256: SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined(), origin: origin)
        guard source.hasPrefix("async function"), !adapter.desktopOnly, adapter.url != nil,
              source.range(of: #"\b(?:require\s*\(|process\s*\.|Buffer\s*\.|module\s*\.|exports\s*\.|import\s|export\s|chrome\s*\.)"#, options: .regularExpression) == nil else { return nil }
        _ = try BrowserSiteCatalog(adapters: [adapter]).validated()
        return adapter
    }
    static func bundled() throws -> PhoneSitePackage? {
        guard let url = Bundle.main.url(forResource: "PhoneSiteCatalog", withExtension: "json") else { return nil }
        let package = try JSONDecoder().decode(PhoneSitePackage.self, from: Data(contentsOf: url))
        _ = try package.catalog.validated()
        guard package.catalog.adapters.allSatisfy({ !$0.desktopOnly }) else { throw BrowserToolError.invalidResult }
        return package
    }
}

struct PhoneSiteBrowserWorkspaceView: View {
    @ObservedObject var workspace: BrowserWorkspace
    @ObservedObject var settings: AgentController
    @State private var showingSites = false
    var body: some View {
        BrowserWorkspaceView(settings: settings, workspace: workspace)
            .safeAreaInset(edge: .top) {
                Button { showingSites = true } label: { Label("手机网站操作", systemImage: "globe") }
                    .font(.footnote).frame(maxWidth: .infinity).padding(8).background(.bar)
                    .accessibilityIdentifier("phone-sites-open")
            }
            .sheet(isPresented: $showingSites) { PhoneSiteCatalogView(workspace: workspace) }
            .task {
                guard workspace.library.catalog.adapters.isEmpty else { return }
                do { if let package = try PhoneSiteCatalogDownloader.bundled() { try workspace.library.installCatalog(package.catalog) } }
                catch { workspace.library.error = error.localizedDescription }
            }
    }
}
struct PhoneSiteCatalogView: View {
    @ObservedObject var workspace: BrowserWorkspace
    @StateObject private var downloader = PhoneSiteCatalogDownloader()
    @State private var unsupported: [String] = []
    @State private var installed: String?
    @Environment(\.dismiss) private var dismiss
    private var busy: Bool { workspace.agent.active || workspace.lockedForTask || workspace.switching }
    var body: some View {
        AppNavigationStack {
            List {
                Section {
                    Text("复用 chrome-use 的公开网站操作，在这台手机的浏览器中执行，使用当前手机账号。")
                    Text("账号：\(workspace.library.active.label)").font(.footnote).foregroundStyle(.secondary)
                    Button("在手机更新网站库") { Task { await update() } }
                        .disabled(downloader.downloading || busy).accessibilityIdentifier("phone-sites-update")
                    if downloader.downloading { ProgressView(value: Double(downloader.downloaded), total: Double(max(1, downloader.total))) }
                    if let error = downloader.error { Text(error).foregroundStyle(.red) }
                    if let installed { Text(installed).font(.footnote) }
                }
                Section("可在手机执行") {
                    NavigationLink("网站操作（\(workspace.library.catalog.adapters.filter { !$0.desktopOnly }.count)）") {
                        BrowserSitesView(workspace: workspace, onBrowse: { dismiss() })
                    }
                }
                if !unsupported.isEmpty {
                    Section("手机暂不支持") {
                        Text("这些适配器依赖 Node.js、OpenCLI 或电脑文件接口，当前手机执行环境不提供这些能力。")
                            .font(.footnote).foregroundStyle(.secondary)
                        ForEach(unsupported, id: \.self) { Text($0).font(.caption) }
                    }
                }
            }
            .navigationTitle("手机网站操作")
            .toolbar { ToolbarItem(placement: .navigationBarTrailing) { Button("完成") { dismiss() } } }
        }
    }
    private func update() async {
        do {
            let package = try await downloader.download()
            guard !busy else { throw BrowserToolError.actionFailed("任务正在执行，结束后再安装网站库") }
            try workspace.library.installCatalog(package.catalog)
            unsupported = package.unsupported
            installed = "已更新 \(package.catalog.adapters.count) 个手机网站操作。"
        } catch { workspace.library.error = error.localizedDescription }
    }
}
