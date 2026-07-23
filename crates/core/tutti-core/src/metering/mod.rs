//! Real-time audio metering.
//!
//! Two independent, opt-in things, both fed from the audio callback by
//! [`meter_output`]:
//!
//! - [`MasterMeter`] — peak/RMS for the master output, published into an
//!   [`AtomicAmplitude`] the UI reads lock-free. Per-track meters use the same
//!   [`AtomicAmplitude`] directly, one per channel strip.
//! - [`AudioTap`] — a ring-buffer copy of the output for off-thread analysis
//!   (spectrum, pitch, transients). See `tutti-analysis`.

mod amplitude;
mod rt;
mod tap;

pub use amplitude::{AtomicAmplitude, MasterMeter};
pub use rt::{meter_output, MeteringContext};
pub use tap::AudioTap;

#[cfg(feature = "bevy")]
pub mod plugin;
#[cfg(feature = "bevy")]
pub use plugin::{MeteringRes, PendingMetering, TuttiMeteringPlugin};
