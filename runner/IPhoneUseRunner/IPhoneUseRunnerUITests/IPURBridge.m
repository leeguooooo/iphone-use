// IPURBridge — the private-XCTest surface of the iphone-use native runner.
//
// Portions derived from callstack/agent-device (MIT License, Copyright (c) 2026 Callstack):
// RunnerAXSnapshotBridge.m (reduced-attribute XCAXClient snapshot, depth ladder, frontier
// re-rooting), RunnerSynthesizedGesture.m / RunnerSynthesizedTextEntry.m /
// RunnerXCTestEventBridge.m (XCSynthesizedEventRecord + XCPointerEventPath synthesis) and
// RunnerTests+Lifecycle.swift (_performWithInteractionOptions:block: quiescence skip bits).
// See runner/README.md for the full attribution and license text.

#import "IPURBridge.h"

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
typedef id (*IPURMsgSendElementAtPoint)(id, SEL, CGPoint, NSError **);
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

+ (int)pidForApplication:(XCUIApplication *)application
{
  return IPURInt(application, @"processID");
}

+ (NSArray *)activeApplicationElements
{
  id active = IPURObject([self axClient], @"activeApplications");
  return [active isKindOfClass:NSArray.class] ? active : @[];
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

+ (nullable id)systemApplicationElement
{
  return IPURObject([self axClient], @"systemApplication");
}

+ (nullable id)foregroundApplicationElementWithProbePoint:(CGPoint)probePoint pid:(int *)pid
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
    // who owns the probe point, like WDA's active-app detection point.
    id axClient = [self axClient];
    SEL hitTest = NSSelectorFromString(@"accessibilityElementForElementAtPoint:error:");
    if (axClient != nil && [axClient respondsToSelector:hitTest]) {
      NSError *error = nil;
      id hit = nil;
      @try {
        hit = ((IPURMsgSendElementAtPoint)objc_msgSend)(axClient, hitTest, probePoint, &error);
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

/// One snapshot node in WDA's /source?format=json shape. No isVisible / isAccessible / isHittable:
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
  // WDA's name: the identifier when there is one, else the label.
  node[@"name"] = identifier ?: label ?: (id)NSNull.null;
  node[@"value"] = value ?: (id)NSNull.null;
  node[@"rawIdentifier"] = identifier ?: (id)NSNull.null;
  node[@"placeholderValue"] = placeholder ?: (id)NSNull.null;
  node[@"rect"] = IPURRect(snapshot);
  node[@"isEnabled"] = IPURBoolString(snapshot, @"enabled", YES);
  node[@"isFocused"] = IPURBoolString(snapshot, @"hasFocus", NO);
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

static NSMutableDictionary<NSString *, NSNumber *> *IPURAcceptedDepths(void)
{
  static NSMutableDictionary<NSString *, NSNumber *> *depths;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    depths = [NSMutableDictionary dictionary];
  });
  return depths;
}

+ (NSDictionary<NSString *, id> *)wdaTreeForAXElement:(id)axElement
                                             maxDepth:(NSInteger)maxDepth
                                             maxNodes:(NSInteger)maxNodes
                                   extensionCallLimit:(NSInteger)extensionCallLimit
                                          rememberKey:(nullable NSString *)rememberKey
{
  return [self wdaTreeForAXElement:axElement
                          maxDepth:maxDepth
                          maxNodes:maxNodes
                extensionCallLimit:extensionCallLimit
                       rememberKey:rememberKey
                   includeElements:NO];
}

+ (NSDictionary<NSString *, id> *)wdaTreeForAXElement:(id)axElement
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

+ (NSDictionary<NSString *, id> *)wdaTreeForSnapshot:(id)snapshot maxNodes:(NSInteger)maxNodes
{
  return [self wdaTreeForSnapshot:snapshot maxNodes:maxNodes includeElements:NO];
}

+ (NSDictionary<NSString *, id> *)wdaTreeForSnapshot:(id)snapshot
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

/// UIInterfaceOrientation for the record. UIDeviceOrientation 1…4 share raw values with
/// UIInterfaceOrientation (device landscapeLeft == interface landscapeRight == 3); face up/down
/// and unknown fall back to portrait.
static long long IPURInterfaceOrientation(void)
{
  NSInteger orientation = 1;
  @try {
    orientation = (NSInteger)XCUIDevice.sharedDevice.orientation;
  } @catch (__unused NSException *exception) {
    orientation = 1;
  }
  return (orientation >= 1 && orientation <= 4) ? orientation : 1;
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

static NSString *IPURCreateGestureRecord(NSString *name, int pid, id *record)
{
  if (![IPURBridge eventSynthesisAvailable]) {
    return @"private XCTest event synthesis unavailable (XCSynthesizedEventRecord / XCPointerEventPath)";
  }
  Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
  SEL displaySelector = NSSelectorFromString(@"initWithName:displayID:interfaceOrientation:");
  SEL orientationSelector = NSSelectorFromString(@"initWithName:interfaceOrientation:");
  long long orientation = IPURInterfaceOrientation();
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
  if (pid > 0 && [created respondsToSelector:targetSelector]) {
    ((IPURMsgSendSetLongLong)objc_msgSend)(created, targetSelector, (long long)pid);
  }
  *record = created;
  return nil;
}

static id IPURNewTouchPath(CGPoint point, double offset)
{
  Class pathClass = NSClassFromString(@"XCPointerEventPath");
  return ((IPURMsgSendInitPath)objc_msgSend)(
    [pathClass alloc], NSSelectorFromString(@"initForTouchAtPoint:offset:"), point, offset);
}

static void IPURMove(id path, CGPoint point, double offset)
{
  ((IPURMsgSendPathMove)objc_msgSend)(path, NSSelectorFromString(@"moveToPoint:atOffset:"), point, offset);
}

static void IPURLift(id path, double offset)
{
  ((IPURMsgSendPathOffset)objc_msgSend)(path, NSSelectorFromString(@"liftUpAtOffset:"), offset);
}

static NSString *IPURSynthesize(id record, id path)
{
  ((IPURMsgSendAddPath)objc_msgSend)(record, NSSelectorFromString(@"addPointerEventPath:"), path);
  NSError *error = nil;
  BOOL ok = ((IPURMsgSendSynthesize)objc_msgSend)(record, NSSelectorFromString(@"synthesizeWithError:"), &error);
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
    NSInteger steps = MIN(60, MAX(2, (NSInteger)ceil(duration / 0.016)));
    for (NSInteger step = 1; step <= steps; step++) {
      double t = (double)step / (double)steps;
      CGPoint point = CGPointMake(start.x + (end.x - start.x) * t, start.y + (end.y - start.y) * t);
      IPURMove(path, point, duration * t);
    }
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
    if (pid > 0 && [record respondsToSelector:targetSelector]) {
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
    BOOL ok = ((IPURMsgSendSynthesize)objc_msgSend)(
      record, NSSelectorFromString(@"synthesizeWithError:"), &synthesisError);
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

static id IPURJPEGEncoding(double quality)
{
  Class encodingClass = NSClassFromString(@"XCTImageEncoding");
  SEL initSelector = NSSelectorFromString(@"initWithUniformTypeIdentifier:compressionQuality:");
  if (encodingClass == Nil || ![encodingClass instancesRespondToSelector:initSelector]) return nil;
  return ((id (*)(id, SEL, NSString *, double))objc_msgSend)(
    [encodingClass alloc], initSelector, @"public.jpeg", quality);
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
  if (dataSource == nil || ![dataSource respondsToSelector:requestSelector] || requestClass == Nil
      || ![requestClass instancesRespondToSelector:initSelector]) {
    if (error) *error = @"screenshot request API unavailable";
    return nil;
  }
  long long screenID = (long long)IPURMainDisplayID();
  id request = ((id (*)(id, SEL, long long, CGRect, id, unsigned long long))objc_msgSend)(
    [requestClass alloc], initSelector, screenID, CGRectNull, encoding, 0ULL);
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
  if (![screen respondsToSelector:selector]) {
    if (error) *error = @"XCUIScreen screenshotWithEncoding:options: unavailable";
    return nil;
  }
  id screenshot = ((id (*)(id, SEL, id, unsigned long long))objc_msgSend)(screen, selector, encoding, 0ULL);
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

// MARK: - Device

+ (BOOL)isScreenLocked:(BOOL *)known
{
  // The same SpringBoardServices calls WDA's fb_isScreenLocked makes.
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

@end
