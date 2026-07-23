//! Runtime audio vocabulary — the `Sample` trait that gates f32/f64 dispatch
//! and the borrowed [`AudioBuffer<T>`] shape passed to plugin instances.
//!
//! These types never cross the wire; the serialized discriminator is
//! [`crate::protocol::SampleFormat`].
//!
//! `Sample` and `AudioBuffer` are re-exported from `tutti-plugin-types` so
//! the host crates (`tutti-{vst2,vst3,clap,au}-host`) and `tutti-plugin`
//! speak the same vocabulary; only the tagged enum [`AudioBufferMut`]
//! lives here, because it's an internal trait-object adaptor not used by
//! the host crates.

pub use tutti_plugin_types::{AudioBuffer, AudioBuffer32, AudioBuffer64, Sample};

/// Sample-format-tagged buffer handed to [`crate::server::PluginInstance::process`].
///
/// The enum keeps the trait dyn-compatible while letting each format's
/// implementation match once and delegate into a single generic inner body.
///
/// Carries [`AudioBuffer`]'s two lifetimes verbatim (`'t` = channel tables,
/// `'d` = sample data, `'d: 't`) so the split survives the enum boundary — a
/// caller can still build the output table with a short-lived borrow.
pub enum AudioBufferMut<'t, 'd: 't> {
    F32(AudioBuffer<'t, 'd, f32>),
    F64(AudioBuffer<'t, 'd, f64>),
}

impl<'t, 'd: 't> AudioBufferMut<'t, 'd> {
    pub fn num_samples(&self) -> usize {
        match self {
            Self::F32(b) => b.num_samples,
            Self::F64(b) => b.num_samples,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        match self {
            Self::F32(b) => b.sample_rate,
            Self::F64(b) => b.sample_rate,
        }
    }
}
