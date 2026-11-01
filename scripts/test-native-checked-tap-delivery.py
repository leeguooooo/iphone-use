#!/usr/bin/env python3
"""Exercise the production checked route with synthetic callbacks; no input device."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / 'scripts/test-native-click-delivery.py').read_text()
namespace = {}
exec(source[source.index('def block('):source.index('\n\nroute =')], namespace)
block = namespace['block']
base = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'
program = 'import Foundation\nimport CoreGraphics\nimport CoreFoundation\n'
program += block((base / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
program += '\n' + block((base / 'RunnerActions.swift').read_text(), 'struct CheckedElementTap {')
program += r'''
struct HTTPRequest {
  func jsonObject() throws -> [String:Any] {
    ["expected_rect":[16,180,358,36],"expected_target":["identifier":"race.target","kind":"Button"]]
  }
}
struct HTTPResponse {
  var status: Int
  var headers: [String:String] = [:]
  static func from(_ error: Error) -> HTTPResponse { HTTPResponse(status:(error as? RunnerError)?.status ?? 500) }
  static func value(_ value: Any) -> HTTPResponse { HTTPResponse(status:200) }
}
struct Node {
  let attributes: [String:Any]
  let axElement: NSObject?
  let rect = CGRect(x:16,y:180,width:358,height:36)
}
final class RouteProbe {
  let scenario: Int
  var touches = 0
  init(_ scenario: Int) { self.scenario = scenario }
  func windowSizePoints() -> CGSize { CGSize(width:390,height:844) }
  func freshNode(_ id: String) throws -> Node {
    if scenario == 3 { throw RunnerError.staleElement(id) }
    return Node(attributes:["rawIdentifier":scenario == 1 ? "race.replaced" : "race.target",
      "type":"XCUIElementTypeButton"], axElement:scenario == 4 ? nil : NSObject())
  }
  func tapPoint(_ point: CGPoint) throws {
    touches += 1
    if scenario == 2 { throw RunnerError.failed("unknown after synthesis") }
  }
  func run() -> HTTPResponse {
    do { return try checkedTapElement("target",HTTPRequest()) }
    catch { return HTTPResponse.from(error) }
  }
'''
program += block((base / 'RunnerRoutes.swift').read_text(), '  private func checkedTapElement(')
program += r'''
}
for (scenario, expected) in [(200,1,false),(409,0,true),(500,1,false),(404,0,true),(409,0,true)].enumerated() {
  let probe = RouteProbe(scenario)
  let result = probe.run()
  guard result.status == expected.0, probe.touches == expected.1,
        (result.headers["X-IPU-Tap-Phase"] == "not_sent") == expected.2 else {
    print("checked route scenario \(scenario): status=\(result.status), touches=\(probe.touches), headers=\(result.headers)")
    exit(1)
  }
}
print("Native checked delivery: 5 scenarios passed; synthetic callbacks only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-checked-delivery-') as folder:
    out = Path(folder)
    (out / 'probe.swift').write_text(program)
    subprocess.run(['xcrun', 'swiftc', str(out / 'probe.swift'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)
