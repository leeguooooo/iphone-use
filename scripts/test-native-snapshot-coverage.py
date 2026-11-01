#!/usr/bin/env python3
"""Execute the production Objective-C tree walk with a bounded synthetic AX provider.

No XCTest, Simulator, phone, or UI. Geometry is stubbed; tree traversal and
coverage decisions are compiled verbatim from IPURBridge.m.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
src = (ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.m').read_text()

def between(start, end):
    return src[src.index(start):src.index(end, src.index(start))]

prefix = r'''
#import <Foundation/Foundation.h>
@interface IPURBridge : NSObject
+ (id)requestSnapshotForElement:(id)e maxDepth:(NSInteger)d maxNodes:(NSInteger)n error:(NSString **)err;
+ (NSString *)elementTypeName:(NSInteger)t;
+ (NSDictionary *)runnerTreeForAXElement:(id)e maxDepth:(NSInteger)d maxNodes:(NSInteger)n extensionCallLimit:(NSInteger)c rememberKey:(NSString *)k includeElements:(BOOL)b;
@end
static NSInteger providerMode = 0;
'''
constants = '\n'.join(line for line in src.splitlines() if line.startswith('NSString *const IPUR'))
types = between('@interface IPURFrontier', '@implementation IPURBridge')
types += next(line for line in src.splitlines() if line.startswith('static NSInteger const IPURDepthLadder'))+'\n'
helpers = between('static id IPURKVC', 'static NSDictionary *IPURRect')
helpers += 'static NSDictionary *IPURRect(id snapshot) { return @{}; }\n'
helpers += between('static NSString *IPURBoolString', '+ (NSDictionary<NSString *, id> *)runnerTreeForAXElement:')
method_start = src.index('+ (NSDictionary<NSString *, id> *)runnerTreeForAXElement:', src.index('+ (NSDictionary<NSString *, id> *)runnerTreeForAXElement:') + 1)
method_end = src.index('\n+ (NSDictionary', method_start + 1)
method = src[method_start:method_end]
mock = r'''
+ (NSString *)elementTypeName:(NSInteger)t { return @"XCUIElementTypeOther"; }
+ (id)requestSnapshotForElement:(id)e maxDepth:(NSInteger)d maxNodes:(NSInteger)n error:(NSString **)err {
  if (d<=1) return @{@"accessibilityElement":e};
  if ([e isEqual:@"app"]) return @{@"children":@[@{@"children":@[(providerMode==3 ? @{} : @{@"accessibilityElement":@"frontier"})]}]};
  if (providerMode == 1) return nil;
  // Model an AX implementation that silently includes subRoot in its node cap.
  NSMutableArray *children = [NSMutableArray array];
  for (NSInteger i=0; i<MIN(4, MAX(0,n-(providerMode==2 ? 0 : 1))); i++) [children addObject:@{@"identifier":@(i)}];
  return @{@"accessibilityElement":@"frontier", @"children":children};
}
@end
static void checkDepth(NSInteger depth, NSInteger cap, NSInteger calls, BOOL partial, NSInteger count) {
  NSDictionary *tree = [IPURBridge runnerTreeForAXElement:@"app" maxDepth:depth maxNodes:cap extensionCallLimit:calls rememberKey:nil includeElements:NO];
  if (![tree[@"ok"] boolValue] || [tree[@"truncated"] boolValue] != partial || [tree[@"nodeCount"] integerValue] != count || (cap<=3 && [tree[@"extensionCalls"] integerValue]!=0)) {
    NSLog(@"coverage mismatch: cap=%ld calls=%ld count=%@ truncated=%@", (long)cap, (long)calls, tree[@"nodeCount"], tree[@"truncated"]); exit(1);
  }
}
static void check(NSInteger cap, NSInteger calls, BOOL partial, NSInteger count) {
  checkDepth(3, cap, calls, partial, count);
}
int main(void) { @autoreleasepool {
  checkDepth(1, 20, 4, YES, 1); // Root-only requests cannot exhaust a frontier.
  check(6, 4, YES, 6); // Hidden descendants must not become proof of absence.
  check(20, 4, NO, 7); // Exhaustive small tree remains complete.
  check(20, 0, YES, 3); // Unexpanded depth frontier.
  check(3, 4, YES, 3); // Exhausted budget sends no extension request.
  providerMode=2;
  check(6, 4, YES, 6); // Child-only provider cap still clips at our own budget.
  providerMode=1;
  check(20, 4, YES, 3); // Failed subtree request.
  providerMode=3;
  check(20, 4, YES, 3); // Frontier has no AX element.
  puts("native snapshot coverage: 8 cases passed (synthetic AX, no UI)");
} return 0; }
'''
with tempfile.TemporaryDirectory(prefix='ipu-native-coverage-') as work:
    path = Path(work)
    (path/'probe.m').write_text(prefix+constants+types+'\n@implementation IPURBridge\n'+helpers+method+mock)
    subprocess.run(['xcrun','clang','-fobjc-arc','-fblocks','-framework','Foundation',str(path/'probe.m'),'-o',str(path/'probe')],check=True)
    subprocess.run([str(path/'probe')],check=True)
