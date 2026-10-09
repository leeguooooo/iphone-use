// Touch geometry shared by the bridge and runner/unit-check.sh (plain C, so the Mac-side check can
// call the same code the runner runs).
#pragma once
#import <CoreGraphics/CoreGraphics.h>

/// Where a point in the interface's coordinates (UIInterfaceOrientation raw value
/// `interfaceOrientation`, the space /window/size and the element rects use) sits in the screen's
/// portrait coordinates, which XCPointerEventPath points are read in. `natural` is the screen in
/// points, either way round. Hardware, iPhone 13 / iOS 27, Safari in landscape right (3): a point
/// sent as (200, 300) landed at (300, 190) in the landscape page whatever orientation the record
/// was built with, so the runner turns points itself.
/// Unknown orientations count as portrait.
static inline CGPoint IPURPortraitPoint(CGPoint point, long long interfaceOrientation, CGSize natural)
{
  CGFloat width = MIN(natural.width, natural.height);    // portrait width (short side)
  CGFloat height = MAX(natural.width, natural.height);   // portrait height (long side)
  switch (interfaceOrientation) {
    case 2:  // portrait upside down
      return CGPointMake(width - point.x, height - point.y);
    case 3:  // landscape right: the interface's x runs down the portrait screen
      return CGPointMake(width - point.y, point.x);
    case 4:  // landscape left: the opposite turn
      return CGPointMake(point.y, height - point.x);
    default:
      return point;
  }
}
