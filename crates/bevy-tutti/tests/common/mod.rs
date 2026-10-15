//! Shared by the integration suites.

/// The reference CLAP plugin and its server, for suites that load one. Not
/// every suite that pulls `common` in does, hence the `dead_code` allowance.
#[cfg(feature = "plugin")]
#[allow(dead_code)]
pub mod plugin;

/// Nodes that show what a modulation lane delivers, for the modulation
/// suites. Not every suite uses them.
#[allow(dead_code)]
pub mod probe;

bevy_tutti::param_graph_node!(probe::ProbeUnit);
