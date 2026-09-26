//! Asynchronous disk I/O for audio file streaming.
//!
//! A background "butler" thread decodes from disk into one SPSC ring per
//! channel; the audio thread only ever pops. Nothing here blocks or allocates on
//! the audio thread — the split is what makes the streaming tier real-time safe.

mod cache;
mod command;
mod config;
pub(crate) mod control;
mod handlers;
mod io;
mod loop_body;
mod loops;
mod metrics;
mod plan;
mod prefetch;
mod preroll;
mod region_map;
mod rt_state;
mod step;
mod streamer;
mod thread;

pub(crate) use command::ButlerCommand;
pub(crate) use config::BufferConfig;
pub(crate) use handlers::SessionRate;
#[cfg(test)]
pub(crate) use loops::GUARD_FRAMES;
pub(crate) use loops::{Arrangement, RingMap};
pub(crate) use plan::ChannelPlan;
pub(crate) use prefetch::SharedReader;
pub(crate) use rt_state::RtState;
pub(crate) use thread::ButlerThread;

// The butler's public face: the handle a host holds to drive disk streaming,
// and the one failure every stream-control command can report.
pub use control::ButlerGone;
pub use prefetch::TakeVoiceError;
pub use streamer::{DiskStreamer, DiskStreamerConfig};

// The hand-driven cycle's verdict. Public only alongside the driver that
// produces it — the threaded butler consumes its own outcomes.
#[cfg(any(test, feature = "test-support"))]
pub use step::StepOutcome;

// Test-only re-exports for unit tests outside the butler module tree (e.g.
// `units::disk_voice`) that build readers directly. Gated so they
// do not count as dead code in normal builds.
#[cfg(test)]
pub(crate) use command::RegionId;
#[cfg(test)]
pub(crate) use prefetch::RegionBuffer;
