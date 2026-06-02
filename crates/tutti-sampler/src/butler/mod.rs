//! Asynchronous disk I/O for audio file streaming.

mod cache;
mod command;
mod config;
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
mod transport;
mod varispeed;

pub(crate) use cache::LruCache;
pub use cache::Stats;
pub(crate) use command::{ButlerCommand, CaptureId, CaptureIdGen};
pub(crate) use config::BufferConfig;
pub(crate) use metrics::Metrics;
pub use metrics::Snapshot;
pub(crate) use plan::ChannelPlan;
pub(crate) use prefetch::{CaptureBuffer, CaptureWriter, RegionReader};
pub(crate) use rt_state::RtState;
pub(crate) use thread::ButlerThread;
pub(crate) use transport::TransportBridge;
pub use varispeed::{PlayDirection, Varispeed};

// Test-only re-exports for unit tests outside the butler module tree (e.g.
// `units::streaming_sampler`) that build readers directly. Gated so they
// don't count as dead code in normal builds.
#[cfg(test)]
pub(crate) use command::RegionId;
#[cfg(test)]
pub(crate) use prefetch::RegionBuffer;
