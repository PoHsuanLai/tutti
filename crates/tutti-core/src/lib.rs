#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{Error, Result};

mod engine;

// `MAX_ROOT_CHANNELS` comes to the root with `Engine`: it is the ceiling on the
// root's own output width, so a host sizing a scratch buffer for `process` has
// to name it, and names it from the root.
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError, MAX_ROOT_CHANNELS};

pub mod transport;
pub use transport::{
    FadeOut, FrozenClock, LoopEdit, LoopRegion, MeterSlot, MeterSlotCell, MeterSlotReader,
    MotionEvent, MotionFsm, MotionState, OfflineTimeline, OfflineTimelineConfig, ProjectMeter,
    QueueFull, RenderClock, SCHEDULE_CAPACITY, ScheduleFull, Then, Timeline, Transport,
    TransportClock, TransportCommand, TransportSettings, TransportState,
};

mod metering;
pub use metering::{
    AtomicAmplitude, AudioTap, LOAD_MEAN_TIME_CONSTANT, LoadMeter, LoadReading, MasterMeter,
    MeterReading, MeteringContext, TapBusy, TapCons, meter_output,
};

// This crate is the engine: the render, transport, metering. Wiring it into an
// ECS (the reconcile pipeline, graph resources, per-subsystem wrappers) is the
// host adapter's business, and lives in `bevy_tutti::graph`.

/// What a host driving the engine names, in one import.
///
/// The shared vocabulary (units, `ChannelLayout`, the I/O edge traits) is
/// `tutti_types::prelude`'s, not repeated here. Not here, and spelled in full
/// instead: `Result`, which would shadow a name a consumer already has.
pub mod prelude {
    pub use crate::{AudioTap, LoadMeter, LoadReading, MasterMeter, MeterReading, TapBusy};
    pub use crate::{Engine, Error, MAX_ROOT_CHANNELS};
    pub use crate::{MotionEvent, Timeline, Transport, TransportState};
}
