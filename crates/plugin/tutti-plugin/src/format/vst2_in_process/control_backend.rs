//! Host-side capability backend for the in-process VST2 host.
//!
//! Implements [`HostParams`], [`HostState`], [`HostEditor`] (VST2 has an
//! embeddable editor) and [`HostRenderMode`]. Holds the same `Arc<Mutex<Vst2Instance>>` the audio unit
//! holds. GUI thread calls take the lock for the duration of one plugin operation
//! — short for parameter / state methods, potentially long for editor ones. The
//! audio thread always uses `try_lock` (in `super::audio_unit`) and falls back to
//! silence on contention so a slow `editor_idle` can't underrun audio.

use std::ffi::c_void;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_vst2_host::Vst2Instance;

use crate::error::EditorError;
use crate::host::handles::capabilities::{HostEditor, HostParams, HostRenderMode, HostState};
use crate::host::node::ParameterChangeSink;
use crate::protocol::{Normalized, ParamAddress, ParameterInfo, RenderMode};
use crate::protocol::{Preset, PresetId};
use crate::util::window::EditorSize;

/// Bundles the shared Mutex with the parameter-change sink so editor
/// idle calls can drain plugin-internal automation events and notify
/// the user-installed callback.
pub(crate) struct InProcessVst2Backend {
    pub(crate) inner: Arc<Mutex<Vst2Instance>>,
    pub(crate) param_sink: ParameterChangeSink,
    /// The cell the audio unit parks a rate change in, shared with every clone
    /// of the node. Drained here because telling a VST2 plugin its rate runs
    /// the `effMainsChanged` bracket, which allocates.
    pub(crate) pending_sample_rate: Arc<AtomicU64>,
}

impl HostParams for InProcessVst2Backend {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        // Reuse the host crate's single narrow→shared map (the same one the
        // server loader's `get_parameter_list` calls) rather than re-mapping
        // `types::ParameterInfo` here. One mapping, two callers.
        Some(self.inner.lock().parameter_list())
    }

    fn parameter_value(&self, id: ParamAddress) -> Option<f32> {
        // Forwarded, not re-wrapped: `Vst2Instance::parameter` already returns
        // `None` for a plugin exposing no `getParameter`, which is exactly this
        // trait's "unavailable". Wrapping it in `Some` would report a missing
        // accessor as a present value. An opaque id addresses nothing in VST2
        // and takes the same `None`.
        self.inner.lock().parameter(id.index()?)
    }

    fn set_parameter_value(&self, id: ParamAddress, value: Normalized) {
        // The audio thread can take this path (PluginHandle is shared);
        // use `try_lock` so we never block audio. Lost writes are
        // recoverable — the GUI thread will retry on the next idle.
        //
        // In-process, so this write reaches the plugin directly with no
        // subprocess boundary to re-clamp at. VST2 is normalized natively, so
        // the value passes through as-is — but it is a `Normalized` rather
        // than a bare float precisely because nothing downstream would catch
        // one that was not.
        let Some(index) = id.index() else { return };
        if let Some(instance) = self.inner.try_lock() {
            instance.set_parameter(index, value.get() as f32);
        }
    }

    /// The plugin's display string, and only for the value it currently holds.
    ///
    /// `effGetParamDisplay` passes the plugin an index and nothing else, so it
    /// formats its own current value — VST 2.4 has no call that formats an
    /// arbitrary one. Answering with the current value's text regardless of
    /// what was asked would give two different values the same label; setting
    /// the parameter in order to read it would make a display query audible.
    /// So a mismatch is `None`, which the caller renders as the raw number.
    ///
    /// `try_lock` for the same reason as
    /// [`set_parameter_value`](Self::set_parameter_value): `PluginHandle` is
    /// shared and the audio thread can reach this. A lost text lookup shows a
    /// number for one frame.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let index = id.index()?;
        let instance = self.inner.try_lock()?;
        let current = instance.parameter(index)?;
        (f64::from(current) == value.get())
            .then(|| instance.parameter_display(index))
            .flatten()
    }

    /// Parse through the plugin's own `effString2Parameter`.
    ///
    /// **This writes**, since that opcode parses by applying — see the trait
    /// method. It therefore takes the same `try_lock` the other write on this
    /// backend does.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let index = id.index()?;
        let instance = self.inner.try_lock()?;
        let value = instance.set_parameter_from_string(index, text)?;
        Some(Normalized::new(f64::from(value)))
    }

    fn is_crashed(&self) -> bool {
        // In-process: if the plugin crashed it took the host with it,
        // so a returning caller can never observe a crashed state.
        false
    }
}

