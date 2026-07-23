//! Runtime audio vocabulary — the `Sample` trait that gates f32/f64 dispatch
//! and the borrowed [`AudioBuffer<T>`] shape passed to plugin instances.
//!
//! These types never cross the wire; the serialized discriminator is
//! [`crate::protocol::SampleFormat`].
//!
//! `Sample`, `AudioBuffer`, and the tagged `AudioBufferMut` are all re-exported
//! from `tutti-plugin-types` so the host crates
//! (`tutti-{vst2,vst3,clap,au}-host`) and `tutti-plugin` speak the same
//! vocabulary.

pub use tutti_plugin_types::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample};
