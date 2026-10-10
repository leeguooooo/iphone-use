// Mac-side stand-in for the parts of IPURBridge the pure runner logic uses (element type names,
// Objective-C exception catching), so runner/unit-check.sh can test it without a device.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import "../IPhoneUseRunner/IPhoneUseRunnerUITests/IPURGeometry.h"

NS_ASSUME_NONNULL_BEGIN

FOUNDATION_EXPORT NSString *const IPURNodeAXElementKey;

@interface IPURBridge : NSObject
+ (NSString *)elementTypeName:(NSInteger)elementType;
+ (nullable NSString *)catchException:(void (NS_NOESCAPE ^)(void))block;
+ (nullable CGImageRef)screenImageWithQuality:(double)quality
                                        scale:(double)scale
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error CF_RETURNS_RETAINED;
+ (nullable NSData *)screenCaptureWithQuality:(double)quality
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error;
+ (nullable CGImageRef)decodeScreenCapture:(NSData *)data scale:(double)scale CF_RETURNS_RETAINED;
+ (nullable NSData *)sizedScreenshotWithMaxSide:(NSUInteger)maxSide
                                            png:(BOOL)png
                                        quality:(double)quality
                                           info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info
                                          error:(NSString *_Nullable *_Nullable)error;
+ (nullable NSData *)fitImage:(NSData *)data
                      maxSide:(NSUInteger)maxSide
                          png:(BOOL)png
                      quality:(double)quality
                         info:(NSDictionary<NSString *, NSNumber *> *_Nullable *_Nullable)info;
/// Test helper: `data` decoded and encoded again as an uncompressed-ish ImageIO PNG, the way the
/// phone's XCTest screenshot is encoded (repo assets are optimized PNGs or JPEGs).
+ (nullable NSData *)reencodePNG:(NSData *)data;
@end

NS_ASSUME_NONNULL_END
