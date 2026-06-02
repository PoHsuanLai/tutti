//! Integration test modules for Tutti
//!
//! Test categories (inspired by Ardour/Zrythm patterns):
//! - engine: Engine lifecycle, initialization, cleanup
//! - transport: Play/stop/seek/loop/tempo operations
//! - graph: Audio graph construction and routing
//! - metering: Amplitude, LUFS, CPU metering
//! - workflow: End-to-end multi-subsystem workflows

pub mod engine;
pub mod graph;
pub mod metering;
pub mod transport;
