mod automation_reader;
mod click;
mod clock;
pub(crate) mod fsm;
mod handle;
pub(crate) mod manager;
mod offline;
pub(crate) mod position;
pub mod sync;
pub(crate) mod tempo_map;

pub use automation_reader::{AutomationEnvelopeFn, AutomationReaderInput};
pub use click::{click, ClickNode, ClickSettings, ClickState, MetronomeMode};
pub use clock::TransportClock;
pub use handle::{MetronomeHandle, TransportHandle};
pub use manager::{Direction, MotionState, TransportManager};
pub use offline::{OfflineTransport, OfflineTransportConfig};
pub use sync::{SmpteFrameRate, SyncSnapshot, SyncSource, SyncState, SyncStatus};
pub use tempo_map::{TempoMap, TimeSignature, BBT};

#[cfg(feature = "bevy")]
pub mod plugin;
#[cfg(feature = "bevy")]
pub use plugin::{PendingTransport, TransportRes, TuttiTransportPlugin};

/// Trait for reading transport state.
///
/// This abstraction allows both live transport (`TransportHandle`) and
/// offline transport (`OfflineTransport`) to be used interchangeably by
/// nodes like `AutomationLane` that need beat position information.
pub trait TransportReader: Send + Sync {
    fn current_beat(&self) -> f64;
    /// Full f64 precision beat position. Defaults to `current_beat()`.
    /// Use this when sub-tick accuracy matters at high beat counts
    /// (f32 ULP exceeds ~0.002 past beat 16384).
    fn current_beat_f64(&self) -> f64 {
        self.current_beat()
    }
    fn is_loop_enabled(&self) -> bool;
    fn get_loop_range(&self) -> Option<(f64, f64)>;
    fn is_playing(&self) -> bool;
    fn is_recording(&self) -> bool;
    fn is_in_preroll(&self) -> bool;
    fn tempo(&self) -> crate::params::Bpm;
}
