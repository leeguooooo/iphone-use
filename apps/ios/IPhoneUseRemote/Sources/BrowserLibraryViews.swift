import SwiftUI
import UniformTypeIdentifiers

struct BrowserLibraryView: View {
    @ObservedObject var workspace: BrowserWorkspace
    @ObservedObject var library: BrowserLibrary
    var onBrowse: () -> Void = {}
    @Environment(\.dismiss) private var dismiss
    @State private var importing = false
    @State private var importSites = false
    @State private var encryptedData: Data?
    @State private var unlocked: BrowserImportPayload?
    @State private var pendingCatalog: BrowserSiteCatalog?
    @State private var removing: BrowserAccount?
    @State private var error: String?

    init(workspace: BrowserWorkspace, onBrowse: @escaping () -> Void = {}) {
        self.workspace = workspace; library = workspace.library; self.onBrowse = onBrowse
    }
    var body: some View {
        AppNavigationStack {
            List {
                if let error = error ?? library.error { Text(error).foregroundStyle(Theme.attention).font(.footnote) }
                if let pending = library.pendingSite {
                    Section("需要核对上次操作") {
                        Text(pending)
                        Text("上次写入没有明确结果，请先检查网站。")
                        Button("我已手动检查结果") { try? library.recordSite(nil) }
                    }
                }
                Section("浏览器账号") {
                    ForEach(library.accounts) { account in
                        Button {
                            Task {
                                do { try await workspace.switchAccount(account.id) }
                                catch { self.error = error.localizedDescription }
                            }
                        } label: {
                            HStack {
                                VStack(alignment: .leading) {
                                    Text(account.label)
                                    if !account.domains.isEmpty { Text(account.domains.joined(separator: ", ")).font(.caption).foregroundStyle(.secondary) }
                                }
                                Spacer()
                                if account.id == library.activeID { Image(systemName: "checkmark") }
                            }
                        }
                        .disabled(workspace.switching || workspace.lockedForTask || workspace.agent.active)
                        .contextMenu {
                            if account.id != BrowserAccount.defaultID { Button("移除账号", role: .destructive) { removing = account } }
                        }
                    }
                }
                Section("导入") {
                    Button("导入账号备份与资料") { importSites = false; importing = true }
                        .accessibilityIdentifier("browser-import-session")
                    Button("导入 chrome-use 网站库") { importSites = true; importing = true }
                        .accessibilityIdentifier("browser-import-sites")
                    Text("可导入加密账号备份，也可直接在手机浏览器登录。登录状态按账号隔离，Cookie 内容不会发给模型。")
                        .font(.footnote).foregroundStyle(.secondary)
                }
                .disabled(workspace.lockedForTask || workspace.agent.active || workspace.switching)
                Section("当前账号") {
                    NavigationLink("书签（\(library.active.bookmarks.count)）") {
                        links(library.active.bookmarks, title: "书签")
                    }
                    NavigationLink("历史记录（\(library.active.history.count)）") {
                        links(library.active.history, title: "历史记录")
                    }
                    NavigationLink("自动填写资料") { BrowserPersonalEditor(library: library) }
                    NavigationLink("chrome-use 网站操作（\(library.catalog.adapters.count)）") { BrowserSitesView(workspace: workspace, onBrowse: onBrowse) }
                    if workspace.browser.hasPage {
                        Button("收藏当前网页") {
                            do { try library.bookmark(workspace.browser.address, title: workspace.browser.title) }
                            catch { self.error = error.localizedDescription }
                        }
                    }
                }
                .disabled(workspace.lockedForTask || workspace.agent.active || workspace.switching)
                Text("网站绑定设备的登录状态可能需要重新登录。iOS 15/16 使用隔离会话并恢复 Cookie 和选定网站的数据；IndexedDB 等其他网站数据不保证在重启后保留。")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            .navigationTitle("账号与网站资料")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("完成") { dismiss() } } }
            .fileImporter(isPresented: $importing, allowedContentTypes: [.data, .json], allowsMultipleSelection: false) { result in
                do {
                    guard let url = try result.get().first else { return }
                    let access = url.startAccessingSecurityScopedResource()
                    defer { if access { url.stopAccessingSecurityScopedResource() } }
                    let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
                    guard size <= 8 * 1024 * 1024 else { throw BrowserToolError.actionFailed("导入文件过大，请按网站分批导入") }
                    let data = try Data(contentsOf: url)
                    if importSites { pendingCatalog = try JSONDecoder().decode(BrowserSiteCatalog.self, from: data).validated() }
                    else { encryptedData = data }
                } catch { self.error = error.localizedDescription }
            }
            .sheet(isPresented: Binding(get: { encryptedData != nil }, set: { if !$0 { encryptedData = nil } })) {
                SessionPasswordSheet(data: encryptedData ?? Data()) { payload in
                    encryptedData = nil
                    // Present review after the password sheet has been dismissed.
                    Task { @MainActor in try? await Task.sleep(nanoseconds: 350_000_000); unlocked = payload }
                }
            }
            .sheet(isPresented: Binding(get: { unlocked != nil }, set: { if !$0 { unlocked = nil } })) {
                if let unlocked { BrowserImportReview(payload: unlocked, workspace: workspace) { self.unlocked = nil } }
            }
            .sheet(isPresented: Binding(get: { pendingCatalog != nil }, set: { if !$0 { pendingCatalog = nil } })) {
                if let pendingCatalog { BrowserCatalogReview(catalog: pendingCatalog, library: library) { self.pendingCatalog = nil } }
            }
            .confirmationDialog("移除这个浏览器账号？", isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }), titleVisibility: .visible) {
                Button("移除账号", role: .destructive) {
                    guard let account = removing, !workspace.lockedForTask, !workspace.agent.active else { return }
                    Task {
                        do {
                            try await workspace.removeAccount(account.id)
                        } catch { self.error = error.localizedDescription }
                        removing = nil
                    }
                }
                Button("取消", role: .cancel) { removing = nil }
            } message: { Text("只移除手机保存的账号资料，电脑 Chrome 和 cookie-use 中的账号保持不变。") }
        }
    }
    private func links(_ records: [BrowserLink], title: String) -> some View {
        List(records) { link in
            Button {
                workspace.browser.open(link.url); dismiss(); onBrowse()
            } label: {
                VStack(alignment: .leading) { Text(link.title); Text(link.url).font(.caption).foregroundStyle(.secondary).lineLimit(1) }
            }.disabled(workspace.lockedForTask || workspace.agent.active)
        }.navigationTitle(title)
    }
}

