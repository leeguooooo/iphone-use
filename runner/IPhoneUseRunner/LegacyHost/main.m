// Minimal XCTest runner host for iOS 15/16: same job as Xcode's XCTRunner in
// "environment specifies the test configuration" mode, but it binds XCTest
// at runtime, so it does not need XCTCommandLineToolHelper (iOS 17+ only).
#import <UIKit/UIKit.h>
#include <dlfcn.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <unistd.h>

static void *ipu_open_xctest(void) {
    const char *paths[] = {
        "/Developer/Library/Frameworks/XCTest.framework/XCTest",          // iOS <= 16 (DDI)
        "/System/Developer/Library/Frameworks/XCTest.framework/XCTest",   // iOS 17+ (cryptex)
        "@rpath/XCTest.framework/XCTest",
    };
    for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); i++) {
        void *h = dlopen(paths[i], RTLD_NOW | RTLD_GLOBAL);
        if (h) return h;
        NSLog(@"ipu-host: dlopen %s failed: %s", paths[i], dlerror());
    }
    return NULL;
}

@interface IPUHostDelegate : UIResponder <UIApplicationDelegate, NSNetServiceBrowserDelegate>
@property (nonatomic, strong) UIWindow *window;
@property (nonatomic, strong) NSNetServiceBrowser *browser;
@end

@implementation IPUHostDelegate
- (BOOL)application:(UIApplication *)app didFinishLaunchingWithOptions:(NSDictionary *)opts {
    [self lanProbe];
    CFRunLoopPerformBlock(CFRunLoopGetMain(), kCFRunLoopCommonModes, ^{
        void *h = ipu_open_xctest();
        void (*xctest_main)(id) = h ? (void (*)(id))dlsym(h, "_XCTestMain") : NULL;
        if (!xctest_main) { NSLog(@"ipu-host: _XCTestMain unavailable"); exit(70); }
        NSLog(@"ipu-host: starting _XCTestMain");
        xctest_main(nil);
    });
    return YES;
}
- (void)lanProbe {
    // Touch the local network once (Bonjour browse) so iOS asks for, and then grants,
    // Local Network access; without it inbound LAN connections to 8100/9100 are dropped.
    if (getenv("IPU_HOST_NO_LAN_PROBE") == NULL) {
        [self.browser stop];
        self.browser = [NSNetServiceBrowser new];
        self.browser.delegate = self;
        [self.browser searchForServicesOfType:@"_iphoneuse._tcp." inDomain:@"local."];
        NSLog(@"ipu-host: local network probe started");
        const char *peer = getenv("IPU_HOST_LAN_PROBE");
        if (peer) {
            int fd = socket(AF_INET, SOCK_STREAM, 0);
            struct sockaddr_in sa = {0}; sa.sin_len = sizeof(sa); sa.sin_family = AF_INET; sa.sin_port = htons(9);
            inet_pton(AF_INET, peer, &sa.sin_addr);
            fcntl(fd, F_SETFL, O_NONBLOCK);
            int rc = connect(fd, (struct sockaddr *)&sa, sizeof(sa));
            NSLog(@"ipu-host: outbound LAN probe to %s rc=%d errno=%d", peer, rc, errno);
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{ close(fd); });
        }
    }
}
- (void)applicationDidBecomeActive:(UIApplication *)app {
    NSLog(@"ipu-host: became active, probing local network");
    [self lanProbe];
}
- (void)netServiceBrowser:(NSNetServiceBrowser *)b didNotSearch:(NSDictionary *)err {
    NSLog(@"ipu-host: local network probe error %@", err);
}
@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil, NSStringFromClass([IPUHostDelegate class]));
    }
}
