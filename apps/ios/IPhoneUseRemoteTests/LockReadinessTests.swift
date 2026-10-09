import XCTest
@testable import IPhoneUseRemote

final class LockReadinessTests: XCTestCase {
    private func status(_ readiness: String?) throws -> PhoneStatus {
        var json = #"{"ok":true,"device_state":"ready","drivable":true"#
        if let readiness { json += #","lock_readiness":"# + readiness }
        json += "}"
        return try JSONDecoder().decode(PhoneStatus.self, from: Data(json.utf8))
    }

    func testPasscodeWithShortAutoLockNeedsAPerson() throws {
        let s = try status(#"""
        {"passcode_protected":true,"auto_lock_secs":30,
         "keep_awake":{"enabled":true,"supported":true,"active":true},
         "verdict":"will_lock_needs_person",
         "hint":{"zh":"这台手机设了锁屏密码，自动锁定 30 秒","en":"This phone has a passcode and Auto-Lock 30 seconds"},
         "checked_at":1791538375}
        """#)
        let readiness = try XCTUnwrap(s.lockReadiness)
        XCTAssertEqual(readiness.verdict, .needsPerson)
        XCTAssertEqual(readiness.passcodeProtected, true)
        XCTAssertEqual(readiness.autoLock, .seconds(30))
        XCTAssertTrue(readiness.keepAwakeActive)
        XCTAssertTrue(readiness.hint.contains("30"))
        XCTAssertEqual(s.lockBadge, .needsPerson)
        XCTAssertTrue(LockBadge.needsPerson.urgent)
    }

    func testNoPasscodeUnlocksByItself() throws {
        let s = try status(#"{"passcode_protected":false,"auto_lock_secs":null,"verdict":"will_lock_auto_unlocks","hint":{"zh":"没有锁屏密码","en":"no passcode"}}"#)
        XCTAssertEqual(s.lockReadiness?.autoLock, nil)
        XCTAssertEqual(s.lockBadge, .autoUnlocks)
        XCTAssertFalse(LockBadge.autoUnlocks.urgent)
    }

    func testAutoLockNeverHasNoBadge() throws {
        let s = try status(#"{"passcode_protected":false,"auto_lock_secs":"never","verdict":"ready","hint":{"zh":"自动锁定为永不","en":"Auto-Lock is Never"}}"#)
        XCTAssertEqual(s.lockReadiness?.verdict, .ready)
        XCTAssertEqual(s.lockReadiness?.autoLock, .never)
        XCTAssertNil(s.lockBadge)
    }

    func testUnknownAndUnrecognizedVerdictsHaveNoBadge() throws {
        XCTAssertNil(try status(#"{"verdict":"unknown"}"#).lockBadge)
        // A verdict from a newer daemon is shown as unknown, not as a wrong badge.
        let future = try status(#"{"verdict":"something_new"}"#)
        XCTAssertEqual(future.lockReadiness?.verdict, .unknown)
        XCTAssertNil(future.lockBadge)
    }

    func testOlderDaemonOrMalformedFieldKeepsTheStatus() throws {
        let old = try status(nil)
        XCTAssertNil(old.lockReadiness)
        XCTAssertNil(old.lockBadge)
        XCTAssertTrue(old.drivable)
        let odd = try status(#""not an object""#)
        XCTAssertNil(odd.lockReadiness)
        XCTAssertTrue(odd.drivable)
    }

    func testBadgeMapping() {
        XCTAssertEqual(LockReadiness(verdict: .needsPerson).badge, .needsPerson)
        XCTAssertEqual(LockReadiness(verdict: .autoUnlocks).badge, .autoUnlocks)
        XCTAssertNil(LockReadiness(verdict: .ready).badge)
        XCTAssertNil(LockReadiness(verdict: .unknown).badge)
        XCTAssertNotEqual(LockBadge.needsPerson.symbol, LockBadge.autoUnlocks.symbol)
        XCTAssertFalse(LockBadge.needsPerson.sentence.isEmpty)
        XCTAssertFalse(LockBadge.autoUnlocks.title.isEmpty)
    }

    func testSettingTexts() {
        XCTAssertNil(LockReadiness(verdict: .unknown).autoLockText)
        XCTAssertNotNil(LockReadiness(verdict: .ready, autoLock: .never).autoLockText)
        let twoMinutes = LockReadiness(verdict: .needsPerson, autoLock: .seconds(120)).autoLockText ?? ""
        XCTAssertTrue(twoMinutes.contains("2"), twoMinutes)
        let thirty = LockReadiness(verdict: .needsPerson, autoLock: .seconds(30)).autoLockText ?? ""
        XCTAssertTrue(thirty.contains("30"), thirty)
        XCTAssertNotEqual(LockReadiness(verdict: .ready, passcodeProtected: true).passcodeText,
                          LockReadiness(verdict: .ready, passcodeProtected: false).passcodeText)
    }
}
