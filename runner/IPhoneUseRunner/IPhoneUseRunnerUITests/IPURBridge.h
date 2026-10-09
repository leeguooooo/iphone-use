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
/// Node key holding the live XCAccessibilityElement, present only in trees requested with
/// `includeElements:YES`. Never JSON-serialize such a tree.
FOUNDATION_EXPORT NSString *const IPURNodeAXElementKey;

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

/// The AX element of an active application with this pid, or nil when it is not active.
+ (nullable id)activeApplicationElementForPID:(int)pid;

/// The foreground application's AX element (XCAccessibilityElement), resolved from
/// `activeApplications`: the only non-SpringBoard active app when there is exactly one, else a
/// hit-test at `probePoint` (screen points), else the first non-SpringBoard app, else SpringBoard.
/// Writes its pid into `pid`.
+ (nullable id)foregroundApplicationElementWithProbePoint:(CGPoint)probePoint pid:(int *)pid;

/// Same, with the probe point computed only when the hit-test needs it.
+ (nullable id)foregroundApplicationElementWithProbe:(CGPoint (NS_NOESCAPE ^)(void))probe pid:(int *)pid
  NS_SWIFT_NAME(foregroundApplicationElement(probePoint:pid:));

/// Per-request cache of the active-application list and SpringBoard element (main thread only).
/// Enabling or disabling clears it; synthesized touches clear it too.
+ (void)setRequestCacheEnabled:(BOOL)enabled;
+ (void)invalidateRequestCache;

/// The system application (SpringBoard) AX element.
+ (nullable id)systemApplicationElement;

/// Pid of an XCAccessibilityElement, or 0.
+ (int)pidForAXElement:(id)element;

/// Bundle id of a running process, via XCUIDevice.applicationMonitor
/// monitoredApplicationWithProcessIdentifier:. Cached per pid.
+ (nullable NSString *)bundleIDForPID:(int)pid;

/// XCUIApplication for a running pid (applicationMonitor), or nil.
+ (nullable XCUIApplication *)applicationForPID:(int)pid;

/// The interface orientation of what is on screen, as a UIInterfaceOrientation raw value (0 when
/// unknown): XCUIApplication.interfaceOrientation, which asks testmanagerd for the active
/// interface orientation whichever app is in front. Not XCUIDevice.orientation, the physical
/// orientation: a phone lying on its side reads landscape while its UI stays portrait. Reads
/// within 250 ms share one answer.
+ (NSInteger)interfaceOrientation;

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

/// Same as above; with `includeElements` every node also carries its live accessibility element
/// under IPURNodeAXElementKey, so it can be re-snapshotted later (element registry).
+ (NSDictionary<NSString *, id> *)wdaTreeForAXElement:(id)axElement
                                             maxDepth:(NSInteger)maxDepth
                                             maxNodes:(NSInteger)maxNodes
                                   extensionCallLimit:(NSInteger)extensionCallLimit
                                          rememberKey:(nullable NSString *)rememberKey
                                      includeElements:(BOOL)includeElements;

/// Serializes an already-taken snapshot (public XCUIElementSnapshot or private XCElementSnapshot)
/// into WDA's /source node shape.
+ (NSDictionary<NSString *, id> *)wdaTreeForSnapshot:(id)snapshot maxNodes:(NSInteger)maxNodes;
+ (NSDictionary<NSString *, id> *)wdaTreeForSnapshot:(id)snapshot
                                            maxNodes:(NSInteger)maxNodes
                                     includeElements:(BOOL)includeElements;

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
/// Several touches in one event record. Each path is an array of steps
/// `{"type": "down"|"move"|"up", "x": pt, "y": pt, "t": seconds}`; the first step must be "down"
/// and a path without a final "up" lifts at its last offset. Paths run concurrently on the shared
/// timeline (offsets are absolute), so sequential touches simply use later offsets.
+ (nullable NSString *)synthesizeTouchPaths:(NSArray<NSArray<NSDictionary<NSString *, id> *> *> *)paths
                                       name:(NSString *)name;
/// Where synthesized touches spent their time since the last call, in ms: "Orientation" (the
/// record's orientation read), "Build" (record construction, including that read), "Wait"
/// (synthesizeWithError: — testmanagerd plays the events and answers once they were delivered),
/// "Hold" (the records' scheduled length) and "Calls". Empty when nothing was synthesized. Resets.
+ (NSDictionary<NSString *, NSNumber *> *)takeSynthesisTiming;

