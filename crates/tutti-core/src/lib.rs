#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{Error, Result};

mod engine;

// The threads the parallel graph executor runs a block on (doc 013, "The parallel executor"):
// the engine's implementation of `tutti_graph::Pool`, which a host hands to
// `Executor::set_pool` before the executor goes to the audio thread.
mod pool;
pub use pool::{OnStart, WorkerPool, WorkerPoolBuilder};

// `MAX_ROOT_CHANNELS` comes to the root with `Engine`: it is the ceiling on the
// root's own output width, so a host sizing a scratch buffer for `process` has
// to name it, and names it from the root.
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError, MAX_ROOT_CHANNELS};

pub mod transport;
pub use transport::{
    ClickNode, ClickSettings, ClickState, FadeOut, FrozenClock, LoopEdit, LoopRegion, MeterSlot,
    MeterSlotCell, MeterSlotReader, MetronomeMode, MotionEvent, MotionFsm, MotionState,
    OfflineTimeline, OfflineTimelineConfig, ProjectMeter, QueueFull, RenderClock,
    SCHEDULE_CAPACITY, ScheduleFull, Then, Timeline, Transport, TransportClock, TransportCommand,
    TransportSettings, TransportState,
};

mod metering;
pub use metering::{
    AtomicAmplitude, AudioTap, LOAD_MEAN_TIME_CONSTANT, LoadMeter, LoadReading, MasterMeter,
    MeterReading, MeteringContext, TapBusy, TapCons, meter_output,
};

// What a source outside the graph pre-rolls by, per channel: the plan's
// compensation, republished with every plan.
mod preroll;
pub use preroll::PrerollTable;

// The graph-node handle, reachable as `tutti_core::AudioNode`. The DAW param
// components are app-side, not here, which is why this module holds one type.
mod node;
pub use node::AudioNode;

// This crate is the engine: the render, transport, metering, PDC. Wiring it
// into an ECS (the reconcile pipeline, graph resources, per-subsystem
// wrappers) is the host adapter's business, and lives in `bevy_tutti::graph`.
// `AudioNode` above carries a gated `Component` derive because it is the one
// handle a host addresses by name.

/// What a host driving the engine names, in one import.
///
/// The shared vocabulary (units, `ChannelLayout`, the I/O edge traits) is
/// `tutti_types::prelude`'s, not repeated here. Not here, and spelled in full
/// instead: `Result`, which would shadow a name a consumer already has.
pub mod prelude {
    pub use crate::AudioNode;
    pub use crate::{AudioTap, LoadMeter, LoadReading, MasterMeter, MeterReading, TapBusy};
    pub use crate::{Engine, Error, MAX_ROOT_CHANNELS};
    pub use crate::{MotionEvent, Timeline, Transport, TransportState};
}