private struct SessionPasswordSheet: View {
    let data: Data
    var onOpen: (BrowserImportPayload) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var password = ""
    @State private var opening = false
    @State private var error: String?
    var body: some View {
        AppNavigationStack {
            Form {
                SecureField("会话包密码", text: $password).textContentType(.password).autocorrectionDisabled().textInputAutocapitalization(.never)
                Text("密码只在手机上解密文件。下一步选择要导入的账号、网站和资料。")
                if let error { Text(error).foregroundStyle(Theme.attention) }
                Button("解密并查看范围") {
                    opening = true
                    Task {
                        do { let payload = try await BrowserSessionCodec.open(data, password: password); password = ""; onOpen(payload) }
                        catch { self.error = error.localizedDescription }
                        opening = false
                    }
                }.disabled(opening || password.isEmpty)
                if opening { ProgressView() }
            }.navigationTitle("导入加密会话")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("取消") { password = ""; dismiss() } } }
        }
    }
}

struct BrowserImportReview: View {
    let payload: BrowserImportPayload
    @ObservedObject var workspace: BrowserWorkspace
    var onDone: () -> Void
    @State private var selected = Set<String>()
    @State private var cookies = true
    @State private var storage = false
    @State private var bookmarks = true
    @State private var history = false
    @State private var personal = false
    @State private var sites = false
    @State private var error: String?
    var body: some View {
        AppNavigationStack {
            Form {
                Section("选择账号与网站") {
                    ForEach(payload.accounts, id: \.id) { account in
                        Toggle(isOn: Binding(get: { selected.contains(account.id) }, set: { if $0 { selected.insert(account.id) } else { selected.remove(account.id) } })) {
                            VStack(alignment: .leading) {
                                Text(account.label ?? account.id)
                                Text(account.site).font(.caption).foregroundStyle(.secondary)
                                Text("\(account.cookies.count) 个 Cookie；分区、过期或超出网站范围的 Cookie 会跳过").font(.caption)
                            }
                        }
                    }
                }
                Section("导入内容") {
                    Toggle("登录 Cookie", isOn: $cookies)
                    Toggle("首个 HTTPS 网站的 localStorage", isOn: $storage)
                    Toggle("书签", isOn: $bookmarks)
                    Toggle("历史记录", isOn: $history)
                    Toggle("姓名、地址等填写资料", isOn: $personal)
                    if payload.site_catalog != nil { Toggle("信任并导入 chrome-use 网站库", isOn: $sites) }
                }
                Text("登录状态仅写入所选账号的独立浏览器。更新已有账号登录状态时，会清理该账号旧的网站数据；其他账号不会被修改。localStorage 只写入列表中的首个 HTTPS 网站，请确认它就是登录来源。")
                    .font(.footnote).foregroundStyle(.secondary)
                if sites { Text("网站适配器会以当前登录账号执行。只信任来自你自己 chrome-use 安装的网站库；写入操作仍需单独确认。") }
                if let error { Text(error).foregroundStyle(Theme.attention) }
                Button("确认导入所选范围") {
                    do {
                        try workspace.library.importPayload(payload, selected: selected, includeCookies: cookies, includeStorage: storage,
                            includeBookmarks: bookmarks, includeHistory: history, includePersonal: personal, trustSites: sites)
                        Task {
                            do { try await workspace.switchAccount(workspace.library.activeID); onDone() }
                            catch { self.error = error.localizedDescription }
                        }
                    } catch { self.error = error.localizedDescription }
                }.disabled(selected.isEmpty || workspace.lockedForTask || workspace.agent.active || workspace.switching)
                    .accessibilityIdentifier("browser-confirm-import")
            }.navigationTitle("确认导入范围")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("取消", action: onDone) } }
        }
    }
}

