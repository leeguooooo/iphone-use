// IPURBridge — the private-XCTest surface of the iphone-use native runner.
//
// Portions derived from callstack/agent-device (MIT License, Copyright (c) 2026 Callstack):
// the private XCAXClient snapshot request with a reduced attribute set
// (RunnerAXSnapshotBridge.m), XCSynthesizedEventRecord / XCPointerEventPath gesture and text
// synthesis (RunnerSynthesizedGesture.m, RunnerSynthesizedTextEntry.m, RunnerXCTestEventBridge.m)
// and the quiescence-skipping interaction options (RunnerTests+Lifecycle.swift).
// See runner/README.md for the full attribution and license text.
//
// Every private class and selector is resolved at runtime and checked before use; a missing one
// surfaces as an error string (or a nil result) so the Swift layer can fall back to public API.

#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <XCTest/XCTest.h>

NS_ASSUME_NONNULL_BEGIN

/// Result keys of +wdaTreeForAXElement:… / +wdaTreeForSnapshot:….
FOUNDATION_EXPORT NSString *const IPURTreeOkKey;         // NSNumber(BOOL)
FOUNDATION_EXPORT NSString *const IPURTreeRootKey;       // NSDictionary, WDA /source JSON shape
FOUNDATION_EXPORT NSString *const IPURTreeErrorKey;      // NSString
FOUNDATION_EXPORT NSString *const IPURTreeNodeCountKey;  // NSNumber
FOUNDATION_EXPORT NSString *const IPURTreeDepthKey;      // NSNumber: the accepted request depth
FOUNDATION_EXPORT NSString *const IPURTreeTruncatedKey;  // NSNumber(BOOL)
FOUNDATION_EXPORT NSString *const IPURTreeExtensionCallsKey; // NSNumber: re-rooted follow-up requests

@interface IPURBridge : NSObject

/// Turns every XCTest quiescence / idle wait into a no-op for the whole process
/// (XCUIApplicationProcess waitForQuiescence…, XCAXClient_iOS
/// waitForQuiescenceOnAllForegroundApplicationsAsPreEvent:, XCUIApplication _waitForQuiescence…).
/// Returns the selectors that were patched, for the startup log.
+ (NSArray<NSString *> *)installQuiescenceBypass;

/// Runs `block` inside XCUIApplication `_performWithInteractionOptions:block:` with both the
/// pre-event and post-event quiescence skip bits set, when the selector exists; otherwise runs it
/// directly. `application` may be nil.
+ (void)performWithoutQuiescence:(nullable XCUIApplication *)application block:(void (NS_NOESCAPE ^)(void))block;

/// Runs `block`, returning "<name>: <reason>" if it raised an Objective-C exception, else nil.
+ (nullable NSString *)catchException:(void (NS_NOESCAPE ^)(void))block;

// MARK: - Applications

/// `XCUIDevice.sharedDevice.accessibilityInterface` (XCAXClient_iOS), or nil.
+ (nullable id)axClient;

/// Pids of the applications the AX client reports as active.
+ (NSArray<NSNumber *> *)activeApplicationPIDs;

/// The foreground application's AX element (XCAccessibilityElement), resolved from
/// `activeApplications`: the only non-SpringBoard active app when there is exactly one, else a
/// hit-test at `probePoint` (screen points), else the first non-SpringBoard app, else SpringBoard.
/// Writes its pid into `pid`.
+ (nullable id)foregroundApplicationElementWithProbePoint:(CGPoint)probePoint pid:(int *)pid;

/// The system application (SpringBoard) AX element.
+ (nullable id)systemApplicationElement;

/// Pid of an XCAccessibilityElement, or 0.
+ (int)pidForAXElement:(id)element;

/// Bundle id of a running process, via XCUIDevice.applicationMonitor
/// monitoredApplicationWithProcessIdentifier:. Cached per pid.
+ (nullable NSString *)bundleIDForPID:(int)pid;

/// XCUIApplication for a running pid (applicationMonitor), or nil.
+ (nullable XCUIApplication *)applicationForPID:(int)pid;

/// Process id of an XCUIApplication (private `processID`), or 0.
+ (int)pidForApplication:(XCUIApplication *)application;

// MARK: - Accessibility tree

/// Snapshots `axElement` through XCAXClient_iOS requestSnapshotForElement:attributes:parameters:error:
/// with only nine attributes, walking a depth ladder on kAXErrorIllegalArgument and re-rooting
/// depth-capped frontier nodes (bounded by `extensionCallLimit`). The root is serialized into
/// WDA's /source JSON node shape. `rememberKey` (e.g. the pid) remembers the accepted depth so
/// later captures of the same process skip known-rejected rungs; pass nil to always probe.
+ (NSDictionary<NSString *, id> *)wdaTreeForAXElement:(id)axElement
                                             maxDepth:(NSInteger)maxDepth
                                             maxNodes:(NSInteger)maxNodes
                                   extensionCallLimit:(NSInteger)extensionCallLimit
                                          rememberKey:(nullable NSString *)rememberKey;

/// Serializes an already-taken snapshot (public XCUIElementSnapshot or private XCElementSnapshot)
/// into WDA's /source node shape.
+ (NSDictionary<NSString *, id> *)wdaTreeForSnapshot:(id)snapshot maxNodes:(NSInteger)maxNodes;

/// WDA's "XCUIElementType…" name for an element type raw value.
+ (NSString *)elementTypeName:(NSInteger)elementType;

// MARK: - Event synthesis (all coordinates are screen points)

/// Whether the private event-synthesis classes and selectors are present.
+ (BOOL)eventSynthesisAvailable;

+ (nullable NSString *)synthesizeTapAt:(CGPoint)point pid:(int)pid;
+ (nullable NSString *)synthesizeLongPressAt:(CGPoint)point duration:(NSTimeInterval)duration pid:(int)pid;
/// Linear drag sampled every ~16 ms, finger lifts at `duration`.
+ (nullable NSString *)synthesizeDragFrom:(CGPoint)start
                                       to:(CGPoint)end
                                 duration:(NSTimeInterval)duration
                                      pid:(int)pid;
/// Types into whatever holds keyboard focus. `charactersPerSecond` 0 → 60.
+ (nullable NSString *)synthesizeText:(NSString *)text
                  charactersPerSecond:(NSUInteger)charactersPerSecond
                                  pid:(int)pid;

@end

NS_ASSUME_NONNULL_END
