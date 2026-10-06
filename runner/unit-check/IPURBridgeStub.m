#import "IPURBridgeStub.h"

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

@end
