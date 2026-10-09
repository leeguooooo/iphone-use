import XCTest
@testable import IPhoneUseRemote

/// The list, tiles and status pill name the phone, not the Mac's address.
final class DeviceIdentityTests: XCTestCase {
    private let iPhoneX = PhoneIdentity(name: "Leo's iPhone", model: "iPhone X", productType: "iPhone10,3", ios: "16.5")

    func testStatusCarriesTheDevice() throws {
        let json = #"{"device_state":"ready","device":{"name":"Leo's iPhone","model":"iPhone X","product_type":"iPhone10,3","ios":"16.5"}}"#
        let status = try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
        XCTAssertEqual(status.device, iPhoneX)
        XCTAssertEqual(status.device?.label, "Leo's iPhone · iPhone X")
    }

    func testOlderDaemonsAndEmptyDevicesDecodeToNil() throws {
        for json in [#"{"device_state":"ready"}"#, #"{"device":null}"#, #"{"device":{}}"#, #"{"device":"x"}"#] {
            let status = try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
            XCTAssertNil(status.device, json)
        }
    }

    func testLabelUsesWhatIsKnown() {
        XCTAssertEqual(PhoneIdentity(name: "iPhone X", model: "iPhone X").label, "iPhone X")
        XCTAssertEqual(PhoneIdentity(name: "Work", model: nil).label, "Work")
        XCTAssertEqual(PhoneIdentity(name: " ", model: "iPhone 13").label, "iPhone 13")
        XCTAssertEqual(PhoneIdentity(name: nil, model: nil, productType: "iPhone99,1").label, "iPhone99,1")
        XCTAssertNil(PhoneIdentity().label)
    }

    func testDisplayNamePrefersCustomThenPhoneThenAddress() {
        var record = DeviceRecord(address: "http://192.168.0.190:45561")
        XCTAssertNil(record.customName)
        XCTAssertEqual(record.displayName, "192.168.0.190:45561")
        XCTAssertEqual(record.secondaryText, "192.168.0.190:45561")

        record.phone = iPhoneX
        XCTAssertEqual(record.displayName, "Leo's iPhone · iPhone X")
        XCTAssertEqual(record.secondaryText, "192.168.0.190:45561")
        XCTAssertEqual(record.defaultDisplayName, "Leo's iPhone · iPhone X")

        record.name = "测试机 X"
        XCTAssertEqual(record.customName, "测试机 X")
        XCTAssertEqual(record.displayName, "测试机 X")
        XCTAssertEqual(record.secondaryText, "Leo's iPhone · iPhone X · 192.168.0.190:45561")

        // The old default (the address) is not a custom name.
        record.name = "192.168.0.190:45561"
        XCTAssertNil(record.customName)
        XCTAssertEqual(record.displayName, "Leo's iPhone · iPhone X")
    }

    func testPhoneIsSavedWithTheRecordAndOldRecordsStillLoad() throws {
        let record = DeviceRecord(address: "http://10.0.0.2:44321", phone: iPhoneX)
        let data = try JSONEncoder().encode([record])
        XCTAssertEqual(try JSONDecoder().decode([DeviceRecord].self, from: data), [record])

        let old = #"[{"id":"7C9E6679-7425-40DE-944B-E07FC1F90AE7","address":"http://10.0.0.2:44321","name":"10.0.0.2:44321"}]"#
        let decoded = try JSONDecoder().decode([DeviceRecord].self, from: Data(old.utf8))
        XCTAssertNil(decoded.first?.phone)
        XCTAssertEqual(decoded.first?.displayName, "10.0.0.2:44321")
    }

    @MainActor
    func testStatusUpdatesTheSavedPhone() throws {
        let suite = "test.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let record = DeviceRecord(address: "http://10.0.0.3:44321")
        DeviceStore.save([record], to: defaults)
        let session = DeviceSession(record: record)
        var noted: [PhoneIdentity] = []
        session.onIdentity = { _, phone in noted.append(phone) }
        let json = #"{"device":{"name":"Leo's iPhone","model":"iPhone X","product_type":"iPhone10,3","ios":"16.5"}}"#
        session.status = try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
        XCTAssertEqual(session.record.phone, iPhoneX)
        XCTAssertEqual(session.name, "Leo's iPhone · iPhone X")
        XCTAssertEqual(session.secondaryName, "10.0.0.3:44321")
        // Unchanged on the next poll: not reported again.
        session.status = try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
        XCTAssertEqual(noted, [iPhoneX])
    }

    func testQualityFallsBackWhenNoFrameCame() {
        let started = Date()
        XCTAssertTrue(DeviceSession.qualityFailed(started: started, lastFrameAt: nil))
        XCTAssertTrue(DeviceSession.qualityFailed(started: started, lastFrameAt: started.addingTimeInterval(-5)))
        XCTAssertFalse(DeviceSession.qualityFailed(started: started, lastFrameAt: started.addingTimeInterval(1)))
    }
}
