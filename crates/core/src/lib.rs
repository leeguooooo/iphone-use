// rustdoc supplies this package as `--extern core`; generated schema macros
// still need the standard-library `core` namespace during doctest collection.
#[cfg(doctest)]
extern crate std as core;

pub mod auth;
pub mod control;
pub mod drag;
pub mod home_layout;
pub mod env;
pub mod progress;
pub mod runner_auth;
pub mod value_read;
pub mod element_selector;