private struct BrowserCatalogReview: View {
    let catalog: BrowserSiteCatalog
    @ObservedObject var library: BrowserLibrary
    var onDone: () -> Void
    @State private var trust = false
    @State private var error: String?
    var body: some View {
        AppNavigationStack {
            Form {
                Text("\(catalog.adapters.count) 个 chrome-use site 操作，其中 \(catalog.adapters.filter(\.desktopOnly).count) 个暂不支持手机执行。")
                ForEach(Array(Set(catalog.adapters.compactMap { $0.url?.host })).sorted(), id: \.self) { Text($0) }
                Toggle("我信任这份 chrome-use 网站库的来源", isOn: $trust)
                Text("适配器会在其声明的网站中、以当前浏览器账号运行。内容哈希校验只能确认文件未被改动，不能代替对来源的信任。")
                if let error { Text(error).foregroundStyle(Theme.attention) }
                Button("导入网站库") {
                    do { try library.installCatalog(catalog); onDone() } catch { self.error = error.localizedDescription }
                }.disabled(!trust)
            }.navigationTitle("确认网站库")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("取消", action: onDone) } }
        }
    }
}

private struct BrowserPersonalEditor: View {
    @ObservedObject var library: BrowserLibrary
    @State private var fields: [String: String] = [:]
    @State private var error: String?
    private let labels = [("name", "姓名"), ("email", "邮箱"), ("phone", "电话"), ("address", "地址"), ("city", "城市"), ("postalCode", "邮编"), ("country", "国家")]
    var body: some View {
        Form {
            ForEach(labels, id: \.0) { field in
                TextField(field.1, text: Binding(get: { fields[field.0] ?? "" }, set: { fields[field.0] = $0 }))
            }
            Text("资料加密保存在手机。助手只知道有哪些字段；每次填写前会显示网站和字段，由你确认。密码、验证码和付款资料仍由你手动填写。")
            if let error { Text(error).foregroundStyle(Theme.attention) }
            Button("保存资料") { do { try library.updatePersonal(fields) } catch { self.error = error.localizedDescription } }
        }.navigationTitle("填写资料").onAppear { fields = library.active.personal }
    }
}

