import XCTest
@testable import IPhoneUseRemote

final class CronSpecTests: XCTestCase {
    func testEveryDay() {
        XCTAssertEqual(CronSpec(hour: 7, minute: 0).cron, "0 7 * * *")
        // All seven days is every day too.
        XCTAssertEqual(CronSpec(hour: 21, minute: 30, weekdays: Set(0...6)).cron, "30 21 * * *")
    }

    func testChosenWeekdaysAreSortedAndListed() {
        XCTAssertEqual(CronSpec(hour: 8, minute: 5, weekdays: [5, 1, 3]).cron, "5 8 * * 1,3,5")
        XCTAssertEqual(CronSpec(hour: 9, minute: 0, weekdays: [0, 6]).cron, "0 9 * * 0,6")
    }

    func testOutOfRangeIsClamped() {
        XCTAssertEqual(CronSpec(hour: 25, minute: -3, weekdays: [9, 2]).cron, "0 23 * * 2")
    }

    func testReadsBackWhatItWrites() {
        for spec in [CronSpec(hour: 7, minute: 0), CronSpec(hour: 23, minute: 59, weekdays: [1, 2, 3, 4, 5]),
                     CronSpec(hour: 0, minute: 15, weekdays: [0])] {
            XCTAssertEqual(CronSpec(cron: spec.cron), spec)
        }
        // Ranges and 7 for Sunday, as people write them by hand.
        XCTAssertEqual(CronSpec(cron: "0 7 * * 1-5")?.weekdays, [1, 2, 3, 4, 5])
        XCTAssertEqual(CronSpec(cron: "0 7 * * 7")?.weekdays, [0])
    }

    func testAnythingElseIsShownAsWritten() {
        XCTAssertNil(CronSpec(cron: "*/5 * * * *"))
        XCTAssertNil(CronSpec(cron: "0 7 1 * *"))
        XCTAssertNil(CronSpec(cron: "0 7 * *"))
        XCTAssertNil(CronSpec(cron: "0 7 * * 5-1"))
    }

    func testSummary() {
        XCTAssertEqual(CronSpec(hour: 7, minute: 0).summary, String(localized: "每天 \("07:00")"))
        XCTAssertEqual(CronSpec(hour: 7, minute: 0, weekdays: [1, 2, 3, 4, 5]).summary,
                       String(localized: "工作日 \("07:00")"))
        XCTAssertTrue(CronSpec(hour: 18, minute: 5, weekdays: [0, 1]).summary.hasSuffix("18:05"))
    }
}

final class FlowIDTests: XCTestCase {
    func testAppPartIsTheBundleIdsLastComponent() {
        XCTAssertEqual(FlowID.appPart(bundle: "com.apple.Preferences"), "preferences")
        XCTAssertEqual(FlowID.appPart(bundle: "com.xingin.discover"), "discover")
        XCTAssertEqual(FlowID.appPart(bundle: "com.tencent.xin"), "xin")
        XCTAssertEqual(FlowID.appPart(bundle: nil, label: "Health"), "health")
        XCTAssertEqual(FlowID.appPart(bundle: nil, label: "健康"), "app")
    }

    func testNameBecomesASlug() {
        XCTAssertEqual(FlowID.make(bundle: "com.apple.Preferences", name: "Battery Check"), "preferences/battery-check")
        XCTAssertEqual(FlowID.make(bundle: "com.apple.Health", name: "  Steps — today!! "), "health/steps-today")
        XCTAssertEqual(FlowID.make(bundle: "com.apple.Health", name: "查 steps 今天"), "health/steps")
    }

    func testANameWithoutLatinLettersGetsADatedName() {
        let date = DateComponents(calendar: Calendar(identifier: .gregorian), timeZone: .current,
                                  year: 2026, month: 10, day: 11, hour: 7, minute: 5).date!
        XCTAssertEqual(FlowID.make(bundle: "com.apple.Preferences", name: "查电量", now: date),
                       "preferences/flow-20261011-0705")
    }

    func testMadeIdsAreValid() {
        for name in ["Battery Check", "查电量", "a_b-c", "X"] {
            XCTAssertTrue(FlowID.isValid(FlowID.make(bundle: "com.Foo.Bar-App", name: name)), name)
        }
    }

    func testValidation() {
        XCTAssertTrue(FlowID.isValid("myapp/daily-check"))
        XCTAssertFalse(FlowID.isValid("MyApp/daily"))
        XCTAssertFalse(FlowID.isValid("myapp"))
        XCTAssertFalse(FlowID.isValid("a/b/c"))
        XCTAssertFalse(FlowID.isValid("myapp/-x"))
        XCTAssertFalse(FlowID.isValid("/x"))
    }
}

