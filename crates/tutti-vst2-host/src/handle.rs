//! RAII wrapper around `vst::host::PluginInstance`.
//!
//! The wrapper exists so [`Vst2Loaded`](crate::Vst2Loaded)'s Drop semantics are
//! spelled out in one place: close the editor, suspend audio, then run the
//! plugin's own teardown — which dispatches `effClose`.
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
//! So the instance is dropped normally and the module is leaked: the leak
//! lives in `vst::host::PluginInstance`, which holds its `Library` in a
//! `ManuallyDrop`. `effClose` runs when the last handle on the instance's core
//! drops: the instance, the parameter object (`Vst2Loaded::params`), the editor and
//! the Processor's render handle all hold it, so no handle outlives the close. On
//! the control side that is `Vst2Loaded`'s drop (the Processor's handles are
//! released before its `Hold`, so they are never last). The module is never
//! unloaded.

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
    /// One of the handles on the plugin's core, which dispatches `effClose` when the
    /// last of them drops. The module is what stays loaded, not the instance — see
    /// the module docs.
    pub(crate) instance: PluginInstance,
    pub(crate) editor: Option<SendEditor>,
    /// Whether the last `effMainsChanged` dispatched carried `value=1`.
    ///
    /// Tracked because `effMainsChanged` is not documented as idempotent and real
    /// plugins reallocate rate-dependent buffers on every `resume(1)`. The
    /// rejoin reads this to make the transition edge-triggered, and `Drop` reads
    /// it so a plugin that is not resumed is not suspended a second time. Lives
    /// here, with the instance it describes.
    pub(crate) resumed: bool,
}

/// Wrapper to make `Box<dyn Editor>` `Send`.
///
/// The editor is only accessed from one thread at a time, because it
/// lives in the `Vst2Loaded` the Controller reaches through `&mut self` (clause
/// 2 of the invariant on `Shell`). GUI methods (`open`, `close`, `idle`) are
/// called on the main thread; the audio thread never touches the editor.
pub(crate) struct SendEditor(pub(crate) Box<dyn vst::editor::Editor>);
// SAFETY: the editor is only accessed from one thread at a time (see the type's
// docs); the audio thread never touches it.
unsafe impl Send for SendEditor {}

impl Vst2Handle {
    pub(crate) fn new(mut instance: PluginInstance) -> Self {
        // get_editor() can only be called once per PluginInstance — the
        // vst crate sets `is_editor_active = true` on first call, returning
        // None on subsequent calls. Probed once here so callers can ask
        // `has_editor()` later without re-entering the plugin.
        let editor = instance.get_editor().map(SendEditor);
        Self {
            instance,
            editor,
            resumed: false,
        }
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.editor.is_some()
    }
}

impl Drop for Vst2Handle {
    fn drop(&mut self) {
        // `effMainsChanged(0)` and `effClose` below are main-thread opcodes,
        // like the `load` that opened the instance. A drop cannot be handed a
        // `MainThread`, so it checks (debug builds, once a host has claimed its
        // main thread): the VST2 lane that splits the handles makes it a type.
        // Not while unwinding: a second panic in a drop aborts, and the first
        // one is the finding.
        if !std::thread::panicking() {
            debug_assert!(
                !tutti_plugin_types::MainThread::claimed_elsewhere(),
                "VST2 plugin dropped off the main thread"
            );
        }

        // Close the editor first — some plugins read audio state during
        // editor teardown (e.g., to flush meter buffers).
        if let Some(mut editor) = self.editor.take() {
            editor.0.close();
        }

        // Suspend audio so the plugin releases any RT-allocated resources
        // before it is closed. Only a plugin that is resumed: a plugin already
        // suspended (a rejoin left it so) would see a second
        // `effMainsChanged(0)` with no start before it. `effStopProcess`, which
        // the SDK puts before it, is the Processor's, or the shell's when no
        // Processor stopped it (`Shell::drop`): whoever started processing
        // knows whether it is still on.
        if self.resumed {
            self.instance.suspend();
            self.resumed = false;
        }

        // `effClose` runs when the last handle on the instance's core drops, which
        // is after this (the parameter object is `Vst2Loaded`'s next field). The
        // shared module is *not* unloaded — that leak lives in `PluginInstance`
        // itself. See the module docs for why the two must not be conflated.
    }
}
