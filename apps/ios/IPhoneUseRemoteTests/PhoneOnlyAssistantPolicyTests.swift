import XCTest
@testable import IPhoneUseRemote

final class PhoneOnlyAssistantPolicyTests: XCTestCase {
    func testPlanCannotRequestDesktopEvenWithImportedMetadata() throws {
        let source = "/* @meta {\"domain\":\"example.com\",\"readOnly\":true,\"args\":{}} */\nasync function(args) { return {}; }"
        let adapter = try XCTUnwrap(PhoneSiteCatalogDownloader.adapter(Data(source.utf8), spec: "fixture/read", origin: "synthetic"))
        let plan = AssistantPlan(phases: [.init(tool: .desktop, goal: "读取测试网页", siteSpec: "fixture/read")])
        XCTAssertThrowsError(try plan.validate(catalog: [adapter]))
        XCTAssertNoThrow(try AssistantPlan(phases: [.init(tool: .site, goal: "读取测试网页", siteSpec: "fixture/read")]).validate(catalog: [adapter]))
    }
}
