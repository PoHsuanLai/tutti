//! Shared fixtures for this crate's integration suites.
//!
//! Each test binary compiles this tree separately, and no single binary uses
//! every item in it, so the unused-code lint is silenced here rather than
//! per-item: an item is "dead" only from the vantage of one binary, and
//! annotating for that would push noise into code that is live elsewhere.

#![allow(dead_code)]

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
pub fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

pub mod active;
pub mod probe_path;
pub mod raw;
