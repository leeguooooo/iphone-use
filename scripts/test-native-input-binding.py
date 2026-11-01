#!/usr/bin/env python3
"""Execute production binding/clear logic with synthetic AX and dispatch callbacks."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'


def block(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        if source[end] == '{': depth += 1
        elif source[end] == '}': depth -= 1
        end += 1
    return source[start:end]


routes = (SOURCE / 'RunnerRoutes.swift').read_text()
methods = '\n'.join(block(routes, '  private func ' + name + '(').replace('private func', 'func', 1)
                    for name in ['inputTarget', 'checkInputTarget', 'checkedInputTarget',
                                 'boundKeys', 'boundClearState', 'clearBoundInput'])
helpers = block((SOURCE / 'RunnerElements.swift').read_text(), 'enum InputTargetCheck')
error = block((SOURCE / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
program = r'''
import Foundation
final class UINode {
  var type = "XCUIElementTypeTextField"
  var isFocused = true
  var isEnabled: Bool? = true
  var pid: Int32 = 42
  var axElement: AnyObject? = NSObject()
  var treeComplete = true
  var nodes: [UINode] = []
  func descendants() -> [UINode] { nodes }
}
struct HTTPRequest { var query: [String:String] = [:] }
struct HTTPResponse {
  var value: Any; var headers: [String:String]
  static func value(_ value: Any, headers: [String:String] = [:]) -> HTTPResponse {
    HTTPResponse(value: value, headers: headers)
  }
}
final class Registry {
  var entries: [String:UINode] = [:]
  func register(_ node: UINode) -> String { let id = UUID().uuidString; entries[id] = node; return id }
  func prune() {}
}
enum IPURBridge { static func invalidateRequestCache() {} }
'''
program += error + '\n' + helpers
program += r'''
final class RunnerTests {
  let elements = Registry()
  let node = UINode()
  let root = UINode()
  var inputBinding: String?
  var foregroundPID: Int32 = 42
  var fullValue: [String:Any] = ["value":"old", "complete":true]
  var sends = 0
  var afterSend: (() -> Void)?
  init() { root.nodes = [node] }
  func foregroundTree() throws -> UINode { root }
  func foreground() -> (pid:Int32, unused:Bool) { (foregroundPID,true) }
  func freshNode(_ id:String) throws -> UINode {
    guard let node = elements.entries[id] else { throw RunnerError.failed("stale") }; return node
  }
  func fullElementValue(_ id:String, limit:Int) throws -> [String:Any] { fullValue }
  func typeIntoFocus(_ text:String, frequency:UInt, targetPID:Int32?) throws -> String {
    precondition(targetPID == 42); sends += 1
    if text.contains("delete") { fullValue = ["value":"", "complete":true] }
    afterSend?(); return "synthetic"
  }
}
enum XCUIKeyboardKey {
  struct Key { var rawValue: String }
  static let delete = Key(rawValue:"delete")
}
extension RunnerTests {
'''
program += methods + '\n}\n'
program += r'''
var checks = 0
func check(_ value:Bool,_ message:String) { checks += 1; if !value { print("FAIL: \(message)");exit(1) } }
func token(_ r:RunnerTests) throws -> String {
  let response = try r.inputTarget(HTTPRequest())
  return (response.value as! [String:String])["token"]!
}
func refused(_ run:() throws -> Void, status:Int,_ message:String) {
  do {try run();check(false,message)}
  catch let e as RunnerError {check(e.status == status,message)}
  catch {check(false,"unexpected error: \(error)")}
}
let r = RunnerTests();let t = try token(r)
_ = try r.boundKeys("once",token:t,frequency:60)
check(r.sends == 1,"one bound dispatch")
for reason in 0..<8 {
  let r = RunnerTests();let t = try token(r)
  switch reason {
    case 0:r.node.isFocused = false
    case 1:r.node.isEnabled = false
    case 2:r.node.isEnabled = nil
    case 3:r.foregroundPID = 99
    case 4:r.node.axElement = nil
    case 5:r.inputBinding = nil
    case 6:r.node.type = "XCUIElementTypeSecureTextField"
    default:r.elements.entries.removeAll()
  }
  refused({_ = try r.boundKeys("stop",token:t,frequency:60)},status:400,"preflight refusal \(reason)")
  check(r.sends == 0,"preflight sends nothing \(reason)")
}
for reason in 0..<3 {
  let r = RunnerTests()
  if reason == 0 {r.root.treeComplete = false}
  if reason == 1 {r.root.nodes = [r.node,UINode()]}
  if reason == 2 {r.root.nodes = []}
  refused({_ = try token(r)},status:400,"unproven unique target refused")
  check(r.inputBinding == nil,"failed bind retains no old target")
}
do {
  let r = RunnerTests();let t = try token(r);r.afterSend = {r.node.isFocused = false}
  refused({_ = try r.boundKeys("once",token:t,frequency:60)},status:500,"postflight outcome unknown")
  check(r.sends == 1 && r.inputBinding == nil,"postflight never replays or retains binding")
}
do {
  let r = RunnerTests();let t = try token(r)
  _ = try r.clearBoundInput(t)
  check(r.sends == 1,"clear dispatches once and verifies full empty value")
}
for full in [[String:Any](),["value":"", "complete":false],["complete":true]] {
  let r = RunnerTests();let t = try token(r);r.fullValue = full
  refused({_ = try r.clearBoundInput(t)},status:400,"unknown clear value refused")
  check(r.sends == 0,"unproven value never authorizes deletion")
}
do {
  let r = RunnerTests();let t = try token(r);r.afterSend = {r.node.isFocused = false}
  refused({_ = try r.clearBoundInput(t)},status:500,"clear postflight is unknown")
  check(r.sends == 1 && r.inputBinding == nil,"clear never refocuses or replays")
}
for path in ["/session/s/wda/keys","/session/s/element/id/clear"] {
  check(InputTargetCheck.preservesBinding(path:path,binding:"id",body:["inputTarget":"id"]),"bound route preserves token")
}
for path in ["/tap","/home","/session/s/wda/keys","/session/s/element/other/clear"] {
  check(!InputTargetCheck.preservesBinding(path:path,binding:"id",body:["inputTarget":"other"]),"another mutation invalidates token")
}
print("Native input binding: \(checks) production-logic checks passed; synthetic services only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-input-binding-') as folder:
    path = Path(folder)
    (path / 'main.swift').write_text(program)
    subprocess.run(['xcrun', 'swiftc', str(path / 'main.swift'), '-o', str(path / 'probe')], check=True)
    subprocess.run([str(path / 'probe')], check=True)
