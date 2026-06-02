//! Fluent API handles for transport and metronome control

use super::click::{ClickState, MetronomeMode};
use super::{MotionState, TransportManager};
use crate::compat::Arc;
use crate::params::{Bpm, Linear};

/// Fluent API handle for metronome control.
///
/// Created via `transport.metronome()`.
///
/// # Example
/// ```ignore
/// engine.transport()
///     .metronome()
///     .volume(0.7)
///     .accent_every(4)
///     .always();
/// ```
#[derive(Clone)]
pub struct MetronomeHandle {
    state: Arc<ClickState>,
}

impl MetronomeHandle {
    pub(crate) fn new(state: Arc<ClickState>) -> Self {
        Self { state }
    }

    /// Volume: 0.0 to 1.0.
    pub fn volume(&self, volume: impl Into<Linear>) -> &Self {
        self.state.set_volume(volume.into().get());
        self
    }

    pub fn get_volume(&self) -> Linear {
        Linear(self.state.volume())
    }

    pub fn accent_every(&self, beats: u32) -> &Self {
        self.state.set_accent_every(beats);
        self
    }

    pub fn get_accent_every(&self) -> u32 {
        self.state.accent_every()
    }

    pub fn mode(&self, mode: MetronomeMode) -> &Self {
        self.state.set_mode(mode);
        self
    }

    pub fn get_mode(&self) -> MetronomeMode {
        self.state.mode()
    }

    pub fn off(&self) -> &Self {
        self.state.set_mode(MetronomeMode::Off);
        self
    }

    pub fn always(&self) -> &Self {
        self.state.set_mode(MetronomeMode::Always);
        self
    }

    pub fn recording_only(&self) -> &Self {
        self.state.set_mode(MetronomeMode::RecordingOnly);
        self
    }

    pub fn preroll_only(&self) -> &Self {
        self.state.set_mode(MetronomeMode::PrerollOnly);
        self
    }
}

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
///     .metronome()
///         .volume(0.7)
///         .accent_every(4)
///         .always()
///     .play();
/// ```
#[derive(Clone)]
pub struct TransportHandle {
    transport: Arc<TransportManager>,
    click_state: Arc<ClickState>,
    metronome: MetronomeHandle,
}

impl TransportHandle {
    /// Wire a transport handle around shared transport + click state.
    /// Normally built through the engine; exposed here so custom constructions
    /// (e.g. export contexts, standalone vocabulary use) can attach a handle.
    pub fn new(transport: Arc<TransportManager>, click_state: Arc<ClickState>) -> Self {
        let metronome = MetronomeHandle::new(click_state.clone());
        Self {
            transport,
            click_state,
            metronome,
        }
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

    pub fn stop_immediate(&self) -> &Self {
        self.transport.stop_immediate();
        self
    }

    pub fn seek(&self, beats: f64) -> &Self {
        self.transport.locate(beats);
        self
    }

    pub fn seek_and_play(&self, beats: f64) -> &Self {
        self.transport.locate_and_play(beats);
        self
    }

    pub fn seek_with_declick(&self, beats: f64) -> &Self {
        self.transport.locate_with_declick(beats);
        self
    }

    pub fn fast_forward(&self) -> &Self {
        self.transport.fast_forward();
        self
    }

    pub fn rewind(&self) -> &Self {
        self.transport.rewind();
        self
    }

    pub fn end_scrub(&self) -> &Self {
        self.transport.end_scrub();
        self
    }

    pub fn reverse(&self) -> &Self {
        self.transport.reverse();
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

    pub fn clear_loop(&self) -> &Self {
        self.transport.clear_loop();
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

    /// Use `locate()` for FSM-based seeking.
    pub fn set_current_beat(&self, beat: f64) -> &Self {
        self.transport.set_current_beat(beat);
        self
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::Rolling)
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::Stopped)
    }

    pub fn is_seeking(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::DeclickToLocate)
    }

    pub fn is_stopping(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::DeclickToStop)
    }

    pub fn is_fast_forwarding(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::FastForward)
    }

    pub fn is_rewinding(&self) -> bool {
        matches!(self.transport.motion_state(), MotionState::Rewind)
    }

    pub fn motion_state(&self) -> MotionState {
        self.transport.motion_state()
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

    pub fn is_in_preroll(&self) -> bool {
        self.transport.is_in_preroll()
    }

    pub fn start_preroll(&self) -> &Self {
        self.transport.set_in_preroll(true);
        self
    }

    pub fn end_preroll(&self) -> &Self {
        self.transport.set_in_preroll(false);
        self
    }

    /// Borrow the metronome control handle owned by this transport.
    ///
    /// Returns a reference to a `MetronomeHandle` built once at
    /// construction time. The metronome's setters chain via `&self`
    /// just like the transport's, but the chain ends at the metronome
    /// — re-entering the transport from within a metronome chain
    /// is not supported. Split the chain into separate statements
    /// when both need to be configured.
    pub fn metronome(&self) -> &MetronomeHandle {
        &self.metronome
    }

    pub fn manager(&self) -> &Arc<TransportManager> {
        &self.transport
    }

    pub fn click_state(&self) -> &Arc<ClickState> {
        &self.click_state
    }
}

impl super::TransportReader for TransportHandle {
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
