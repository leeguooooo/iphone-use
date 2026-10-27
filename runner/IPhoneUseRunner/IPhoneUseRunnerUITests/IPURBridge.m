// IPURBridge — the private-XCTest surface of the iphone-use native runner.
//
// Portions derived from callstack/agent-device (MIT License, Copyright (c) 2026 Callstack):
// RunnerAXSnapshotBridge.m (reduced-attribute XCAXClient snapshot, depth ladder, frontier
// re-rooting), RunnerSynthesizedGesture.m / RunnerSynthesizedTextEntry.m /
// RunnerXCTestEventBridge.m (XCSynthesizedEventRecord + XCPointerEventPath synthesis) and
// RunnerTests+Lifecycle.swift (_performWithInteractionOptions:block: quiescence skip bits).
// See runner/README.md for the full attribution and license text.

#import "IPURBridge.h"
#import "IPURGeometry.h"
#import "IPURImageCodec.h"

#import <ImageIO/ImageIO.h>
#import <dlfcn.h>
#import <stdatomic.h>
#import <mach/mach.h>
#import <objc/message.h>
#import <objc/runtime.h>

NSString *const IPURTreeOkKey = @"ok";
NSString *const IPURTreeRootKey = @"root";
NSString *const IPURTreeErrorKey = @"error";
NSString *const IPURTreeNodeCountKey = @"nodeCount";
NSString *const IPURTreeDepthKey = @"depth";
NSString *const IPURTreeTruncatedKey = @"truncated";
NSString *const IPURTreeExtensionCallsKey = @"extensionCalls";
NSString *const IPURNodeAXElementKey = @"__axElement";

static NSString *const IPURSpringBoardBundleID = @"com.apple.springboard";

// Deep trees (React Native feeds) make the AX server reject a bulk request with
// kAXErrorIllegalArgument once the depth crosses a content-dependent limit; a shallower retry
// succeeds. Same rungs agent-device ships.
static NSInteger const IPURDepthLadder[] = {56, 40, 24, 12};

typedef id (*IPURMsgSendObject)(id, SEL);
typedef int (*IPURMsgSendInt)(id, SEL);
typedef long long (*IPURMsgSendLongLong)(id, SEL);
typedef id (*IPURMsgSendObjectInt)(id, SEL, int);
typedef id (*IPURMsgSendSnapshotRequest)(id, SEL, id, id, id, NSError **);
typedef id (*IPURMsgSendHitTestSnapshot)(id, SEL, id, CGPoint, NSError **);
typedef id (*IPURMsgSendMapAttributes)(id, SEL, id, BOOL);
typedef void (*IPURMsgSendPerformWithOptions)(id, SEL, unsigned int, void (^)(void));
typedef id (*IPURMsgSendInitRecordDisplay)(id, SEL, NSString *, unsigned long long, long long);
typedef id (*IPURMsgSendInitRecordOrientation)(id, SEL, NSString *, long long);
typedef id (*IPURMsgSendInitRecordName)(id, SEL, NSString *);
typedef void (*IPURMsgSendSetLongLong)(id, SEL, long long);
typedef id (*IPURMsgSendInitPath)(id, SEL, CGPoint, double);
typedef void (*IPURMsgSendPathMove)(id, SEL, CGPoint, double);
typedef void (*IPURMsgSendPathOffset)(id, SEL, double);
typedef void (*IPURMsgSendAddPath)(id, SEL, id);
typedef BOOL (*IPURMsgSendSynthesize)(id, SEL, NSError **);
typedef void (*IPURMsgSendTypeText)(id, SEL, NSString *, double, unsigned long long, BOOL);

static BOOL IPURPointHitTestAvailable(id client)
{
  @try {
    SEL selector = NSSelectorFromString(@"hitTestElement:withPoint:error:");
    if (![client respondsToSelector:selector]) return NO;
    NSMethodSignature *signature = [client methodSignatureForSelector:selector];
    return signature.numberOfArguments == 5 && signature.methodReturnType[0] == '@'
      && [signature getArgumentTypeAtIndex:2][0] == '@'
      && strcmp([signature getArgumentTypeAtIndex:3], @encode(CGPoint)) == 0
      && [signature getArgumentTypeAtIndex:4][0] == '^';
  } @catch (__unused NSException *exception) { return NO; }
}

/// A childless serialized node remembered with its depth relative to the request that produced
/// it. The deepest level of a depth-capped request is where the AX server withheld children.
@interface IPURFrontier : NSObject
@property(nonatomic, strong) id snapshot;
@property(nonatomic, strong) NSMutableDictionary *node;
@property(nonatomic, assign) NSInteger depth;
@end

@implementation IPURFrontier
@end

typedef struct {
  NSInteger nodeCount;
  NSInteger maxNodes;
  BOOL truncated;
  BOOL includeElements;
} IPURWalk;

@implementation IPURBridge

// MARK: - Small runtime helpers

static id IPURObject(id target, NSString *selectorName)
{
  if (target == nil) return nil;
  SEL selector = NSSelectorFromString(selectorName);
  if (![target respondsToSelector:selector]) return nil;
  return ((IPURMsgSendObject)objc_msgSend)(target, selector);
}

static int IPURInt(id target, NSString *selectorName)
{
  if (target == nil) return 0;
  SEL selector = NSSelectorFromString(selectorName);
  if (![target respondsToSelector:selector]) return 0;
  // processIdentifier / processID return pid_t (int32): read them through an int-returning
  // cast so the upper half of x0 is never trusted.
  NSMethodSignature *signature = [target methodSignatureForSelector:selector];
  const char *returnType = signature.methodReturnType;
  if (returnType != NULL && strcmp(returnType, @encode(int)) == 0) {
    return ((IPURMsgSendInt)objc_msgSend)(target, selector);
  }
  return (int)((IPURMsgSendLongLong)objc_msgSend)(target, selector);
}

+ (nullable NSString *)catchException:(void (NS_NOESCAPE ^)(void))block
{
  @try {
    block();
    return nil;
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name ?: @"NSException",
                                      exception.reason ?: @"(no reason)"];
  }
}

// MARK: - Quiescence

static void IPURNoopVoidMethod(Class cls, NSString *selectorName, NSMutableArray<NSString *> *patched)
{
  if (cls == Nil) return;
  Method method = class_getInstanceMethod(cls, NSSelectorFromString(selectorName));
  if (method == NULL) return;
  char returnType[8] = {0};
  method_getReturnType(method, returnType, sizeof(returnType));
  // Only void waits are neutralised; a wait that returns a verdict keeps its implementation.
  if (returnType[0] != 'v') return;
  // Extra arguments (BOOL flags, an activity) are ignored by the block; that is safe for the
  // register-passed scalars and pointers these selectors take.
  IMP noop = imp_implementationWithBlock(^(__unused id receiver) {
  });
  method_setImplementation(method, noop);
  [patched addObject:[NSString stringWithFormat:@"-[%@ %@]", NSStringFromClass(cls), selectorName]];
}

+ (NSArray<NSString *> *)installQuiescenceBypass
{
  static NSArray<NSString *> *installed;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    NSMutableArray<NSString *> *patched = [NSMutableArray array];
    Class process = NSClassFromString(@"XCUIApplicationProcess");
    IPURNoopVoidMethod(process, @"waitForQuiescenceIncludingAnimationsIdle:", patched);
    IPURNoopVoidMethod(process, @"waitForQuiescenceIncludingAnimationsIdle:isPreEvent:", patched);
    IPURNoopVoidMethod(process, @"waitForQuiescenceIncludingAnimationsIdle:usingActivity:isPreEvent:", patched);
    Class axClient = NSClassFromString(@"XCAXClient_iOS");
    IPURNoopVoidMethod(axClient, @"waitForQuiescenceOnAllForegroundApplicationsAsPreEvent:", patched);
    Class application = NSClassFromString(@"XCUIApplication");
    IPURNoopVoidMethod(application, @"_waitForQuiescence", patched);
    IPURNoopVoidMethod(application, @"_waitForQuiescenceAsPreEvent:", patched);
    installed = patched.copy;
  });
  return installed;
}

+ (void)performWithoutQuiescence:(nullable XCUIApplication *)application block:(void (NS_NOESCAPE ^)(void))block
{
  SEL selector = NSSelectorFromString(@"_performWithInteractionOptions:block:");
  if (application == nil || ![application respondsToSelector:selector]) {
    block();
    return;
  }
  // Bit 0 skips the pre-event wait, bit 1 the post-event wait.
  unsigned int options = 1u | 2u;
  ((IPURMsgSendPerformWithOptions)objc_msgSend)(application, selector, options, block);
}

// MARK: - Applications

+ (nullable id)axClient
{
  return IPURObject(XCUIDevice.sharedDevice, @"accessibilityInterface");
}

+ (int)pidForAXElement:(id)element
{
  return IPURInt(element, @"processIdentifier");
}

+ (BOOL)sameAXElement:(id)left other:(id)right
{
  int pid = [self pidForAXElement:left];
  return pid > 0 && pid == [self pidForAXElement:right] && [left isEqual:right];
}

+ (int)pidForApplication:(XCUIApplication *)application
{
  return IPURInt(application, @"processID");
}

// Request-scoped cache (main thread only): the active-application list and SpringBoard element are
// AX round trips; within one request they are read once until something may have changed them.
static BOOL IPURRequestCacheEnabled = NO;
static NSArray *IPURCachedActive = nil;
static id IPURCachedSystem = nil;
static NSValue *IPURCachedScreenSize = nil;

+ (void)setRequestCacheEnabled:(BOOL)enabled
{
  if (!NSThread.isMainThread) return;
  IPURRequestCacheEnabled = enabled;
  IPURCachedActive = nil;
  IPURCachedSystem = nil;
  IPURCachedScreenSize = nil;
}

+ (void)invalidateRequestCache
{
  if (!NSThread.isMainThread) return;
  IPURCachedActive = nil;
  IPURCachedSystem = nil;
  IPURCachedScreenSize = nil;
}

static BOOL IPURUseRequestCache(void)
{
  return IPURRequestCacheEnabled && NSThread.isMainThread;
}

+ (NSArray *)activeApplicationElements
{
  if (IPURUseRequestCache() && IPURCachedActive != nil) return IPURCachedActive;
  id active = IPURObject([self axClient], @"activeApplications");
  NSArray *result = [active isKindOfClass:NSArray.class] ? active : @[];
  if (IPURUseRequestCache()) IPURCachedActive = result;
  return result;
}

+ (NSArray<NSNumber *> *)activeApplicationPIDs
{
  NSMutableArray<NSNumber *> *pids = [NSMutableArray array];
  for (id element in [self activeApplicationElements]) {
    int pid = [self pidForAXElement:element];
    if (pid > 0) [pids addObject:@(pid)];
  }
  return pids;
}

+ (nullable id)activeApplicationElementForPID:(int)pid
{
  if (pid <= 0) return nil;
  for (id element in [self activeApplicationElements]) {
    if ([self pidForAXElement:element] == pid) return element;
  }
  return nil;
}

+ (nullable id)systemApplicationElement
{
  if (IPURUseRequestCache() && IPURCachedSystem != nil) return IPURCachedSystem;
  id element = IPURObject([self axClient], @"systemApplication");
  if (IPURUseRequestCache()) IPURCachedSystem = element;
  return element;
}

+ (nullable id)foregroundApplicationElementWithProbePoint:(CGPoint)probePoint pid:(int *)pid
{
  return [self foregroundApplicationElementWithProbe:^CGPoint { return probePoint; } pid:pid];
}

