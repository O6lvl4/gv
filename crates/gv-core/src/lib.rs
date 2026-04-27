//! gv-core: shared library for the gv CLI and shim.

pub mod install;
pub mod lock;
pub mod manifest;
pub mod paths;
pub mod platform;
pub mod project;
pub mod proxy;
pub mod registry;
pub mod release;
pub mod resolve;
pub mod store;
pub mod tool;
pub mod workspace;

pub use paths::Paths;
pub use platform::Platform;
