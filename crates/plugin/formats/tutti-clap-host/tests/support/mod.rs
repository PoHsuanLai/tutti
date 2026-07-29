//! Shared fixtures for this crate's integration suites.
//!
//! Each test binary compiles this tree separately, and no single binary uses
//! every item in it, so the unused-code lint is silenced here rather than
//! per-item: an item is "dead" only from the vantage of one binary, and
//! annotating for that would push noise into code that is live elsewhere.

#![allow(dead_code)]

pub mod probe_path;
