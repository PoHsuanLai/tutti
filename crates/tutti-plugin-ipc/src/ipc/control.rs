//! [`Control`] over the bridge, for a plugin loaded ([`Ipc`]) or active
//! ([`IpcController`]): the same calls either way, answered by the bridge.
//!
//! None of them reads its [`MainThread`] token: the bridge answers from any
//! thread (design 021 §10.4 (d)), and the token is the in-process trait's
//! proof, not a hop. A caller with no token uses the bridge directly
//! ([`Ipc::bridge`]).

use dpi::Size;
use tutti_plugin_types::{
    AnyThread, AutomationMode, AutomationModeError, BlockCounts, ContentScale, Control, Delivered,
    EditorCapabilities, EditorPresence, EditorWindow, Features, Health, LoadedPlugin, MainThread,
    Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginFailure, PluginResult,
    Preset, PresetId, ProcessWake, RenderMode, RenderModeSupport, Request, SampleRate, StateError,
    StateUse, TerminationReason, TrackInfo, WindowHandle,
};

use super::{Ipc, IpcController};

/// `$ty`'s [`AnyThread`], [`EditorWindow`] and [`Control`], all through its
/// `link`.
///
/// Each call both traits carry has **one** body, in `AnyThread`; `Control`'s
/// forwards to it. The two exist because `Control` takes the main-thread
/// token a served plugin does not need, and a handle's off-thread calls have
/// none to give.
macro_rules! served_control {
    ($ty:ty) => {
        impl AnyThread for $ty {
            fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
                self.link.bridge.parameters()
            }

            fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
                self.link.bridge.parameter(id)
            }

            /// Stays `Normalized` to the wire, which narrows it to an `f32`.
            /// The server's decode refuses a value outside 0..=1 (NaN
            /// included): the wire is a foreign boundary and the peer is
            /// another process.
            fn set_parameter_value(&self, id: ParamId, value: Normalized) {
                let _ = self.link.bridge.set_parameter_rt(id, value);
            }

            fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
                self.link.bridge.parameter_plain(id, value)
            }

            fn parameter_normalized(&self, id: ParamId, plain: f64) -> Option<Normalized> {
                self.link.bridge.parameter_normalized(id, plain)
            }

            fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
                self.link.bridge.parameter_text(id, value)
            }

            /// Asked of the audio instance, like every other parameter read.
            /// On VST2 this also *writes* that instance (`effString2Parameter`
            /// parses by applying), the instance `set_parameter_value`
            /// targets; the editor's mirror follows through the existing
            /// `PluginParamValuesChanged` refresh.
            fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
                self.link.bridge.parameter_value_from_text(id, text)
            }

            fn save_state(&self) -> Result<Vec<u8>, StateError> {
                self.link.bridge.save_state(StateUse::Project)
            }

            /// Queued on the command bus as the blocks around it are, so the
            /// change lands between two blocks. `true` when queued, which is
            /// not whether the plugin acts on the mode (the load-time
            /// `Features::RENDER_MODE` says), and never needs a restart.
            fn set_render_mode(&self, mode: RenderMode) -> bool {
                !self.link.bridge.is_crashed() && self.link.bridge.set_render_mode_rt(mode)
            }

            /// Delivered to both the audio subprocess and the in-process GUI
            /// (the knob-glow lives in the GUI). [`Delivered`] says whether
            /// the command was *queued*; no format confirms the plugin
            /// visibly reacted.
            fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError> {
                if self.link.bridge.is_crashed() {
                    return Err(AutomationModeError::PluginDead);
                }
                automation_push_outcome(self.link.bridge.set_automation_state_rt(mode))
            }
        }

        impl EditorWindow for $ty {
            fn has_editor(&self, _main: &MainThread) -> bool {
                !matches!(self.link.descriptor.editor, EditorPresence::Absent)
            }

            fn open_editor(
                &mut self,
                main: &MainThread,
                parent: WindowHandle,
                scale: ContentScale,
            ) -> PluginResult<Size> {
                Ok(self.link.bridge.open_editor(main, parent, scale)?)
            }

            fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
                Ok(self.link.bridge.open_floating_editor(main)?)
            }

            fn set_editor_scale(
                &mut self,
                main: &MainThread,
                scale: ContentScale,
            ) -> PluginResult<()> {
                Ok(self.link.bridge.set_editor_scale(main, scale)?)
            }

            fn close_editor(&mut self, main: &MainThread) {
                let _ = self.link.bridge.close_editor(main);
            }

            fn editor_idle(&mut self, main: &MainThread) {
                self.link.bridge.editor_idle(main);
            }

            fn wakeups(&mut self, main: &MainThread, out: &mut tutti_plugin_types::Wakeups) {
                self.link.bridge.wakeups(main, out);
            }

            fn wake(
                &mut self,
                main: &MainThread,
                ready: &[tutti_plugin_types::Ready],
                now: std::time::Instant,
            ) {
                self.link.bridge.wake(main, ready, now);
            }

            fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
                self.link.bridge.editor_capabilities(main)
            }

            fn set_editor_size(
                &mut self,
                main: &MainThread,
                requested: Size,
            ) -> PluginResult<Size> {
                Ok(self.link.bridge.set_editor_size(main, requested)?)
            }

            fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<Size> {
                self.link.bridge.poll_editor_resize_request(main)
            }
        }

        impl Control for $ty {
            fn info(&self) -> &PluginDescriptor {
                &self.link.descriptor
            }

            fn wiring(&self, _main: &MainThread) -> LoadedPlugin {
                self.link.loaded.clone()
            }

            fn parameters(&self, _main: &MainThread) -> Vec<ParameterInfo> {
                AnyThread::parameter_descriptors(self).unwrap_or_default()
            }

            fn parameter(&self, _main: &MainThread, id: ParamId) -> Option<Normalized> {
                AnyThread::parameter_value(self, id)
            }

            fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
                AnyThread::set_parameter_value(self, id, value);
            }

            fn parameter_plain(
                &self,
                _main: &MainThread,
                id: ParamId,
                value: Normalized,
            ) -> Option<f64> {
                AnyThread::parameter_plain(self, id, value)
            }

            fn parameter_normalized(
                &self,
                _main: &MainThread,
                id: ParamId,
                plain: f64,
            ) -> Option<Normalized> {
                AnyThread::parameter_normalized(self, id, plain)
            }

            fn parameter_text(
                &self,
                _main: &MainThread,
                id: ParamId,
                value: Normalized,
            ) -> Option<String> {
                AnyThread::parameter_text(self, id, value)
            }

            fn parameter_value_from_text(
                &self,
                _main: &MainThread,
                id: ParamId,
                text: &str,
            ) -> Option<Normalized> {
                AnyThread::parameter_value_from_text(self, id, text)
            }

            fn save_state(
                &mut self,
                _main: &MainThread,
                state_use: StateUse,
            ) -> PluginResult<Vec<u8>> {
                self.link
                    .bridge
                    .save_state(state_use)
                    .map_err(PluginError::StateFailed)
            }

            fn load_state(
                &mut self,
                main: &MainThread,
                bytes: &[u8],
                state_use: StateUse,
            ) -> PluginResult<()> {
                self.link
                    .bridge
                    .load_state(main, bytes, state_use)
                    .map_err(PluginError::StateFailed)
            }

            fn presets(&mut self, _main: &MainThread) -> Vec<Preset> {
                self.link.bridge.presets().unwrap_or_default()
            }

            fn load_preset(&mut self, _main: &MainThread, id: &PresetId) -> bool {
                !self.link.bridge.is_crashed() && self.link.bridge.load_preset(id.clone())
            }

            fn current_preset(&mut self, _main: &MainThread) -> Option<PresetId> {
                self.link.bridge.current_preset()
            }

            /// Advisory only: the outcome `AnyThread::set_automation_mode`
            /// reports has no `Control` form.
            fn set_automation_state(&mut self, _main: &MainThread, mode: AutomationMode) {
                let _ = AnyThread::set_automation_mode(self, mode);
            }

            /// What the plugin re-reported since the last call (a latency or
            /// a tail), as the requests the lifecycle answers by re-reading.
            fn take_requests(&mut self, _main: &MainThread, out: &mut Vec<Request>) {
                let mut asked = self
                    .link
                    .events
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                out.append(&mut asked);
            }

            /// Sent to the server, whose format answers the plugin's reads
            /// from it: queued ([`Yes`](Delivered::Yes)), refused because the
            /// queue is full ([`Dropped`](Delivered::Dropped)) or because the
            /// plugin is gone ([`PluginDead`](Delivered::PluginDead)).
            fn set_track_info(&mut self, _main: &MainThread, info: &TrackInfo) -> Delivered {
                self.link.bridge.set_track_info(info)
            }

            /// What the server's `ProcessRequested` raises, from the bridge's
            /// thread.
            fn bind_wake(&self, wake: ProcessWake) {
                let _ = self.link.events.wake.set(wake);
            }

            /// `Live` from here wherever the plugin has a mode: the server
            /// tells it or activates it again itself, so the host never
            /// reactivates a served plugin for its mode.
            fn render_mode_support(&self, _main: &MainThread) -> RenderModeSupport {
                if self.link.loaded.features.contains(Features::RENDER_MODE) {
                    RenderModeSupport::Live
                } else {
                    RenderModeSupport::Unsupported
                }
            }

            fn set_render_mode(&mut self, _main: &MainThread, mode: RenderMode) -> bool {
                AnyThread::set_render_mode(self, mode)
            }

            /// The edits the host lost: counted by the bridge, which forwards
            /// a served plugin's edits through its listener
            /// ([`Ipc::forward_events`]).
            fn dropped_reported_edits(&self) -> u64 {
                self.link.bridge.dropped_parameter_events()
            }
        }
    };
}

