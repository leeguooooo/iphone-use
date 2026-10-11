import XCTest
@testable import IPhoneUseRemote

final class AgentProgressTests: XCTestCase {
    func testThinkingDoesNotInventAnAction() {
        let task = AgentTaskSnapshot(id: "t", goal: "看电量", state: .running, phase: "assist")
        let progress = AgentProgress(task: task)
        XCTAssertEqual(progress.next, String(localized: "选择下一步"))
        XCTAssertFalse(progress.terminal)
    }

    func testSelectedActionShowsTheRealTargetWithoutTypedContent() throws {
        let task = try JSONDecoder().decode(AgentTaskSnapshot.self, from: Data(#"{"id":"t","status":"running","phase":"acting","planned_action":{"action":"TYPE_TEXT","target":"搜索","text":"private-value"}}"#.utf8))
        let progress = AgentProgress(task: task)
        XCTAssertTrue(progress.current.contains("搜索"))
        XCTAssertFalse(progress.current.contains("private-value"))
        XCTAssertEqual(progress.next, String(localized: "读取新页面"))
    }

    func testConfirmationAndBlockedResultAreNotDisplayedAsSuccess() {
        let waiting = AgentTaskSnapshot(id: "t", goal: "测试确认", state: .needsConfirm,
                                       confirm: AgentConfirm(action: "CLICK", target: "确认"))
        XCTAssertTrue(AgentProgress(task: waiting).next.hasPrefix(String(localized: "批准后：")))
        let blocked = AgentProgress(task: AgentTaskSnapshot(id: "t", goal: "看电量", state: .blocked))
        XCTAssertTrue(blocked.terminal)
        XCTAssertFalse(blocked.succeeded)
    }
}
