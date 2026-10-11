import SwiftUI
import WebKit
import Combine

struct BrowserDialog: Identifiable {
    enum Kind { case alert, confirm, prompt }
    let id = UUID()
    let kind: Kind
    let message: String
    let initialText: String
    let origin: String
}

@MainActor
final class BrowserSession: NSObject, ObservableObject, WKNavigationDelegate, WKUIDelegate {
    let webView: WKWebView
    @Published var address = ""
    @Published var title = "浏览器"
    @Published var loading = false
    @Published var canGoBack = false
    @Published var canGoForward = false
    @Published var error: String?
    @Published var hasPage = false
    @Published var dialog: BrowserDialog?
    private var dialogResponse: ((Bool, String?) -> Void)?
    private var urlObservation: NSKeyValueObservation?

    override convenience init() { self.init(dataStore: .default()) }

    init(dataStore: WKWebsiteDataStore) {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = dataStore
        webView = WKWebView(frame: .zero, configuration: configuration)
        super.init()
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.allowsBackForwardNavigationGestures = true
        webView.accessibilityIdentifier = "browser-page"
        urlObservation = webView.observe(\.url, options: [.new]) { [weak self] _, _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.address = self.webView.url?.absoluteString ?? self.address
                self.canGoBack = self.webView.canGoBack
                self.canGoForward = self.webView.canGoForward
            }
        }
    }

    static func destination(_ input: String) -> URL? {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        if text.contains("://") {
            guard let url = URL(string: text), ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
                  url.host != nil else { return nil }
            return url
        }
        if !text.contains(where: \.isWhitespace), text.contains(".") || text == "localhost" {
            return URL(string: "https://" + text)
        }
        var components = URLComponents(string: "https://www.google.com/search")!
        components.queryItems = [URLQueryItem(name: "q", value: text)]
        return components.url
    }

    func open(_ input: String) {
        guard let url = Self.destination(input) else {
            error = String(localized: "请输入网页地址或搜索内容")
            return
        }
        error = nil
        hasPage = true
        address = url.absoluteString
        loading = true
        webView.load(URLRequest(url: url))
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        error = nil
        loading = true
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) { update() }
    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        address = webView.url?.absoluteString ?? address
        canGoBack = webView.canGoBack
        canGoForward = webView.canGoForward
    }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        answerDialog(false)
        loading = false
        error = String(localized: "网页已停止运行，请重新加载")
    }
    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { failed(error) }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { failed(error) }

    func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
                 decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        let scheme = navigationAction.request.url?.scheme?.lowercased() ?? ""
        if !["http", "https", "about"].contains(scheme) {
            error = String(localized: "这个链接需要在其他 App 中打开，请手动操作")
        }
        decisionHandler(["http", "https", "about"].contains(scheme) ? .allow : .cancel)
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if navigationAction.targetFrame == nil, let url = navigationAction.request.url { open(url.absoluteString) }
        return nil
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor () -> Void) {
        dialogResponse = { _, _ in completionHandler() }
        dialog = .init(kind: .alert, message: message, initialText: "", origin: frame.request.url?.host ?? webView.url?.host ?? "网页")
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor (Bool) -> Void) {
        dialogResponse = { accepted, _ in completionHandler(accepted) }
        dialog = .init(kind: .confirm, message: message, initialText: "", origin: frame.request.url?.host ?? webView.url?.host ?? "网页")
    }

    func webView(_ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String, defaultText: String?,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor (String?) -> Void) {
        dialogResponse = { accepted, text in completionHandler(accepted ? text : nil) }
        dialog = .init(kind: .prompt, message: prompt, initialText: defaultText ?? "", origin: frame.request.url?.host ?? webView.url?.host ?? "网页")
    }

    func answerDialog(_ accepted: Bool, text: String? = nil) {
        let response = dialogResponse
        dialogResponse = nil
        dialog = nil
        response?(accepted, text)
    }

    private func update() {
        loading = false
        canGoBack = webView.canGoBack
        canGoForward = webView.canGoForward
        address = webView.url?.absoluteString ?? address
        title = webView.title ?? String(localized: "浏览器")
    }

    private func failed(_ failure: Error) {
        update()
        if (failure as NSError).code != NSURLErrorCancelled { error = failure.localizedDescription }
    }

    /// A local page snapshot. No page data is sent to a model by this operation.
    func read() async throws -> String {
        let value = try await webView.evaluateJavaScript("document.body ? document.body.innerText.slice(0, 24000) : ''")
        return value as? String ?? ""
    }
}

struct BrowserSurface: UIViewRepresentable {
    let session: BrowserSession
    func makeUIView(context: Context) -> WKWebView { session.webView }
    func updateUIView(_ uiView: WKWebView, context: Context) {}
}

struct BrowserView: View {
    @ObservedObject var session: BrowserSession
    @ObservedObject var agent: BrowserAgent
    @ObservedObject var settings: AgentController
    var workspace: BrowserWorkspace? = nil
    @State private var libraryOpen = false
    @State private var input = ""
    @State private var pageText: String?
    @State private var reading = false
    @State private var conversationOpen = false
    @State private var dialogText = ""
    @FocusState private var addressing: Bool

    private var legacyPrompt: Bool {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { return false }
        return session.dialog?.kind == .prompt
    }

