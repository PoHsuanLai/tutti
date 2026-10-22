//! Real-time audio metering.
//!
//! Two independent, opt-in things, both fed from the audio callback by
//! [`meter_output`]:
//!
//! - [`MasterMeter`] — peak/RMS for the master output, published into an
//!   [`AtomicAmplitude`] the UI reads lock-free. Per-track meters use the same
//!   [`AtomicAmplitude`] directly, one per channel strip.
//! - [`AudioTap`] — a ring-buffer copy of the output for off-thread consumers:
//!   analysis (spectrum, pitch, transients) or recording. `tutti-io`'s `TapIn`
//!   adapts its consumer end into an `AudioIn`, which is what lets a pump write
//!   the master output to a file.
//!
//! And one always on, fed by the engine itself: [`LoadMeter`], each block's
//! render time against its real-time budget.

mod amplitude;
mod load;
mod rt;
mod tap;

pub use amplitude::{AtomicAmplitude, MasterMeter, MeterReading};
pub use load::{LoadMeter, LoadReading, LOAD_MEAN_TIME_CONSTANT};
pub use rt::{meter_output, MeteringContext};
pub use tap::{AudioTap, TapBusy, TapCons};

// The Bevy resource wrapper (`MeteringRes`) belongs to the host adapter,
// `bevy_tutti::graph`, not to this crate.
