#import "IPURBridgeStub.h"
#import "../IPhoneUseRunner/IPhoneUseRunnerUITests/IPURImageCodec.h"

NSString *const IPURNodeAXElementKey = @"__axElement";

@implementation IPURBridge

+ (NSString *)elementTypeName:(NSInteger)elementType
{
  // Same table as IPURBridge.m (XCUIElementType raw values 0…82).
  static NSArray<NSString *> *names;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
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

+ (nullable NSString *)catchException:(void (NS_NOESCAPE ^)(void))block
{
  @try {
    block();
    return nil;
  } @catch (NSException *exception) {
    return [NSString stringWithFormat:@"%@: %@", exception.name, exception.reason];
  }
}

+ (nullable CGImageRef)screenImageWithQuality:(double)quality
                                        scale:(double)scale
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error
{
  if (error) *error = @"no screen on the Mac";
  return NULL;
}

+ (nullable NSData *)screenCaptureWithQuality:(double)quality
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error
{
  if (error) *error = @"no screen on the Mac";
  return nil;
}

+ (nullable CGImageRef)decodeScreenCapture:(NSData *)data scale:(double)scale
{
  return NULL;
}

+ (nullable NSData *)sizedScreenshotWithMaxSide:(NSUInteger)maxSide
                                            png:(BOOL)png
                                        quality:(double)quality
                                           info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info
                                          error:(NSString *_Nullable *_Nullable)error
{
  if (error) *error = @"no screen on the Mac";
  return nil;
}

// The runner's own codec (IPURImageCodec.h), so the check measures what the phone runs.
+ (nullable NSData *)fitImage:(NSData *)data
                      maxSide:(NSUInteger)maxSide
                          png:(BOOL)png
                      quality:(double)quality
                         info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info
{
  IPURImageInfo fitted = {0};
  NSData *output = IPURFitImage(data, maxSide, png, quality, &fitted);
  if (output != nil && info) *info = IPURImageInfoDictionary(fitted);
  return output;
}

+ (nullable NSData *)reencodePNG:(NSData *)data
{
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)data, NULL);
  if (source == NULL) return nil;
  CGImageRef image = CGImageSourceCreateImageAtIndex(source, 0, NULL);
  CFRelease(source);
  if (image == NULL) return nil;
  NSMutableData *output = [NSMutableData data];
  CGImageDestinationRef destination =
    CGImageDestinationCreateWithData((__bridge CFMutableDataRef)output, CFSTR("public.png"), 1, NULL);
  CGImageDestinationAddImage(destination, image, NULL);
  BOOL ok = CGImageDestinationFinalize(destination);
  CFRelease(destination);
  CGImageRelease(image);
  return ok ? output : nil;
}

@end