struct BrowserSitesView: View {
    @ObservedObject var workspace: BrowserWorkspace
    var onBrowse: () -> Void = {}
    @State private var search = ""
    var body: some View {
        List {
            ForEach(workspace.library.catalog.adapters.filter { search.isEmpty || $0.spec.localizedCaseInsensitiveContains(search) || $0.description.localizedCaseInsensitiveContains(search) }) { adapter in
                NavigationLink { BrowserSiteCommandView(adapter: adapter, workspace: workspace, onBrowse: onBrowse) } label: {
                    VStack(alignment: .leading) {
                        Text(adapter.spec)
                        Text(adapter.description).font(.caption).foregroundStyle(.secondary)
                        Text(adapter.desktopOnly ? "手机暂不支持" : (adapter.readOnly ? "手机浏览器 · 读取" : "手机浏览器 · 需要确认"))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
        }.navigationTitle("chrome-use site").searchable(text: $search, placement: .navigationBarDrawer(displayMode: .always))
    }
}

private struct BrowserSiteCommandView: View {
    let adapter: BrowserSiteAdapter
    @ObservedObject var workspace: BrowserWorkspace
    var onBrowse: () -> Void
    @State private var values: [String: String] = [:]
    @State private var result: String?
    @State private var confirmation = false
    @State private var running = false
    @State private var outcomeUnknown = false
    var body: some View {
        Form {
            Text(adapter.description)
            Text(adapter.url?.absoluteString ?? "").font(.caption)
            Text("账号：\(workspace.library.active.label)")
            ForEach(adapter.arguments.keys.sorted(), id: \.self) { key in
                TextField(key, text: Binding(get: { values[key] ?? "" }, set: { values[key] = $0 }))
            }
            if adapter.desktopOnly { Text("该操作依赖 Node.js、OpenCLI 或电脑文件接口，当前手机执行环境不支持。") }
            Button("执行网站操作") { if adapter.readOnly { execute() } else { confirmation = true } }
                .disabled(running || outcomeUnknown || workspace.library.pendingSite != nil || adapter.desktopOnly || workspace.switching || workspace.lockedForTask || workspace.agent.active)
            if running { ProgressView() }
            if let result { Text(result).textSelection(.enabled) }
        }.navigationTitle(adapter.spec)
            .confirmationDialog("以当前账号执行这个写入操作？", isPresented: $confirmation, titleVisibility: .visible) {
                Button("允许这一次", action: execute)
                Button("取消", role: .cancel) {}
            } message: { Text("网站：\(adapter.url?.host ?? "")\n账号：\(workspace.library.active.label)\n操作：\(adapter.spec)") }
    }
    private func execute() {
        let args = values.filter { !$0.value.isEmpty }.mapValues(JSONValue.string)
        do { try adapter.validate(arguments: args) } catch { result = error.localizedDescription; return }
        do { if !adapter.readOnly { try workspace.library.recordSite("\(adapter.spec) · \(adapter.desktopOnly ? (workspace.library.chrome?.profile ?? "电脑") : workspace.library.active.label)") } }
        catch { result = error.localizedDescription; return }
        running = true
        workspace.lockedForTask = true
        Task {
            defer { workspace.lockedForTask = false }
            do {
                guard let url = adapter.url else { throw BrowserToolError.invalidResult }
                let data: JSONValue
                if adapter.desktopOnly {
                    throw BrowserToolError.actionFailed("当前手机执行环境不支持这个网站操作")
                } else {
                    if workspace.browser.webView.url?.host != url.host { workspace.browser.open(url.absoluteString) }
                    data = try await workspace.browser.runSite(adapter, arguments: args).result
                }
                result = String(decoding: try JSONEncoder().encode(BrowserRedactor.value(data, secrets: workspace.library.active.cookies.map(\.value))), as: UTF8.self)
                if !adapter.readOnly { try workspace.library.recordSite(nil) }
            } catch {
                outcomeUnknown = !adapter.readOnly
                result = outcomeUnknown ? "操作结果未知，请手动检查网页，不要重复执行这次写入。" : error.localizedDescription
            }
            running = false
        }
    }
}
