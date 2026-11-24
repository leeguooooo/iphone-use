// Touch geometry shared by the bridge and runner/unit-check.sh (plain C, so the Mac-side check can
// call the same code the runner runs).
#pragma once
#import <CoreGraphics/CoreGraphics.h>
#import <math.h>

/// XCTest's system application frame is in the screen's coordinate space. UIKit can describe
/// only the runner host's compatibility viewport (320x480 on an iPhone 16, issue #257).
static inline CGSize IPURScreenSizeFromFrame(CGRect systemFrame, CGSize fallback)
{
  if (systemFrame.origin.x == 0 && systemFrame.origin.y == 0
      && isfinite(systemFrame.size.width) && isfinite(systemFrame.size.height)
      && systemFrame.size.width > 0 && systemFrame.size.height > 0) {
    return systemFrame.size;
  }
  return fallback;
}

/// The screen the UI is drawn on, from screen-space frames read together: the system
/// application's and each active application's root. The largest valid one wins (by area, so
/// either orientation compares alike); none valid gives `fallback`. On a foldable (iPhone Duo,
/// issue #447) the open inner display is 669x951 while a read taken folded, or a SpringBoard root
/// that still describes the outer display, is 466x678: the foreground app's root, which the
/// element tree reports as the screen, then carries the real size. An app root that is smaller
/// than the system frame (a compatibility-sized app) never shrinks it.
static inline CGSize IPURScreenSizeFromFrames(const CGRect *frames, size_t count, CGSize fallback)
{
  CGSize best = CGSizeZero;
  for (size_t i = 0; i < count; i++) {
    CGSize size = IPURScreenSizeFromFrame(frames[i], CGSizeZero);
    if (size.width * size.height > best.width * best.height) best = size;
  }
  return best.width > 0 ? best : fallback;
}

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