    var body: some View {
        AppNavigationStack {
            VStack(spacing: 0) {
                HStack(spacing: 8) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("搜索或输入网址", text: $input)
                        .textContentType(.URL)
                        .keyboardType(.webSearch)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .submitLabel(.go)
                        .focused($addressing)
                        .onSubmit { navigate(input) }
                        .accessibilityIdentifier("browser-address")
                        .disabled(agent.active)
                    if session.loading {
                        Button { session.webView.stopLoading(); session.loading = false } label: {
                            Image(systemName: "xmark")
                        }.accessibilityLabel("停止加载")
                    } else if session.hasPage {
                        Button { session.webView.reload() } label: { Image(systemName: "arrow.clockwise") }
                            .accessibilityLabel("重新加载")
                    }
                }
                .padding(12)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                .padding(.horizontal).padding(.bottom, 8)
                if let error = session.error {
                    Text(error).font(.footnote).foregroundStyle(.red).padding()
                }
                if session.hasPage {
                    BrowserSurface(session: session).allowsHitTesting(!agent.active)
                } else {
                    startPage
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) { BrowserTaskBar(agent: agent, settings: settings, expanded: $conversationOpen).disabled(workspace?.lockedForTask == true && !agent.active) }
            .background(Color(uiColor: .systemBackground))
            .sheet(isPresented: $libraryOpen) { if let workspace { BrowserLibraryView(workspace: workspace) } }
            .navigationTitle("浏览器")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItemGroup(placement: .navigationBarLeading) {
                    Button { session.webView.goBack() } label: { Image(systemName: "chevron.left") }
                        .disabled(!session.canGoBack || agent.active).accessibilityLabel("后退")
                    Button { session.webView.goForward() } label: { Image(systemName: "chevron.right") }
                        .disabled(!session.canGoForward || agent.active).accessibilityLabel("前进")
                }
                ToolbarItemGroup(placement: .navigationBarTrailing) {
                    if workspace != nil {
                        Button { libraryOpen = true } label: { Image(systemName: "person.crop.circle") }.accessibilityLabel("账号与网站资料")
                    }
                    Button {
                        reading = true
                        Task {
                            defer { reading = false }
                            do { pageText = try await session.read() }
                            catch { session.error = error.localizedDescription }
                        }
                    } label: { Image(systemName: "doc.text.magnifyingglass") }
                        .disabled(!session.hasPage || reading || session.loading || agent.active)
                        .accessibilityLabel("读取页面")
                        .accessibilityIdentifier("browser-read")
                    if let url = URL(string: session.address), session.hasPage {
                        AppShareLink(item: url).accessibilityLabel("分享网页")
                    }
                }
            }
            .appOnChange(of: session.address) { _, address in if !addressing { input = address } }
            .appOnChange(of: session.dialog?.id) { _, id in
                if id != nil { conversationOpen = false; dialogText = session.dialog?.initialText ?? ""; agent.pause() }
            }
            .alert(session.dialog?.origin ?? String(localized: "网页消息"),
                   isPresented: Binding(get: { session.dialog != nil && !legacyPrompt }, set: { _ in }),
                   presenting: session.dialog) { dialog in
                if dialog.kind == .prompt { TextField("输入内容", text: $dialogText) }
                if dialog.kind != .alert { Button("取消", role: .cancel) { session.answerDialog(false) } }
                Button("确认") { session.answerDialog(true, text: dialogText) }
            } message: { dialog in Text(dialog.message) }
            .sheet(isPresented: Binding(get: { legacyPrompt }, set: {
                if !$0 && legacyPrompt { session.answerDialog(false) }
            })) {
                AppNavigationStack {
                    Form {
                        if let dialog = session.dialog {
                            Text(dialog.message)
                            TextField("输入内容", text: $dialogText)
                                .accessibilityIdentifier("browser-dialog-input")
                        }
                    }
                    .navigationTitle(session.dialog?.origin ?? String(localized: "网页消息"))
                    .navigationBarTitleDisplayMode(.inline)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("取消") { session.answerDialog(false) }
                        }
                        ToolbarItem(placement: .confirmationAction) {
                            Button("确认") { session.answerDialog(true, text: dialogText) }
                        }
                    }
                }
            }
            .sheet(isPresented: Binding(get: { pageText != nil }, set: { if !$0 { pageText = nil } })) {
                AppNavigationStack {
                    ScrollView { Text(pageText ?? "").textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding() }
                        .navigationTitle("页面内容")
                        .navigationBarTitleDisplayMode(.inline)
                        .toolbar { ToolbarItem(placement: .confirmationAction) { Button("完成") { pageText = nil } } }
                }
            }
        }
    }

    private var startPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                Text("收藏").font(.title2.bold())
                HStack(spacing: 24) {
                    favorite("Apple", symbol: "apple.logo", url: "https://www.apple.com")
                    favorite("GitHub", symbol: "chevron.left.forwardslash.chevron.right", url: "https://github.com")
                }
                Text("在这里登录和浏览网页。浏览数据保存在 App 内，与 Safari 分开。")
                    .font(.footnote).foregroundStyle(.secondary)
            }.frame(maxWidth: 600, alignment: .leading).padding(24)
        }
    }

    private func favorite(_ name: String, symbol: String, url: String) -> some View {
        Button { navigate(url) } label: {
            VStack(spacing: 8) {
                Image(systemName: symbol).font(.title2).frame(width: 64, height: 64)
                    .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
                Text(name).font(.caption).foregroundStyle(.primary)
            }
        }.buttonStyle(.plain)
    }

    private func navigate(_ value: String) { guard !agent.active else { return }; addressing = false; session.open(value); input = session.address }
}
