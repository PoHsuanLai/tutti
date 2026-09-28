//! Shared by the integration suites.

/// The reference CLAP plugin and its server, for suites that load one. Not
/// every suite that pulls `common` in does, hence the `dead_code` allowance.
#[cfg(feature = "plugin")]
#[allow(dead_code)]
pub mod plugin;

/// A host's own `ParamNode`, registered with `param_graph_node!`, for the
/// suites whose subject is capturing and routing to its params. Not every
/// suite uses it.
#[allow(dead_code)]
pub mod drive_unit;
