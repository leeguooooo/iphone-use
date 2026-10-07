// Mac-side stand-in for the parts of IPURBridge the pure runner logic uses (element type names,
// Objective-C exception catching), so runner/unit-check.sh can test it without a device.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>

NS_ASSUME_NONNULL_BEGIN

FOUNDATION_EXPORT NSString *const IPURNodeAXElementKey;

@interface IPURBridge : NSObject
+ (NSString *)elementTypeName:(NSInteger)elementType;
+ (nullable NSString *)catchException:(void (NS_NOESCAPE ^)(void))block;
+ (nullable CGImageRef)screenImageWithQuality:(double)quality
                                        scale:(double)scale
                                         path:(NSString *_Nullable *_Nullable)path
                                        error:(NSString *_Nullable *_Nullable)error CF_RETURNS_RETAINED;
@end

NS_ASSUME_NONNULL_END