/// Types into whatever holds keyboard focus. `charactersPerSecond` 0 → 60.
+ (nullable NSString *)synthesizeText:(NSString *)text
                  charactersPerSecond:(NSUInteger)charactersPerSecond
                                  pid:(int)pid;

// MARK: - Screen capture

/// The screen as PNG through testmanagerd's screenshot request, which works off the main thread
/// (XCUIScreen's public screenshot is main-only). nil when that path fails or returns another
/// format; the caller then captures on main.
+ (nullable NSData *)requestedPNGScreenshotWithError:(NSString *_Nullable *_Nullable)error
  NS_SWIFT_NAME(requestedPNGScreenshot(error:));

/// One JPEG of the main screen, safe to call off the main thread. Tries, fastest first:
/// 1. `XCUIDevice.screenDataSource requestScreenshotWithRequest:withReply:` with an
///    XCTScreenshotRequest asking testmanagerd for JPEG at `quality` (what WDA's MJPEG server uses);
/// 2. `XCUIScreen screenshotWithEncoding:options:` with the same JPEG encoding;
/// 3. the public `XCUIScreen.mainScreen.screenshot` PNG, re-encoded.
/// `scale` (0 < scale ≤ 1) downsizes with ImageIO and re-encodes at `quality` (0…1).
/// `path` receives which capture path produced the frame ("request", "encoding", "public").
+ (nullable NSData *)jpegScreenshotWithQuality:(double)quality
                                         scale:(double)scale
                                          path:(NSString *_Nullable *_Nullable)path
                                         error:(NSString *_Nullable *_Nullable)error;
/// The screen as an 8-bit grayscale thumbnail whose longer side is at most `maxSide` pixels,
/// for cheap on-device change detection. Row-major, `width` × `height` bytes.
+ (nullable NSData *)grayScreenWithMaxSide:(NSUInteger)maxSide
                                     width:(NSUInteger *)width
                                    height:(NSUInteger *)height
                                     error:(NSString *_Nullable *_Nullable)error;

/// The main screen as a decoded image no larger than `scale` of full size, for the H.264 stream
/// (RunnerH264.swift): the same capture paths as `jpegScreenshotWithQuality:`, decoded once by
/// ImageIO instead of re-encoded. Caller releases the image.
+ (nullable CGImageRef)screenImageWithQuality:(double)quality
                                        scale:(double)scale
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error CF_RETURNS_RETAINED;

/// The encoded bytes of one capture (JPEG at `quality`, or PNG on the public path), undecoded:
/// the H.264 stream compares them with the previous capture and skips an unchanged screen
/// without decoding it.
+ (nullable NSData *)screenCaptureWithQuality:(double)quality
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error;

/// Decode `screenCaptureWithQuality:` bytes no larger than `scale` of full size. Caller releases.
+ (nullable CGImageRef)decodeScreenCapture:(NSData *)data scale:(double)scale CF_RETURNS_RETAINED;

// MARK: - Device

/// Screen lock state from SpringBoardServices (SBGetScreenLockStatus), like WDA. Writes NO into
/// `known` when the private function is unavailable (the return value is then NO).
+ (BOOL)isScreenLocked:(BOOL *)known;

/// `-[XCUIDevice pressLockButton]`; returns an error string when unavailable.
+ (nullable NSString *)pressLockButton;

/// Screen lock state and whether a passcode is required now (false on an unlocked phone even
/// with a passcode set, seen on iOS 15), both from SBGetScreenLockStatus. nil when
/// SpringBoardServices is unavailable. Keys: "locked", "passcodeEnabled" (NSNumber BOOL).
+ (nullable NSDictionary<NSString *, NSNumber *> *)screenLockStatus;

/// The Auto-Lock setting, read from ManagedConfiguration (`MCProfileConnection`
/// `effectiveValueForSetting:@"maxInactivity"`, the value Settings > Display & Brightness >
/// Auto-Lock writes). Reads only: nothing on screen changes. nil when the framework or the
/// value is unavailable. Keys: "secs" (NSNumber, seconds) and "never" (NSNumber BOOL).
+ (nullable NSDictionary<NSString *, id> *)autoLockSetting;

/// Resets SpringBoard's idle (auto-lock) timer the way a key press would, without changing
/// anything on screen: one press of F13 (keyboard usage page 0x07, usage 0x68), a key iOS maps to
/// nothing, sent through XCUIDevice's device-event channel. Takes ~0.25 s; it shares the event
/// channel with taps, so call it on the main thread between commands. Returns an error string,
/// or nil when the event was delivered.
+ (nullable NSString *)resetIdleTimer;

@end

NS_ASSUME_NONNULL_END