served_control!(Ipc);
served_control!(IpcController);

/// A served plugin's liveness, from the bridge and the server's guard.
impl Health for IpcController {
    fn is_crashed(&self) -> bool {
        self.link.bridge.is_crashed()
    }

    fn crash_cause(&self) -> Option<PluginFailure> {
        self.link.bridge.crash_cause()
    }

    fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        self.link.bridge.rate_refusal()
    }

    fn block_counts(&self) -> BlockCounts {
        self.link.bridge.block_counts()
    }

    fn terminate(&self, reason: TerminationReason) {
        // The cause first: the kill makes the bridge thread read an EOF, and
        // the first cause latched wins.
        self.link
            .bridge
            .mark_crashed(PluginFailure::Terminated { reason });
        self.link.guard.terminate();
    }
}

/// Turn a queue outcome into the error a UI shows.
///
/// The two failures produce **different** errors because they call for
/// different responses: a dropped push is transient and worth retrying, a dead
/// plugin is permanent.
fn automation_push_outcome(delivered: Delivered) -> Result<(), AutomationModeError> {
    match delivered {
        Delivered::Yes => Ok(()),
        Delivered::Dropped => Err(AutomationModeError::Dropped),
        Delivered::PluginDead => Err(AutomationModeError::PluginDead),
    }
}

