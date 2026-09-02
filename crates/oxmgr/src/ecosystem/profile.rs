//! Shim module: implementation moved to `oxmgr-manager` (workspace-crate-layout
//! phase 6). Re-exported under the historical `crate::profile::…` paths so call
//! sites compile untouched during the phased extraction.
pub use oxmgr_manager::ecosystem::profile::*;
