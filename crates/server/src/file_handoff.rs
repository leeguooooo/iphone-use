//! Packaged CLI entry for the shared, journaled native Files receiver.
//! Exec keeps cancellation and the receiver in one process; no source checkout
//! or temporary executable files are required.
use std::ffi::OsString;

#[cfg(target_os = "macos")]
fn bootstrap() -> String {
    use base64::Engine as _;
    let sources = serde_json::json!({
        "common": include_str!("../../../experiments/usb-file-bridge/mac/common.py"),
        "publication": include_str!("../../../experiments/usb-file-bridge/mac/publication.py"),
        "runner_files": include_str!("../../../experiments/usb-file-bridge/mac/runner_files.py"),
    });
    let payload = base64::engine::general_purpose::STANDARD.encode(sources.to_string());
    format!(
        r#"import base64,json,sys,types
_sources=json.loads(base64.b64decode('{payload}'))
for _name in ('common','publication','runner_files'):
    _module=types.ModuleType(_name)
    _module.__file__='<iphone-use/files/'+_name+'.py>'
    sys.modules[_name]=_module
for _name in ('common','publication','runner_files'):
    exec(compile(_sources[_name],sys.modules[_name].__file__,'exec'),sys.modules[_name].__dict__)
sys.argv[0]='iphone-use files'
try:
    sys.modules['runner_files'].main()
except Exception as _error:
    print(json.dumps({{'event':'error','detail':str(_error)}}),file=sys.stderr)
    sys.exit(1)
"#
    )
}

/// Does not return after successful exec. The receiver preserves its own
/// transfer journal and signal handling, including uncertain publication.
pub fn main(args: &[OsString]) -> i32 {
    if args.first().is_some_and(|arg| arg == "__native-read") {
        return crate::file_provider::main(args);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt as _;
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("could not bind Files receiver to this CLI: {error}");
                return 1;
            }
        };
        let mut command = std::process::Command::new("/usr/bin/python3");
        command
            .args(["-I", "-c"])
            .arg(bootstrap())
            .env("IPHONE_USE_FILES_HOST_BIN", executable);
        if args.is_empty() {
            command.arg("--help");
        } else {
            command.args(args);
        }
        let error = command.exec();
        eprintln!(
            "could not start Files receiver with /usr/bin/python3 (Xcode developer tools required): {error}"
        );
        1
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        eprintln!("iphone-use files requires macOS and Xcode CoreDevice tools");
        2
    }
}
