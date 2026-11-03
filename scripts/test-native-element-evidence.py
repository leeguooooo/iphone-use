#!/usr/bin/env python3
"""Compile production element polling, gates and subtree routing with synthetic AX.

No XCTest, signing, simulator, phone, or input. A retained handle and recorded
provider calls prove that a scoped read cannot quietly fetch a screen tree.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests'
helpers = (ROOT / 'scripts/test-native-click-delivery.py').read_text()
ns = {}
exec(helpers[helpers.index('def block('):helpers.index('\n\nroute =')], ns)
block = ns['block']
elements = (BASE / 'RunnerElements.swift').read_text()
routes = (BASE / 'RunnerRoutes.swift').read_text()
program = '''import Foundation
import CoreGraphics
let IPURNodeAXElementKey = "_ax"
let IPURTreeOkKey = "ok", IPURTreeRootKey = "root", IPURTreeTruncatedKey = "truncated"
enum RunnerElementTypes { static func rawValue(of name: String) -> Int { 1 } }
'''
program += block((BASE / 'RunnerHTTP.swift').read_text(), 'struct RunnerError: Error') + '\n'
program += block(elements, 'final class UINode {') + '\n'
program += block(elements, 'enum ElementFrameStability {') + '\n'
program += block((BASE / 'RunnerActions.swift').read_text(), 'enum TapBounds {') + '\n'
program += r'''
struct HTTPRequest { var query: [String:String] = [:] }
struct HTTPResponse {
  let value: Any
  let headers: [String:String]
  static func value(_ value: Any, headers: [String:String] = [:]) -> HTTPResponse {
    HTTPResponse(value:value, headers:headers)
  }
}
final class Registry {
  var stored: UINode?
  func node(_ id: String) -> UINode? { stored }
}
enum IPURBridge {
  static var pid: Int32 = 42
  static var editable: NSNumber?
  static var calls: [(AnyObject,Int,Int,Int)] = []
  static var fail = false
  static func pid(forAXElement element: AnyObject) -> Int32 { pid }
  static func invalidateRequestCache() {}
  static func editableState(for element: AnyObject) -> NSNumber? { editable }
  static func runnerTree(forAXElement element: AnyObject, maxDepth: Int, maxNodes: Int,
                         extensionCallLimit: Int, rememberKey: String?) -> [String:Any] {
    calls.append((element,maxDepth,maxNodes,extensionCallLimit))
    if fail { return ["ok":false] }
    return ["ok":true,"root":["type":"XCUIElementTypeCell",
      "rect":["x":0,"y":0,"width":20,"height":20]],"truncated":maxDepth == 1]
  }
}
let ax = NSObject()
func node(_ x: Double = 10, enabled: Any? = nil, editable: Any? = nil,
          handle: Bool = true, identifier: String = "target") -> UINode {
  var raw: [String:Any] = ["type":"XCUIElementTypeTextField","rawIdentifier":identifier,
    "rect":["x":x,"y":20,"width":80,"height":30]]
  if let enabled { raw["isEnabled"] = enabled }
  if let editable { raw["isEditable"] = editable }
  if handle { raw[IPURNodeAXElementKey] = ax }
  return UINode(raw:raw,parent:nil,pid:42)
}
final class Probe {
  static let defaultMaxDepth = 64, defaultMaxNodes = 5000
  let elements = Registry()
  var sequence: [UINode] = []
  var reads = 0
  var mutations = 0
  var reveals = 0
  func scrollIntoView(_ id: String) throws { reveals += 1 }
  func windowSizePoints() -> CGSize { CGSize(width:390,height:844) }
  func tapPoint(_ point: CGPoint) throws { mutations += 1 }
  func freshNode(_ id: String) throws -> UINode {
    reads += 1
    return sequence.isEmpty ? elements.stored! : sequence.removeFirst()
  }
  func treeHeaders(_ tree: [String:Any], backend: String, pid: Int32) -> [String:String] {
    ["X-IPU-Tree-Complete":tree["truncated"] as? Bool == false ? "1":"0"]
  }
  func rectValue(_ r: CGRect) -> [String:Any] { ["x":r.minX,"y":r.minY,"width":r.width,"height":r.height] }
  func attempt(editing: Bool = false) throws {
    _ = try actionNode("E", editing:editing)
    mutations += 1
  }
'''
for name in ['liveElement', 'elementSubtree', 'observeElement', 'editableEvidence', 'elementEvidence', 'actionNode', 'revealElement', 'clickElement']:
    program += block(routes, '  private func ' + name + '(').replace('private func', 'func', 1) + '\n'
program += r'''
}
var checks = 0
func require(_ value: Bool, _ message: String) {
  checks += 1
  if !value { print(message); exit(1) }
}
func refused(_ code: Int, _ action: () throws -> Void) {
  do { try action(); require(false,"expected refusal \(code)") }
  catch let error as RunnerError { require(error.status == code,"unexpected refusal: \(error)") }
  catch { require(false,"unexpected error \(error)") }
}
func probe(_ target: UINode = node()) -> Probe {
  IPURBridge.editable = nil; IPURBridge.fail = false; IPURBridge.calls = []; IPURBridge.pid = 42
  let p = Probe(); p.elements.stored = target; return p
}
let ordinary = probe()
try ordinary.attempt(editing:true)
require(ordinary.mutations == 1 && ordinary.reads == 2,"unknown attributes should not invent disabled/read-only state")
require(IPURBridge.calls.isEmpty,"element action fetched tree")
for disabled in [false,"0",NSNumber(value:false)] {
  let p = probe(node(enabled:disabled))
  refused(400) { try p.attempt() }
  require(p.mutations == 0,"disabled target mutated")
  refused(400) { _ = try p.clickElement("E") }
  require(p.mutations == 0 && p.reveals == 0,"clickElement bypassed disabled target gate")
}
for editable in [false,"false"] {
  let p = probe(node(editable:editable))
  refused(400) { try p.attempt(editing:true) }
  require(p.mutations == 0,"read-only target mutated")
}
let optional = probe(); IPURBridge.editable = false
refused(400) { try optional.attempt(editing:true) }
require(optional.mutations == 0,"on-demand read-only AX evidence ignored")
let clickReadonly = probe(); IPURBridge.editable = false
try clickReadonly.attempt()
require(clickReadonly.mutations == 1,"editable gate applied to non-edit action")
let moving = probe(); moving.sequence = [node(1),node(10),node(20),node(30)]
refused(400) { try moving.attempt() }
require(moving.reads == 4 && moving.mutations == 0,"moving target was retried/mutated/unbounded")
let settling = probe(); settling.sequence = [node(1),node(20),node(20)]
try settling.attempt()
require(settling.reads == 3 && settling.mutations == 1,"settling target should receive exactly one mutation")
let changed = probe(); changed.sequence = [node(),node(identifier:"replacement")]
refused(404) { try changed.attempt() }
require(changed.mutations == 0,"changed identity mutated")
let invalid = probe(UINode(raw:["type":"XCUIElementTypeOther",IPURNodeAXElementKey:ax],parent:nil,pid:42))
refused(400) { try invalid.attempt() }
require(invalid.reads == 1 && invalid.mutations == 0,"empty frame should be unknown and not actionable")
let cached = probe(node(handle:false))
refused(501) { try cached.attempt() }
require(cached.reads == 0,"cached node presented as live evidence")
for target in [node(5000), node(5000,enabled:false),
  UINode(raw:["type":"XCUIElementTypeCell",IPURNodeAXElementKey:ax],parent:nil,pid:42)] {
  let p = probe(target)
  _ = try p.revealElement("E")
  require(p.reveals == 1 && p.reads == 0,"scrollTo demanded a stable/laid-out/enabled target before revealing it")
}
let unpolled = probe()
let ev = try unpolled.elementEvidence("E",HTTPRequest()).value as! [String:Any]
require(unpolled.reads == 1 && ev["stable"] is NSNull && ev["enabled"] is NSNull && ev["editable"] is NSNull,"unknown state lost or unrequested polling ran")
let polled = probe(node(enabled:false)); IPURBridge.editable = false
let known = try polled.elementEvidence("E",HTTPRequest(query:["stability":"1"])).value as! [String:Any]
require(known["stable"] as? Bool == true && known["enabled"] as? Bool == false && known["editable"] as? Bool == false,"known evidence lost")
require(known["samples"] as? Int == 2,"wrong sample count")
refused(400) { _ = try polled.elementEvidence("E",HTTPRequest(query:["stability":"yes"])) }
let sub = probe()
let reply = try sub.elementSubtree("E",HTTPRequest(query:["max_depth":"3","max_nodes":"7"]))
require(sub.reads == 0 && IPURBridge.calls.count == 1,"subtree fetched a screen/extra tree")
let call = IPURBridge.calls[0]
require(call.0 === ax && call.1 == 3 && call.2 == 7 && call.3 == 0,"subtree not rooted/bounded at provider")
require(reply.headers["X-IPU-Scope"] == "element" && reply.headers["X-IPU-Tree-Complete"] == "1","subtree coverage lost")
_ = try sub.elementSubtree("E",HTTPRequest(query:["max_depth":"1"]))
let partial = try sub.elementSubtree("E",HTTPRequest(query:["max_depth":"1"]))
require(partial.headers["X-IPU-Tree-Complete"] == "0","depth-capped subtree became complete")
for query in [["max_depth":"0"],["max_depth":"65"],["max_nodes":"5001"],["max_nodes":"bad"]] {
  let p = probe()
  refused(400) { _ = try p.elementSubtree("E",HTTPRequest(query:query)) }
  require(IPURBridge.calls.isEmpty,"bad bounds dispatched AX read")
}
let absent = probe(); absent.elements.stored = nil
refused(404) { _ = try absent.elementSubtree("E",HTTPRequest()) }
let stalePID = probe(); IPURBridge.pid = 7
refused(404) { _ = try stalePID.elementSubtree("E",HTTPRequest()) }
let noAX = probe(node(handle:false))
refused(501) { _ = try noAX.elementSubtree("E",HTTPRequest()) }
require(IPURBridge.calls.isEmpty,"missing AX fell back")
let failed = probe(); IPURBridge.fail = true
refused(500) { _ = try failed.elementSubtree("E",HTTPRequest()) }
require(IPURBridge.calls.count == 1 && failed.reads == 0,"AX failure fell back or retried")
// Injected time makes the polling deadline test deterministic; no real waits.
var now = 0.0, reads = 0
let exhausted = try ElementFrameStability.observe(read:{ reads += 1; return node() },wait:{ now += 0.3 },now:{ now })
require(exhausted.stable == nil && exhausted.samples == 1 && reads == 1,"deadline invented stability evidence or allowed another AX call")
print("Native element evidence/subtree: \(checks) assertions passed; synthetic AX only")
'''
with tempfile.TemporaryDirectory(prefix='ipu-element-evidence-') as work:
    work = Path(work)
    (work/'probe.swift').write_text(program)
    subprocess.run(['xcrun','swiftc',str(work/'probe.swift'),'-o',str(work/'probe')],check=True)
    subprocess.run([str(work/'probe')],check=True)

# Exercise the actual optional AX attribute bridge: mapping/read failures and
# malformed booleans must remain unknown, rather than accepting a text-field kind.
bridge = (BASE/'IPURBridge.m').read_text()
objc = r'''
#import <Foundation/Foundation.h>
#import <objc/message.h>
#import <string.h>
typedef id (*IPURMsgSendMapAttributes)(id, SEL, id, BOOL);
static NSInteger mode = 0;
@interface XCElementSnapshot : NSObject
+ (id)axAttributesForElementSnapshotKeyPaths:(id)paths isMacOS:(BOOL)mac;
@end
@implementation XCElementSnapshot
+ (id)axAttributesForElementSnapshotKeyPaths:(id)paths isMacOS:(BOOL)mac {
  if (![paths isEqual:@[@"isEditable"]]) exit(90);
  if (mode == 1) return @[];
  if (mode == 2) @throw [NSException exceptionWithName:@"unknown attribute" reason:nil userInfo:nil];
  return @[@"XC_kAXXCAttributeIsEditable"];
}
@end
@interface Client : NSObject
- (id)attributesForElement:(id)e attributes:(id)attrs error:(NSError **)err;
@end
@implementation Client
- (id)attributesForElement:(id)e attributes:(id)attrs error:(NSError **)err {
  if (![e isEqual:@"selected"] || ![attrs isEqual:@[@"XC_kAXXCAttributeIsEditable"]]) exit(91);
  if (mode == 3) { *err = [NSError errorWithDomain:@"test" code:1 userInfo:nil]; return nil; }
  if (mode == 4) return @{};
  id value = @YES;
  if (mode == 5) value = @NO;
  if (mode == 6) value = @2;
  if (mode == 7) value = @1.0;
  if (mode == 8) value = NSNull.null;
  if (mode == 9) value = @"false";
  if (mode == 10) value = @"unknown";
  return @{@"XC_kAXXCAttributeIsEditable":value};
}
@end
@interface IPURBridge : NSObject
+ (id)axClient;
+ (NSNumber *)editableStateForAXElement:(id)element;
@end
@implementation IPURBridge
+ (id)axClient { return mode == 11 ? nil : [Client new]; }
'''
objc += block(bridge, '+ (nullable NSNumber *)editableStateForAXElement:')
objc += r'''
@end
int main(void) { @autoreleasepool {
  for (mode = 0; mode < 12; mode++) {
    NSNumber *state = [IPURBridge editableStateForAXElement:@"selected"];
    NSNumber *expected = mode == 0 ? @YES : (mode == 5 || mode == 9 ? @NO : nil);
    if (expected == nil ? state != nil : ![state isEqual:expected]) {
      NSLog(@"editable bridge mode %ld: %@ expected %@",(long)mode,state,expected); return 1;
    }
  }
  puts("Native editable AX bridge: 12 cases passed; synthetic provider only");
} return 0; }
'''
with tempfile.TemporaryDirectory(prefix='ipu-editable-evidence-') as work:
    work = Path(work)
    (work/'probe.m').write_text(objc)
    subprocess.run(['xcrun','clang','-fobjc-arc','-fblocks','-framework','Foundation',str(work/'probe.m'),'-o',str(work/'probe')],check=True)
    subprocess.run([str(work/'probe')],check=True)
