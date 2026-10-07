pub mod apps;
pub mod config;
pub mod flows;
pub mod focus;
pub mod http;
pub mod instance;
pub mod lockdown;
pub mod pairing;
pub mod protocol;
pub mod redaction;
pub mod runtime_dir;
pub mod schedules;
pub mod timing;
pub mod update;
pub mod usbmux;
pub mod video;
pub mod wda;

/// Re-export of the `core` crate under a non-`core` name.
///
/// Downstream integration-test crates that depend on `server` need the core
/// types but must NOT bring a dependency literally named `core` into their
/// extern prelude — doing so shadows the std `core` crate and breaks any
/// `core::`-emitting macro (e.g. `#[tokio::test]`). Reaching the core types
/// through this alias avoids that footgun.
pub use ::core as core_crate;
