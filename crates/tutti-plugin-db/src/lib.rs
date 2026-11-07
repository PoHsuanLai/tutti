//! Plugin discovery and persistence.
//!
//! The pieces, from pure to stateful:
//!
//! - [`discover`] walks directories and returns plugin paths. It touches no
//!   database.
//! - [`classify`], [`needs_rescan`] and the rest of the decisions are pure
//!   functions of a [`FileFacts`] read once at the edge and a [`FileRow`].
//! - [`PluginDb`] is a plain table, `path -> FileRow` (modification time, then
//!   either the plugins the file holds or a blacklist). The file is the unit
//!   because a crash blacklists a file before any plugin id is known. A
//!   [`Delta`] replaces or drops one row, and [`PluginDb::apply`] is the only
//!   code that changes the table. With feature `json`, `save` and `load`
//!   write and read it as a versioned file.
//! - [`Scanner`] probes the files that are new or changed since their row was
//!   stamped, through the probe it is given (the host supplies an out-of-process
//!   one). A scan runs on a worker over a snapshot of the table and sends
//!   deltas; the owner applies them with [`Scan::poll`], so the table stays
//!   readable for the whole scan. A sentinel file (the dead-man's pedal)
//!   outlives a host that dies mid-probe and blacklists the culprit on the next
//!   run.
//!
//! The crate never loads a plugin and never touches the audio thread.

#![forbid(unsafe_code)]

mod facts;
mod format;
mod fs;
mod pedal;
mod scanner;
#[cfg(feature = "json")]
mod store;
mod table;

pub use facts::{
    FileFacts, ScanDecision, classify, is_blacklisted_and_unchanged, missing, needs_rescan,
};
pub use format::{
    AuComponentType, ClapFeature, PluginClass, PluginDescriptor, PluginFormat, PluginRole,
    Vst2Category, Vst3PlugType, Vst3SubCategories,
};
pub use fs::{discover, format_from_path};
pub use scanner::{
    ProbeFailure, Scan, ScanBusy, ScanPhase, ScanPoll, ScanProgress, ScanResult, Scanner,
    probe_plugin_fallback,
};
#[cfg(feature = "json")]
pub use store::{load, save};
pub use table::{BlacklistedFile, Delta, FileRow, FileState, PluginDb, PluginEntry};