final class FlowDecodingTests: XCTestCase {
    private func object(_ json: String) -> Any? {
        try? JSONSerialization.jsonObject(with: Data(json.utf8))
    }

    func testStoppedRecordingDraft() {
        let draft = FlowDraft(json: object("""
        {"ok": true, "recording": false, "draft": {"source": "current",
         "app": {"bundle": "com.apple.Preferences", "label": "设置"}, "recorded_steps": 3,
         "flow": {"version": 1, "name": "设置 task", "app": "com.apple.Preferences",
                  "steps": [{"kind": "launch_app"}, {"kind": "wait_for"}, {"kind": "tap_locator"}]},
         "todo": ["Give it a real name"]}}
        """))
        XCTAssertEqual(draft?.bundle, "com.apple.Preferences")
        XCTAssertEqual(draft?.label, "设置")
        XCTAssertEqual(draft?.steps, 3)
        XCTAssertEqual(draft?.todo, ["Give it a real name"])
        let flow = draft?.flow.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }
        XCTAssertEqual(flow?["version"] as? Int, 1)
        XCTAssertNil(FlowDraft(json: object(#"{"ok": true, "recording": false, "draft": null}"#)))
    }

    func testFlowList() throws {
        struct List: Decodable { var flows: [FlowEntry] }
        let list = try JSONDecoder().decode(List.self, from: Data("""
        {"ok": true, "flows": [
          {"id": "health/steps", "name": "Steps", "steps": 6, "inputs": [], "app": "com.apple.Health",
           "source": "official", "verified": true, "risk": "read_only", "sha256": "x", "compat": {}},
          {"id": "wechat/send", "name": "Send", "steps": 4, "inputs": ["to", "text"], "source": "local",
           "risk": "side_effect", "example_inputs": {"to": "Leo"}}]}
        """.utf8))
        XCTAssertEqual(list.flows.map(\.id), ["health/steps", "wechat/send"])
        XCTAssertFalse(list.flows[0].isLocal)
        XCTAssertTrue(list.flows[1].isLocal)
        XCTAssertTrue(list.flows[1].hasSideEffects)
        XCTAssertEqual(list.flows[1].exampleInputs["to"], "Leo")
    }

    func testSchedulesAndRuns() throws {
        struct List: Decodable { var schedules: [FlowSchedule] }
        let list = try JSONDecoder().decode(List.self, from: Data("""
        {"ok": true, "schedules": [{"id": "s1", "cron": "0 7 * * *", "kind": "flow", "target": "health/steps",
          "inputs": {"day": "today"}, "confirm_side_effects": false, "enabled": true, "window_mins": 30,
          "created_at": 1760000000, "next_run_at": 1760100000,
          "last_run": {"id": "r1", "schedule_id": "s1", "scheduled_for": 1760000000, "deadline": 1760001800,
                       "state": "failed", "manual": false, "attempts": 1, "error": "phone locked"}}],
         "mcp_available": true}
        """.utf8))
        let schedule = try XCTUnwrap(list.schedules.first)
        XCTAssertEqual(schedule.inputs, ["day": "today"])
        XCTAssertEqual(schedule.nextRunAt, Date(timeIntervalSince1970: 1_760_100_000))
        XCTAssertEqual(schedule.lastRun?.state, "failed")
        XCTAssertTrue(schedule.lastRun?.failed ?? false)
        XCTAssertEqual(schedule.lastRun?.error, "phone locked")
    }

    func testNewScheduleBody() throws {
        let body = NewSchedule(cron: "0 7 * * 1,3", target: "health/steps", inputs: [:], confirmSideEffects: false,
                               name: nil)
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(body)) as? [String: Any]
        XCTAssertEqual(json?["cron"] as? String, "0 7 * * 1,3")
        XCTAssertEqual(json?["kind"] as? String, "flow")
        XCTAssertEqual(json?["confirm_side_effects"] as? Bool, false)
        XCTAssertNil(json?["name"])
    }

    func testFlowRunResult() {
        let ok = FlowRunResult(json: object(#"{"ok": true, "result": {"ok": true, "outputs": {"steps": "8017", "km": 3.2}}}"#))
        XCTAssertTrue(ok.ok)
        XCTAssertEqual(ok.outputs.map(\.key), ["km", "steps"])
        XCTAssertEqual(ok.outputs.map(\.value), ["3.2", "8017"])
        let failed = FlowRunResult(json: object(#"{"ok": false, "result": {"ok": false, "error": "expectation_timeout", "failed_step": 3}}"#))
        XCTAssertFalse(failed.ok)
        XCTAssertEqual(failed.error, "expectation_timeout")
        XCTAssertEqual(failed.failedStep, 3)
    }
}
