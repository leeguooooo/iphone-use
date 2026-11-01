#!/usr/bin/env python3
"""Execute production text dispatch with synthetic callbacks, without UI or devices."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCES = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'


def block(source, start):
    begin = source.index(start)
    brace = source.index('{', begin)
    depth, end = 1, brace + 1
    while depth:
        if source[end] == '{': depth += 1
        elif source[end] == '}': depth -= 1
        end += 1
    return source[begin:end]


route = block((SOURCES / 'RunnerRoutes.swift').read_text(), '  func typeIntoFocus(')
literal_route = block((SOURCES / 'RunnerTests.swift').read_text(), '  func typeText(')
error = block((SOURCES / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
harness = r'''
import Foundation
var events: [String] = []
var capability = true
var synthesisError: String? = nil
var publicError: String? = nil
var publicIssue: (() -> Void)? = nil
struct HTTPRequest {
  let body: [String: Any]
  func jsonObject() throws -> [String: Any] { body }
}
struct HTTPResponse {
  let headers: [String: String]
  static func value(_ value: Any, headers: [String: String] = [:]) -> HTTPResponse {
    HTTPResponse(headers: headers)
  }
}
enum IPURBridge {
  static func textSynthesisAvailable() -> Bool { events.append("capability"); return capability }
  static func synthesizeText(_ text: String, charactersPerSecond: UInt, pid: Int32) -> String? {
    events.append("synthesis:\(text)")
    return synthesisError
  }
  static func performWithoutQuiescence(_ app: App, body: () -> Void) { body() }
  static func catchException(_ body: () -> Void) -> String? { body(); return publicError }
}
final class App {
  func typeText(_ text: String) { events.append("public:\(text)"); publicIssue?() }
}
'''
harness += error
harness += r'''
final class RunnerTests {
  var recordedIssues: [String] = []
  static func mapWebDriverKeys(_ text: String) -> String {
    text.replacingOccurrences(of: "\u{E003}", with: "mapped-delete")
  }
  func number(_ body: [String: Any], _ key: String) -> Double? { body[key] as? Double }
  func foreground() -> (application: App, pid: Int32) {
    events.append("foreground")
    return (App(), 42)
  }
'''
harness += route + '\n' + literal_route + '\n}\n'
harness += r'''
var checks = 0
func check(_ value: Bool, _ message: String) {
  checks += 1
  if !value { print("FAIL: \(message); events=\(events)"); exit(1) }
}
func reset() {
  events = []; capability = true; synthesisError = nil; publicError = nil; publicIssue = nil
}
// Even strings used for capability absence cannot authorize a replay after dispatch.
for error in ["timeout after prefix", "NSException: connection lost",
  "private XCTest text synthesis unavailable",
  "private XCTest text synthesis failed: could not create text event"] {
  reset(); synthesisError = error
  let runner = RunnerTests()
  do {
    _ = try runner.typeIntoFocus("once")
    check(false, "uncertain synthesis must throw")
  } catch let result as RunnerError {
    check(result.message.contains("unknown"), "failure explicitly preserves uncertainty")
  } catch { check(false, "unexpected error type") }
  check(events.filter { $0.hasPrefix("synthesis:") }.count == 1, "one synthesis attempt")
  check(!events.contains("foreground"), "no new app selection after dispatch")
  check(!events.contains { $0.hasPrefix("public:") }, "no public replay after dispatch")
}
reset()
check(try RunnerTests().typeIntoFocus("once") == "synthesized", "available synthesis succeeds")
check(events == ["capability", "synthesis:once"], "only the selected synthesis path runs")
reset(); capability = false
check(try RunnerTests().typeIntoFocus("once") == "xcui-typetext", "unavailable capability selects public input")
check(events == ["capability", "foreground", "public:once"], "public path selected before any synthesis")
for issue in [false, true] {
  reset(); capability = false
  let runner = RunnerTests()
  if issue { publicIssue = { runner.recordedIssues.append("XCTest issue after input") } }
  else { publicError = "exception after input" }
  do {
    _ = try runner.typeIntoFocus("once")
    check(false, "failed public path must throw")
  } catch let result as RunnerError {
    check(result.message.contains("unknown"), "public failure retains uncertainty")
  } catch { check(false, "unexpected error type") }
  check(events == ["capability", "foreground", "public:once"], "failed public input is not retried")
}
reset()
check(try RunnerTests().typeIntoFocus("") == "none", "empty input is a no-op")
check(events.isEmpty, "empty input makes no calls")
for available in [false, true] {
  reset(); capability = available
  let result = try RunnerTests().typeText(HTTPRequest(body: ["text":"raw\u{E003}"]))
  check(result.headers["X-IPU-Gesture"] == (available ? "synthesized" : "xcui-typetext"), "literal route preserves path receipt")
  check(events.last == (available ? "synthesis:raw\u{E003}" : "public:raw\u{E003}"), "literal route preserves private-use characters")
}
reset()
_ = try RunnerTests().typeIntoFocus("\u{E003}")
check(events.last == "synthesis:mapped-delete", "WebDriver route keeps key mapping")
reset(); synthesisError = "timeout after prefix"
do {
  _ = try RunnerTests().typeText(HTTPRequest(body: ["text":"raw"]))
  check(false, "literal route must stop on uncertain synthesis")
} catch let result as RunnerError {
  check(result.message.contains("unknown"), "literal route retains uncertainty")
} catch { check(false, "unexpected error type") }
check(events == ["capability", "synthesis:raw"], "literal route does not replay")
reset()
do {
  _ = try RunnerTests().typeText(HTTPRequest(body: ["text":42]))
  check(false, "malformed literal request must refuse")
} catch let result as RunnerError {
  check(result.status == 400, "malformed literal request is invalid")
} catch { check(false, "unexpected error type") }
check(events.isEmpty, "malformed literal request sends nothing")
print("Native text delivery: \(checks) checks passed; production Swift with synthetic callbacks only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-text-delivery-') as folder:
    out = Path(folder)
    (out / 'probe.swift').write_text(harness)
    subprocess.run(['xcrun', 'swiftc', str(out / 'probe.swift'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)

# Execute the real Objective-C capability method with a controlled class inventory.
# This does not resolve XCTest classes, contact testmanagerd or synthesize input.
bridge = (SOURCES / 'IPURBridge.m').read_text()
availability = block(bridge, '+ (BOOL)textSynthesisAvailable')
constants = '\n'.join(line for line in bridge.splitlines()
                      if line.startswith(('static NSString *const IPURText', 'static NSString *const IPURRecord')))
objc = r'''
#import <Foundation/Foundation.h>
static unsigned flags;
@interface FakeRecord : NSObject @end
@implementation FakeRecord
+ (BOOL)instancesRespondToSelector:(SEL)s {
  if (flags & 128) @throw [NSException exceptionWithName:@"fixture" reason:@"probe failure" userInfo:nil];
  NSString *name=NSStringFromSelector(s);
  if ([name isEqualToString:@"initWithName:"]) return (flags & 4) != 0;
  if ([name isEqualToString:@"addPointerEventPath:"]) return (flags & 8) != 0;
  if ([name isEqualToString:@"synthesizeWithError:"]) return (flags & 16) != 0;
  return NO;
}
@end
@interface FakePath : NSObject @end
@implementation FakePath
+ (BOOL)instancesRespondToSelector:(SEL)s {
  NSString *name=NSStringFromSelector(s);
  if ([name isEqualToString:@"initForTextInput"]) return (flags & 32) != 0;
  if ([name isEqualToString:@"typeText:atOffset:typingSpeed:shouldRedact:"]) return (flags & 64) != 0;
  return NO;
}
@end
static Class fixtureClass(NSString *name) {
  if (flags & 256) @throw [NSException exceptionWithName:@"fixture" reason:@"class lookup failure" userInfo:nil];
  if ([name isEqualToString:@"XCSynthesizedEventRecord"]) return (flags & 1) ? FakeRecord.class : Nil;
  if ([name isEqualToString:@"XCPointerEventPath"]) return (flags & 2) ? FakePath.class : Nil;
  return Nil;
}
#define NSClassFromString fixtureClass
@interface IPURBridge : NSObject
+ (BOOL)textSynthesisAvailable;
@end
@implementation IPURBridge
'''
objc += constants + '\n' + availability + '\n@end\n'
objc += r'''
int main(void) { @autoreleasepool {
  for (flags=0;flags<128;flags++) {
    if ([IPURBridge textSynthesisAvailable] != (flags==127)) {
      fprintf(stderr,"capability mismatch for flags %u\n", flags);return 1;
    }
  }
  flags=127|128;
  if ([IPURBridge textSynthesisAvailable]) return 1;
  flags=127|256;
  if ([IPURBridge textSynthesisAvailable]) return 1;
  puts("Native text capability: 130 synthetic inventory/exception checks passed; production ObjC method");
} return 0; }
'''
with tempfile.TemporaryDirectory(prefix='ipu-text-capability-') as folder:
    out = Path(folder)
    (out / 'probe.m').write_text(objc)
    subprocess.run(['xcrun', 'clang', '-fobjc-arc', '-framework', 'Foundation',
                    str(out / 'probe.m'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)
