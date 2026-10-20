//! The engine's aggregate error: the session's own.
//!
//! [`enum@Error`] is `tutti_session::Error`, re-exported item by item (design
//! 015 §4.7): every subsystem error wraps into it via `#[from]`, so `?`
//! propagates across crate boundaries, and a device failure arrives as one
//! `Device` variant, `tutti_cpal::Error` owning that distinction.

pub use tutti_session::{Error, Result};
