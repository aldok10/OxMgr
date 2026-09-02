//! Shim module: implementation moved to `oxmgr-core` (workspace-crate-layout
//! phase 1). Re-exported under the historical `crate::numeric::…` paths so call
//! sites compile untouched during the phased extraction.
pub use oxmgr_core::numeric::*;