+ (nullable id)foregroundApplicationElementWithProbe:(CGPoint (NS_NOESCAPE ^)(void))probe pid:(int *)pid
{
  if (pid != NULL) *pid = 0;
  NSArray *active = [self activeApplicationElements];
  int springBoardPID = [self pidForAXElement:[self systemApplicationElement] ?: NSNull.null];
  NSMutableArray *candidates = [NSMutableArray array];
  id springBoard = nil;
  for (id element in active) {
    int elementPID = [self pidForAXElement:element];
    if (elementPID <= 0) continue;
    BOOL isSpringBoard = springBoardPID > 0
      ? elementPID == springBoardPID
      : [[self bundleIDForPID:elementPID] isEqualToString:IPURSpringBoardBundleID];
    if (isSpringBoard) {
      springBoard = element;
    } else {
      [candidates addObject:element];
    }
  }
  id chosen = nil;
  if (candidates.count == 1) {
    chosen = candidates.firstObject;
  } else if (candidates.count > 1) {
    // Several apps report active (split screen, a PiP, an app extension): ask the AX server
    // who owns the probe point, like runner's active-app detection point.
    id axClient = [self axClient];
    SEL hitTest = NSSelectorFromString(@"hitTestElement:withPoint:error:");
    if (IPURPointHitTestAvailable(axClient)) {
      NSError *error = nil;
      id hit = nil;
      @try {
        // iOS forwards this point query globally to its remote AX interface;
        // the snapshot context is unused for foreground discovery. Do not call
        // the macOS-only accessibilityElementForElementAtPoint assertion stub.
        hit = ((IPURMsgSendHitTestSnapshot)objc_msgSend)(axClient, hitTest, nil, probe(), &error);
      } @catch (__unused NSException *exception) {
        hit = nil;
      }
      int hitPID = [self pidForAXElement:hit];
      for (id element in candidates) {
        if (hitPID > 0 && [self pidForAXElement:element] == hitPID) {
          chosen = element;
          break;
        }
      }
    }
    if (chosen == nil) chosen = candidates.firstObject;
  } else {
    chosen = springBoard ?: [self systemApplicationElement];
  }
  if (chosen != nil && pid != NULL) *pid = [self pidForAXElement:chosen];
  return chosen;
}

+ (nullable XCUIApplication *)applicationForPID:(int)pid
{
  if (pid <= 0) return nil;
  id monitor = IPURObject(XCUIDevice.sharedDevice, @"applicationMonitor");
  SEL selector = NSSelectorFromString(@"monitoredApplicationWithProcessIdentifier:");
  if (monitor == nil || ![monitor respondsToSelector:selector]) return nil;
  id application = nil;
  @try {
    application = ((IPURMsgSendObjectInt)objc_msgSend)(monitor, selector, pid);
  } @catch (__unused NSException *exception) {
    application = nil;
  }
  return [application isKindOfClass:XCUIApplication.class] ? application : nil;
}

+ (NSInteger)interfaceOrientation
{
  static XCUIApplication *springBoard;
  static NSInteger cached;
  static CFAbsoluteTime cachedAt;
  static dispatch_once_t once;
  static NSObject *lock;
  dispatch_once(&once, ^{
    springBoard = [[XCUIApplication alloc] initWithBundleIdentifier:IPURSpringBoardBundleID];
    lock = [NSObject new];
  });
  @synchronized(lock) {
    if (cachedAt > 0 && CFAbsoluteTimeGetCurrent() - cachedAt < 0.25) return cached;
  }
  SEL selector = NSSelectorFromString(@"interfaceOrientation");
  if (![springBoard respondsToSelector:selector]) return 0;
  NSInteger orientation = 0;
  @try {
    orientation = (NSInteger)((IPURMsgSendLongLong)objc_msgSend)(springBoard, selector);
  } @catch (__unused NSException *exception) {
    orientation = 0;
  }
  if (orientation < 1 || orientation > 4) orientation = 0;
  @synchronized(lock) {
    cached = orientation;
    cachedAt = CFAbsoluteTimeGetCurrent();
  }
  return orientation;
}

+ (nullable NSString *)bundleIDForPID:(int)pid
{
  if (pid <= 0) return nil;
  static NSMutableDictionary<NSNumber *, NSString *> *cache;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    cache = [NSMutableDictionary dictionary];
  });
  @synchronized(cache) {
    NSString *cached = cache[@(pid)];
    if (cached != nil) return cached;
  }
  id bundleID = IPURObject([self applicationForPID:pid], @"bundleID");
  if (![bundleID isKindOfClass:NSString.class] || [(NSString *)bundleID length] == 0) {
    // XCUIApplicationMonitor applicationProcessWithPID: knows processes XCTest did not launch.
    id monitor = IPURObject(XCUIDevice.sharedDevice, @"applicationMonitor");
    SEL processSelector = NSSelectorFromString(@"applicationProcessWithPID:");
    if (monitor != nil && [monitor respondsToSelector:processSelector]) {
      @try {
        id process = ((IPURMsgSendObjectInt)objc_msgSend)(monitor, processSelector, pid);
        bundleID = IPURObject(process, @"bundleID");
      } @catch (__unused NSException *exception) {
        bundleID = nil;
      }
    }
  }
  if ((![bundleID isKindOfClass:NSString.class] || [(NSString *)bundleID length] == 0)
      && pid == [self pidForAXElement:[self systemApplicationElement] ?: NSNull.null]) {
    bundleID = IPURSpringBoardBundleID;
  }
  if (![bundleID isKindOfClass:NSString.class] || [(NSString *)bundleID length] == 0) return nil;
  @synchronized(cache) {
    cache[@(pid)] = bundleID;
  }
  return bundleID;
}

// MARK: - Accessibility tree

+ (NSString *)elementTypeName:(NSInteger)elementType
{
  static NSArray<NSString *> *names;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    // XCUIElementType raw values 0…82, in declaration order (XCUIElementTypes.h).
    names = @[
      @"Any", @"Other", @"Application", @"Group", @"Window", @"Sheet", @"Drawer", @"Alert",
      @"Dialog", @"Button", @"RadioButton", @"RadioGroup", @"CheckBox", @"DisclosureTriangle",
      @"PopUpButton", @"ComboBox", @"MenuButton", @"ToolbarButton", @"Popover", @"Keyboard",
      @"Key", @"NavigationBar", @"TabBar", @"TabGroup", @"Toolbar", @"StatusBar", @"Table",
      @"TableRow", @"TableColumn", @"Outline", @"OutlineRow", @"Browser", @"CollectionView",
      @"Slider", @"PageIndicator", @"ProgressIndicator", @"ActivityIndicator",
      @"SegmentedControl", @"Picker", @"PickerWheel", @"Switch", @"Toggle", @"Link", @"Image",
      @"Icon", @"SearchField", @"ScrollView", @"ScrollBar", @"StaticText", @"TextField",
      @"SecureTextField", @"DatePicker", @"TextView", @"Menu", @"MenuItem", @"MenuBar",
      @"MenuBarItem", @"Map", @"WebView", @"IncrementArrow", @"DecrementArrow", @"Timeline",
      @"RatingIndicator", @"ValueIndicator", @"SplitGroup", @"Splitter", @"RelevanceIndicator",
      @"ColorWell", @"HelpTag", @"Matte", @"DockItem", @"Ruler", @"RulerMarker", @"Grid",
      @"LevelIndicator", @"Cell", @"LayoutArea", @"LayoutItem", @"Handle", @"Stepper", @"Tab",
      @"TouchBar", @"StatusItem",
    ];
  });
  NSString *name = (elementType >= 0 && elementType < (NSInteger)names.count) ? names[elementType] : @"Other";
  return [@"XCUIElementType" stringByAppendingString:name];
}

/// The nine AX attributes the serializer reads, mapped from snapshot key paths by XCElementSnapshot
/// (the AX server ignores raw key-path strings). The mapper adds expensive extras (automation type,
/// window display id, base type); only the nine needed attributes are kept.
+ (NSArray *)snapshotAttributes
{
  static NSArray *attributes;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    NSArray<NSString *> *keyPaths = @[
      @"elementType", @"identifier", @"label", @"value", @"placeholderValue", @"frame",
      @"enabled", @"selected", @"hasFocus", @"children",
    ];
    NSArray *mappedAttributes = keyPaths;
    Class snapshotClass = NSClassFromString(@"XCElementSnapshot");
    SEL mapSelector = NSSelectorFromString(@"axAttributesForElementSnapshotKeyPaths:isMacOS:");
    if ([snapshotClass respondsToSelector:mapSelector]) {
      id mapped = ((IPURMsgSendMapAttributes)objc_msgSend)(snapshotClass, mapSelector, keyPaths, NO);
      if ([mapped isKindOfClass:NSSet.class]) mapped = [(NSSet *)mapped allObjects];
      if ([mapped isKindOfClass:NSArray.class] && [(NSArray *)mapped count] > 0) {
        NSArray<NSString *> *needed = @[
          @"ElementType", @"Identifier", @"Label", @"Value", @"PlaceholderValue", @"Frame",
          @"Enabled", @"Selected", @"Focus",
        ];
        NSMutableArray *filtered = [NSMutableArray array];
        for (id attribute in (NSArray *)mapped) {
          NSString *name = [attribute description];
          for (NSString *suffix in needed) {
            if ([name hasSuffix:suffix]) {
              [filtered addObject:attribute];
              break;
            }
          }
        }
        mappedAttributes = filtered.count > 0 ? filtered.copy : mapped;
      }
    }
    attributes = mappedAttributes;
  });
  return attributes;
}

+ (nullable NSDictionary<NSString *, id> *)readValueForAXElement:(id)element
                                               maxUTF16Length:(NSUInteger)limit
                                                  placeholder:(NSString *)placeholder
                                                        error:(NSString **)errorMessage
{
  id client = [self axClient];
  SEL selector = NSSelectorFromString(@"attributesForElement:attributes:error:");
  if (client == nil || ![client respondsToSelector:selector]) {
    if (errorMessage) *errorMessage = @"full AX value read unavailable";
    return nil;
  }
  NSMethodSignature *signature = [client methodSignatureForSelector:selector];
  if (signature.numberOfArguments != 5 || signature.methodReturnType[0] != '@' ||
      [signature getArgumentTypeAtIndex:4][0] != '^') {
    if (errorMessage) *errorMessage = @"full AX value selector shape unavailable";
    return nil;
  }
  NSError *error = nil;
  id attributes = nil;
  @try {
    typedef id (*ReadAttributes)(id, SEL, id, id, NSError **);
    attributes = ((ReadAttributes)objc_msgSend)(client, selector, element,
      @[@"XC_kAXXCAttributeValue"], &error);
  } @catch (__unused NSException *exception) {
    if (errorMessage) *errorMessage = @"full AX value read raised an exception";
    return nil;
  }
  if (![attributes isKindOfClass:NSDictionary.class] || attributes[@"XC_kAXXCAttributeValue"] == nil) {
    if (errorMessage) *errorMessage = @"full AX value attribute unavailable";
    return nil;
  }
  id raw = attributes[@"XC_kAXXCAttributeValue"];
  if (raw != NSNull.null && ![raw isKindOfClass:NSString.class] && ![raw isKindOfClass:NSNumber.class]) {
    if (errorMessage) *errorMessage = @"full AX value is not scalar";
    return nil;
  }
  NSString *value = raw == NSNull.null ? nil : ([raw isKindOfClass:NSString.class] ? raw : [(NSNumber *)raw stringValue]);
  NSUInteger total = value.length;
  limit = MAX((NSUInteger)1, MIN((NSUInteger)65536, limit));
  NSUInteger end = MIN(total, limit);
  if (end < total && end > 0 && CFStringIsSurrogateHighCharacter([value characterAtIndex:end - 1]) &&
      CFStringIsSurrogateLowCharacter([value characterAtIndex:end])) end -= 1;
  NSMutableDictionary *result = [@{@"value": value == nil ? (id)NSNull.null : [value substringToIndex:end],
           @"complete": end == total ? @YES : @NO, @"truncated": end < total ? @YES : @NO,
           @"utf16_length": @(total), @"returned_utf16_length": @(end),
           @"limit_utf16": @(limit), @"source": @"ax_value_attribute"} mutableCopy];
  // Compare before clipping, including when the caller requests only one UTF-16
  // unit. This private flag alone does not establish that the field is empty.
  result[@"_value_matches_placeholder"] = @([value isEqualToString:placeholder] && placeholder.length > 0);
  return result;
}

