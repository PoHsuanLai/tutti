//! Shared by the integration suites.
//!
//! It held `both_backends!`, which ran a suite on both graph runtimes from
//! design doc 013's PR 11 until PR 13 deleted the `Net` one; every suite now
//! runs on the one graph.

/// The reference CLAP plugin and its server, for suites that load one. Not
/// every suite that pulls `common` in does, hence the `dead_code` allowance.
#[cfg(feature = "plugin")]
#[allow(dead_code)]
pub mod plugin;

/// A test-local `AudioUnit` modulatable through the `ModTargetRegistry`,
/// for suites whose subject is that path. Not every suite uses it.
#[allow(dead_code)]
pub mod drive_unit;
