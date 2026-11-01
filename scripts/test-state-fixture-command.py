#!/usr/bin/env python3
"""Execute the fixture's real command handler with synthetic responder callbacks."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / 'scripts/fixtures/state-availability/main.swift').read_text()
start = source.index('  func consumeCommand()')
end = source.index('  func save()', start)
method = source[start:end]
program = r'''
import Foundation
struct Selection {
  var start: Int; var end: Int
  var isEmpty: Bool { start == end }
}
final class Field {
  let beginningOfDocument = 0
  var selectedTextRange: Selection?
  func position(from:Int,offset:Int) -> Int? { (0...500).contains(offset) ? offset : nil }
  func textRange(from:Int,to:Int) -> Selection? { Selection(start:from,end:to) }
  func offset(from:Int,to:Int) -> Int { to-from }
  var isFirstResponder = false
  var accept = true
  var calls = 0
  func becomeFirstResponder() -> Bool { calls += 1; if accept {isFirstResponder = true}; return accept }
  func resignFirstResponder() -> Bool { calls += 1; if accept {isFirstResponder = false}; return accept }
}
final class StateController {
  let runID = "owned"
  let documents: URL
  var sequence = 0
  var focusReceipt: [String:Any]?
  let field = Field()
  init(_ folder:URL) { documents = folder }
  func save() {}
'''
program += method + '\n}\n'
program += r'''
var checks = 0
func check(_ value:Bool,_ message:String) { checks += 1; if !value {print("FAIL: \(message)");exit(1)} }
let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
try FileManager.default.createDirectory(at:folder,withIntermediateDirectories:true)
defer {try? FileManager.default.removeItem(at:folder)}
func command(_ sequence:Int,_ wanted:Bool,_ runID:String = "owned",_ pid:Int = Int(ProcessInfo.processInfo.processIdentifier),caret:Int? = nil) throws {
  var value:[String:Any] = ["sequence":sequence,"focused":wanted,"run_id":runID,"pid":pid]
  if let caret { value["caret_utf16"] = caret }
  try JSONSerialization.data(withJSONObject:value).write(to:folder.appendingPathComponent("state-command.json"),options:.atomic)
}
for wanted in [false,true] {
  for accepted in [false,true] {
    let c = StateController(folder);c.field.isFirstResponder = !wanted;c.field.accept = accepted
    try command(1,wanted);c.consumeCommand()
    check(c.sequence == 1 && c.field.calls == 1,"one responder invocation")
    check(c.focusReceipt?["sequence"] as? Int == 1 && c.focusReceipt?["requested"] as? Bool == wanted,"bind receipt to the one command")
    check(c.focusReceipt?["acknowledged"] as? Bool == accepted,"record actual API acknowledgement")
    check(c.focusReceipt?["before"] as? Bool == !wanted && c.focusReceipt?["after"] as? Bool == (accepted ? wanted : !wanted),"retain actual before and after")
    c.consumeCommand()
    check(c.field.calls == 1,"no replay even when responder refuses")
    try command(2,wanted,"foreign");c.consumeCommand()
    try command(2,wanted,"owned",0);c.consumeCommand()
    check(c.sequence == 1 && c.field.calls == 1,"foreign identity cannot invoke responder")
  }
}
do {
  let c = StateController(folder);c.field.isFirstResponder = true
  try command(1,true,caret:250);c.consumeCommand()
  check(c.field.calls == 0,"caret placement never refocuses")
  check(c.focusReceipt?["acknowledged"] as? Bool == true && c.field.selectedTextRange?.start == 250,"requested caret confirmed from selection")
  try command(2,true,caret:501);c.consumeCommand()
  check(c.focusReceipt?["acknowledged"] as? Bool == false && c.field.selectedTextRange?.start == 250,"out-of-range caret changes no selection")
  c.consumeCommand();check(c.sequence == 2 && c.field.calls == 0,"failed caret command is consumed once")
}
print("Fixture focus receipt: \(checks) production-handler checks passed; synthetic responders only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-state-receipt-') as folder:
    path = Path(folder)
    (path / 'main.swift').write_text(program)
    subprocess.run(['xcrun', 'swiftc', str(path / 'main.swift'), '-o', str(path / 'probe')], check=True)
    subprocess.run([str(path / 'probe')], check=True)
