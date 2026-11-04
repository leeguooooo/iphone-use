#!/usr/bin/env python3
"""Run production host-role selection with synthetic symbols and temporary paths; no UI/device."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]

def block(source, start):
    begin = source.index(start)
    brace = source.index('{', begin)
    depth, end = 1, brace + 1
    while depth:
        if source[end] == '{': depth += 1
        elif source[end] == '}': depth -= 1
        end += 1
    return source[begin:end]

boot = (ROOT / 'crates/server/src/boot.rs').read_text()
role = block(boot, 'pub enum EmbeddedHost')
paths = block(boot, 'fn embedded_control_dir(') + '\n' + block(boot, 'fn prepare_runner_storage(')
ffi = (ROOT / 'crates/ios-embed/src/lib.rs').read_text()
exports = '\n'.join(block(ffi, 'pub extern "C" fn ' + name + '(')
                    for name in ['ipu_daemon_start', 'ipu_daemon_start_runner'])
rust = '#![allow(dead_code)]\nuse std::ffi::c_char;\nmod server {pub mod boot { #[derive(Debug,PartialEq)]' + role + '}}\n'
rust += 'use server::boot::EmbeddedHost;\n' + paths + '\n' + exports + r'''
static CALL: std::sync::Mutex<Option<(EmbeddedHost,u16,u16,u16)>> = std::sync::Mutex::new(None);
#[allow(clippy::too_many_arguments)]
fn start_daemon(role: EmbeddedHost, h: *const c_char, p: u16, r: u16, m: u16,
 t: *const c_char, a: *const c_char, s: *const c_char) -> i32 {
 assert!(h.is_null() && t.is_null() && a.is_null() && s.is_null());
 *CALL.lock().unwrap()=Some((role,p,r,m)); 17
}
fn main() {
 let home=std::path::Path::new("/owned/container");
 assert_eq!(embedded_control_dir(EmbeddedHost::App,home),home.join("Documents"));
 let private=embedded_control_dir(EmbeddedHost::Runner,home);
 assert_eq!(private,home.join("Library/Application Support/iphone-use/runner-control"));
 assert!(!private.starts_with(home.join("Documents")));
 let n=std::ptr::null();
 assert_eq!(ipu_daemon_start(n,1,2,3,n,n,n),17);
 assert_eq!(CALL.lock().unwrap().take(),Some((EmbeddedHost::App,1,2,3)));
 assert_eq!(ipu_daemon_start_runner(n,4,5,6,n,n,n),17);
 assert_eq!(CALL.lock().unwrap().take(),Some((EmbeddedHost::Runner,4,5,6)));
 let root=std::path::PathBuf::from(std::env::args().nth(1).unwrap());
 let home=root.join("container");
 let docs=home.join("Documents");let state=home.join("Library/state");
 std::fs::create_dir_all(&docs).unwrap();std::fs::create_dir_all(&state).unwrap();
 let control=embedded_control_dir(EmbeddedHost::Runner,&home);
 prepare_runner_storage(&home,&state,&control).unwrap();
 use std::os::unix::fs::PermissionsExt as _;
 assert_eq!(std::fs::metadata(&control).unwrap().permissions().mode() & 0o777,0o700);
 let bad=docs.join("state");std::fs::create_dir(&bad).unwrap();
 assert!(prepare_runner_storage(&home,&bad,&control).is_err());
 std::fs::remove_dir_all(&control).unwrap();std::os::unix::fs::symlink(&docs,&control).unwrap();
 assert!(prepare_runner_storage(&home,&state,&control).is_err());
 println!("Runner private storage: real temporary directories reject public state and symlinked control; mode0700 verified");
 println!("Runner control: private path and both unchanged C argument lists passed; synthetic start callback");
}
'''
swift = (ROOT / 'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerDaemon.swift').read_text()
# Replace only dynamic symbol resolution; execute the entire production Swift file.
swift = swift.replace('dlsym(', 'fixtureSymbol(')
swift += r'''
var calls: [String] = []
var available: Set<String> = []
typealias FixtureStart = @convention(c) (UnsafePointer<CChar>?,UInt16,UInt16,UInt16,UnsafePointer<CChar>?,UnsafePointer<CChar>?,UnsafePointer<CChar>?) -> Int32
@_cdecl("fixture_app") func fixtureApp(_ h:UnsafePointer<CChar>?,_ p:UInt16,_ r:UInt16,_ m:UInt16,_ t:UnsafePointer<CChar>?,_ a:UnsafePointer<CChar>?,_ s:UnsafePointer<CChar>?) -> Int32 {calls.append("app");return 0}
@_cdecl("fixture_runner") func fixtureRunner(_ h:UnsafePointer<CChar>?,_ p:UInt16,_ r:UInt16,_ m:UInt16,_ t:UnsafePointer<CChar>?,_ a:UnsafePointer<CChar>?,_ s:UnsafePointer<CChar>?) -> Int32 {calls.append("runner");return 0}
func fixtureSymbol(_ handle:UnsafeMutableRawPointer?,_ name:String) -> UnsafeMutableRawPointer? {
 guard available.contains(name) else {return nil}
 let fn:FixtureStart = name == "ipu_daemon_start_runner" ? fixtureRunner : fixtureApp
 return unsafeBitCast(fn,to:UnsafeMutableRawPointer.self)
}
struct HTTPResponse {
 static func value(_ v:Any) -> HTTPResponse {HTTPResponse()}
 static func error(_ s:Int,_ c:String,_ m:String) -> HTTPResponse {HTTPResponse()}
}
// Read real Foundation bundle metadata, rather than only injecting the flag.
let bundleRoot=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
try FileManager.default.createDirectory(at:bundleRoot,withIntermediateDirectories:true)
defer {try? FileManager.default.removeItem(at:bundleRoot)}
for (i,value) in [nil, true, false, "invalid"].enumerated() {
 let path=bundleRoot.appendingPathComponent("Fixture\(i).bundle")
 let contents=path.appendingPathComponent("Contents")
 try FileManager.default.createDirectory(at:contents,withIntermediateDirectories:true)
 var info:[String:Any]=["CFBundleIdentifier":"owned.control.fixture.\(i)","CFBundlePackageType":"BNDL"]
 if let value {info["UIFileSharingEnabled"]=value}
 let data=try PropertyListSerialization.data(fromPropertyList:info,format:.xml,options:0)
 try data.write(to:contents.appendingPathComponent("Info.plist"))
 guard let bundle=Bundle(path:path.path) else {fatalError("fixture bundle unavailable")}
 guard RunnerDaemon.documentsMayBeShared(bundle:bundle) == (i==1 || i==3) else {fatalError("wrong bundle sharing flag")}
}
print("Runner bundle metadata: 4 Foundation fixtures passed")
var count=0
for shared in [false,true] {
 for old in [false,true] {
  for new in [false,true] {
   available=[];calls=[]
   if old {available.insert("ipu_daemon_start")}
   if new {available.insert("ipu_daemon_start_runner")}
   RunnerDaemon.start(environment:[:],runnerPort:8100,mjpegPort:9100,fileSharingEnabled:shared)
   let expected = new ? ["runner"] : (!shared && old ? ["app"] : [])
   guard calls == expected else {fatalError("wrong startup route \(calls)")}
   guard (RunnerDaemon.startRefusal != nil) == (shared && old && !new) else {fatalError("missing or spurious refusal")}
   if shared && old && !new {
    let status=RunnerDaemon.statusValue()
    guard status["state"] as? String == "blocked",(status["error"] as? String)?.contains("runner_update_required") == true else {fatalError("missing blocked status")}
   }
   count += 1
  }
 }
}
available=["ipu_daemon_start"];calls=[]
RunnerDaemon.start(environment:["IPU_DAEMON_PORT":"0"],runnerPort:8100,mjpegPort:9100,fileSharingEnabled:true)
guard calls.isEmpty && RunnerDaemon.startRefusal == nil else {fatalError("disabled daemon tried starting")}
print("Runner daemon startup: \(count+1) symbol/Files/off scenarios passed; no actual daemon start")
'''
with tempfile.TemporaryDirectory(prefix='ipu-private-control-') as folder:
    out = Path(folder)
    (out/'probe.rs').write_text(rust)
    subprocess.run(['rustc','--edition','2021',str(out/'probe.rs'),'-o',str(out/'rust-probe')],check=True)
    subprocess.run([str(out/'rust-probe'),str(out/'storage')],check=True)
    (out/'probe.swift').write_text(swift)
    subprocess.run(['xcrun','swiftc',str(out/'probe.swift'),'-o',str(out/'swift-probe')],check=True)
    subprocess.run([str(out/'swift-probe')],check=True)
