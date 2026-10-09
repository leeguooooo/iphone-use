import XCTest
@testable import IPhoneUseRemote

final class AddressParsingTests: XCTestCase {
    private func parse(_ text: String) -> String? { DaemonClient.parse(address: text)?.absoluteString }

    func testBareLANAddressesGetTheDaemonPort() {
        XCTAssertEqual(parse("192.168.1.11"), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("  192.168.1.11  "), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("192.168.1.11:44322"), "http://192.168.1.11:44322")
        XCTAssertEqual(parse("mac-mini.local"), "http://mac-mini.local:44321")
        XCTAssertEqual(parse("macmini"), "http://macmini:44321")
        XCTAssertEqual(parse("localhost:8787"), "http://localhost:8787")
    }

    func testSchemesAndPathsAreNormalized() {
        XCTAssertEqual(parse("http://192.168.1.11:44321/"), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("HTTP://192.168.1.11:44321/phone?x=1#y"), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("http://192.168.1.11"), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("https://phone.example.com/phone"), "https://phone.example.com")
    }

    func testPublicNamesAreTunnelsOverHTTPS() {
        XCTAssertEqual(parse("abc-def.trycloudflare.com"), "https://abc-def.trycloudflare.com")
        XCTAssertEqual(parse("Phone.Example.com:443"), "https://phone.example.com")
        // An explicit http:// is respected.
        XCTAssertEqual(parse("http://phone.example.com"), "http://phone.example.com:44321")
    }

    func testFullWidthPunctuationFromChineseKeyboards() {
        XCTAssertEqual(parse("192。168。1。11：44321"), "http://192.168.1.11:44321")
        XCTAssertEqual(parse("192.168.1.11:44321."), "http://192.168.1.11:44321")
    }

    func testGarbageIsRejected() {
        XCTAssertNil(parse(""))
        XCTAssertNil(parse("   "))
        XCTAssertNil(parse("ftp://192.168.1.11"))
        XCTAssertNil(parse("http://"))
    }

    func testPastedPairingLinkIsRecognized() {
        let input = AddressInput(" http://192.168.1.11:44321/pair?c=ABCD1234 ")
        XCTAssertEqual(input, .pair(PairLink(base: URL(string: "http://192.168.1.11:44321")!, code: "ABCD1234")))
        let landing = AddressInput("iphoneuse://pair?u=http%3A%2F%2F10.0.0.2%3A44321&c=XYZ")
        XCTAssertEqual(landing, .pair(PairLink(base: URL(string: "http://10.0.0.2:44321")!, code: "XYZ")))
        XCTAssertEqual(AddressInput("10.0.0.2"), .address(URL(string: "http://10.0.0.2:44321")!))
        XCTAssertNil(AddressInput("not a url at all ://"))
    }
}

final class ConnectProblemTests: XCTestCase {
    func testDaemonErrorsMapToActionableProblems() {
        XCTAssertEqual(ConnectProblem(DaemonError.wrongPassword), .wrongPassword)
        XCTAssertEqual(ConnectProblem(DaemonError.lockedOut), .lockedOut)
        XCTAssertEqual(ConnectProblem(DaemonError.pairingCodeInvalid), .pairCodeExpired)
        XCTAssertEqual(ConnectProblem(DaemonError.pairingRevoked), .pairingRevoked)
        XCTAssertEqual(ConnectProblem(DaemonError.http(404, "<html>")), .notIphoneUse)
        XCTAssertEqual(ConnectProblem(DaemonError.http(502, "<html>bad gateway</html>")), .server(502))
        XCTAssertEqual(ConnectProblem(DaemonError.transport(.timedOut, "")), .unreachable(.timedOut))
        XCTAssertEqual(ConnectProblem(DaemonError.transport(.cannotConnectToHost, "")), .unreachable(.refused))
        XCTAssertEqual(ConnectProblem(DaemonError.transport(.cannotFindHost, "")), .unreachable(.noHost))
        XCTAssertEqual(ConnectProblem(DaemonError.transport(.notConnectedToInternet, "")), .unreachable(.offline))
        XCTAssertEqual(ConnectProblem(DaemonError.localNetworkDenied), .unreachable(.localNetworkDenied))
    }

    func testNoMessageLeaksRawHTTP() {
        let error = DaemonError.http(500, "<html><body>Internal Server Error</body></html>")
        let text = error.localizedDescription
        XCTAssertFalse(text.contains("<html>"))
        XCTAssertFalse(text.contains("http("))
    }

