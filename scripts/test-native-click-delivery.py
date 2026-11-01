#!/usr/bin/env python3
"""Execute the production Native click control flow with synthetic delivery callbacks.

Compiles the actual Swift method, error type and bounds helper. No XCTest,
Simulator, device, gesture synthesis or signing is used.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCES = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'


def block(source, start):
    begin = source.index(start)
    brace = source.index('{', begin)
    depth = 1
    end = brace + 1
    while depth:
        if source[end] == '{': depth += 1
        elif source[end] == '}': depth -= 1
        end += 1
    return source[begin:end]


route = block((SOURCES / 'RunnerRoutes.swift').read_text(), '  private func clickElement(')
error = block((SOURCES / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
bounds = block((SOURCES / 'RunnerActions.swift').read_text(), 'enum TapBounds {')
harness = r'''
import Foundation
import CoreGraphics
struct HTTPResponse {
  let status: Int
  static func value(_ value: Any) -> HTTPResponse { HTTPResponse(status: 200) }
}
struct Node {
  let rect: CGRect
  var center: CGPoint { CGPoint(x: rect.midX, y: rect.midY) }
}
'''
harness += error + '\n' + bounds + '\n'
harness += r'''
final class RunnerTests {
  let scenario: Int
  var reads = 0
  var scrolls = 0
  var touches = 0
  init(_ scenario: Int) { self.scenario = scenario }
  func freshNode(_ id: String) throws -> Node {
    reads += 1
    if scenario == 0 || (scenario == 1 && reads > 1) { throw RunnerError.staleElement(id) }
    let outside = scenario != 5 && scenario != 6 && (reads == 1 || scenario == 3)
    return Node(rect: CGRect(x: 20, y: outside ? 1500 : 200, width: 80, height: 40))
  }
  func windowSizePoints() -> CGSize { CGSize(width: 390, height: 844) }
  func scrollIntoView(_ id: String) throws { scrolls += 1 }
  func tapPoint(_ point: CGPoint) throws {
    touches += 1
    if scenario == 2 { throw RunnerError.staleElement("callback-after-input") }
    if scenario == 6 { throw RunnerError.failed("synthesis failure after input") }
  }
  func run() -> Int {
    do { return try clickElement("fixture").status }
    catch let error as RunnerError { return error.status }
    catch { return -1 }
  }
'''
harness += route + '\n}\n'
harness += r'''
let expected = [(404,0,0),(500,1,0),(500,1,1),(500,1,0),(200,1,1),(200,0,1),(500,0,1)]
for (scenario, wanted) in expected.enumerated() {
  let runner = RunnerTests(scenario)
  let status = runner.run()
  if status != wanted.0 || runner.scrolls != wanted.1 || runner.touches != wanted.2 {
    print("scenario \(scenario): status=\(status) scrolls=\(runner.scrolls) touches=\(runner.touches); expected=\(wanted)")
    exit(1)
  }
}
print("Native click delivery: 7 scenarios passed; production Swift, synthetic callbacks only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-click-delivery-') as folder:
    out = Path(folder)
    (out / 'probe.swift').write_text(harness)
    subprocess.run(['xcrun', 'swiftc', str(out / 'probe.swift'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)
