mod automation_reader;
mod click;
mod clock;
pub(crate) mod fsm;
mod handle;
pub(crate) mod manager;
mod offline;
pub(crate) mod position;
mod state;

pub use automation_reader::{AutomationEnvelopeFn, AutomationReaderInput};
pub use click::{click, ClickNode, ClickSettings, ClickState, MetronomeMode};
pub use clock::TransportClock;
pub use handle::{MetronomeHandle, TransportHandle};
pub use manager::{Direction, MotionState, TransportManager};
pub use offline::{OfflineTransport, OfflineTransportConfig};
pub use state::{
    beat_from_ports, ClockInputs, Declick, LoopSpan, SeekSlot, TransportState, BEAT_PORTS,
};

#[cfg(feature = "bevy")]
pub mod plugin;
#[cfg(feature = "bevy")]
pub use plugin::{PendingTransport, TransportClockNode, TransportRes, TuttiTransportPlugin};

/// Read-only view of transport state — "what time is it, and are we rolling".
///
/// Implemented by both the live [`TransportHandle`] and [`OfflineTransport`],
/// so a consumer can be driven by either.
///
/// # When to use this instead of a beat edge
///
/// Pure DSP nodes should **not** implement against this trait. A node that is a
/// function of musical time takes the beat as a signal on its input ports (see
/// [`BEAT_PORTS`]) — that is per-sample accurate, works unchanged offline, and
/// makes the transport→node relationship a visible graph edge.
///
/// What legitimately remains here is what a beat signal cannot express:
///
/// - **Boolean gating** — `is_playing` / `is_recording` / `is_in_preroll` drive
///   early returns with state-reset side effects. "Emit nothing" is not the
///   same as "emit a level", and a paused transport still has a valid beat, so
///   pausedness is not recoverable from the beat.
/// - **Nodes with no ports** — the MIDI sources implement `poll_into` and have
///   no `BufferRef` to read a beat from.
/// - **Non-audio consumers** — the UI playhead and the plugin ABI bridge
///   (`TransportSource`), which are not in the audio graph at all.
pub trait TransportClockRead: Send + Sync {
    fn current_beat(&self) -> f64;
    fn is_loop_enabled(&self) -> bool;
    fn get_loop_range(&self) -> Option<(f64, f64)>;
    fn is_playing(&self) -> bool;
    fn is_recording(&self) -> bool;
    fn is_in_preroll(&self) -> bool;
    fn tempo(&self) -> crate::params::Bpm;
}
