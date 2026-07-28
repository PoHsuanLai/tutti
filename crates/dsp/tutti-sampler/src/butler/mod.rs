//! Asynchronous disk I/O for audio file streaming.

mod cache;
mod command;
mod config;
pub(crate) mod control;
mod crossfader;
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
mod streamer;
mod thread;

pub(crate) use command::ButlerCommand;
pub(crate) use config::BufferConfig;
pub(crate) use plan::ChannelPlan;
pub(crate) use prefetch::SharedReader;
pub(crate) use rt_state::RtState;
pub(crate) use thread::ButlerThread;

// The butler's public face: the handle a host holds to drive disk streaming.
pub use streamer::{DiskStreamer, DiskStreamerConfig};

// Test-only re-exports for unit tests outside the butler module tree (e.g.
// `units::disk_voice`) that build readers directly. Gated so they
// don't count as dead code in normal builds.
#[cfg(test)]
pub(crate) use command::RegionId;
#[cfg(test)]
pub(crate) use prefetch::{share_reader, RegionBuffer};