// Selection metadata belongs to the same live AX element as its value. Keep the
// actual text private to input operations; the read-only route exposes only counts.
static NSDictionary *IPURReadTextSelection(id client, id element, NSString **errorMessage)
{
  SEL read = NSSelectorFromString(@"attributesForElement:attributes:error:");
  if (client == nil || ![client respondsToSelector:read]) {
    if (errorMessage) *errorMessage = @"AX text selection read unavailable";
    return nil;
  }
  NSMethodSignature *signature = [client methodSignatureForSelector:read];
  if (signature.numberOfArguments != 5 || signature.methodReturnType[0] != '@' ||
      [signature getArgumentTypeAtIndex:4][0] != '^') {
    if (errorMessage) *errorMessage = @"AX text selection read signature unavailable";
    return nil;
  }
  @try {
    NSError *error = nil;
    id attributes = ((id (*)(id, SEL, id, id, NSError **))objc_msgSend)(client, read, element,
      @[@"XC_kAXXCAttributeValue", @"XC_kAXXCAttributeSelectedTextRange"], &error);
    if (![attributes isKindOfClass:NSDictionary.class]) {
      if (errorMessage) *errorMessage = @"AX text selection response unavailable";
      return nil;
    }
    if (error != nil) {
      if (errorMessage) *errorMessage = @"AX text selection read failed";
      return nil;
    }
    id value = attributes[@"XC_kAXXCAttributeValue"];
    id selected = attributes[@"XC_kAXXCAttributeSelectedTextRange"];
    NSString *text = value == NSNull.null ? @"" : ([value isKindOfClass:NSString.class] ? value : nil);
    if (text == nil && [value isKindOfClass:NSNumber.class]) text = [value stringValue];
    BOOL nsRange = [selected isKindOfClass:NSValue.class] && strcmp([selected objCType], @encode(NSRange)) == 0;
    BOOL cfRange = [selected isKindOfClass:NSValue.class] && strcmp([selected objCType], @encode(CFRange)) == 0;
    BOOL stringRange = [selected isKindOfClass:NSString.class];
    if (text == nil || (!nsRange && !cfRange && !stringRange)) {
      if (errorMessage) *errorMessage = [NSString stringWithFormat:@"AX selected range unavailable (%@)", NSStringFromClass([selected class]) ?: @"none"];
      return nil;
    }
    NSRange range;
    if (nsRange) range = [selected rangeValue];
    else if (stringRange) {
      NSScanner *scanner = [NSScanner scannerWithString:selected];
      unsigned long long location, length;
      if (![scanner scanString:@"{" intoString:NULL] || ![scanner scanUnsignedLongLong:&location] ||
          ![scanner scanString:@"," intoString:NULL] || ![scanner scanUnsignedLongLong:&length] ||
          ![scanner scanString:@"}" intoString:NULL] || !scanner.isAtEnd ||
          location > NSUIntegerMax || length > NSUIntegerMax) {
        if (errorMessage) *errorMessage = @"AX selected range string is malformed";
        return nil;
      }
      range = NSMakeRange((NSUInteger)location, (NSUInteger)length);
    } else {
      CFRange raw;
      [selected getValue:&raw size:sizeof(raw)];
      if (raw.location < 0 || raw.length < 0) return nil;
      range = NSMakeRange((NSUInteger)raw.location, (NSUInteger)raw.length);
    }
    if (range.location > text.length || range.length > text.length - range.location) {
      if (errorMessage) *errorMessage = @"AX selected range is inconsistent with the current value";
      return nil;
    }
    return @{@"text": text, @"range": selected, @"location": @(range.location), @"length": @(range.length), @"utf16_length": @(text.length)};
  } @catch (__unused NSException *exception) {
    if (errorMessage) *errorMessage = @"AX text selection read raised an exception";
    return nil;
  }
}

+ (nullable NSString *)shortClearKeysForAXElement:(id)element error:(NSString **)errorMessage
{
  NSDictionary *state = IPURReadTextSelection([self axClient], element, errorMessage);
  if (state == nil) return nil;
  NSString *text = state[@"text"];
  if (text.length > 100) return nil;
  NSUInteger start = [state[@"location"] unsignedIntegerValue];
  NSUInteger end = start + [state[@"length"] unsignedIntegerValue];
  // Avoid splitting a composed character when translating a UTF-16 selection
  // into keyboard deletions. Such a caret uses confirmed Select All instead.
  for (NSNumber *boundary in @[@(start), @(end)]) {
    NSUInteger index = boundary.unsignedIntegerValue;
    if (index < text.length && [text rangeOfComposedCharacterSequenceAtIndex:index].location != index) return nil;
  }
  // This text-event path implements backspace reliably, but does not deliver
  // forward-delete on all iOS runtimes. Only plan deletion when no suffix remains.
  // Other caret positions use confirmed Select All before any destructive input.
  if (end != text.length) return nil;
  __block NSUInteger before = 0;
  [text enumerateSubstringsInRange:NSMakeRange(0, start) options:NSStringEnumerationByComposedCharacterSequences
    usingBlock:^(__unused NSString *substring, __unused NSRange range, __unused NSRange enclosing, __unused BOOL *stop) { before++; }];
  NSMutableString *keys = [NSMutableString string];
  if (end > start) [keys appendString:@"\b"];
  for (NSUInteger i = 0; i < before; i++) [keys appendString:@"\b"];
  // Do not append deletes beyond the initial value/selection's composed
  // characters. An App may focus another field when this one becomes empty.
  return keys;
}

static NSString *IPURSynthesize(id record, id path);
static unsigned long long IPURMainDisplayID(void);

static BOOL IPURTextSelectionSynthesisAvailable(void)
{
  Class record = NSClassFromString(@"XCSynthesizedEventRecord");
  Class path = NSClassFromString(@"XCPointerEventPath");
  SEL key = NSSelectorFromString(@"typeKey:modifiers:atOffset:");
  NSMethodSignature *signature = [path instanceMethodSignatureForSelector:key];
  return [record instancesRespondToSelector:NSSelectorFromString(@"initWithName:displayID:")] &&
    [record instancesRespondToSelector:NSSelectorFromString(@"setTargetProcessID:")] &&
    [record instancesRespondToSelector:NSSelectorFromString(@"synthesizeWithError:")] &&
    [path instancesRespondToSelector:NSSelectorFromString(@"initForTextInput")] &&
    signature.numberOfArguments == 5 && signature.methodReturnType[0] == 'v' &&
    [signature getArgumentTypeAtIndex:2][0] == '@' &&
    strchr("QL", [signature getArgumentTypeAtIndex:3][0]) != NULL &&
    [signature getArgumentTypeAtIndex:4][0] == 'd';
}

+ (nullable NSDictionary<NSString *, NSNumber *> *)textSelectionForAXElement:(id)element error:(NSString **)errorMessage
{
  id client = [self axClient];
  NSDictionary *state = IPURReadTextSelection(client, element, errorMessage);
  if (state == nil) return nil;
  return @{@"location": state[@"location"], @"length": state[@"length"], @"utf16_length": state[@"utf16_length"],
           @"can_select": IPURTextSelectionSynthesisAvailable() ? @YES : @NO};
}

+ (nullable NSDictionary<NSString *, id> *)selectAllTextForAXElement:(id)element pid:(int)pid retryState:(NSDictionary<NSString *, id> *)retryState error:(NSString **)errorMessage
{
  id client = [self axClient];
  NSDictionary *before = IPURReadTextSelection(client, element, errorMessage);
  if (before == nil) return nil;
  NSString *text = [before[@"text"] copy];
  if (retryState != nil && (![retryState[@"retry_value"] isKindOfClass:NSString.class] ||
                            ![retryState[@"retry_value"] isEqualToString:text])) {
    if (errorMessage) *errorMessage = @"value changed before selection retry; no further key was sent";
    return nil;
  }
  if (pid <= 0 || [self pidForAXElement:element] != pid) {
    if (errorMessage) *errorMessage = @"selection process no longer matches; nothing was sent";
    return nil;
  }
  if (text.length > 0 && ([before[@"location"] unsignedIntegerValue] != 0 || [before[@"length"] unsignedIntegerValue] != text.length)) {
    if (retryState != nil && (![before[@"location"] isEqual:retryState[@"location"]] ||
                              ![before[@"length"] isEqual:retryState[@"length"]])) {
      if (errorMessage) *errorMessage = @"selection changed before retry; no further key was sent";
      return nil;
    }
    @try {
      Class pathClass = NSClassFromString(@"XCPointerEventPath");
      SEL key = NSSelectorFromString(@"typeKey:modifiers:atOffset:");
      if (!IPURTextSelectionSynthesisAvailable()) {
        if (errorMessage) *errorMessage = @"targeted selection key unavailable; nothing was sent";
        return nil;
      }
      Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
      SEL initRecord = NSSelectorFromString(@"initWithName:displayID:");
      SEL target = NSSelectorFromString(@"setTargetProcessID:");
      if (![recordClass instancesRespondToSelector:initRecord] || ![recordClass instancesRespondToSelector:target]) {
        if (errorMessage) *errorMessage = @"targeted selection record unavailable; nothing was sent";
        return nil;
      }
      // Keyboard records use the main display and no touch orientation, matching
      // XCTest's keyboard-event shape. They retain the original target process.
      id record = ((id (*)(id, SEL, id, unsigned long long))objc_msgSend)(
        [recordClass alloc], initRecord, @"ipu-select-all", IPURMainDisplayID());
      if (record == nil) { if (errorMessage) *errorMessage = @"selection record unavailable; nothing was sent"; return nil; }
      ((IPURMsgSendSetLongLong)objc_msgSend)(record, target, (long long)pid);
      id path = ((id (*)(id, SEL))objc_msgSend)([pathClass alloc], NSSelectorFromString(@"initForTextInput"));
      if (path == nil) { if (errorMessage) *errorMessage = @"selection key path unavailable; nothing was sent"; return nil; }
      ((void (*)(id, SEL, id, unsigned long long, double))objc_msgSend)(path, key, @"a", (unsigned long long)XCUIKeyModifierCommand, 0.0);
      NSString *failure = IPURSynthesize(record, path);
      if (failure != nil) { if (errorMessage) *errorMessage = @"selection key outcome unknown; no delete was sent"; return nil; }
    } @catch (__unused NSException *exception) {
      if (errorMessage) *errorMessage = @"selection key raised an exception; no delete was sent";
      return nil;
    }
  }
  NSTimeInterval until = NSProcessInfo.processInfo.systemUptime + 0.75;
  BOOL selectionUnchanged = YES;
  for (;;) {
    NSDictionary *after = IPURReadTextSelection(client, element, errorMessage);
    if (after == nil) return nil;
    if (after != nil && ![after[@"text"] isEqualToString:text]) {
      if (errorMessage) *errorMessage = @"value changed during selection; no delete was sent";
      return nil;
    }
    if (after != nil && [after[@"location"] unsignedIntegerValue] == 0 &&
        [after[@"length"] unsignedIntegerValue] == text.length) break;
    if (![after[@"location"] isEqual:before[@"location"]] || ![after[@"length"] isEqual:before[@"length"]]) selectionUnchanged = NO;
    if (NSProcessInfo.processInfo.systemUptime >= until) {
      if (selectionUnchanged && [after[@"location"] isEqual:before[@"location"]] && [after[@"length"] isEqual:before[@"length"]]) {
        // Synthesis acknowledged, all readbacks succeeded and the original value
        // and selection are unchanged. No destructive input has been dispatched.
        return @{@"confirmed": @NO, @"no_effect": @YES, @"retry_value": text,
                 @"location": before[@"location"], @"length": before[@"length"],
                 @"utf16_length": @(text.length)};
      }
      if (errorMessage) *errorMessage = @"full selection could not be confirmed; no delete was sent";
      return nil;
    }
    [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]];
  }
  return @{@"confirmed": @YES, @"utf16_length": @(text.length), @"location": @0, @"length": @(text.length)};
}