impl HostState for InProcessVst2Backend {
    fn save_state(&self) -> Option<Vec<u8>> {
        self.inner.lock().save_state().ok()
    }

    fn load_state(&self, data: &[u8]) {
        let _ = self.inner.lock().load_state(data);
    }
}

impl crate::backend::HostPresets for InProcessVst2Backend {
    /// VST2 programs, as `Preset`s.
    ///
    /// The index **is** the identifier here — `effProgramChange` takes a
    /// position in `[0, numPrograms)` — so unlike AU's sparse selectors these
    /// really are dense. `bank` is `None`: VST2 exposes one flat set, and
    /// inventing a bank name to fill the field would be a claim the format
    /// never made.
    fn presets(&self) -> Vec<Preset> {
        self.inner
            .lock()
            .programs()
            .into_iter()
            .map(|(index, name)| Preset::new(PresetId::Number(index), name))
            .collect()
    }

    /// Switch program, bracketed by `effBeginSetProgram` / `effEndSetProgram`.
    ///
    /// `false` for an id this format cannot address — a `Program` or `Location`
    /// belongs to another format and names no VST2 program, so it is refused
    /// rather than coerced into an index.
    fn load_preset(&self, id: &PresetId) -> bool {
        match id.number() {
            Some(index) => self.inner.lock().set_program(index),
            None => false,
        }
    }

    fn current_preset(&self) -> Option<PresetId> {
        Some(PresetId::Number(self.inner.lock().current_program()))
    }
}

impl HostRenderMode for InProcessVst2Backend {
    /// Always `true`: VST2 carries the mode on `audioMasterGetCurrentProcessLevel`,
    /// a callback the *host* answers whenever the plugin asks, so there is no
    /// query a plugin could decline. This matches `probed::VST2`, which lists
    /// `RENDER_MODE` unconditionally for the same reason.
    ///
    /// Takes the lock rather than caching the flag here: the answer lives on the
    /// `HostState` the plugin already polls, and a second copy could disagree
    /// with it. `super::audio_unit::InProcessVst2Client::set_render_mode` writes
    /// the same cell through the same `Arc`, so the two routes cannot drift.
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.inner.lock().set_offline_render(mode.is_offline());
        true
    }
}

impl HostEditor for InProcessVst2Backend {
    fn open_editor(&self, parent_ptr: *mut c_void) -> std::result::Result<EditorSize, EditorError> {
        // SAFETY: caller supplied a valid native window handle (NSView*,
        // HWND, X11 window id). vst2-host only forwards it to the
        // plugin's effEditOpen.
        let parent = unsafe { tutti_vst2_host::WindowHandle::from_ptr(parent_ptr) };
        let mut instance = self.inner.lock();
        instance
            .open_editor(parent)
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    fn close_editor(&self) {
        self.inner.lock().close_editor();
    }

    fn editor_idle(&self) {
        // Before the lock below, not inside it: the drain takes the same lock
        // and would deadlock under that guard. This runs on the main thread,
        // every editor frame, which is what makes it the delivery point for the
        // rate the audio thread had to park.
        super::audio_unit::drain_sample_rate(&self.pending_sample_rate, &self.inner);

        let mut instance = self.inner.lock();
        instance.editor_idle();
        // Drain any plugin-internal parameter changes (knob movement on
        // the editor surface) and forward them to the user sink. Fires
        // on the GUI thread — same threading contract as the parameter
        // sink callback for the subprocess backend (which fires on the
        // bridge thread).
        for (index, value) in instance.drain_param_changes() {
            self.param_sink.fire(index as u32, value);
        }
    }
}
