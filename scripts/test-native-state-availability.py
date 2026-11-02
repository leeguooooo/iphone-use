#!/usr/bin/env python3
"""Run the real AX serializer on synthetic states; no phone or Simulator."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
coverage = (ROOT / 'scripts/test-native-snapshot-coverage.py').read_text()
namespace = {'__file__': str(ROOT / 'scripts/test-native-snapshot-coverage.py')}
exec(coverage[:coverage.index('\nmock =')], namespace)
mock = r'''
+ (NSString *)elementTypeName:(NSInteger)t { return @"XCUIElementTypeButton"; }
+ (id)requestSnapshotForElement:(id)e maxDepth:(NSInteger)d maxNodes:(NSInteger)n error:(NSString **)err { return nil; }
@end
static NSInteger checks=0;
static NSDictionary *serialize(NSDictionary *raw) {
  IPURWalk walk={0,50,NO,NO};
  return IPURSerialize(raw,0,&walk,[NSMutableArray array]);
}
static void expect(NSDictionary *raw,NSString *enabled,NSString *focused,NSString *selected) {
  NSDictionary *node=serialize(raw);
  NSDictionary *wanted=@{@"isEnabled":enabled ?: (id)NSNull.null,
    @"isFocused":focused ?: (id)NSNull.null,@"isSelected":selected ?: (id)NSNull.null};
  for (NSString *key in wanted) {
    id actual=node[key] ?: (id)NSNull.null;
    if (![actual isEqual:wanted[key]]) {
      NSLog(@"state availability mismatch raw=%@ key=%@ actual=%@ wanted=%@",raw,key,actual,wanted[key]);exit(1);
    }
    checks++;
  }
}
int main(void) { @autoreleasepool {
  expect(@{},nil,nil,nil);
  expect(@{@"enabled":NSNull.null,@"hasFocus":NSNull.null,@"selected":NSNull.null},nil,nil,nil);
  expect(@{@"enabled":@YES,@"hasFocus":@NO,@"hasKeyboardFocus":@NO,@"selected":@NO},@"1",@"0",@"0");
  expect(@{@"enabled":@NO,@"hasFocus":@YES,@"selected":@YES},@"0",@"1",@"1");
  expect(@{@"hasKeyboardFocus":@YES},nil,@"1",nil);
  expect(@{@"hasFocus":@NO},nil,nil,nil);
  expect(@{@"hasKeyboardFocus":@NO},nil,nil,nil);
  // A web page's links and texts: focus-engine focus without the keyboard is not focus.
  expect(@{@"hasFocus":@YES,@"hasKeyboardFocus":@NO},nil,@"0",nil);
  expect(@{@"hasFocus":@NO,@"hasKeyboardFocus":@YES},nil,@"1",nil);
  expect(@{@"enabled":@2,@"hasFocus":@"unknown",@"hasKeyboardFocus":@NO,@"selected":@[]},nil,nil,nil);
  expect(@{@"enabled":@1.0,@"hasFocus":@0.0,@"hasKeyboardFocus":@NO,@"selected":@1.25},nil,nil,nil);
  expect(@{@"enabled":@"false",@"hasFocus":@"0",@"hasKeyboardFocus":@"false",@"selected":@"1"},@"0",@"0",@"1");
  NSMutableDictionary *known=[serialize(@{@"enabled":@YES,@"hasFocus":@NO,@"hasKeyboardFocus":@NO,@"selected":@NO}) mutableCopy];
  NSUInteger withSelection=[NSJSONSerialization dataWithJSONObject:known options:0 error:NULL].length;
  [known removeObjectForKey:@"isSelected"];
  NSUInteger withoutSelection=[NSJSONSerialization dataWithJSONObject:known options:0 error:NULL].length;
  printf("Known-false selection raw snapshot cost: %lu bytes for this synthetic node only\n",(unsigned long)(withSelection-withoutSelection));
  printf("Native state availability: %ld assertions passed; synthetic AX only\n",(long)checks);
} return 0; }
'''
program = namespace['prefix'] + namespace['constants'] + namespace['types']
program += '\n@implementation IPURBridge\n' + namespace['helpers'] + namespace['method'] + mock
with tempfile.TemporaryDirectory(prefix='ipu-native-states-') as folder:
    out = Path(folder)
    (out / 'probe.m').write_text(program)
    subprocess.run(['xcrun', 'clang', '-fobjc-arc', '-fblocks', '-framework', 'Foundation',
                    str(out / 'probe.m'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)


extract_source = (ROOT / 'scripts/test-native-click-delivery.py').read_text()
helpers = {}
exec(extract_source[extract_source.index('def block('):extract_source.index('\n\nroute =')], helpers)
node_source = helpers['block']((ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerElements.swift').read_text(), 'final class UINode {')
program = 'import Foundation\nimport CoreGraphics\nlet IPURNodeAXElementKey = "_ax"\nenum RunnerElementTypes { static func rawValue(of name: String) -> Int { 1 } }\n' + node_source
program += r'''
var checks = 0
func require(_ value: Bool, _ message: String) {
  checks += 1
  if !value { print(message); exit(1) }
}
let screen = CGSize(width:390,height:844)
func node(_ state: [String:Any]) -> UINode { UINode(raw:state,parent:nil,pid:123) }
let unknown = node([:]).predicateObject(screen:screen)
for key in ["enabled","isEnabled","wdEnabled","focused","hasFocus","isFocused","wdFocused","selected","isSelected","wdSelected"] {
  require(unknown[key] == nil,"unknown state exposed as a boolean: \(key)")
  for value in [false,true] {
    require(!NSPredicate(format:"%K == %@",key,NSNumber(value:value)).evaluate(with:unknown),"unknown state matched \(key)=\(value)")
  }
}
for value in [false,true] {
  let known = node(["isEnabled":value,"isFocused":value,"isSelected":value]).predicateObject(screen:screen)
  for key in ["enabled","focused","selected"] {
    require(NSPredicate(format:"%K == %@",key,NSNumber(value:value)).evaluate(with:known),"known state lost: \(key)")
    require(!NSPredicate(format:"%K == %@",key,NSNumber(value:!value)).evaluate(with:known),"known state inverted: \(key)")
  }
}
for value: Any in [NSNull(),2,"unknown",[true],NSNumber(value:0.0),NSNumber(value:1.0)] {
  let invalid=node(["isEnabled":value,"isFocused":value,"isSelected":value]).predicateObject(screen:screen)
  for key in ["enabled","focused","selected"] { require(invalid[key] == nil,"malformed state became known: \(key)") }
}
print("Native state predicates: \(checks) assertions passed; actual UINode, no UI")
'''
program += '\n' + helpers['block']((ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerHTTP.swift').read_text(), 'struct RunnerError: Error')
program += r'''
struct HTTPResponse {
  let status: Int
  static func value(_ value: Any) -> HTTPResponse { HTTPResponse(status:200) }
}
final class Register { func register(_ node: UINode) -> String { "E" } }
enum ElementReference { static func make(_ id: String) -> [String:String] { ["ELEMENT":id] } }
final class ActiveProbe {
  let root: UINode
  let elements = Register()
  init(_ raw: [String:Any], complete: Bool = true) { root = UINode(raw:["children":[raw]],parent:nil,pid:123,treeComplete:complete) }
  func foregroundTree() throws -> UINode { root }
  func status() -> Int {
    do { return try activeElement().status }
    catch let error as RunnerError { return error.status }
    catch { return -1 }
  }
'''
program += helpers['block']((ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerRoutes.swift').read_text(), '  private func activeElement()')
program += r'''
}
for (raw,wanted): ([String:Any],Int) in [(["type":"XCUIElementTypeTextField"],500),(["type":"XCUIElementTypeTextField","isFocused":"0"],404),(["type":"XCUIElementTypeTextField","isFocused":"1"],200)] {
  require(ActiveProbe(raw).status()==wanted,"active element fabricated focus absence")
}
require(ActiveProbe(["type":"XCUIElementTypeTextField","isFocused":"0"],complete:false).status()==500,"truncated tree fabricated focus absence")
require(ActiveProbe(["type":"XCUIElementTypeTextField","isFocused":"1"],complete:false).status()==200,"partial tree lost positive focus evidence")
print("Native active-element availability: 5 scenarios passed; no UI")
'''
with tempfile.TemporaryDirectory(prefix='ipu-node-states-') as folder:
    out = Path(folder)
    (out / 'probe.swift').write_text(program)
    subprocess.run(['xcrun', 'swiftc', str(out / 'probe.swift'), '-o', str(out / 'probe')], check=True)
    subprocess.run([str(out / 'probe')], check=True)