+ (nullable id)requestSnapshotForElement:(id)element
                                maxDepth:(NSInteger)maxDepth
                                maxNodes:(NSInteger)maxNodes
                                   error:(NSString **)errorMessage
{
  id axClient = [self axClient];
  SEL selector = NSSelectorFromString(@"requestSnapshotForElement:attributes:parameters:error:");
  if (axClient == nil || ![axClient respondsToSelector:selector]) {
    if (errorMessage) *errorMessage = @"XCAXClient requestSnapshotForElement:attributes:parameters:error: unavailable";
    return nil;
  }
  NSMutableDictionary *parameters = [NSMutableDictionary dictionary];
  id defaults = IPURObject(axClient, @"defaultParameters");
  if ([defaults isKindOfClass:NSDictionary.class]) [parameters addEntriesFromDictionary:defaults];
  parameters[@"maxDepth"] = @(MAX(1, maxDepth));
  parameters[@"maxChildren"] = @(MAX(1, maxNodes));
  parameters[@"maxArrayCount"] = @(MAX(1, maxNodes));
  parameters[@"traverseFromParentsToChildren"] = @YES;

  NSError *error = nil;
  id result = nil;
  @try {
    result = ((IPURMsgSendSnapshotRequest)objc_msgSend)(
      axClient, selector, element, [self snapshotAttributes], parameters.copy, &error);
  } @catch (NSException *exception) {
    if (errorMessage) *errorMessage = [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
    return nil;
  }
  if (result == nil) {
    if (errorMessage) *errorMessage = error.localizedDescription ?: @"AX snapshot request returned nil";
    return nil;
  }
  id root = nil;
  @try {
    root = [result valueForKey:@"_rootElementSnapshot"];
  } @catch (__unused NSException *exception) {
    root = nil;
  }
  return root ?: result;
}

static id IPURKVC(id snapshot, NSString *key)
{
  @try {
    id value = [snapshot valueForKey:key];
    return value == NSNull.null ? nil : value;
  } @catch (__unused NSException *exception) {
    return nil;
  }
}

static NSString *IPURNonEmptyString(id value)
{
  if (value == nil) return nil;
  NSString *string = [value isKindOfClass:NSString.class] ? value : [value description];
  return string.length > 0 ? string : nil;
}

static NSString *IPURValueString(id value)
{
  if (value == nil) return nil;
  if ([value isKindOfClass:NSString.class]) return value;
  if ([value isKindOfClass:NSNumber.class]) return [(NSNumber *)value stringValue];
  return [value description];
}

static NSDictionary *IPURRect(id snapshot)
{
  CGRect frame = CGRectZero;
  id value = IPURKVC(snapshot, @"frame");
  if ([value isKindOfClass:NSValue.class] && strcmp([(NSValue *)value objCType], @encode(CGRect)) == 0) {
    [(NSValue *)value getValue:&frame];
  }
  if (CGRectIsNull(frame) || CGRectIsInfinite(frame)) frame = CGRectZero;
  return @{
    @"x": @(frame.origin.x),
    @"y": @(frame.origin.y),
    @"width": @(frame.size.width),
    @"height": @(frame.size.height),
  };
}

// A valid system frame is this device's screen and does not change: callers apply the
// interface orientation from its short and long sides. So it is read once per runner launch
// rather than once per request (a snapshot round trip before every touch). The UIKit fallback
// is not kept, so a later call tries the system frame again.
static CGSize IPURStableScreenSize;
static BOOL IPURHasStableScreenSize = NO;

+ (CGSize)screenSizePoints
{
  @synchronized (self) {
    if (IPURHasStableScreenSize) return IPURStableScreenSize;
  }
  if (IPURUseRequestCache() && IPURCachedScreenSize != nil) return IPURCachedScreenSize.CGSizeValue;
  // Read the system root only: no foreground resolution (which itself needs screenCenter),
  // children, or application content. UIKit's nativeBounds can be compatibility-sized too.
  id system = [self systemApplicationElement];
  id snapshot = system == nil ? nil : [self requestSnapshotForElement:system
                                                            maxDepth:1 maxNodes:1 error:NULL];
  CGRect frame = CGRectZero;
  id value = IPURKVC(snapshot, @"frame");
  if ([value isKindOfClass:NSValue.class] && strcmp([(NSValue *)value objCType], @encode(CGRect)) == 0) {
    [(NSValue *)value getValue:&frame];
  }
  CGSize systemSize = IPURScreenSizeFromFrame(frame, CGSizeZero);
  if (systemSize.width > 0) {
    @synchronized (self) {
      IPURStableScreenSize = systemSize;
      IPURHasStableScreenSize = YES;
    }
    return systemSize;
  }
  CGSize size = UIScreen.mainScreen.bounds.size;
  if (IPURUseRequestCache()) IPURCachedScreenSize = [NSValue valueWithCGSize:size];
  return size;
}

static NSString *IPURBoolString(id snapshot, NSString *key, BOOL fallback)
{
  id value = IPURKVC(snapshot, key);
  BOOL flag = [value respondsToSelector:@selector(boolValue)] ? [value boolValue] : fallback;
  return flag ? @"1" : @"0";
}

static NSArray *IPURChildren(id snapshot)
{
  id children = IPURKVC(snapshot, @"children");
  return [children isKindOfClass:NSArray.class] ? children : @[];
}

/// One snapshot node in runner's /source?format=json shape. No isVisible / isAccessible / isHittable:
/// computing them costs extra AX round trips per node, which is exactly what this runner avoids.
static NSMutableDictionary *IPURSerialize(
  id snapshot, NSInteger depth, IPURWalk *walk, NSMutableArray<IPURFrontier *> *leaves)
{
  if (snapshot == nil) return nil;
  if (walk->nodeCount >= walk->maxNodes) {
    walk->truncated = YES;
    return nil;
  }
  walk->nodeCount += 1;

  NSMutableDictionary *node = [NSMutableDictionary dictionaryWithCapacity:12];
  id typeValue = IPURKVC(snapshot, @"elementType");
  NSInteger elementType = [typeValue respondsToSelector:@selector(integerValue)] ? [typeValue integerValue] : 1;
  NSString *identifier = IPURNonEmptyString(IPURKVC(snapshot, @"identifier"));
  NSString *label = IPURNonEmptyString(IPURKVC(snapshot, @"label"));
  NSString *value = IPURValueString(IPURKVC(snapshot, @"value"));
  NSString *placeholder = IPURNonEmptyString(IPURKVC(snapshot, @"placeholderValue"));

  node[@"type"] = [IPURBridge elementTypeName:elementType];
  node[@"label"] = label ?: (id)NSNull.null;
  // runner's name: the identifier when there is one, else the label.
  node[@"name"] = identifier ?: label ?: (id)NSNull.null;
  node[@"value"] = value ?: (id)NSNull.null;
  id valueTruncated = IPURKVC(snapshot, @"isTruncatedValue");
  if ([valueTruncated isKindOfClass:NSNumber.class]) node[@"value_truncated"] = @([valueTruncated boolValue]);
  node[@"rawIdentifier"] = identifier ?: (id)NSNull.null;
  node[@"placeholderValue"] = placeholder ?: (id)NSNull.null;
  node[@"rect"] = IPURRect(snapshot);
  node[@"isEnabled"] = IPURBoolString(snapshot, @"enabled", YES);
  // hasFocus is the focus engine's (tvOS-style) flag; a text field holding the keyboard reports
  // only hasKeyboardFocus (hardware: Settings' search field, iOS 27).
  BOOL focused = [IPURKVC(snapshot, @"hasFocus") boolValue] || [IPURKVC(snapshot, @"hasKeyboardFocus") boolValue];
  node[@"isFocused"] = focused ? @"1" : @"0";
  // Selected (a picked photo in select mode, the current tab, a chosen segment). The attribute was
  // already fetched with the snapshot; only a selected node carries the key, so the common case
  // costs no bytes.
  if ([IPURKVC(snapshot, @"selected") boolValue]) node[@"isSelected"] = @"1";
  if (walk->includeElements) {
    id element = IPURKVC(snapshot, @"accessibilityElement");
    if (element != nil) node[IPURNodeAXElementKey] = element;
  }

  NSMutableArray *children = [NSMutableArray array];
  for (id child in IPURChildren(snapshot)) {
    NSMutableDictionary *childNode = IPURSerialize(child, depth + 1, walk, leaves);
    if (childNode != nil) [children addObject:childNode];
    if (walk->nodeCount >= walk->maxNodes) {
      walk->truncated = YES;
      break;
    }
  }
  if (children.count > 0) {
    node[@"children"] = children;
  } else if (leaves != nil) {
    IPURFrontier *leaf = [[IPURFrontier alloc] init];
    leaf.snapshot = snapshot;
    leaf.node = node;
    leaf.depth = depth;
    [leaves addObject:leaf];
  }
  return node;
}

/// Only childless nodes on the request's deepest possible level can be branches whose children
/// the server withheld (it emits `maxDepth` node levels, so the deepest is maxDepth - 1).
static NSMutableArray<IPURFrontier *> *IPURCappedFrontiers(NSArray<IPURFrontier *> *leaves, NSInteger maxDepth)
{
  NSMutableArray<IPURFrontier *> *frontiers = [NSMutableArray array];
  NSInteger deepest = -1;
  for (IPURFrontier *leaf in leaves) deepest = MAX(deepest, leaf.depth);
  if (deepest < maxDepth - 1) return frontiers;
  for (IPURFrontier *leaf in leaves) {
    if (leaf.depth == deepest) [frontiers addObject:leaf];
  }
  return frontiers;
}

/// A remembered depth expires: the app may have left the screen that needed it (the key is a pid,
/// and the same app on a lighter screen accepts the full depth again).
static const NSTimeInterval IPURRememberedDepthSeconds = 30;

static NSMutableDictionary<NSString *, NSDate *> *IPURAcceptedDepthTimes(void)
{
  static NSMutableDictionary<NSString *, NSDate *> *times;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    times = [NSMutableDictionary dictionary];
  });
  return times;
}