    func testWhoFixesWhat() {
        XCTAssertTrue(ConnectProblem.wrongPassword.needsLogin)
        XCTAssertFalse(ConnectProblem.wrongPassword.retryable)
        XCTAssertTrue(ConnectProblem.pairingRevoked.needsLogin)
        XCTAssertTrue(ConnectProblem.unreachable(.timedOut).retryable)
        XCTAssertFalse(ConnectProblem.unreachable(.timedOut).needsLogin)
        XCTAssertTrue(ConnectProblem.lockedOut.retryable)
        XCTAssertEqual(ConnectProblem.lockedOut.retryAfter, 30)
        XCTAssertFalse(ConnectProblem.notIphoneUse.retryable)
    }

    func testBackoffGrowsAndCaps() {
        let problem = ConnectProblem.unreachable(.timedOut)
        XCTAssertEqual(RetryPolicy.delay(attempt: 0, problem: problem), 2)
        XCTAssertEqual(RetryPolicy.delay(attempt: 1, problem: problem), 4)
        XCTAssertEqual(RetryPolicy.delay(attempt: 50, problem: problem), 30)
        XCTAssertEqual(RetryPolicy.delay(attempt: 0, problem: .lockedOut), 30)
    }
}

final class ConnectionPresentationTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_000_000)

    private func status(_ json: String) -> PhoneStatus {
        try! JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
    }

    private func present(_ phase: ConnectionInputs.Phase, _ statusJSON: String? = nil, video: Bool = true,
                         configure: (inout ConnectionInputs) -> Void = { _ in }) -> ConnectionPresentation {
        var inputs = ConnectionInputs(phase: phase, status: statusJSON.map(status), videoLive: video, now: t0)
        configure(&inputs)
        return ConnectionPresentation.make(inputs)
    }

    func testReadyPhoneShowsNothing() {
        let p = present(.connected, #"{"drivable":true,"device_state":"ready"}"#)
        XCTAssertEqual(p.placement, .none)
        XCTAssertEqual(p.tone, .ok)
    }

    func testConnectingCountsSeconds() {
        let p = present(.connecting) { $0.connectingSince = self.t0.addingTimeInterval(-9) }
        XCTAssertTrue(p.progress)
        XCTAssertEqual(p.elapsed, 9)
        XCTAssertFalse(p.detail.isEmpty, "a slow attempt says why it may be slow")
        XCTAssertNil(p.primary)
    }

    func testUnreachableOffersRetryAndCountsDown() {
        let p = present(.failed(.unreachable(.timedOut))) { $0.nextRetryAt = self.t0.addingTimeInterval(7.2) }
        XCTAssertEqual(p.primary, .retry)
        XCTAssertEqual(p.retryIn, 8)
        XCTAssertEqual(p.placement, .cover)
    }

    func testLoginProblemsAskForPasswordOrScan() {
        for problem in [ConnectProblem.wrongPassword, .pairingRevoked, .sessionExpired, .noCredentials, .pairCodeExpired] {
            let p = present(.failed(problem))
            XCTAssertEqual(p.primary, .login, "\(problem)")
            XCTAssertEqual(p.secondary, .rescan, "\(problem)")
            XCTAssertEqual(p.tone, .attention)
        }
    }

    func testLocalNetworkDeniedOpensSettings() {
        XCTAssertEqual(present(.failed(.unreachable(.localNetworkDenied))).primary, .openSettings)
    }

    func testLinkDownWhileConnectedIsNotAFrozenPicture() {
        let p = present(.connected, #"{"drivable":true}"#) {
            $0.linkDownSince = self.t0.addingTimeInterval(-12)
            $0.linkProblem = .unreachable(.timedOut)
        }
        XCTAssertEqual(p.placement, .cover)
        XCTAssertEqual(p.elapsed, 12)
        XCTAssertTrue(p.progress)
    }

    func testRunnerStartingShowsElapsedAndGetsHonestWhenSlow() {
        let early = present(.connected, #"{"reconnecting":true,"device_state":"reconnecting"}"#) {
            $0.startingSince = self.t0.addingTimeInterval(-12)
        }
        XCTAssertEqual(early.title, String(localized: "正在启动设备服务…"))
        XCTAssertEqual(early.elapsed, 12)
        XCTAssertTrue(early.progress)
        let slow = present(.connected, #"{"reconnecting":true}"#) { $0.startingSince = self.t0.addingTimeInterval(-75) }
        XCTAssertNotEqual(slow.detail, early.detail)
    }

    func testBlockersNameTheProblemAndUseTheDaemonsWords() {
        let wifi = present(.connected, #"{"reconnecting":true,"setup_blocked_on":"wifi_automation_refused","next_step":{"zh":"插一次线","en":"plug in once"}}"#)
        XCTAssertEqual(wifi.title, String(localized: "需要插一次线"))
        XCTAssertFalse(wifi.detail.isEmpty)
        XCTAssertNil(wifi.primary, "retrying over Wi-Fi does not help")
        let fallback = present(.connected, #"{"setup_blocked_on":"wifi_automation_refused"}"#)
        XCTAssertFalse(fallback.detail.isEmpty, "an older daemon without next_step still gets an explanation")
        let tooOld = present(.connected, #"{"setup_blocked_on":"ios_too_old","device_state":"blocked"}"#)
        XCTAssertEqual(tooOld.title, String(localized: "iPhone 系统太旧"))
        XCTAssertNil(tooOld.primary)
        let runner = present(.connected, #"{"setup_blocked_on":"wda","device_state":"blocked","recovery_owner":"daemon"}"#)
        XCTAssertEqual(runner.primary, .wakePhone)
    }

    func testIdleReleasedOffersWakeAndShowsProgressWhileWaking() {
        let idle = present(.connected, #"{"released":true,"device_state":"released"}"#)
        XCTAssertEqual(idle.primary, .wakePhone)
        let waking = present(.connected, #"{"released":true}"#) { $0.waking = true }
        XCTAssertTrue(waking.progress)
        XCTAssertNil(waking.primary)
    }

    func testOwnedByAnotherSessionLeavesThePictureVisible() {
        let p = present(.connected, #"{"drivable":true,"owner":"agent-7","owner_lease_remaining_secs":40}"#)
        XCTAssertEqual(p.placement, .banner)
        XCTAssertTrue(p.title.contains("agent-7"))
    }

    func testLockedAndHandedBack() {
        XCTAssertEqual(present(.connected, #"{"wda_locked":true,"wda":true}"#).symbol, "lock")
        XCTAssertEqual(present(.connected, #"{"released":true,"human_handoff":true}"#).primary, .wakePhone)
    }

    func testOfflineRunnerOnlyOffersWakeWhenTheDaemonOwnsIt() {
        XCTAssertEqual(present(.connected, #"{"device_state":"offline","recovery_owner":"daemon"}"#).primary, .wakePhone)
        XCTAssertNil(present(.connected, #"{"device_state":"offline","recovery_owner":"external"}"#).primary)
    }

    func testMissingPictureTimesOutIntoAReloadButton() {
        let waiting = present(.connected, #"{"drivable":true}"#, video: false) { $0.videoWaitSince = self.t0.addingTimeInterval(-4) }
        XCTAssertTrue(waiting.progress)
        XCTAssertNil(waiting.primary)
        let stuck = present(.connected, #"{"drivable":true}"#, video: false) { $0.videoWaitSince = self.t0.addingTimeInterval(-20) }
        XCTAssertEqual(stuck.primary, .reloadVideo)
        XCTAssertFalse(stuck.progress, "no endless spinner")
    }

    func testTileAndScreenAgree() {
        // The tile's words and the screen's title come from one value.
        let p = present(.connected, #"{"released":true}"#)
        XCTAssertEqual(p.short, String(localized: "空闲"))
        XCTAssertEqual(p.title, String(localized: "设备空闲中"))
    }
}

final class DevicePersistenceTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        super.setUp()
        suite = "test.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    func testLastFocusedDeviceComesBack() {
        let a = DeviceRecord(address: "http://a:44321"), b = DeviceRecord(address: "http://b:44321")
        XCTAssertNil(DeviceStore.loadFocused(from: defaults, in: [a, b]))
        DeviceStore.saveFocused(b.id, to: defaults)
        XCTAssertEqual(DeviceStore.loadFocused(from: defaults, in: [a, b]), b.id)
        // A forgotten device is not restored.
        XCTAssertNil(DeviceStore.loadFocused(from: defaults, in: [a, DeviceRecord(address: "http://c:44321")]))
        // One device: always straight to it.
        XCTAssertEqual(DeviceStore.loadFocused(from: defaults, in: [a]), a.id)
        DeviceStore.saveFocused(nil, to: defaults)
        XCTAssertNil(DeviceStore.loadFocused(from: defaults, in: [a, b]))
    }

    func testEditingToAnAddressAlreadySavedIsCaught() {
        let a = DeviceRecord(address: "http://192.168.0.13:44321", name: "Mac A")
        let b = DeviceRecord(address: "http://192.168.0.190:44321")
        XCTAssertEqual(DeviceStore.conflict("HTTP://192.168.0.13:44321/", in: [a, b], except: b.id)?.name, "Mac A")
        XCTAssertNil(DeviceStore.conflict("http://192.168.0.13:44321", in: [a, b], except: a.id))
    }
}
