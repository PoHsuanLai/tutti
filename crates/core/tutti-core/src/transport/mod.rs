mod beat_window;
mod click;
mod clock;
pub(crate) mod fsm;
mod handle;
mod meter;
mod motion;
mod offline;
pub(crate) mod position;
mod settings;
mod state;

pub use beat_window::{BeatWindow, BeatWindowSync};
pub use click::{ClickNode, ClickSettings, ClickState, MetronomeMode};
pub use clock::TransportClock;
pub use handle::Transport;
pub use meter::TimeSignature;
pub use motion::{MotionEvent, MotionFsm, MotionState, QueueFull};
pub use offline::{OfflineTimeline, OfflineTimelineConfig};
pub use settings::TransportSettings;
pub use state::{beat_from_ports, ClockInputs, Declick, LoopRange, LoopSpan, SeekSlot, BEAT_PORTS};

// The Bevy wrappers (`TransportRes` / `MetronomeRes` + their claims +
// `TuttiTransportPlugin`) live in `crate::ecs::transport` — import them from
// `tutti_core::ecs`.

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
/// Recording, looping, and preroll are live-session facts, not timeline facts:
/// an offline render either answers `false`/`None` forever or handles them by
/// direct field access, never through this trait. They live on the
/// [`TransportState`] supertrait (record + loop) and on [`TransportSettings`]
/// (preroll), read only where a genuinely live transport is required.
pub trait Timeline: Send + Sync {
    /// Current position on the timeline.
    fn beat(&self) -> crate::params::Beat;
    /// Current tempo.
    fn tempo(&self) -> crate::params::Bpm;
    /// Whether time is advancing. An offline render is always rolling.
    fn is_rolling(&self) -> bool;
}

/// A live transport: a [`Timeline`] that also carries the record/loop state a
/// plugin host asks for.
///
/// This is the "transport state" bundle plugins request by name — VST3's opt-in
/// flag is literally `kNeedTransportState`, VST2 prefixes the flags
/// `TRANSPORT_RECORDING`/`TRANSPORT_CYCLE_ACTIVE`, CLAP groups them under
/// `clap_transport_flags`. Splitting it off keeps [`Timeline`] minimal so an
/// [`OfflineTimeline`] stays a first-class `Timeline`: an offline render has no
/// record state and folds looping into its own `advance()`, so it implements
/// `Timeline` only and keeps `loop_range` as an inherent method.
///
/// The live [`Transport`] implements this; a consumer that needs record or loop
/// state (the plugin `TransportSource`, `ParamAutomationSource`) depends on
/// `dyn TransportState`, never the concrete backend.
pub trait TransportState: Timeline {
    /// Whether the transport is armed and recording. Always `false` offline.
    fn is_recording(&self) -> bool;
    /// The active loop region, or `None` when not looping. Always a valid,
    /// non-empty region — see [`LoopRange`].
    fn loop_range(&self) -> Option<LoopRange>;
}
