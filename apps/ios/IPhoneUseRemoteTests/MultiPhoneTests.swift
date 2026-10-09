import XCTest
@testable import IPhoneUseRemote

final class DeviceStoreTests: XCTestCase {
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

    func testLegacySinglePairingMigratesWithoutRepair() {
        defaults.set("http://192.168.0.190:44321", forKey: DeviceStore.legacyAddressKey)
        let list = DeviceStore.load(from: defaults) { $0 == "http://192.168.0.190:44321" }
        XCTAssertEqual(list.count, 1)
        // Same address, so the Keychain items keyed by it keep working.
        XCTAssertEqual(list[0].address, "http://192.168.0.190:44321")
        XCTAssertEqual(list[0].name, "192.168.0.190:44321")
        XCTAssertTrue(list[0].showInGrid)
        // Saved: a second launch reads the same record (same id).
        let again = DeviceStore.load(from: defaults) { _ in true }
        XCTAssertEqual(again, list)
    }

    func testLegacyAddressWithoutCredentialsIsNotMigrated() {
        defaults.set("http://10.0.0.2:44321", forKey: DeviceStore.legacyAddressKey)
        XCTAssertEqual(DeviceStore.load(from: defaults) { _ in false }, [])
        // Migration runs once: credentials appearing later do not resurrect it.
        XCTAssertEqual(DeviceStore.load(from: defaults) { _ in true }, [])
    }

    func testFreshInstallStartsEmpty() {
        XCTAssertEqual(DeviceStore.load(from: defaults) { _ in true }, [])
    }

    func testSaveAndLoadRoundTrip() {
        let list = [DeviceRecord(address: "http://a:44321", name: "iPhone 13"),
                    DeviceRecord(address: "http://a:44322", showInGrid: false)]
        DeviceStore.save(list, to: defaults)
        XCTAssertEqual(DeviceStore.load(from: defaults) { _ in false }, list)
    }

    func testUpsertKeepsOneEntryPerDaemon() {
        let first = DeviceStore.upsert("http://192.168.0.190:44321", into: [])
        XCTAssertEqual(first.list.count, 1)
        let same = DeviceStore.upsert("HTTP://192.168.0.190:44321/", into: first.list)
        XCTAssertEqual(same.list.count, 1)
        XCTAssertEqual(same.record.id, first.record.id)
        // Another instance on the same Mac is another phone.
        let other = DeviceStore.upsert("http://192.168.0.190:44322", into: same.list)
        XCTAssertEqual(other.list.count, 2)
        XCTAssertNotEqual(other.record.id, first.record.id)
    }

    func testDefaultNameTellsInstancesApart() {
        XCTAssertEqual(DeviceStore.defaultName(for: "http://192.168.0.13:44321"), "192.168.0.13:44321")
        XCTAssertEqual(DeviceStore.defaultName(for: "https://phone.example.com"), "phone.example.com")
    }

    func testRecordDecodesWithoutNewerFields() throws {
        let json = #"[{"id":"7A1D3C4B-0000-4000-8000-000000000001","address":"http://a:1"}]"#
        let list = try JSONDecoder().decode([DeviceRecord].self, from: Data(json.utf8))
        XCTAssertEqual(list[0].name, "a:1")
        XCTAssertTrue(list[0].showInGrid)
    }
}

final class DeliveryOutcomeTests: XCTestCase {
    private func classify(_ status: Int, _ body: String) -> DeliveryOutcome {
        DeliveryOutcome.classify(status: status, body: Data(body.utf8))
    }

    func testApplied() {
        XCTAssertEqual(classify(200, #"{"ok":true}"#), .ok)
    }

    func testOwnedByAnotherSessionIsSkipped() {
        let body = #"{"ok":false,"error":"phone_owned","owner":"agent-7","owner_lease_remaining_secs":120,"outcome":"not_sent"}"#
        XCTAssertEqual(classify(409, body), .owned(by: "agent-7"))
    }

    func testUnknownOutcomeIsNeverNotSent() {
        XCTAssertEqual(classify(504, #"{"ok":false,"error":"outcome_unknown","outcome":"unknown","retry_safe":false}"#),
                       .outcomeUnknown)
        XCTAssertEqual(classify(502, #"{"ok":false,"error":"outcome_unknown","outcome":"unknown"}"#), .outcomeUnknown)
        // A tunnel's own gateway error carries no outcome: it may have landed.
        XCTAssertEqual(classify(524, "<html>timeout</html>"), .outcomeUnknown)
        XCTAssertEqual(classify(502, ""), .outcomeUnknown)
    }

    func testNotSent() {
        XCTAssertEqual(classify(408, #"{"ok":false,"error":"not_sent","outcome":"not_sent","retry_safe":true}"#),
                       .notSent(reason: "not_sent"))
        XCTAssertEqual(classify(502, #"{"ok":false,"error":"wda_pre_dispatch_failed","outcome":"not_sent"}"#),
                       .notSent(reason: "wda_pre_dispatch_failed"))
        XCTAssertEqual(classify(503, #"{"ok":false,"error":"released","reconnecting":false}"#),
                       .notSent(reason: "released"))
        XCTAssertEqual(classify(400, #"{"ok":false,"error":"invalid_control_deadline"}"#),
                       .notSent(reason: "invalid_control_deadline"))
        XCTAssertEqual(classify(401, #"{"ok":false,"error":"unauthorized"}"#), .notSent(reason: "unauthorized"))
    }

    func testOtherClientErrorsFail() {
        XCTAssertEqual(classify(404, "not found"), .failed(reason: "http_404"))
    }

    func testTransportErrors() {
        XCTAssertEqual(DeliveryOutcome.classify(transport: .cannotConnectToHost), .notSent(reason: "unreachable"))
        XCTAssertEqual(DeliveryOutcome.classify(transport: .notConnectedToInternet), .notSent(reason: "unreachable"))
        XCTAssertEqual(DeliveryOutcome.classify(transport: .timedOut), .outcomeUnknown)
        XCTAssertEqual(DeliveryOutcome.classify(transport: .networkConnectionLost), .outcomeUnknown)
    }
}

final class PhoneStatusOwnerTests: XCTestCase {
    private func status(_ json: String) throws -> PhoneStatus {
        try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
    }

    func testOwnerLease() throws {
        XCTAssertTrue(try status(#"{"owner":"agent-7","owner_lease_remaining_secs":200}"#).ownedByOther)
        XCTAssertFalse(try status(#"{"owner":"ios-remote","owner_lease_remaining_secs":200}"#).ownedByOther)
        XCTAssertFalse(try status(#"{"owner":null,"owner_lease_remaining_secs":0}"#).ownedByOther)
        XCTAssertFalse(try status(#"{"owner":"agent-7","owner_lease_remaining_secs":0}"#).ownedByOther)
        XCTAssertFalse(try status("{}").ownedByOther)
    }
}
