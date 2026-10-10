// Screenshot resizing and re-encoding shared by the bridge and runner/unit-check.sh (plain
// ImageIO, so the Mac-side check runs and measures the same code the runner runs).
#pragma once
#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>

/// What `IPURFitImage` produced: the capture's size, the returned image's size (pixels) and
/// whether the returned bytes are PNG.
typedef struct {
  NSUInteger sourceWidth;
  NSUInteger sourceHeight;
  NSUInteger width;
  NSUInteger height;
  BOOL png;
} IPURImageInfo;

static inline BOOL IPURDataIsPNG(NSData *data)
{
  static const uint8_t magic[4] = {0x89, 'P', 'N', 'G'};
  return data.length >= 4 && memcmp(data.bytes, magic, 4) == 0;
}

/// `input` (PNG or JPEG) as PNG (`png`) or JPEG at `quality` (0…1), its longer side at most
/// `maxSide` pixels (0: full size), aspect ratio kept. The input comes back untouched when it is
/// already that format and no larger. nil when it cannot be decoded or encoded.
static __attribute__((unused)) NSData *IPURFitImage(NSData *input, NSUInteger maxSide, BOOL png,
                                                    double quality, IPURImageInfo *info)
{
  if (input.length == 0) return nil;
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)input, NULL);
  if (source == NULL) return nil;
  NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(source, 0, NULL));
  NSUInteger width = [properties[(id)kCGImagePropertyPixelWidth] unsignedIntegerValue];
  NSUInteger height = [properties[(id)kCGImagePropertyPixelHeight] unsignedIntegerValue];
  if (width == 0 || height == 0) {
    CFRelease(source);
    return nil;
  }
  NSUInteger longSide = MAX(width, height);
  BOOL shrink = maxSide > 0 && maxSide < longSide;
  BOOL inputPNG = IPURDataIsPNG(input);
  if (!shrink && inputPNG == png) {
    CFRelease(source);
    if (info) *info = (IPURImageInfo){width, height, width, height, png};
    return input;
  }
  CGImageRef image = NULL;
  if (shrink) {
    NSDictionary *options = @{
      (id)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
      (id)kCGImageSourceThumbnailMaxPixelSize: @(maxSide),
      (id)kCGImageSourceCreateThumbnailWithTransform: @YES,
      (id)kCGImageSourceShouldCacheImmediately: @YES,
    };
    image = CGImageSourceCreateThumbnailAtIndex(source, 0, (__bridge CFDictionaryRef)options);
  } else {
    image = CGImageSourceCreateImageAtIndex(source, 0, NULL);
  }
  CFRelease(source);
  if (image == NULL) return nil;
  NSUInteger outWidth = CGImageGetWidth(image);
  NSUInteger outHeight = CGImageGetHeight(image);
  NSMutableData *output = [NSMutableData data];
  CGImageDestinationRef destination = CGImageDestinationCreateWithData(
    (__bridge CFMutableDataRef)output, png ? CFSTR("public.png") : CFSTR("public.jpeg"), 1, NULL);
  if (destination == NULL) {
    CGImageRelease(image);
    return nil;
  }
  NSDictionary *encodeOptions = png
    ? @{}
    : @{(id)kCGImageDestinationLossyCompressionQuality: @(MIN(1.0, MAX(0.01, quality)))};
  CGImageDestinationAddImage(destination, image, (__bridge CFDictionaryRef)encodeOptions);
  BOOL ok = CGImageDestinationFinalize(destination);
  CFRelease(destination);
  CGImageRelease(image);
  if (!ok) return nil;
  if (info) *info = (IPURImageInfo){width, height, outWidth, outHeight, png};
  return output;
}

/// `IPURFitImage`'s result as the dictionary the Swift side reads.
static __attribute__((unused)) NSDictionary<NSString *, NSNumber *> *IPURImageInfoDictionary(IPURImageInfo info)
{
  return @{
    @"sourceWidth": @(info.sourceWidth),
    @"sourceHeight": @(info.sourceHeight),
    @"width": @(info.width),
    @"height": @(info.height),
    @"png": @(info.png),
  };
}