static NSMutableDictionary<NSString *, NSNumber *> *IPURAcceptedDepths(void)
{
  static NSMutableDictionary<NSString *, NSNumber *> *depths;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    depths = [NSMutableDictionary dictionary];
  });
  return depths;
}

+ (NSDictionary<NSString *, id> *)runnerTreeForAXElement:(id)axElement
                                             maxDepth:(NSInteger)maxDepth
                                             maxNodes:(NSInteger)maxNodes
                                   extensionCallLimit:(NSInteger)extensionCallLimit
                                          rememberKey:(nullable NSString *)rememberKey
{
  return [self runnerTreeForAXElement:axElement
                          maxDepth:maxDepth
                          maxNodes:maxNodes
                extensionCallLimit:extensionCallLimit
                       rememberKey:rememberKey
                   includeElements:NO];
}

+ (NSDictionary<NSString *, id> *)runnerTreeForAXElement:(id)axElement
                                             maxDepth:(NSInteger)maxDepth
                                             maxNodes:(NSInteger)maxNodes
                                   extensionCallLimit:(NSInteger)extensionCallLimit
                                          rememberKey:(nullable NSString *)rememberKey
                                      includeElements:(BOOL)includeElements
{
  maxDepth = MAX(1, maxDepth);
  maxNodes = MAX(1, maxNodes);
  NSMutableArray<NSNumber *> *depths = [NSMutableArray arrayWithObject:@(maxDepth)];
  for (size_t index = 0; index < sizeof(IPURDepthLadder) / sizeof(IPURDepthLadder[0]); index++) {
    if (IPURDepthLadder[index] < maxDepth) [depths addObject:@(IPURDepthLadder[index])];
  }
  NSNumber *remembered = nil;
  if (rememberKey != nil) {
    @synchronized(IPURAcceptedDepths()) {
      remembered = IPURAcceptedDepths()[rememberKey];
      NSDate *at = IPURAcceptedDepthTimes()[rememberKey];
      if (remembered != nil && (at == nil || -at.timeIntervalSinceNow > IPURRememberedDepthSeconds)) {
        [IPURAcceptedDepths() removeObjectForKey:rememberKey];
        [IPURAcceptedDepthTimes() removeObjectForKey:rememberKey];
        remembered = nil;
      }
    }
  }
  if (remembered != nil && remembered.integerValue < maxDepth) {
    NSIndexSet *keep = [depths indexesOfObjectsPassingTest:^BOOL(NSNumber *depth, NSUInteger idx, BOOL *stop) {
      return depth.integerValue <= remembered.integerValue;
    }];
    if (keep.count > 0) depths = [[depths objectsAtIndexes:keep] mutableCopy];
  }

  id root = nil;
  NSInteger acceptedDepth = 0;
  NSString *lastError = @"AX snapshot request failed";
  for (NSNumber *depth in depths) {
    NSString *error = nil;
    root = [self requestSnapshotForElement:axElement maxDepth:depth.integerValue maxNodes:maxNodes error:&error];
    if (root != nil) {
      acceptedDepth = depth.integerValue;
      break;
    }
    lastError = error ?: lastError;
    NSLog(@"ipu-runner: ax snapshot rejected at depth %ld: %@", (long)depth.integerValue, lastError);
  }
  if (root == nil) {
    return @{IPURTreeOkKey: @NO, IPURTreeErrorKey: lastError};
  }
  if (rememberKey != nil && acceptedDepth < maxDepth) {
    @synchronized(IPURAcceptedDepths()) {
      IPURAcceptedDepths()[rememberKey] = @(acceptedDepth);
      IPURAcceptedDepthTimes()[rememberKey] = [NSDate date];
    }
  }

  IPURWalk walk = {.nodeCount = 0, .maxNodes = maxNodes, .truncated = NO, .includeElements = includeElements};
  NSMutableArray<IPURFrontier *> *leaves = extensionCallLimit > 0 ? [NSMutableArray array] : nil;
  NSMutableDictionary *rootNode = IPURSerialize(root, 0, &walk, leaves);
  if (rootNode == nil) {
    return @{IPURTreeOkKey: @NO, IPURTreeErrorKey: @"AX snapshot root could not be serialized"};
  }

  // Re-root the same request at each depth-capped frontier: the depth limit is per request, so
  // this reaches content the app-rooted request could not, without a larger depth parameter.
  NSInteger calls = 0;
  NSMutableArray<IPURFrontier *> *frontiers = IPURCappedFrontiers(leaves ?: @[], acceptedDepth);
  while (frontiers.count > 0) {
    if (calls >= extensionCallLimit || walk.nodeCount >= maxNodes) {
      walk.truncated = YES;
      break;
    }
    IPURFrontier *frontier = frontiers.firstObject;
    [frontiers removeObjectAtIndex:0];
    id element = IPURKVC(frontier.snapshot, @"accessibilityElement");
    if (element == nil) continue;
    calls += 1;
    id subRoot = [self requestSnapshotForElement:element
                                        maxDepth:acceptedDepth
                                        maxNodes:maxNodes - walk.nodeCount
                                           error:NULL];
    if (subRoot == nil) continue;
    NSMutableArray<IPURFrontier *> *subLeaves = [NSMutableArray array];
    NSMutableArray *children = [NSMutableArray array];
    for (id child in IPURChildren(subRoot)) {
      NSMutableDictionary *childNode = IPURSerialize(child, 1, &walk, subLeaves);
      if (childNode != nil) [children addObject:childNode];
      if (walk.nodeCount >= maxNodes) {
        walk.truncated = YES;
        break;
      }
    }
    if (children.count > 0) frontier.node[@"children"] = children;
    [frontiers addObjectsFromArray:IPURCappedFrontiers(subLeaves, acceptedDepth)];
  }

  return @{
    IPURTreeOkKey: @YES,
    IPURTreeRootKey: rootNode,
    IPURTreeNodeCountKey: @(walk.nodeCount),
    IPURTreeDepthKey: @(acceptedDepth),
    IPURTreeTruncatedKey: @(walk.truncated),
    IPURTreeExtensionCallsKey: @(calls),
  };
}

+ (NSDictionary<NSString *, id> *)runnerTreeForSnapshot:(id)snapshot maxNodes:(NSInteger)maxNodes
{
  return [self runnerTreeForSnapshot:snapshot maxNodes:maxNodes includeElements:NO];
}

+ (NSDictionary<NSString *, id> *)runnerTreeForSnapshot:(id)snapshot
                                            maxNodes:(NSInteger)maxNodes
                                     includeElements:(BOOL)includeElements
{
  IPURWalk walk = {.nodeCount = 0, .maxNodes = MAX(1, maxNodes), .truncated = NO, .includeElements = includeElements};
  NSMutableDictionary *rootNode = IPURSerialize(snapshot, 0, &walk, nil);
  if (rootNode == nil) {
    return @{IPURTreeOkKey: @NO, IPURTreeErrorKey: @"snapshot could not be serialized"};
  }
  return @{
    IPURTreeOkKey: @YES,
    IPURTreeRootKey: rootNode,
    IPURTreeNodeCountKey: @(walk.nodeCount),
    IPURTreeDepthKey: @0,
    IPURTreeTruncatedKey: @(walk.truncated),
    IPURTreeExtensionCallsKey: @0,
  };
}

// MARK: - Event synthesis

+ (BOOL)eventSynthesisAvailable
{
  Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
  Class pathClass = NSClassFromString(@"XCPointerEventPath");
  return recordClass != Nil && pathClass != Nil
    && [recordClass instancesRespondToSelector:NSSelectorFromString(@"synthesizeWithError:")]
    && [recordClass instancesRespondToSelector:NSSelectorFromString(@"addPointerEventPath:")]
    && [pathClass instancesRespondToSelector:NSSelectorFromString(@"initForTouchAtPoint:offset:")]
    && [pathClass instancesRespondToSelector:NSSelectorFromString(@"moveToPoint:atOffset:")]
    && [pathClass instancesRespondToSelector:NSSelectorFromString(@"liftUpAtOffset:")]
    && ([recordClass instancesRespondToSelector:NSSelectorFromString(@"initWithName:displayID:interfaceOrientation:")]
        || [recordClass instancesRespondToSelector:NSSelectorFromString(@"initWithName:interfaceOrientation:")]);
}

/// UIInterfaceOrientation the touch points are given in: the interface orientation on screen, not
/// the physical device orientation (a phone lying on its side reads landscape while its UI stays
/// portrait); unknown falls back to portrait.
static long long IPURInterfaceOrientation(void)
{
  NSInteger orientation = 1;
  @try {
    orientation = [IPURBridge interfaceOrientation];
  } @catch (__unused NSException *exception) {
    orientation = 1;
  }
  return (orientation >= 1 && orientation <= 4) ? orientation : 1;
}

// Where synthesized touches spent their time since the last takeSynthesisTiming (main thread):
// the orientation read for the record, building the record, and synthesizeWithError: — the
// testmanagerd round trip that plays the events in real time (so it covers the scheduled hold) and
// answers once they were delivered. Hardware, iPhone 13 / iOS 27: Wait = Hold + ~220 ms for every
// hold tried (20, 50, 100, 200 ms); the orientation read and the build take under 1 ms, and a 10 ms
// implicit-confirmation interval did not shorten Wait, so the fixed part is testmanagerd's own.
static double IPURSynthOrientationMs, IPURSynthBuildMs, IPURSynthWaitMs, IPURSynthHoldMs;
static NSInteger IPURSynthCalls;

static double IPURNowMs(void)
{
  return CFAbsoluteTimeGetCurrent() * 1000.0;
}

+ (NSDictionary<NSString *, NSNumber *> *)takeSynthesisTiming
{
  NSDictionary *timing = IPURSynthCalls == 0 ? @{} : @{
    @"Orientation": @(IPURSynthOrientationMs),
    @"Build": @(IPURSynthBuildMs),
    @"Wait": @(IPURSynthWaitMs),
    @"Hold": @(IPURSynthHoldMs),
    @"Calls": @(IPURSynthCalls),
  };
  IPURSynthOrientationMs = IPURSynthBuildMs = IPURSynthWaitMs = IPURSynthHoldMs = 0;
  IPURSynthCalls = 0;
  return timing;
}

/// Runs synthesizeWithError: on a built record, timing it (and the record's last offset).
static BOOL IPURSynthesizeRecord(id record, NSError **error)
{
  SEL maximumOffset = NSSelectorFromString(@"maximumOffset");
  if ([record respondsToSelector:maximumOffset]) {
    IPURSynthHoldMs += ((double (*)(id, SEL))objc_msgSend)(record, maximumOffset) * 1000.0;
  }
  double started = IPURNowMs();
  BOOL ok = ((IPURMsgSendSynthesize)objc_msgSend)(record, NSSelectorFromString(@"synthesizeWithError:"), error);
  IPURSynthWaitMs += IPURNowMs() - started;
  IPURSynthCalls += 1;
  return ok;
}

static unsigned long long IPURMainDisplayID(void)
{
  static unsigned long long displayID;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    id screen = XCUIScreen.mainScreen;
    SEL selector = NSSelectorFromString(@"displayID");
    if ([screen respondsToSelector:selector]) {
      displayID = (unsigned long long)((IPURMsgSendLongLong)objc_msgSend)(screen, selector);
    }
  });
  return displayID;
}

