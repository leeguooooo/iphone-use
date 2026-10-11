#if DEBUG
import SwiftUI
import CryptoKit

/// Explicit developer acceptance mode: isolated synthetic data, no production drafts.
struct BrowserDeviceAcceptanceView: View {
    static var requestedURL: URL? {
        guard let raw = UserDefaults.standard.string(forKey: "browserAcceptanceURL"), let url = URL(string: raw), url.scheme == "http", url.port == 47019, let host = url.host else { return nil }
        let octets = host.split(separator: ".").compactMap { Int($0) }
        guard octets.count == 4, octets.allSatisfy({ (0...255).contains($0) }), octets[0] == 10 || (octets[0] == 192 && octets[1] == 168) || (octets[0] == 172 && (16...31).contains(octets[1])) || host == "127.0.0.1" else { return nil }
        return url
    }
    let address: URL
    @State private var workspace: BrowserWorkspace?
    @State private var assistant: UnifiedAssistant?
    @State private var fixtureApp: AppModel?
    @State private var message = "正在验证手机浏览器"
    var body: some View {
        VStack {
            Text("合成数据真机验证").font(.headline)
            Text(message).font(.caption)
            if let workspace { BrowserSurface(session: workspace.browser) }
        }.task { await run() }
    }
    private func run() async {
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].appendingPathComponent("acceptance", isDirectory: true)
        var report: [String: JSONValue] = ["fixture": .string("synthetic-browser"), "success": .bool(false), "url": .string(address.absoluteString)]
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let suite = "device-acceptance-" + UUID().uuidString
            let defaults = UserDefaults(suiteName: suite)!
            let withPhone = UserDefaults.standard.bool(forKey: "browserAcceptancePhone")
            if withPhone {
                // Reuse only this phone's existing credential references; no production task/draft data.
                let local = DeviceStore.load(from: .standard, hasCredentials: DeviceSession.hasCredentials).filter(\.isThisPhone)
                guard !local.isEmpty else { throw BrowserToolError.actionFailed("合成跨工具验收需要本机已配对") }
                DeviceStore.save(local, to: defaults)
            }
            let app = AppModel(defaults: defaults); fixtureApp = app
            app.agent.settings = ModelSettings() // Only built-in free providers in a fresh suite.
            let key = Data((0..<32).map { UInt8($0) }) // Synthetic archive only.
            let library = BrowserLibrary(file: directory.appendingPathComponent(UUID().uuidString + ".enc"), key: key)
            let started = Date()
            let payload = try await BrowserSessionCodec.open(Data(#"{"cookie_use_bundle":1,"id":"fixture/example","site":"example.com","kdf":"argon2id","salt":"AAECAwQFBgcICQoLDA0ODw==","ciphertext":"AAECAwQFBgcICQoL01Jy3GV/gF4qP0wGo7sKr+E0f9LNo9Ra+SKAllaK4lYmZUuJ5BSSPsifIT8T95Ef4BAdKZqp6RRnevzyplpahiWmREU0viprPMf0J1Uef41LMRf3sg4ACFJdR8JA1OvIqZPOLDKau2ZpX+Z32UAgJ1PX5Uy6Ruw0ooLQo3sWThpkn38VNFa9EFg6wmWdAdvnHXx1XYVQHye9Vuwdb1aVYPotWYWuUH7a5J/FR4W6n0Fiqe6n6RRrg5MVnMJu3QdmdTXmRcaWfpyWBr9YOMWWFDf2FeBItuLIRAZ6cFem+Nb30UcUR0+B+C/td37tdJhI8hL6U6dbzSrCD8F8WpEzk3uuGZd/gg=="}"#.utf8), password: "synthetic-cookie-use-password")
            report["decode_ms"] = .number(Date().timeIntervalSince(started) * 1000)
            report["decrypted_accounts"] = .number(Double(payload.accounts.count))
            try library.importPayload(payload, selected: ["fixture/example"], includeCookies: true, includeStorage: true, includeBookmarks: false, includeHistory: false, includePersonal: false, trustSites: false)
            let account = library.accounts.first { $0.externalID == "fixture/example" }!
            try library.select(account.id)
            let workspace = BrowserWorkspace(library: library); self.workspace = workspace
            for _ in 0..<100 where workspace.switching { try await Task.sleep(for: .milliseconds(20)) }
            workspace.browser.open(address.absoluteString)
            try await workspace.browser.waitUntilReady()
            let assistant = UnifiedAssistant(app: app, workspace: workspace, defaults: defaults, file: directory.appendingPathComponent(UUID().uuidString + ".enc"), key: key)
            self.assistant = assistant
            if withPhone {
                for _ in 0..<400 where !assistant.phoneAvailable { try await Task.sleep(for: .milliseconds(50)) }
                guard assistant.phoneAvailable else { throw BrowserToolError.actionFailed("本机执行服务未连接") }
            }
            assistant.mode = withPhone ? .automatic : .browser
            let goal = withPhone
                ? "按三个步骤依次完成：1 在内置浏览器读取当前网页标题。2 用手机打开设置读取当前电池电量，不修改任何设置。3 回到内置浏览器，在 Name 输入框填入 Phone Use，点击 Continue，核验段落 Form updated，再合并网页标题与电量结果。不要点击 Prompt fixture。"
                : "在 Name 输入框填入 Phone Use，然后点击 Continue。段落显示 Form updated 后才完成。不要点击 Prompt fixture。"
            guard assistant.start(goal) else { throw BrowserToolError.actionFailed("合成任务未启动") }
            for _ in 0..<(withPhone ? 7200 : 2400) {
                if assistant.task?.state == .done || assistant.task?.state == .failed || (assistant.task?.state == .paused && assistant.task?.automaticPause != true) || assistant.task?.state == .needsReview { break }
                try await Task.sleep(for: .milliseconds(50))
            }
            let value = try await workspace.browser.webView.evaluateJavaScript("document.querySelector('input').value") as? String
            let paragraph = try await workspace.browser.webView.evaluateJavaScript("document.querySelector('p').textContent") as? String
            let tools = assistant.task?.phases.map { $0.tool.rawValue } ?? []
            let nativeIDs = assistant.task?.phases.compactMap(\.nativeID) ?? []
            let crossToolsVerified = !withPhone || (tools == ["browser", "phone", "browser"] && nativeIDs.count == 1)
            let success = assistant.task?.state == .done && value == "Phone Use" && paragraph == "Form updated" && crossToolsVerified
            report["cross_tools"] = .bool(withPhone)
            report["phase_tools"] = .array(tools.map(JSONValue.string))
            report["native_ids"] = .array(nativeIDs.map(JSONValue.string))
            report["success"] = .bool(success); report["state"] = .string(assistant.task?.state.rawValue ?? "none")
            report["field_value"] = .string(value ?? ""); report["paragraph"] = .string(paragraph ?? "")
            report["reply"] = .string(assistant.task?.reply ?? assistant.message ?? "")
            report["cookie_count"] = .number(Double(library.active.cookies.count))
            report["profile_isolated"] = .bool(library.activeID != BrowserAccount.defaultID)
            message = success ? "浏览器任务、免费模型和会话解密验证通过" : "验证未通过，请检查报告"
            defaults.removePersistentDomain(forName: suite)
        } catch { report["error"] = .string(error.localizedDescription); message = "验证失败：" + error.localizedDescription }
        if let data = try? JSONEncoder().encode(report) {
            try? data.write(to: directory.appendingPathComponent("browser-result.json"), options: [.atomic, .completeFileProtectionUnlessOpen])
        }
    }
}
#endif