#[cfg(test)]
mod automation_outcome_tests {
    use super::*;

    /// A dropped push and a dead plugin must not produce the same error.
    ///
    /// This is the property the `bool` could not express, and the only reason
    /// [`Delivered`] exists rather than a two-state answer: one is transient and
    /// worth retrying, the other is permanent. Asserting they *differ* is what
    /// fails if someone later folds the two arms back together — an assertion
    /// on either arm alone would survive that.
    #[test]
    fn a_dropped_push_and_a_dead_plugin_report_differently() {
        let dropped = automation_push_outcome(Delivered::Dropped)
            .expect_err("a dropped push is not a success");
        let dead = automation_push_outcome(Delivered::PluginDead)
            .expect_err("a dead plugin is not a success");

        assert!(
            matches!(dropped, AutomationModeError::Dropped),
            "a full queue is the plugin declining to be reached, not a crash: {dropped:?}"
        );
        assert!(
            matches!(dead, AutomationModeError::PluginDead),
            "a dead plugin must report as crashed: {dead:?}"
        );
        assert_ne!(
            dropped.to_string(),
            dead.to_string(),
            "the two failures must be distinguishable by a user reading the message"
        );
    }

    /// The negative half: a delivered push is not reported as a failure. Without
    /// this, folding every arm to `Err` would still pass the test above.
    #[test]
    fn a_delivered_push_is_not_an_error() {
        assert!(automation_push_outcome(Delivered::Yes).is_ok());
    }
}