/// The interface orientation the touch paths being built are turned from (main thread; set per record).
static long long IPURTouchOrientation = 1;

static CGPoint IPURTouchPoint(CGPoint point)
{
  return IPURPortraitPoint(point, IPURTouchOrientation, [IPURBridge screenSizePoints]);
}

static NSString *IPURCreateGestureRecord(NSString *name, int pid, id *record)
{
  if (![IPURBridge eventSynthesisAvailable]) {
    return @"private XCTest event synthesis unavailable (XCSynthesizedEventRecord / XCPointerEventPath)";
  }
  Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
  SEL displaySelector = NSSelectorFromString(@"initWithName:displayID:interfaceOrientation:");
  SEL orientationSelector = NSSelectorFromString(@"initWithName:interfaceOrientation:");
  double readStarted = IPURNowMs();
  IPURTouchOrientation = IPURInterfaceOrientation();
  IPURSynthOrientationMs += IPURNowMs() - readStarted;
  // The record takes portrait points: its orientation did not turn them on hardware (see
  // IPURPortraitPoint), so the paths below turn them and the record says portrait.
  long long orientation = 1;
  unsigned long long displayID = IPURMainDisplayID();
  id created = nil;
  if (displayID != 0 && [recordClass instancesRespondToSelector:displaySelector]) {
    created = ((IPURMsgSendInitRecordDisplay)objc_msgSend)(
      [recordClass alloc], displaySelector, name, displayID, orientation);
  } else if ([recordClass instancesRespondToSelector:orientationSelector]) {
    created = ((IPURMsgSendInitRecordOrientation)objc_msgSend)(
      [recordClass alloc], orientationSelector, name, orientation);
  } else {
    created = ((IPURMsgSendInitRecordDisplay)objc_msgSend)(
      [recordClass alloc], displaySelector, name, displayID, orientation);
  }
  if (created == nil) return @"private XCTest event synthesis failed: could not create event record";
  SEL targetSelector = NSSelectorFromString(@"setTargetProcessID:");
  if (pid > 0) {
    if (![created respondsToSelector:targetSelector]) return @"targeted gesture synthesis unavailable; nothing was sent";
    ((IPURMsgSendSetLongLong)objc_msgSend)(created, targetSelector, (long long)pid);
  }
  *record = created;
  return nil;
}

static id IPURNewTouchPath(CGPoint point, double offset)
{
  Class pathClass = NSClassFromString(@"XCPointerEventPath");
  return ((IPURMsgSendInitPath)objc_msgSend)(
    [pathClass alloc], NSSelectorFromString(@"initForTouchAtPoint:offset:"), IPURTouchPoint(point), offset);
}

static void IPURMove(id path, CGPoint point, double offset)
{
  ((IPURMsgSendPathMove)objc_msgSend)(path, NSSelectorFromString(@"moveToPoint:atOffset:"), IPURTouchPoint(point), offset);
}

static void IPURLift(id path, double offset)
{
  ((IPURMsgSendPathOffset)objc_msgSend)(path, NSSelectorFromString(@"liftUpAtOffset:"), offset);
}

static NSString *IPURSynthesize(id record, id path)
{
  ((IPURMsgSendAddPath)objc_msgSend)(record, NSSelectorFromString(@"addPointerEventPath:"), path);
  NSError *error = nil;
  [IPURBridge invalidateRequestCache];  // a touch can change the foreground app
  BOOL ok = IPURSynthesizeRecord(record, &error);
  if (!ok) {
    return [NSString stringWithFormat:@"private XCTest event synthesis failed: %@",
                                      error.localizedDescription ?: @"synthesizeWithError returned NO"];
  }
  return nil;
}

+ (nullable NSString *)synthesizeTapAt:(CGPoint)point pid:(int)pid
{
  return [self synthesizeLongPressAt:point duration:0.05 pid:pid name:@"ipu-tap"];
}

+ (BOOL)focusRecoveryAvailable
{
  @try {
    Class record = NSClassFromString(@"XCSynthesizedEventRecord");
    if (![self eventSynthesisAvailable] || ![record instancesRespondToSelector:NSSelectorFromString(@"setTargetProcessID:")]) return NO;
    return IPURPointHitTestAvailable([self axClient]);
  } @catch (__unused NSException *exception) { return NO; }
}

+ (nullable NSString *)synthesizeFocusTapForAXElement:(id)element atPoint:(CGPoint)point pid:(int)pid
                                    allowDispatch:(BOOL (NS_NOESCAPE ^)(void))allowDispatch
{
  @try {
    if (![self focusRecoveryAvailable]) return @"original focus hit-test or targeting unavailable; nothing was tapped";
    id record = nil;
    NSString *failure = IPURCreateGestureRecord(@"ipu-original-field-focus", pid, &record);
    if (failure != nil) return failure;
    id path = IPURNewTouchPath(point, 0.0);
    if (path == nil || pid <= 0 || [self pidForAXElement:element] != pid) {
      return @"original focus target unavailable; nothing was tapped";
    }
    IPURLift(path, 0.05);
    id client = [self axClient];
    SEL selector = NSSelectorFromString(@"hitTestElement:withPoint:error:");
    NSString *snapshotError = nil;
    id snapshot = [self requestSnapshotForElement:element maxDepth:1 maxNodes:1 error:&snapshotError];
    id snapshotAX = IPURKVC(snapshot, @"accessibilityElement");
    id type = IPURKVC(snapshot, @"elementType");
    if (snapshot == nil || snapshotAX == nil || ![snapshotAX isEqual:element]
        || ![type respondsToSelector:@selector(integerValue)]
        || ![@[@(XCUIElementTypeTextField), @(XCUIElementTypeSearchField), @(XCUIElementTypeTextView)] containsObject:type]
        || ![IPURKVC(snapshot, @"enabled") boolValue]) {
      return @"original focus snapshot changed or unavailable; nothing was tapped";
    }
    // Clearing can install a new sibling/overlay. Refresh the application
    // hierarchy, not only the retained leaf, before asking who owns this point.
    id application = [self activeApplicationElementForPID:pid];
    NSString *applicationError = nil;
    id applicationSnapshot = application == nil ? nil : [self requestSnapshotForElement:application
      maxDepth:8 maxNodes:128 error:&applicationError];
    if (applicationSnapshot == nil || [self pidForAXElement:application] != pid) {
      return @"original focus application snapshot unavailable; nothing was tapped";
    }
    NSError *error = nil;
    id hit = ((IPURMsgSendHitTestSnapshot)objc_msgSend)(client, selector, applicationSnapshot, point, &error);
    if (error != nil || hit == nil || [self pidForAXElement:hit] != pid || ![hit isEqual:element]) {
      return @"original focus target occluded or changed; nothing was tapped";
    }
    if (!allowDispatch()) return @"original focus binding or scene changed; nothing was tapped";
    return IPURSynthesize(record, path);
  } @catch (__unused NSException *exception) {
    return @"original focus hit-test or synthesis raised; inspect original field";
  }
}

+ (nullable NSString *)synthesizeLongPressAt:(CGPoint)point duration:(NSTimeInterval)duration pid:(int)pid
{
  return [self synthesizeLongPressAt:point duration:duration pid:pid name:@"ipu-longpress"];
}

