//! The out-of-process handle backend: a [`PluginBridge`] and the server's
//! lifetime guard, behind the handles' capability traits.

use std::ffi::c_void;
use std::sync::Arc;

use tutti_types::SampleRate;

use tutti_plugin_ipc::PluginBridge;
use tutti_plugin_types::BlockCounts;
use tutti_plugin_types::{
    AutomationMode, ContentScale, Delivered, EditorCapabilities, EditorError, EditorSize,
    MainThread, Normalized, ParamId, ParameterInfo, Preset, PresetId, RenderMode, StateError,
    WindowHandle,
};

use crate::host::handles::capabilities::{
    HostAutomationState, HostEditor, HostParams, HostPresets, HostRenderMode, HostState,
};
use crate::host::node::ProcessGuard;

/// Host-side capability backend for the out-of-process VST3 / CLAP / AU path —
/// implements [`HostParams`],
/// [`HostState`], and
/// [`HostEditor`].
///
/// Bundles the `PluginBridge` (audio IPC + lazy in-process GUI loader)
/// with the subprocess lifetime guard so dropping this backend tears
/// down both. The guard is `Arc`-shared with the `PluginClient` audio
/// node so the subprocess only dies once both the audio node and all
/// handles are gone.
pub(crate) struct SubprocessBackend {
    bridge: Arc<PluginBridge>,
    _guard: Arc<ProcessGuard>,
}

impl SubprocessBackend {
    pub(crate) fn new(bridge: Arc<PluginBridge>, guard: Arc<ProcessGuard>) -> Self {
        Self {
            bridge,
            _guard: guard,
        }
    }
}

impl HostParams for SubprocessBackend {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        self.bridge.parameters()
    }

    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        self.bridge.parameter(id)
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.bridge.parameter_text(id, value)
    }

    /// Asked of the audio instance, like every other parameter read here.
    ///
    /// On VST2 this also *writes* that instance (`effString2Parameter` parses by
    /// applying), which is the same instance `set_parameter_value` targets — so
    /// the value lands where a knob poke would, and the GUI mirror follows
    /// through the existing `PluginParamValuesChanged` refresh rather than a
    /// second write from here.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        self.bridge.parameter_value_from_text(id, text)
    }

    fn set_parameter_value(&self, id: ParamId, value: Normalized) {
        // Stays `Normalized` all the way to the wire, which narrows it to an
        // `f32`. The subprocess re-clamps on receipt — it must, since the wire
        // is a foreign boundary and the peer is another process — but the value
        // leaving here is already on the unit interval.
        self.bridge.set_parameter_rt(id, value);
    }

    fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    fn crash_cause(&self) -> Option<String> {
        self.bridge.crash_cause()
    }

    fn block_counts(&self) -> BlockCounts {
        self.bridge.block_counts()
    }

    fn terminate(&self, cause: &str) {
        // The cause first: the kill makes the bridge thread read an EOF, and
        // the first cause latched wins.
        self.bridge.mark_crashed(cause);
        self._guard.terminate();
    }

    fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        self.bridge.rate_refusal()
    }

    fn dropped_parameter_events(&self) -> u64 {
        self.bridge.dropped_parameter_events()
    }
}

impl HostState for SubprocessBackend {
    fn save_state(&self) -> std::result::Result<Vec<u8>, StateError> {
        self.bridge.save_state()
    }

    fn load_state(&self, main: &MainThread, data: &[u8]) -> std::result::Result<(), StateError> {
        self.bridge.load_state(main, data)
    }
}

impl HostEditor for SubprocessBackend {
    fn open_editor(
        &self,
        main: &MainThread,
        parent_ptr: *mut c_void,
        scale: ContentScale,
    ) -> std::result::Result<EditorSize, EditorError> {
        // SAFETY: `parent_ptr` is the platform-native window pointer
        // extracted by `extract_platform_ptr` from a caller-supplied
        // `RawWindowHandle`, which is alive for the open_editor call.
        let parent = unsafe { WindowHandle::from_ptr(parent_ptr) };
        self.bridge.open_editor(main, parent, scale)
    }

    fn open_floating_editor(&self, main: &MainThread) -> std::result::Result<(), EditorError> {
        self.bridge.open_floating_editor(main)
    }

    fn close_editor(&self, main: &MainThread) {
        self.bridge.close_editor(main);
    }

    fn editor_idle(&self, main: &MainThread) {
        self.bridge.editor_idle(main);
    }

    fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        self.bridge.editor_capabilities(main)
    }

    fn set_editor_size(
        &self,
        main: &MainThread,
        requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.set_editor_size(main, requested)
    }

    fn poll_editor_resize_request(&self, main: &MainThread) -> Option<EditorSize> {
        self.bridge.poll_editor_resize_request(main)
    }
}

impl HostAutomationState for SubprocessBackend {
    fn set_automation_mode(&self, mode: AutomationMode) -> std::result::Result<(), EditorError> {
        if self.bridge.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }
        // Delivered to both the audio subprocess and the in-process GUI (the
        // knob-glow lives in the GUI). [`Delivered`] says whether the command
        // was *queued*; no format confirms the plugin visibly reacted. The
        // format-neutral `AutomationMode` flows all the way to each ABI edge,
        // which encodes it.
        automation_push_outcome(self.bridge.set_automation_state_rt(mode))
    }
}

/// Turn a queue outcome into the error a UI shows.
///
/// A free function so it is reachable from a test: building a
/// [`SubprocessBackend`] needs a live subprocess, and the interesting behaviour
/// here is the mapping, not the plumbing.
///
/// The two failures produce **different** errors because they call for
/// different responses. Collapsed into one "not delivered" message, a UI could
/// report the failure but never tell the user whether retrying was worth it.
fn automation_push_outcome(delivered: Delivered) -> std::result::Result<(), EditorError> {
    match delivered {
        Delivered::Yes => Ok(()),
        Delivered::Dropped => Err(EditorError::PluginError(
            "automation-state push dropped: the command queue is full; \
             the next change should land"
                .into(),
        )),
        Delivered::PluginDead => Err(EditorError::PluginCrashed),
    }
}

impl HostPresets for SubprocessBackend {
    fn presets(&self, _main: &MainThread) -> Vec<Preset> {
        // A crashed subprocess yields `None`; flattened to an empty list
        // because the trait's contract is "what the plugin advertises", and a
        // caller distinguishing "cannot ask" from "listed nothing" reads
        // `Features::PRESET_LIST` rather than a sentinel here.
        self.bridge.presets().unwrap_or_default()
    }

    fn load_preset(&self, _main: &MainThread, id: &PresetId) -> bool {
        !self.bridge.is_crashed() && self.bridge.load_preset(id.clone())
    }

    fn current_preset(&self, _main: &MainThread) -> Option<PresetId> {
        self.bridge.current_preset()
    }
}

impl HostRenderMode for SubprocessBackend {
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        !self.bridge.is_crashed() && self.bridge.set_render_mode_rt(mode)
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
            matches!(dropped, EditorError::PluginError(_)),
            "a full queue is the plugin declining to be reached, not a crash: {dropped:?}"
        );
        assert!(
            matches!(dead, EditorError::PluginCrashed),
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
