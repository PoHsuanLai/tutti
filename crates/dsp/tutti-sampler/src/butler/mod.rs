//! Asynchronous disk I/O for audio file streaming.

mod cache;
mod command;
mod config;
pub(crate) mod control;
mod crossfader;
mod handlers;
mod io;
mod loop_body;
mod metrics;
mod plan;
mod prefetch;
mod region_map;
mod rt_state;
mod thread;
mod varispeed;

pub(crate) use command::ButlerCommand;
pub(crate) use config::BufferConfig;
pub use io::capture::{CaptureFormat, WavOut};
pub(crate) use plan::ChannelPlan;
pub(crate) use prefetch::SharedReader;
pub(crate) use rt_state::RtState;
pub(crate) use thread::ButlerThread;
pub(crate) use varispeed::PlayDirection;

// Test-only re-exports for unit tests outside the butler module tree (e.g.
// `units::streaming_sampler`) that build readers directly. Gated so they
// don't count as dead code in normal builds.
#[cfg(test)]
pub(crate) use command::RegionId;
#[cfg(test)]
pub(crate) use prefetch::{share_reader, RegionBuffer};