+ (nullable NSString *)synthesizeLongPressAt:(CGPoint)point
                                    duration:(NSTimeInterval)duration
                                         pid:(int)pid
                                        name:(NSString *)name
{
  @try {
    id record = nil;
    NSString *error = IPURCreateGestureRecord(name, pid, &record);
    if (error != nil) return error;
    id path = IPURNewTouchPath(point, 0.0);
    if (path == nil) return @"private XCTest event synthesis failed: could not create pointer path";
    IPURLift(path, MAX(0.01, duration));
    return IPURSynthesize(record, path);
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

+ (nullable NSString *)synthesizeDragFrom:(CGPoint)start
                                       to:(CGPoint)end
                                 duration:(NSTimeInterval)duration
                                      pid:(int)pid
{
  @try {
    id record = nil;
    NSString *error = IPURCreateGestureRecord(@"ipu-drag", pid, &record);
    if (error != nil) return error;
    id path = IPURNewTouchPath(start, 0.0);
    if (path == nil) return @"private XCTest event synthesis failed: could not create pointer path";
    duration = MAX(0.05, duration);
    // XCTest interpolates the segment. Sub-frame samples can shorten its
    // playback time (covered by the simulator touch-receiver fixture).
    IPURMove(path, end, duration);
    IPURLift(path, duration);
    return IPURSynthesize(record, path);
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

+ (nullable NSString *)synthesizeText:(NSString *)text
                  charactersPerSecond:(NSUInteger)charactersPerSecond
                                  pid:(int)pid
{
  @try {
    Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
    Class pathClass = NSClassFromString(@"XCPointerEventPath");
    SEL initRecord = NSSelectorFromString(@"initWithName:");
    SEL initPath = NSSelectorFromString(@"initForTextInput");
    SEL typeText = NSSelectorFromString(@"typeText:atOffset:typingSpeed:shouldRedact:");
    if (recordClass == Nil || pathClass == Nil || ![recordClass instancesRespondToSelector:initRecord]
        || ![pathClass instancesRespondToSelector:initPath] || ![pathClass instancesRespondToSelector:typeText]) {
      return @"private XCTest text synthesis unavailable";
    }
    id record = ((IPURMsgSendInitRecordName)objc_msgSend)([recordClass alloc], initRecord, @"ipu-type");
    id path = ((IPURMsgSendObject)objc_msgSend)([pathClass alloc], initPath);
    if (record == nil || path == nil) return @"private XCTest text synthesis failed: could not create text event";
    SEL targetSelector = NSSelectorFromString(@"setTargetProcessID:");
    if (pid > 0) {
      if (![record respondsToSelector:targetSelector]) return @"targeted text synthesis unavailable; nothing was sent";
      ((IPURMsgSendSetLongLong)objc_msgSend)(record, targetSelector, (long long)pid);
    }
    ((IPURMsgSendTypeText)objc_msgSend)(
      path, typeText, text, 0.0, (unsigned long long)(charactersPerSecond > 0 ? charactersPerSecond : 60), NO);
    return IPURSynthesize(record, path);
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

+ (nullable NSString *)synthesizeTouchPaths:(NSArray<NSArray<NSDictionary<NSString *, id> *> *> *)paths
                                       name:(NSString *)name
{
  if (paths.count == 0) return @"no touch paths to synthesize";
  @try {
    double buildStarted = IPURNowMs();
    id record = nil;
    NSString *error = IPURCreateGestureRecord(name, 0, &record);
    if (error != nil) return error;
    for (NSArray<NSDictionary<NSString *, id> *> *steps in paths) {
      NSDictionary *first = steps.firstObject;
      if (first == nil || ![first[@"type"] isEqual:@"down"]) {
        return @"touch path must start with a down step";
      }
      double lastOffset = [first[@"t"] doubleValue];
      id path = IPURNewTouchPath(CGPointMake([first[@"x"] doubleValue], [first[@"y"] doubleValue]), lastOffset);
      if (path == nil) return @"private XCTest event synthesis failed: could not create pointer path";
      BOOL lifted = NO;
      for (NSUInteger index = 1; index < steps.count && !lifted; index++) {
        NSDictionary *step = steps[index];
        double offset = MAX(lastOffset, [step[@"t"] doubleValue]);
        if ([step[@"type"] isEqual:@"move"]) {
          IPURMove(path, CGPointMake([step[@"x"] doubleValue], [step[@"y"] doubleValue]), offset);
        } else if ([step[@"type"] isEqual:@"up"]) {
          IPURLift(path, offset);
          lifted = YES;
        } else {
          return [NSString stringWithFormat:@"unsupported touch step type %@", step[@"type"]];
        }
        lastOffset = offset;
      }
      if (!lifted) IPURLift(path, lastOffset + 0.01);
      ((IPURMsgSendAddPath)objc_msgSend)(record, NSSelectorFromString(@"addPointerEventPath:"), path);
    }
    NSError *synthesisError = nil;
    IPURSynthBuildMs += IPURNowMs() - buildStarted;
    [IPURBridge invalidateRequestCache];  // a touch can change the foreground app
    BOOL ok = IPURSynthesizeRecord(record, &synthesisError);
    if (!ok) {
      return [NSString stringWithFormat:@"private XCTest event synthesis failed: %@",
                                        synthesisError.localizedDescription ?: @"synthesizeWithError returned NO"];
    }
    return nil;
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

// MARK: - Screen capture

static id IPURImageEncoding(NSString *uti, double quality)
{
  Class encodingClass = NSClassFromString(@"XCTImageEncoding");
  SEL initSelector = NSSelectorFromString(@"initWithUniformTypeIdentifier:compressionQuality:");
  if (encodingClass == Nil || ![encodingClass instancesRespondToSelector:initSelector]) return nil;
  return ((id (*)(id, SEL, NSString *, double))objc_msgSend)(
    [encodingClass alloc], initSelector, uti, quality);
}

static id IPURJPEGEncoding(double quality)
{
  return IPURImageEncoding(@"public.jpeg", quality);
}

/// XCTImage / XCUIScreenshot → its encoded bytes.
static NSData *IPURImageData(id image)
{
  if (image == nil) return nil;
  id data = IPURObject(image, @"data");
  if ([data isKindOfClass:NSData.class]) return data;
  return nil;
}

static NSData *IPURCaptureViaRequest(id encoding, NSString **error)
{
  id dataSource = IPURObject(XCUIDevice.sharedDevice, @"screenDataSource");
  SEL requestSelector = NSSelectorFromString(@"requestScreenshotWithRequest:withReply:");
  Class requestClass = NSClassFromString(@"XCTScreenshotRequest");
  SEL initSelector = NSSelectorFromString(@"initWithScreenID:rect:encoding:options:");
  // iOS 15/16 XCTest has the initializer without `options:`.
  SEL legacyInitSelector = NSSelectorFromString(@"initWithScreenID:rect:encoding:");
  BOOL hasOptions = requestClass != Nil && [requestClass instancesRespondToSelector:initSelector];
  BOOL hasLegacy = requestClass != Nil && [requestClass instancesRespondToSelector:legacyInitSelector];
  if (dataSource == nil || ![dataSource respondsToSelector:requestSelector] || (!hasOptions && !hasLegacy)) {
    if (error) *error = @"screenshot request API unavailable";
    return nil;
  }
  long long screenID = (long long)IPURMainDisplayID();
  id request = hasOptions
    ? ((id (*)(id, SEL, long long, CGRect, id, unsigned long long))objc_msgSend)(
        [requestClass alloc], initSelector, screenID, CGRectNull, encoding, 0ULL)
    : ((id (*)(id, SEL, long long, CGRect, id))objc_msgSend)(
        [requestClass alloc], legacyInitSelector, screenID, CGRectNull, encoding);
  if (request == nil) {
    if (error) *error = @"could not build XCTScreenshotRequest";
    return nil;
  }
  __block NSData *result = nil;
  __block NSString *failure = nil;
  dispatch_semaphore_t done = dispatch_semaphore_create(0);
  void (^reply)(id, NSError *) = ^(id image, NSError *replyError) {
    result = IPURImageData(image);
    if (result == nil) failure = replyError.localizedDescription ?: @"screenshot reply carried no data";
    dispatch_semaphore_signal(done);
  };
  ((void (*)(id, SEL, id, id))objc_msgSend)(dataSource, requestSelector, request, reply);
  if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, (int64_t)(2 * NSEC_PER_SEC))) != 0) {
    if (error) *error = @"screenshot request timed out";
    return nil;
  }
  if (result == nil && error) *error = failure;
  return result;
}

static NSData *IPURCaptureViaEncoding(id encoding, NSString **error)
{
  id screen = XCUIScreen.mainScreen;
  SEL selector = NSSelectorFromString(@"screenshotWithEncoding:options:");
  // iOS 15/16 XCTest has the variant without `options:`.
  SEL legacySelector = NSSelectorFromString(@"screenshotWithEncoding:");
  id screenshot = nil;
  if ([screen respondsToSelector:selector]) {
    screenshot = ((id (*)(id, SEL, id, unsigned long long))objc_msgSend)(screen, selector, encoding, 0ULL);
  } else if ([screen respondsToSelector:legacySelector]) {
    screenshot = ((id (*)(id, SEL, id))objc_msgSend)(screen, legacySelector, encoding);
  } else {
    if (error) *error = @"XCUIScreen screenshotWithEncoding: unavailable";
    return nil;
  }
  NSData *data = IPURImageData(IPURObject(screenshot, @"internalImage"));
  if (data == nil && error) *error = @"encoded screenshot carried no data";
  return data;
}

/// Downscales and/or re-encodes as JPEG with ImageIO. `scale` 1 with `reencode` NO returns the
/// input untouched.
static NSData *IPURJPEGTranscode(NSData *input, double scale, double quality, BOOL reencode)
{
  if (input == nil) return nil;
  if (scale >= 0.999 && !reencode) return input;
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)input, NULL);
  if (source == NULL) return nil;
  CGImageRef image = NULL;
  if (scale < 0.999) {
    NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(source, 0, NULL));
    double width = [properties[(id)kCGImagePropertyPixelWidth] doubleValue];
    double height = [properties[(id)kCGImagePropertyPixelHeight] doubleValue];
    NSUInteger maxPixels = (NSUInteger)MAX(16.0, round(MAX(width, height) * scale));
    NSDictionary *options = @{
      (id)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
      (id)kCGImageSourceThumbnailMaxPixelSize: @(maxPixels),
      (id)kCGImageSourceCreateThumbnailWithTransform: @YES,
      (id)kCGImageSourceShouldCacheImmediately: @YES,
    };
    image = CGImageSourceCreateThumbnailAtIndex(source, 0, (__bridge CFDictionaryRef)options);
  } else {
    image = CGImageSourceCreateImageAtIndex(source, 0, NULL);
  }
  CFRelease(source);
  if (image == NULL) return nil;
  NSMutableData *output = [NSMutableData data];
  CGImageDestinationRef destination =
    CGImageDestinationCreateWithData((__bridge CFMutableDataRef)output, CFSTR("public.jpeg"), 1, NULL);
  if (destination == NULL) {
    CGImageRelease(image);
    return nil;
  }
  NSDictionary *properties = @{(id)kCGImageDestinationLossyCompressionQuality: @(quality)};
  CGImageDestinationAddImage(destination, image, (__bridge CFDictionaryRef)properties);
  BOOL ok = CGImageDestinationFinalize(destination);
  CFRelease(destination);
  CGImageRelease(image);
  return ok ? output : nil;
}

+ (nullable NSData *)jpegScreenshotWithQuality:(double)quality
                                         scale:(double)scale
                                          path:(NSString *_Nullable *_Nullable)path
                                         error:(NSString *_Nullable *_Nullable)error
{
  quality = MIN(1.0, MAX(0.01, quality));
  scale = (scale <= 0 || scale > 1) ? 1.0 : scale;
  // Paths that failed once are not retried on every frame.
  static atomic_bool requestBroken = false;
  static atomic_bool encodingBroken = false;
  NSString *lastError = nil;
  @try {
    id encoding = IPURJPEGEncoding(quality);
    if (encoding != nil && !atomic_load(&requestBroken)) {
      NSString *failure = nil;
      NSData *jpeg = IPURCaptureViaRequest(encoding, &failure);
      if (jpeg != nil) {
        if (path) *path = @"request";
        return IPURJPEGTranscode(jpeg, scale, quality, NO);
      }
      // A timeout is transient (testmanagerd busy, a reply routed through a busy main thread);
      // anything else means the path does not work here.
      if ([failure isEqualToString:@"screenshot request timed out"]) {
        NSLog(@"ipu-runner: screenshot request timed out; using the next path for this frame");
      } else {
        NSLog(@"ipu-runner: screenshot request path failed, not retrying it: %@", failure);
        atomic_store(&requestBroken, true);
      }
      lastError = failure;
    }
    if (encoding != nil && !atomic_load(&encodingBroken)) {
      NSString *failure = nil;
      NSData *jpeg = IPURCaptureViaEncoding(encoding, &failure);
      if (jpeg != nil) {
        if (path) *path = @"encoding";
        return IPURJPEGTranscode(jpeg, scale, quality, NO);
      }
      NSLog(@"ipu-runner: screenshot encoding path failed, not retrying it: %@", failure);
      atomic_store(&encodingBroken, true);
      lastError = failure;
    }
    NSData *png = XCUIScreen.mainScreen.screenshot.PNGRepresentation;
    NSData *jpeg = IPURJPEGTranscode(png, scale, quality, YES);
    if (jpeg != nil) {
      if (path) *path = @"public";
      return jpeg;
    }
    lastError = @"public screenshot could not be encoded";
  } @catch (NSException *exception) {
    lastError = [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
  if (error) *error = lastError;
  return nil;
}

/// Encoded screenshot bytes (JPEG, or PNG on the public path) → one decoded image no larger than
/// `scale` of full size.
static CGImageRef IPURDecodeScaled(NSData *input, double scale) CF_RETURNS_RETAINED
{
  if (input == nil) return NULL;
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)input, NULL);
  if (source == NULL) return NULL;
  CGImageRef image = NULL;
  if (scale < 0.999) {
    NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(source, 0, NULL));
    double width = [properties[(id)kCGImagePropertyPixelWidth] doubleValue];
    double height = [properties[(id)kCGImagePropertyPixelHeight] doubleValue];
    NSUInteger maxPixels = (NSUInteger)MAX(16.0, round(MAX(width, height) * scale));
    NSDictionary *options = @{
      (id)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
      (id)kCGImageSourceThumbnailMaxPixelSize: @(maxPixels),
      (id)kCGImageSourceCreateThumbnailWithTransform: @YES,
      (id)kCGImageSourceShouldCacheImmediately: @YES,
    };
    image = CGImageSourceCreateThumbnailAtIndex(source, 0, (__bridge CFDictionaryRef)options);
  } else {
    NSDictionary *options = @{(id)kCGImageSourceShouldCacheImmediately: @YES};
    image = CGImageSourceCreateImageAtIndex(source, 0, (__bridge CFDictionaryRef)options);
  }
  CFRelease(source);
  return image;
}

+ (nullable NSData *)screenCaptureWithQuality:(double)quality
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error
{
  quality = MIN(1.0, MAX(0.01, quality));
  static atomic_bool requestBroken = false;
  static atomic_bool encodingBroken = false;
  NSString *lastError = nil;
  @try {
    id encoding = IPURJPEGEncoding(quality);
    if (encoding != nil && !atomic_load(&requestBroken)) {
      NSString *failure = nil;
      NSData *jpeg = IPURCaptureViaRequest(encoding, &failure);
      if (jpeg != nil) {
        if (path) *path = @"request";
        return jpeg;
      }
      if (![failure isEqualToString:@"screenshot request timed out"]) {
        atomic_store(&requestBroken, true);
      }
      lastError = failure;
    }
    if (encoding != nil && !atomic_load(&encodingBroken)) {
      NSString *failure = nil;
      NSData *jpeg = IPURCaptureViaEncoding(encoding, &failure);
      if (jpeg != nil) {
        if (path) *path = @"encoding";
        return jpeg;
      }
      atomic_store(&encodingBroken, true);
      lastError = failure;
    }
    NSData *png = XCUIScreen.mainScreen.screenshot.PNGRepresentation;
    if (png != nil) {
      if (path) *path = @"public";
      return png;
    }
    lastError = @"public screenshot returned no data";
  } @catch (NSException *exception) {
    lastError = [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
  if (error) *error = lastError;
  return nil;
}

+ (nullable NSData *)requestedPNGScreenshotWithError:(NSString *_Nullable *_Nullable)error
{
  @try {
    id encoding = IPURImageEncoding(@"public.png", 1.0);
    if (encoding == nil) {
      if (error) *error = @"XCTImageEncoding unavailable";
      return nil;
    }
    NSData *data = IPURCaptureViaRequest(encoding, error);
    static const uint8_t pngMagic[4] = {0x89, 'P', 'N', 'G'};
    if (data != nil && (data.length < 4 || memcmp(data.bytes, pngMagic, 4) != 0)) {
      if (error) *error = @"screenshot request did not return PNG";
      return nil;
    }
    return data;
  } @catch (NSException *exception) {
    if (error) *error = [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
    return nil;
  }
}

+ (nullable NSData *)sizedScreenshotWithMaxSide:(NSUInteger)maxSide
                                            png:(BOOL)png
                                        quality:(double)quality
                                           info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info
                                          error:(NSString *_Nullable *_Nullable)error
{
  NSString *failure = nil;
  NSData *capture = nil;
  if (png) {
    capture = [self requestedPNGScreenshotWithError:&failure];
  } else {
    // A capture that is about to be shrunk is taken at high quality, so the one lossy pass that
    // matters is the final encode at `quality`.
    capture = [self screenCaptureWithQuality:(maxSide > 0 ? MAX(quality, 0.9) : quality)
                                        path:NULL
                                       error:&failure];
  }
  if (capture == nil) {
    if (error) *error = failure ?: @"screen capture failed";
    return nil;
  }
  NSData *output = [self fitImage:capture maxSide:maxSide png:png quality:quality info:info];
  if (output == nil && error) *error = @"the capture could not be resized";
  return output;
}

+ (nullable NSData *)fitImage:(NSData *)data
                      maxSide:(NSUInteger)maxSide
                          png:(BOOL)png
                      quality:(double)quality
                         info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info
{
  IPURImageInfo fitted = {0};
  NSData *output = nil;
  @autoreleasepool {
    output = IPURFitImage(data, maxSide, png, quality, &fitted);
  }
  if (output != nil && info) *info = IPURImageInfoDictionary(fitted);
  return output;
}

+ (nullable CGImageRef)decodeScreenCapture:(NSData *)data scale:(double)scale
{
  scale = (scale <= 0 || scale > 1) ? 1.0 : scale;
  return IPURDecodeScaled(data, scale);
}

+ (nullable CGImageRef)screenImageWithQuality:(double)quality
                                        scale:(double)scale
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error
{
  NSData *capture = [self screenCaptureWithQuality:quality path:path error:error];
  if (capture == nil) return NULL;
  CGImageRef image = [self decodeScreenCapture:capture scale:scale];
  if (image == NULL && error) *error = @"screenshot could not be decoded";
  return image;
}

+ (nullable NSData *)grayScreenWithMaxSide:(NSUInteger)maxSide
                                     width:(NSUInteger *)width
                                    height:(NSUInteger *)height
                                     error:(NSString *_Nullable *_Nullable)error
{
  maxSide = MAX((NSUInteger)16, MIN((NSUInteger)512, maxSide));
  NSString *failure = nil;
  NSData *jpeg = [self jpegScreenshotWithQuality:0.5 scale:1.0 path:NULL error:&failure];
  if (jpeg == nil) {
    if (error) *error = failure ?: @"screen capture failed";
    return nil;
  }
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)jpeg, NULL);
  if (source == NULL) {
    if (error) *error = @"capture could not be decoded";
    return nil;
  }
  NSDictionary *options = @{
    (id)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
    (id)kCGImageSourceThumbnailMaxPixelSize: @(maxSide),
    (id)kCGImageSourceCreateThumbnailWithTransform: @YES,
  };
  CGImageRef image = CGImageSourceCreateThumbnailAtIndex(source, 0, (__bridge CFDictionaryRef)options);
  CFRelease(source);
  if (image == NULL) {
    if (error) *error = @"capture thumbnail failed";
    return nil;
  }
  size_t w = CGImageGetWidth(image), h = CGImageGetHeight(image);
  NSMutableData *pixels = [NSMutableData dataWithLength:w * h];
  CGColorSpaceRef gray = CGColorSpaceCreateDeviceGray();
  CGContextRef context = CGBitmapContextCreate(pixels.mutableBytes, w, h, 8, w, gray, kCGImageAlphaNone);
  CGColorSpaceRelease(gray);
  if (context == NULL) {
    CGImageRelease(image);
    if (error) *error = @"grayscale context failed";
    return nil;
  }
  CGContextDrawImage(context, CGRectMake(0, 0, w, h), image);
  CGContextRelease(context);
  CGImageRelease(image);
  if (width) *width = w;
  if (height) *height = h;
  return pixels;
}

// MARK: - Device

+ (BOOL)isScreenLocked:(BOOL *)known
{
  // The same SpringBoardServices calls runner's fb_isScreenLocked makes.
  typedef mach_port_t (*IPURServerPort)(void);
  typedef void (*IPURLockStatus)(mach_port_t, BOOL *, BOOL *);
  static IPURServerPort serverPort;
  static IPURLockStatus lockStatus;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    void *handle = dlopen(
      "/System/Library/PrivateFrameworks/SpringBoardServices.framework/SpringBoardServices", RTLD_LAZY);
    if (handle != NULL) {
      serverPort = (IPURServerPort)dlsym(handle, "SBSSpringBoardServerPort");
      lockStatus = (IPURLockStatus)dlsym(handle, "SBGetScreenLockStatus");
    }
  });
  if (serverPort == NULL || lockStatus == NULL) {
    if (known != NULL) *known = NO;
    return NO;
  }
  BOOL locked = NO;
  BOOL passcodeEnabled = NO;
  lockStatus(serverPort(), &locked, &passcodeEnabled);
  if (known != NULL) *known = YES;
  return locked;
}

