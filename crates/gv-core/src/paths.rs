//! gv's filesystem paths come from anyv-core, parameterized with the app
//! name `"gv"`. Thin re-export so the rest of gv-core uses
//! `crate::paths::Paths` without caring where the implementation lives.

pub use anyv_core::paths::{ensure_dir, Paths as AnyvPaths};

use anyhow::Result;

pub type Paths = AnyvPaths;

pub fn discover() -> Result<Paths> {
    AnyvPaths::discover("gv")
}
