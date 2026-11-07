//! Plugin discovery and persistence, in two layers.
//!
//! **Pure** — [`discover`] walks directories and returns paths. It touches no
//! store. Every decision about a candidate ([`classify`], [`needs_rescan`], [`blacklist_record`], [`missing`]) is a pure
//! function of a [`FileFacts`] read once at the edge and a catalog record. An app
//! that wants to own persistence entirely (records in a CRDT, a database, or just
//! a `Vec`) needs nothing else from this crate.
//!
//! **Stateful** — [`PluginScanner`] walks directories and probes each plugin
//! through the probe it is given (the host supplies an out-of-process one),
//! recording results in any [`PluginCatalog`]. This layer exists for the two things the pure functions structurally cannot do:
//! skip unchanged plugins on a rescan (probing is expensive; mtime comparison
//! against stored records is what makes startup #2 fast) and survive a plugin
//! that hard-crashes the scanner (a dead-man's pedal sentinel outlives the
//! process and auto-blacklists the culprit on the next run).
//!
//! Persistence is pluggable — implement [`PluginCatalog`] over whatever store
//! you like. `JsonCatalog` is one ready-made implementation, behind the
//! opt-in `json` feature.

#![forbid(unsafe_code)]

mod catalog;
#[cfg(feature = "json")]
mod database;
mod facts;
mod fs;
mod pedal;
mod record;
mod scanner;

pub use catalog::{CatalogExt, PluginCatalog};
#[cfg(feature = "json")]
pub use database::{JsonCatalog, parse_records};
pub use facts::{
    FileFacts, ScanDecision, blacklist_record, classify, is_blacklisted_and_unchanged, missing,
    needs_rescan,
};
pub use fs::{discover, format_from_path};
pub use record::{
    AuComponentType, Blacklist, ClapFeature, PluginClass, PluginDescriptor, PluginFormat,
    PluginRecord, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories,
};
pub use scanner::{
    PluginScanner, ProbeFailure, ScanHandle, ScanPhase, ScanProgress, ScanResult,
    probe_plugin_fallback,
};
