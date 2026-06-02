//! RAII wrapper around `vst::host::PluginInstance`.
//!
//! The wrapper exists so [`Vst2Instance`](crate::instance::Vst2Instance)'s
//! Drop semantics are spelled out in one place: close the editor, suspend
//! audio, then leak the underlying `PluginInstance` so JUCE-based plugins
//! don't crash inside their static destructor sequence on host shutdown.
//! Same workaround the rest of tutti's plugin-bridge uses for VST3/CLAP
//! (see `tutti-plugin/src/bridge/composite.rs`).

use std::mem::ManuallyDrop;

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
    /// `ManuallyDrop` so [`Drop::drop`] can deliberately skip running the
    /// inner destructor (the JUCE-leak workaround documented at the
    /// module level).
    pub(crate) instance: ManuallyDrop<PluginInstance>,
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
        // None on subsequent calls. We probe it here so callers can ask
        // `has_editor()` later without re-entering the plugin.
        let editor = instance.get_editor().map(SendEditor);
        Self {
            instance: ManuallyDrop::new(instance),
            editor,
        }
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
        // resources before we walk away from the instance.
        self.instance.suspend();

        // JUCE-based plugins (and several others) crash during their
        // static destructor sequence on dlclose. Skipping the destructor
        // by *not* calling `ManuallyDrop::drop` is the standard host-side
        // mitigation — same approach used for VST3/CLAP in
        // `tutti-plugin/src/bridge/composite.rs::Drop`. The plugin's
        // memory is reclaimed when the host process exits.
    }
}
