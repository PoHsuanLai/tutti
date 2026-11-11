//! [`Control`] over the bridge, for a plugin loaded ([`Ipc`]) or active
//! ([`IpcController`]): the same calls either way, answered by the bridge.
//!
//! None of them reads its [`MainThread`] token: the bridge answers from any
//! thread (design 021 §10.4 (d)), and the token is the in-process trait's
//! proof, not a hop. A caller with no token uses the bridge directly
//! ([`Ipc::bridge`]).

use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, EditorPresence, EditorSize, EditorWindow, LoadedPlugin,
    MainThread, Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginResult,
    Preset, PresetId, RenderMode, Request, WindowHandle,
};

use super::{Ipc, IpcController, Link};

impl Link {
    fn state_error(error: tutti_plugin_types::StateError) -> PluginError {
        PluginError::State(error.to_string())
    }
}

/// `$ty`'s [`EditorWindow`] and [`Control`], both through its `link`.
macro_rules! served_control {
    ($ty:ty) => {
        impl EditorWindow for $ty {
            fn has_editor(&self, _main: &MainThread) -> bool {
                !matches!(self.link.descriptor.editor, EditorPresence::Absent)
            }

            fn open_editor(
                &mut self,
                main: &MainThread,
                parent: WindowHandle,
                scale: ContentScale,
            ) -> PluginResult<EditorSize> {
                Ok(self.link.bridge.open_editor(main, parent, scale)?)
            }

            fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
                Ok(self.link.bridge.open_floating_editor(main)?)
            }

            fn close_editor(&mut self, main: &MainThread) {
                let _ = self.link.bridge.close_editor(main);
            }

            fn editor_idle(&mut self, main: &MainThread) {
                self.link.bridge.editor_idle(main);
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
                self.link.bridge.parameters().unwrap_or_default()
            }

            fn parameter(&self, _main: &MainThread, id: ParamId) -> Option<Normalized> {
                self.link.bridge.parameter(id)
            }

            fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
                let _ = self.link.bridge.set_parameter_rt(id, value);
            }

            fn parameter_text(
                &self,
                _main: &MainThread,
                id: ParamId,
                value: Normalized,
            ) -> Option<String> {
                self.link.bridge.parameter_text(id, value)
            }

            fn parameter_value_from_text(
                &self,
                _main: &MainThread,
                id: ParamId,
                text: &str,
            ) -> Option<Normalized> {
                self.link.bridge.parameter_value_from_text(id, text)
            }

            fn save_state(&mut self, _main: &MainThread) -> PluginResult<Vec<u8>> {
                self.link.bridge.save_state().map_err(Link::state_error)
            }

            fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
                self.link
                    .bridge
                    .load_state(main, bytes)
                    .map_err(Link::state_error)
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

            fn set_automation_state(&mut self, _main: &MainThread, mode: AutomationMode) {
                let _ = self.link.bridge.set_automation_state_rt(mode);
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

            /// Queued on the command bus as the blocks around it are, so the
            /// change lands between two blocks. `true` when queued, which is
            /// not whether the plugin acts on the mode (the load-time
            /// `Features::RENDER_MODE` says), and never needs a restart.
            fn set_render_mode(&mut self, _main: &MainThread, mode: RenderMode) -> bool {
                !self.link.bridge.is_crashed() && self.link.bridge.set_render_mode_rt(mode)
            }

            /// Nothing here: a served plugin's edits reach the host through
            /// the bridge's listener ([`Ipc::forward_events`]).
            fn dropped_reported_edits(&self) -> u64 {
                self.link.bridge.dropped_parameter_events()
            }
        }
    };
}

served_control!(Ipc);
served_control!(IpcController);