+ (nullable NSString *)pressLockButton
{
  SEL selector = NSSelectorFromString(@"pressLockButton");
  if (![XCUIDevice.sharedDevice respondsToSelector:selector]) {
    return @"XCUIDevice pressLockButton is unavailable";
  }
  @try {
    ((void (*)(id, SEL))objc_msgSend)(XCUIDevice.sharedDevice, selector);
    return nil;
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

+ (nullable NSDictionary<NSString *, NSNumber *> *)screenLockStatus
{
  typedef mach_port_t (*IPURServerPort)(void);
  typedef void (*IPURLockStatus)(mach_port_t, BOOL *, BOOL *);
  void *handle = dlopen(
    "/System/Library/PrivateFrameworks/SpringBoardServices.framework/SpringBoardServices", RTLD_LAZY);
  if (handle == NULL) return nil;
  IPURServerPort serverPort = (IPURServerPort)dlsym(handle, "SBSSpringBoardServerPort");
  IPURLockStatus lockStatus = (IPURLockStatus)dlsym(handle, "SBGetScreenLockStatus");
  if (serverPort == NULL || lockStatus == NULL) return nil;
  BOOL locked = NO;
  BOOL passcodeEnabled = NO;
  lockStatus(serverPort(), &locked, &passcodeEnabled);
  return @{@"locked": @(locked), @"passcodeEnabled": @(passcodeEnabled)};
}

+ (nullable NSDictionary<NSString *, id> *)autoLockSetting
{
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    dlopen("/System/Library/PrivateFrameworks/ManagedConfiguration.framework/ManagedConfiguration",
           RTLD_LAZY);
  });
  Class connectionClass = NSClassFromString(@"MCProfileConnection");
  SEL shared = NSSelectorFromString(@"sharedConnection");
  SEL effective = NSSelectorFromString(@"effectiveValueForSetting:");
  if (connectionClass == Nil || ![connectionClass respondsToSelector:shared]) return nil;
  id value = nil;
  @try {
    id connection = ((id (*)(id, SEL))objc_msgSend)(connectionClass, shared);
    if (![connection respondsToSelector:effective]) return nil;
    value = ((id (*)(id, SEL, id))objc_msgSend)(connection, effective, @"maxInactivity");
  } @catch (NSException *exception) {
    return nil;
  }
  if (![value isKindOfClass:[NSNumber class]]) return nil;
  NSInteger secs = [(NSNumber *)value integerValue];
  // "Never" is stored as INT_MAX; anything beyond a day is no Auto-Lock a phone offers.
  BOOL never = secs <= 0 || secs >= 86400;
  return @{@"secs": @(secs), @"never": @(never)};
}

+ (nullable NSString *)resetIdleTimer
{
  // Measured on an iPhone 13 / iOS 27 with Auto-Lock at 30 s: F13 every 10 s kept the phone
  // awake for minutes, did not wake a dark (locked) screen, and left the foreground UI and an
  // open software keyboard (and the text in its field) untouched. Each press returns in ~0.25 s.
  // Ruled out on the same phone: IOPMAssertionDeclareUserActivity, a PreventUserIdleDisplaySleep
  // assertion and UIApplication.idleTimerDisabled in the runner (Auto-Lock fired anyway); a touch
  // aimed at the runner's own pid (it landed in the foreground app); consumer usage 0 (it holds
  // off Auto-Lock too, but XCTest waits 5 s for a confirmation it never gets and refuses every
  // tap meanwhile: "only one gesture can be performed at a time").
  static const unsigned int kKeyboardPage = 0x07;
  static const unsigned int kF13 = 0x68;
  Class eventClass = NSClassFromString(@"XCDeviceEvent");
  SEL make = NSSelectorFromString(@"deviceEventWithPage:usage:duration:");
  SEL perform = NSSelectorFromString(@"performDeviceEvent:error:");
  if (eventClass == Nil || ![eventClass respondsToSelector:make]
      || ![XCUIDevice.sharedDevice respondsToSelector:perform]) {
    return @"XCDeviceEvent / XCUIDevice performDeviceEvent:error: is unavailable";
  }
  @try {
    id event = ((id (*)(id, SEL, unsigned int, unsigned int, double))objc_msgSend)(
      eventClass, make, kKeyboardPage, kF13, 0.005);
    if (event == nil) return @"XCDeviceEvent could not be created";
    NSError *error = nil;
    BOOL ok = ((BOOL (*)(id, SEL, id, NSError **))objc_msgSend)(XCUIDevice.sharedDevice, perform, event, &error);
    if (ok) return nil;
    return error.localizedDescription ?: @"performDeviceEvent:error: returned NO";
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

@end
