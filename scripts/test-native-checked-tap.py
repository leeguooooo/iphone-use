#!/usr/bin/env python3
"""Compile actual Native tap preconditions; synthetic frames, no UI or phone."""
from pathlib import Path
import importlib.util
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
# Read extraction code without running the delivery test's main program.
source = (ROOT / 'scripts/test-native-click-delivery.py').read_text()
helpers = source[source.index('def block('):source.index('\n\nroute =')]
namespace = {}
exec(helpers, namespace)
block = namespace['block']
base = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'
program = 'import Foundation\nimport CoreGraphics\nimport CoreFoundation\n'
program += block((base / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
program += '\n' + block((base / 'RunnerActions.swift').read_text(), 'enum TapBounds {')
program += '\n' + block((base / 'RunnerActions.swift').read_text(), 'struct CheckedElementTap {')
program += r'''
let live = CGRect(x: 16, y: 180, width: 358, height: 36)
let screen = CGSize(width: 390, height: 844)
var checks = 0
func require(_ value: Bool, _ message: String) {
  checks += 1
  if !value { print(message); exit(1) }
}
let ordinary: [String:Any] = ["expected_rect":[16,180,358,36]]
let plan = try CheckedElementTap(ordinary)
require(try plan.point(live:live,screen:screen) == CGPoint(x:195,y:198), "same-frame centre changed")
let explicit = try CheckedElementTap(["expected_rect":[16,180,358,36],"point":[195,190]])
require(try explicit.point(live:live,screen:screen) == CGPoint(x:195,y:190), "clear-part point changed")
for (rect, size) in [(CGRect(x:16,y:728,width:358,height:36),screen),(live,CGSize.zero)] {
  do { _ = try plan.point(live:rect,screen:size); require(false,"changed pose/viewport passed") }
  catch let error as RunnerError { require(error.status == 409 && error.code == "target changed", "wrong safe refusal") }
}
for body: [String:Any] in [
  ["expected_rect":[true,180,358,36]], ["expected_rect":["16",180,358,36]],
  ["expected_rect":[16,180,0,36]], ["expected_rect":NSNull()],
  ["expected_rect":[16,180,358,36],"point":NSNull()],
  ["expected_rect":[16,180,358,36],"unknown":1]
] {
  do { _ = try CheckedElementTap(body); require(false,"invalid checked request accepted") }
  catch let error as RunnerError { require(error.status == 400, "wrong request refusal") }
}
let outside = try CheckedElementTap(["expected_rect":[16,180,358,36],"point":[2,2]])
do { _ = try outside.point(live:live,screen:screen); require(false,"outside point accepted") }
catch let error as RunnerError { require(error.status == 409, "wrong outside-point refusal") }
for point in [[374,198],[195,216]] {
  let edge = try CheckedElementTap(["expected_rect":[16,180,358,36],"point":point])
  do { _ = try edge.point(live:live,screen:screen); require(false,"exclusive target edge accepted") }
  catch let error as RunnerError { require(error.status == 409,"wrong target edge refusal") }
}
let screenEdge = try CheckedElementTap(["expected_rect":[370,100,40,40],"point":[390,120]])
do { _ = try screenEdge.point(live:CGRect(x:370,y:100,width:40,height:40),screen:screen); require(false,"exclusive screen edge accepted") }
catch let error as RunnerError { require(error.status == 409,"wrong screen edge refusal") }
print("Native checked tap: \(checks) assertions passed; no input delivered")
'''
with tempfile.TemporaryDirectory(prefix='ipu-checked-tap-') as folder:
    out = Path(folder)
    (out / 'probe.swift').write_text(program)
    subprocess.run(['xcrun', 'swiftc', str(out / 'probe.swift'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)
