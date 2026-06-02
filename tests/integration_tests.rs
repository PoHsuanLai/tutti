//! Integration tests for Tutti audio engine
//!
//! Test structure inspired by Ardour (CppUnit fixtures, dummy backend, reference data)
//! and Zrythm (GoogleTest with mock audio I/O, manual cycle control, round-trip testing).
//!
//! Test categories:
//! - Engine: lifecycle, initialization, cleanup
//! - Transport: play/stop/seek/loop/tempo/metronome
//! - Graph: node creation, routing, signal flow
//! - Metering: amplitude, LUFS, CPU, correlation
//!
//! Run with:
//! ```bash
//! cargo test -p tutti --test integration_tests
//! ```

#[allow(clippy::duplicate_mod)]
mod helpers;
mod integration;

// Re-run individual test modules
pub use integration::*;
