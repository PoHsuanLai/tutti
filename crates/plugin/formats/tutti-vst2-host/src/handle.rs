//! RAII wrapper around `vst::host::PluginInstance`.
//!
//! The wrapper exists so [`Vst2Instance`](crate::instance::Vst2Instance)'s
//! Drop semantics are spelled out in one place: close the editor, suspend
//! audio, then run the plugin's own teardown — which dispatches `effClose`.
//!
//! # `effClose` vs `dlclose`
//!
//! These are two different events and the JUCE static-destructor crash people
//! work around belongs to only one of them.
//!
//! * `effClose` tells the plugin to release *this instance*: free its
//!   `AEffect`, drop its DSP state, and — for licensed plugins — hand back the
//!   session seat. It is safe, and it is mandatory. Skip it and every A/B of a
//!   plugin slot leaks one live instance and one licence.
//! * `dlclose` unloads the shared *module*, running its static destructors.
//!   That is the step that crashes with JUCE-based plugins, so hosts (JUCE and
//!   Ardour both) never do it.
//!
//! This handle used to be `ManuallyDrop` and skipped the instance destructor
//! entirely, which had it exactly backwards: `effClose` never ran, while the
//! `Arc<Library>` inside still dropped. The module leak now lives where it
//! belongs — `vst::host::PluginInstance` holds its `Library` in a
//! `ManuallyDrop` — so dropping the instance here closes the plugin without
//! ever unloading the module.

use vst::host::PluginInstance;
use vst::plugin::Plugin as _;

/// Owns one VST2 plugin instance plus its (single) editor handle.
///
/// VST2 fuses the editor and audio processor into the same `AEffect`, so
/// the editor is bound to this instance — `get_editor()` is called once
/// at construction and the `Box<dyn vst::editor::Editor>` is stored
/// alongside. There is no way to load a second editor against the same
/// audio instance.
pub(crate) struct Vst2Handle {
    /// Dropped normally: its destructor dispatches `effClose`. The module is
    /// what stays loaded, not the instance — see the module docs.
    pub(crate) instance: PluginInstance,
    pub(crate) editor: Option<SendEditor>,
}

/// Wrapper to make `Box<dyn Editor>` `Send`.
///
/// SAFETY: the editor is only accessed from one thread at a time. In the
/// in-process backend a `parking_lot::Mutex` serializes access; the
/// subprocess server is single-threaded for plugin operations. GUI
/// methods (`open`, `close`, `idle`) are called on the main thread; the
/// audio thread never touches the editor.
pub(crate) struct SendEditor(pub(crate) Box<dyn vst::editor::Editor>);
unsafe impl Send for SendEditor {}

impl Vst2Handle {
    pub(crate) fn new(mut instance: PluginInstance) -> Self {
        // get_editor() can only be called once per PluginInstance — the
        // vst crate sets `is_editor_active = true` on first call, returning
        // None on subsequent calls. Probed once here so callers can ask
        // `has_editor()` later without re-entering the plugin.
        let editor = instance.get_editor().map(SendEditor);
        Self { instance, editor }
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.editor.is_some()
    }
}

impl Drop for Vst2Handle {
    fn drop(&mut self) {
        // Close the editor first — some plugins read audio state during
        // editor teardown (e.g., to flush meter buffers).
        if let Some(mut editor) = self.editor.take() {
            editor.0.close();
        }

        // Suspend audio processing so the plugin releases any RT-allocated
        // resources before it is closed.
        self.instance.suspend();

        // `self.instance` is then dropped normally, which dispatches
        // `effClose`. The shared module is *not* unloaded — that leak lives in
        // `PluginInstance` itself. See the module docs for why the two must not
        // be conflated.
    }
}
