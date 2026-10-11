//! The MCP server's building blocks as a library: the daemon client, flows and
//! the flow registry, test suites and Jev. `iphone-use-mcp` is one front end;
//! the daemon hosted on the phone (crates/ios-embed) calls the same code in
//! process, because an iOS app cannot start another program.

pub mod client;
pub mod compact;
pub mod compat;
pub mod contrib;
pub mod device;
pub mod flow;
pub mod hints;
pub mod http;
pub mod jev;
pub mod outputs;
pub mod registry;
pub mod schedule;
pub mod screen;
pub mod server;
pub mod suite;
pub mod task;
pub mod types;

/// Tests that change process-wide environment variables (the flow store and
/// source, the daemon URL, fixture dirs, the `gh` stub) all hold this one
/// lock. Separate per-module locks let a test in one module unset a variable
/// while another module's test was using it, which then read the real
/// `~/.iphone-use/flows` and failed on any machine with flows installed.
#[cfg(test)]
pub(crate) static TEST_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
