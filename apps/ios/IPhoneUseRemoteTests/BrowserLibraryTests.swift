import XCTest
import CryptoKit
import WebKit
@testable import IPhoneUseRemote

@MainActor
final class BrowserLibraryTests: XCTestCase {
    private func fixture() -> (BrowserLibrary, URL, Data) {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".enc")
        let key = Data(repeating: 7, count: 32)
        return (BrowserLibrary(file: file, key: key), file, key)
    }
    private func payload(_ id: String, secret: String) throws -> BrowserImportPayload {
        let data = Data("""
        {"accounts":[{"id":"\(id)","site":"example.com","label":"Fixture","cookies":[{"name":"sid","value":"\(secret)","domain":".example.com","secure":true,"httpOnly":true}],"local_storage":{"fixture":"\(secret)"}}],"browser_data":{"personal":{"name":"Synthetic Person","password":"never-import"},"bookmarks":[{"url":"https://example.com/path","title":"Example"},{"url":"https://outside.test/","title":"Outside"}]}}
        """.utf8)
        return try JSONDecoder().decode(BrowserImportPayload.self, from: data)
    }
    func testEncryptedImportAndProfileIsolation() async throws {
        let (library, file, key) = fixture(); defer { try? FileManager.default.removeItem(at: file) }
        try library.importPayload(payload("work", secret: "synthetic-work-cookie"), selected: ["work"], includeCookies: true, includeStorage: true, includeBookmarks: true, includeHistory: true, includePersonal: true, trustSites: false)
        try library.importPayload(payload("home", secret: "synthetic-home-cookie"), selected: ["home"], includeCookies: true, includeStorage: true, includeBookmarks: true, includeHistory: true, includePersonal: true, trustSites: false)
        let work = try XCTUnwrap(library.accounts.first { $0.externalID == "work" })
        let home = try XCTUnwrap(library.accounts.first { $0.externalID == "home" })
        XCTAssertNotEqual(work.id, home.id)
        XCTAssertEqual(work.bookmarks.count, 1); XCTAssertNil(work.personal["password"])
        XCTAssertFalse(String(decoding: try Data(contentsOf: file), as: UTF8.self).contains("synthetic-work-cookie"))
        let restored = BrowserLibrary(file: file, key: key)
        XCTAssertEqual(restored.accounts.count, 3)
        try restored.select(work.id)
        let workspace = BrowserWorkspace(library: restored)
        for _ in 0..<100 where workspace.switching { try await Task.sleep(for: .milliseconds(20)) }
        let first = await cookies(workspace)
        XCTAssertEqual(first.first(where: { $0.name == "sid" })?.value, "synthetic-work-cookie")
        XCTAssertTrue(first.first(where: { $0.name == "sid" })?.isHTTPOnly == true)
        try await workspace.switchAccount(home.id)
        let second = await cookies(workspace)
        XCTAssertEqual(second.first(where: { $0.name == "sid" })?.value, "synthetic-home-cookie")
        workspace.lockedForTask = true
        do { try await workspace.switchAccount(work.id); XCTFail("Task must freeze account") } catch {}
    }
    private func cookies(_ workspace: BrowserWorkspace) async -> [HTTPCookie] {
        await withCheckedContinuation { continuation in workspace.browser.webView.configuration.websiteDataStore.httpCookieStore.getAllCookies { continuation.resume(returning: $0) } }
    }
    func testLegacyEphemeralProfilesRestoreSeparately() async throws {
        let previous = UserDefaults.standard.bool(forKey: "legacyUI")
        UserDefaults.standard.set(true, forKey: "legacyUI")
        defer { UserDefaults.standard.set(previous, forKey: "legacyUI") }
        try await testEncryptedImportAndProfileIsolation()
    }
    func testIndependentCookieUseV1ThroughNativeCodec() async throws {
        let bundle = Data(#"{"cookie_use_bundle":1,"id":"fixture/example","site":"example.com","kdf":"argon2id","salt":"AAECAwQFBgcICQoLDA0ODw==","ciphertext":"AAECAwQFBgcICQoL01Jy3GV/gF4qP0wGo7sKr+E0f9LNo9Ra+SKAllaK4lYmZUuJ5BSSPsifIT8T95Ef4BAdKZqp6RRnevzyplpahiWmREU0viprPMf0J1Uef41LMRf3sg4ACFJdR8JA1OvIqZPOLDKau2ZpX+Z32UAgJ1PX5Uy6Ruw0ooLQo3sWThpkn38VNFa9EFg6wmWdAdvnHXx1XYVQHye9Vuwdb1aVYPotWYWuUH7a5J/FR4W6n0Fiqe6n6RRrg5MVnMJu3QdmdTXmRcaWfpyWBr9YOMWWFDf2FeBItuLIRAZ6cFem+Nb30UcUR0+B+C/td37tdJhI8hL6U6dbzSrCD8F8WpEzk3uuGZd/gg=="}"#.utf8)
        let payload = try await BrowserSessionCodec.open(bundle, password: "synthetic-cookie-use-password")
        XCTAssertEqual(payload.accounts.first?.id, "fixture/example")
        XCTAssertEqual(payload.accounts.first?.cookies.first?.value, "synthetic-cookie-value")
    }
    func testInvalidCookieScopeAndExpiredCookiesAreNotInstalled() {
        var cookie = BrowserCookie(name: "sid", value: "synthetic", domain: ".outside.test")
        XCTAssertNil(cookie.cookie(allowedDomains: ["example.com"]))
        cookie.domain = ".example.com"; cookie.expires = 1
        XCTAssertNil(cookie.cookie(allowedDomains: ["example.com"]))
        cookie.expires = nil; cookie.partitionKey = .object(["topLevelSite": .string("https://example.com")])
        XCTAssertNil(cookie.cookie(allowedDomains: ["example.com"]))
    }
    func testCorruptVaultIsPreserved() throws {
        let (_, file, key) = fixture(); let original = Data("synthetic-corrupt-file".utf8)
        try original.write(to: file); defer { try? FileManager.default.removeItem(at: file) }
        let library = BrowserLibrary(file: file, key: key)
        XCTAssertNotNil(library.error)
        XCTAssertThrowsError(try library.importPayload(payload("work", secret: "synthetic"), selected: ["work"], includeCookies: true, includeStorage: true, includeBookmarks: false, includeHistory: false, includePersonal: false, trustSites: false))
        XCTAssertEqual(try Data(contentsOf: file), original)
    }
    func testDesktopPairingAndEncryptedVaultBinding() throws {
        let (library, file, key) = fixture(); defer { try? FileManager.default.removeItem(at: file) }
        let scope = String(repeating: "a", count: 24), grantKey = Data(repeating: 2, count: 32)
        var url = URLComponents(string: "phoneuse-chrome://pair")!
        url.queryItems = [URLQueryItem(name: "u", value: "http://127.0.0.1:45929"), URLQueryItem(name: "s", value: scope), URLQueryItem(name: "k", value: grantKey.base64EncodedString())]
        let binding = try ChromeBinding.parse(url.string!); try library.setChrome(binding)
        XCTAssertEqual(BrowserLibrary(file: file, key: key).chrome?.key, grantKey)
        XCTAssertFalse(String(decoding: try Data(contentsOf: file), as: UTF8.self).contains(grantKey.base64EncodedString()))
        XCTAssertThrowsError(try ChromeBinding.parse("https://example.com/?k=not-a-grant"))
    }
    func testInstalledChromeUseSourceRunsInWebKit() async throws {
        let browser = BrowserSession(dataStore: .nonPersistent())
        browser.loading = true; browser.hasPage = true
        browser.webView.loadHTMLString("<h1>Synthetic site fixture</h1>", baseURL: URL(string: "https://example.com"))
        try await browser.waitUntilReady()
        let source = "async function(args) { args.progress('Reading fixture'); return {text:document.querySelector('h1').textContent,limit:args.limit}; }"
        let hash = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
        let adapter = BrowserSiteAdapter(spec: "example/read", meta: ["domain": .string("example.com"), "readOnly": .bool(true), "args": .object(["limit": .object(["type": .string("number")])])], source: source, sha256: hash, origin: "synthetic installed site fixture")
        let result = try await browser.runSite(adapter, arguments: ["limit": .number(3)])
        XCTAssertEqual(result.result.object?["text"]?.string, "Synthetic site fixture")
        XCTAssertEqual(result.result.object?["limit"], .number(3))
        XCTAssertEqual(result.progress, ["Reading fixture"])
    }
    func testSiteValidationAndRedaction() throws {
        let source = "async function(args) { return {text:'fixture'}; }"
        let hash = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
        var adapter = BrowserSiteAdapter(spec: "example/read", meta: ["domain": .string("example.com"), "readOnly": .bool(true)], source: source, sha256: hash, origin: "synthetic")
        XCTAssertNoThrow(try BrowserSiteCatalog(adapters: [adapter]).validated())
        adapter.spec = "example/send"; XCTAssertFalse(adapter.readOnly)
        adapter.source += " // changed"
        XCTAssertThrowsError(try BrowserSiteCatalog(adapters: [adapter]).validated())
        let value = BrowserRedactor.value(.object(["cookie": .string("secret"), "text": .string("synthetic-cookie-value")]), secrets: ["synthetic-cookie-value"])
        let text = String(decoding: try JSONEncoder().encode(value), as: UTF8.self)
        XCTAssertFalse(text.contains("synthetic-cookie-value")); XCTAssertFalse(text.contains("secret"))
    }
    func testShortPersonalValuesAndCredentialScalarsAreRedacted() {
        XCTAssertEqual(BrowserRedactor.text("Hello Leo; token=abc123", secrets: ["Leo", "abc123"]), "Hello [已隐藏]; token=[已隐藏]")
        XCTAssertEqual(BrowserRedactor.value(.string("1"), secrets: ["1"]), .string("[已隐藏]"))
        XCTAssertEqual(BrowserRedactor.value(.object(["session": .string("synthetic")]), secrets: []), .object([:]))
    }
    func testDesktopFingerprintUsesVerifiedMetadataBytes() throws {
        let source = "async function(args) { return {ok:true}; }"
        let raw = #"{"readOnly":true,"domain":"example.com","args":{"limit":{"default":3.0,"type":"number"}}}"#
        let metadata = try JSONDecoder().decode([String: JSONValue].self, from: Data(raw.utf8))
        let hash = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
        var adapter = BrowserSiteAdapter(spec: "example/read", meta: metadata, source: source, sha256: hash, origin: "synthetic", metaJSON: raw)
        let expected = SHA256.hash(data: Data((raw + source).utf8)).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(adapter.fingerprint, expected)
        XCTAssertNoThrow(try BrowserSiteCatalog(adapters: [adapter]).validated())
        adapter.meta["readOnly"] = .bool(false)
        XCTAssertThrowsError(try BrowserSiteCatalog(adapters: [adapter]).validated())
    }
    func testLocalPlanRejectsDesktopEvenWithAPairedCatalog() throws {
        let source = "async function(args) { return {}; }"
        let hash = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
        var adapter = BrowserSiteAdapter(spec: "example/read", meta: ["domain": .string("example.com"), "readOnly": .bool(true)], source: source, sha256: hash, origin: "synthetic")
        let plan = AssistantPlan(phases: [.init(tool: .desktop, goal: "Read fixture", siteSpec: adapter.spec)])
        XCTAssertThrowsError(try plan.validate(catalog: [adapter]))
        adapter.pairedScope = String(repeating: "a", count: 24)
        XCTAssertThrowsError(try plan.validate(catalog: [adapter]))
        XCTAssertTrue(adapter.desktopOnly)
    }
    func testPlanRejectsInventedSiteAndDesktopOnlyMobileCommand() throws {
        XCTAssertThrowsError(try AssistantPlan(phases: [.init(tool: .site, goal: "fixture", siteSpec: "invented/read")]).validate(catalog: []))
        XCTAssertThrowsError(try AssistantPlan(phases: []).validate(catalog: []))
        XCTAssertNoThrow(try AssistantPlan(phases: [.init(tool: .browser, goal: "read fixture"), .init(tool: .phone, goal: "read battery"), .init(tool: .browser, goal: "compare results")]).validate(catalog: []))
    }
}
