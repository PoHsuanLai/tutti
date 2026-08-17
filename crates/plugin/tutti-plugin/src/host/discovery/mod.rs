//! Plugin discovery and persistence, in two layers.
//!
//! **Pure** — [`discover`] walks directories and returns paths;
//! [`PluginRecord::probe`](record::PluginRecord::probe) turns one path into one
//! record. Neither touches a store. An app that wants to own persistence
//! entirely (records in a CRDT, a database, or just a `Vec`) needs nothing else
//! from this module.
//!
//! **Stateful** — [`PluginScanner`] walks directories and probes each plugin in
//! a sandboxed subprocess, recording results in any [`PluginCatalog`]. This
//! layer exists for the two things the pure functions structurally cannot do:
//! skip unchanged plugins on a rescan (probing is expensive; mtime comparison
//! against stored records is what makes startup #2 fast) and survive a plugin
//! that hard-crashes the scanner (a dead-man's pedal sentinel outlives the
//! process and auto-blacklists the culprit on the next run).
//!
//! Persistence is pluggable — implement [`PluginCatalog`] over whatever store
//! you like. `JsonCatalog` is one ready-made implementation, behind the
//! opt-in `json` feature.

pub mod catalog;
#[cfg(feature = "json")]
pub mod database;
mod fs;
mod pedal;
pub mod record;
pub mod scanner;

pub use catalog::{CatalogExt, PluginCatalog};
#[cfg(feature = "json")]
pub use database::JsonCatalog;
// `file_modification_time` is reached as `crate::host::discovery::…` from
// `plugins.rs`, but only under `cfg(test)` — so a lib-only build sees this
// re-export as unused while removing it breaks `cargo test`.
#[allow(unused_imports)]
pub use fs::{discover, file_modification_time, format_from_path};
pub use record::{
    AuComponentType, Blacklist, ClapFeature, PluginClass, PluginDescriptor, PluginFormat,
    PluginRecord, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories,
};
pub use scanner::{PluginScanner, ScanHandle, ScanPhase, ScanProgress, ScanResult};
