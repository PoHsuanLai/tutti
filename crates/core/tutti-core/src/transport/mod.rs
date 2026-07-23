mod automation_reader;
mod click;
mod clock;
pub(crate) mod fsm;
mod handle;
mod motion;
mod offline;
pub(crate) mod position;
mod settings;
mod state;

pub use automation_reader::{AutomationEnvelopeFn, AutomationReaderInput};
pub use click::{ClickNode, ClickSettings, ClickState, MetronomeHandle, MetronomeMode};
pub use clock::TransportClock;
pub use fsm::Direction;
pub use handle::Transport;
pub use motion::{MotionEvent, MotionFsm, MotionState, QueueFull};
pub use offline::{OfflineTimeline, OfflineTimelineConfig};
pub use settings::TransportSettings;
pub use state::{
    beat_from_ports, ClockInputs, Declick, LoopRange, LoopSpan, SeekSlot, TransportState,
    BEAT_PORTS,
};

#[cfg(feature = "bevy")]
pub mod plugin;
#[cfg(feature = "bevy")]
pub use plugin::{
    MetronomeRes, PendingMetronome, PendingTransport, TransportClockNode, TransportRes,
    TuttiTransportPlugin,
};

/// A musical timeline: where we are, how fast, and whether it is moving.
///
/// Implemented by the live [`Transport`] and by [`OfflineTimeline`], so a
/// beat-driven source can be handed either one and not care which.
///
/// # When to use this instead of a beat edge
///
/// Pure DSP nodes should **not** implement against this trait. A node that is a
/// function of musical time takes the beat as a signal on its input ports (see
/// [`BEAT_PORTS`]) — that is per-sample accurate, works unchanged offline, and
/// makes the timeline→node relationship a visible graph edge.
///
/// What legitimately remains here is what a beat signal cannot express:
///
/// - **Boolean gating** — `is_rolling` drives early returns with state-reset
///   side effects. "Emit nothing" is not the same as "emit a level", and a
///   paused timeline still has a valid beat, so rolling-ness is not
///   recoverable from the beat.
/// - **Nodes with no ports** — the MIDI sources implement `poll_into` and have
///   no `BufferRef` to read a beat from.
///
/// # What is deliberately NOT here
///
/// Recording and preroll are live-session facts, not timeline facts: an
/// offline render answers `false` to both forever. They live on
/// [`TransportSettings`] and are read directly by the one consumer that needs
/// them (the metronome).
pub trait Timeline: Send + Sync {
    /// Current position on the timeline.
    fn beat(&self) -> crate::params::Beat;
    /// Current tempo.
    fn tempo(&self) -> crate::params::Bpm;
    /// Whether time is advancing. An offline render is always rolling.
    fn is_rolling(&self) -> bool;
    /// The active loop region, or `None` when not looping. Always a valid,
    /// non-empty region — see [`LoopRange`].
    fn loop_range(&self) -> Option<LoopRange>;
}
