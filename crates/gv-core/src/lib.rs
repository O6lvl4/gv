//! gv-core: shared library for the gv CLI and shim.

pub mod paths;
pub mod platform;
pub mod release;
pub mod store;
pub mod resolve;
pub mod manifest;
pub mod install;

pub use paths::Paths;
pub use platform::Platform;
