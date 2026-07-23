//! Fluent API handle for transport control.

use super::{MotionState, TransportManager};
use crate::params::Bpm;
use std::sync::Arc;

/// Fluent API handle for transport control.
///
/// Created via `engine.transport()`.
///
/// # Example
/// ```ignore
/// engine.transport()
///     .tempo(128.0)
///     .loop_range(0.0, 16.0)
///     .enable_loop()
///     .play();
/// ```
#[derive(Clone)]
pub struct TransportHandle {
    transport: Arc<TransportManager>,
}

impl TransportHandle {
    /// Wire a transport handle around shared transport state.
    /// Normally built through the engine; exposed here so custom constructions
    /// (e.g. export contexts, standalone vocabulary use) can attach a handle.
    pub fn new(transport: Arc<TransportManager>) -> Self {
        Self { transport }
    }

    pub fn tempo(&self, bpm: impl Into<Bpm>) -> &Self {
        self.transport.set_tempo(bpm.into());
        self
    }

    pub fn get_tempo(&self) -> Bpm {
        self.transport.get_tempo()
    }

    pub fn play(&self) -> &Self {
        self.transport.play();
        self
    }

    pub fn stop(&self) -> &Self {
        self.transport.stop();
        self
    }

    pub fn seek(&self, beats: f64) -> &Self {
        self.transport.locate(beats);
        self
    }

    pub fn loop_range(&self, start: f64, end: f64) -> &Self {
        self.transport.set_loop_range_fsm(start, end);
        self
    }

    pub fn enable_loop(&self) -> &Self {
        self.transport.set_loop_enabled(true);
        self
    }

    pub fn disable_loop(&self) -> &Self {
        self.transport.set_loop_enabled(false);
        self
    }

    pub fn toggle_loop(&self) -> &Self {
        self.transport.toggle_loop();
        self
    }

    pub fn get_loop_range(&self) -> Option<(f64, f64)> {
        self.transport.get_loop_range()
    }

    pub fn is_loop_enabled(&self) -> bool {
        self.transport.is_loop_enabled()
    }

    pub fn current_beat(&self) -> f64 {
        self.transport.get_current_beat()
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::Rolling)
    }

    pub fn is_recording(&self) -> bool {
        self.transport.is_recording()
    }

    pub fn record(&self) -> &Self {
        self.transport.set_recording(true);
        self
    }

    pub fn stop_recording(&self) -> &Self {
        self.transport.set_recording(false);
        self
    }
}

impl super::TransportClockRead for TransportHandle {
    fn current_beat(&self) -> f64 {
        self.transport.get_current_beat()
    }

    fn is_loop_enabled(&self) -> bool {
        self.transport.is_loop_enabled()
    }

    fn get_loop_range(&self) -> Option<(f64, f64)> {
        self.transport.get_loop_range()
    }

    fn is_playing(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::Rolling)
    }

    fn is_recording(&self) -> bool {
        self.transport.is_recording()
    }

    fn is_in_preroll(&self) -> bool {
        self.transport.is_in_preroll()
    }

    fn tempo(&self) -> Bpm {
        self.transport.get_tempo()
    }
}
