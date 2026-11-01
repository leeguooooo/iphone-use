#!/usr/bin/env python3
"""Execute the production selection bridge using Foundation and synthetic AX/events."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.m').read_text()
a = source.index('// Selection metadata belongs')
b = source.index('+ (nullable id)requestSnapshotForElement:', a)
methods = source[a:b]
program = r'''
#import <Foundation/Foundation.h>
#import <CoreFoundation/CoreFoundation.h>
#import <objc/message.h>
static NSString *text;
static id range;
static BOOL synthFailure, noEffect;
static int synthCount, targetPID;
static const unsigned long long XCUIKeyModifierCommand=16;
typedef void (*IPURMsgSendSetLongLong)(id,SEL,long long);
@interface FixtureAX : NSObject
- (NSDictionary *)attributesForElement:(id)e attributes:(NSArray *)a error:(NSError **)err;
@end
@implementation FixtureAX
- (NSDictionary *)attributesForElement:(id)e attributes:(NSArray *)a error:(NSError **)err {
  return @{@"XC_kAXXCAttributeValue":text ?: (id)NSNull.null,
           @"XC_kAXXCAttributeSelectedTextRange":range ?: (id)NSNull.null};
}
@end
@interface FixtureRecord : NSObject
- (instancetype)initWithName:(id)name displayID:(unsigned long long)display;
- (void)setTargetProcessID:(long long)pid;
- (id)synthesizeWithError:(id)e;
@end
@implementation FixtureRecord
- (instancetype)initWithName:(id)name displayID:(unsigned long long)display{return [super init];}
- (void)setTargetProcessID:(long long)pid{targetPID=(int)pid;}
- (id)synthesizeWithError:(id)e{return nil;}
@end
@interface FixturePath : NSObject
- (instancetype)initForTextInput;
- (void)typeKey:(id)key modifiers:(unsigned long long)modifiers atOffset:(double)offset;
@end
@implementation FixturePath
- (instancetype)initForTextInput{return [super init];}
- (void)typeKey:(id)key modifiers:(unsigned long long)modifiers atOffset:(double)offset{}
@end
static Class fixtureClass(NSString *name){return [name isEqualToString:@"XCSynthesizedEventRecord"]?FixtureRecord.class:FixturePath.class;}
#define NSClassFromString fixtureClass
@interface IPURBridge : NSObject
+ (id)axClient;
+ (int)pidForAXElement:(id)e;
@end
@implementation IPURBridge
+ (id)axClient{return [FixtureAX new];}
+ (int)pidForAXElement:(id)e{return 42;}
'''
program += methods + r'''
@end
static unsigned long long IPURMainDisplayID(void){return 1;}
static NSString *IPURSynthesize(id record,id path){synthCount++;if(synthFailure)return @"uncertain";if(!noEffect)range=[NSValue valueWithRange:NSMakeRange(0,text.length)];return nil;}
static int checks;
static void check(BOOL condition,const char *message){checks++;if(!condition){fprintf(stderr,"FAIL: %s\n",message);exit(1);}}
int main(void){@autoreleasepool{
 NSString *error=nil;text=@"abc";range=[NSValue valueWithRange:NSMakeRange(3,0)];
 check([[IPURBridge shortClearKeysForAXElement:@1 error:&error] isEqualToString:@"\b\b\b"],"short end-caret clear bounded by original text");
 range=[NSValue valueWithRange:NSMakeRange(1,0)];check([IPURBridge shortClearKeysForAXElement:@1 error:&error]==nil,"middle caret never authorizes suffix-blind backspaces");
 text=[@"x" stringByPaddingToLength:500 withString:@"x" startingAtIndex:0];range=@"{500,0}";
 check([IPURBridge shortClearKeysForAXElement:@1 error:&error]==nil,"long field selects constant-size path");
 NSDictionary *selected=[IPURBridge selectAllTextForAXElement:@1 pid:42 retryState:nil error:&error];
 check([selected[@"confirmed"] boolValue] && [selected[@"length"] intValue]==500,"readback confirms whole original range");
 check(synthCount==1 && targetPID==42,"one selection event targets original process");
 NSDictionary *read=[IPURBridge textSelectionForAXElement:@1 error:&error];check([read[@"location"] intValue]==0&&[read[@"length"] intValue]==500,"selection metadata preserves UTF16 range");
 range=@"{501,0}";check([IPURBridge textSelectionForAXElement:@1 error:&error]==nil,"out-of-range selection cannot pass");
 range=@"{0,500}junk";check([IPURBridge textSelectionForAXElement:@1 error:&error]==nil,"trailing range data cannot pass");
 range=@"{500,0}";int before=synthCount;check([IPURBridge selectAllTextForAXElement:@1 pid:99 retryState:nil error:&error]==nil&&synthCount==before,"wrong PID dispatches nothing");
 synthFailure=YES;check([IPURBridge selectAllTextForAXElement:@1 pid:42 retryState:nil error:&error]==nil&&synthCount==before+1,"uncertain selection is not replayed");
 printf("Native text selection: %d production ObjC checks passed; synthetic AX/events only\n",checks);
}return 0;}
'''
with tempfile.TemporaryDirectory(prefix='ipu-text-selection-') as folder:
    path = Path(folder)
    (path / 'probe.m').write_text(program)
    subprocess.run(['xcrun', 'clang', '-fobjc-arc', '-framework', 'Foundation', str(path / 'probe.m'), '-o', str(path / 'probe')], check=True)
    subprocess.run([str(path / 'probe')], check=True)
