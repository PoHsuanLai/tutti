//! Host-side capability backend for the in-process VST2 host.
//!
//! Implements [`HostParams`], [`HostState`], [`HostEditor`] (VST2 has an
//! embeddable editor) and [`HostRenderMode`]. Holds the same `Arc<Mutex<Instance>>` the audio unit
//! holds. GUI thread calls take the lock for the duration of one plugin operation
//! — short for parameter / state methods, potentially long for editor ones. The
//! audio thread always uses `try_lock` (in `super::client`) and falls back to
//! silence on contention so a slow `editor_idle` can't underrun audio.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tutti_plugin_types::MainThread;

use parking_lot::Mutex;
use tutti_plugin_types::Control;

use super::instance::Instance;
use crate::error::{EditorError, StateError};
use crate::host::handles::capabilities::{HostEditor, HostParams, HostRenderMode, HostState};
use crate::host::node::ParameterChangeSink;
use crate::protocol::{Normalized, ParamId, ParameterInfo, RenderMode};
use crate::protocol::{Preset, PresetId};
use crate::util::window::{ContentScale, EditorSize};

/// Bundles the shared Mutex with the parameter-change sink so editor
/// idle calls can drain plugin-internal automation events and notify
/// the user-installed callback.
pub(crate) struct InProcessVst2Backend {
    pub(super) inner: Arc<Mutex<Instance>>,
    pub(crate) param_sink: ParameterChangeSink,
    /// The cell the audio unit parks a rate change in, shared with the
    /// node. Drained here because telling a VST2 plugin its rate runs
    /// the `effMainsChanged` bracket, which allocates.
    pub(crate) pending_sample_rate: Arc<AtomicU64>,
    /// Where a re-read that found the plugin's latency, tail or pins moved is
    /// reported (`PluginHandle::on_invalidate`).
    pub(crate) invalidate_sink: crate::host::node::InvalidateSink,
}

impl HostParams for InProcessVst2Backend {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        // Reuse the host crate's single narrow→shared map (the same one the
        // server loader's `Control::parameters` calls) rather than re-mapping
        // `types::ParameterInfo` here. One mapping, two callers.
        Some(self.inner.lock().parameter_list())
    }

    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        // Forwarded, not re-wrapped: `Control::parameter` already returns
        // `None` for a plugin exposing no `getParameter`, which is exactly this
        // trait's "unavailable". Wrapping it in `Some` would report a missing
        // accessor as a present value. An id the plugin does not have takes the
        // same `None`. `getParameter` is any-thread under the VST2 SDK.
        self.inner.lock().parameter_value(id)
    }

    fn set_parameter_value(&self, id: ParamId, value: Normalized) {
        // The audio thread can take this path (PluginHandle is shared);
        // use `try_lock` so we never block audio. Lost writes are
        // recoverable — the GUI thread will retry on the next idle.
        //
        // In-process, so this write reaches the plugin directly with no
        // subprocess boundary to re-clamp at. VST2 is normalized natively, so
        // the value passes through as-is — but it is a `Normalized` rather
        // than a bare float precisely because nothing downstream would catch
        // one that was not.
        if let Some(instance) = self.inner.try_lock() {
            let _ = instance.write_parameter(id, value);
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
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        let instance = self.inner.try_lock()?;
        instance.display_if_current(id, value)
    }

    /// Parse through the plugin's own `effString2Parameter`.
    ///
    /// **This writes**, since that opcode parses by applying — see the trait
    /// method. It therefore takes the same `try_lock` the other write on this
    /// backend does.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let instance = self.inner.try_lock()?;
        instance.set_parameter_from_string(id, text)
    }

    fn is_crashed(&self) -> bool {
        // In-process: if the plugin crashed it took the host with it,
        // so a returning caller can never observe a crashed state.
        false
    }

    /// The `audioMasterAutomate` reports the instance's bounded queue
    /// refused. A blocking `lock`, like the other reads here: a count that
    /// read `0` on contention would go backwards.
    fn dropped_parameter_events(&self) -> u64 {
        self.inner.lock().dropped_param_changes()
    }
}

impl HostState for InProcessVst2Backend {
    /// The plugin's own failure, forwarded rather than dropped — the same
    /// change `load_state` below already made.
    ///
    /// This was `.ok()`, which threw away a reason that was already in hand and
    /// left the caller unable to tell "the plugin has no state" from "asking it
    /// failed". The in-process path cannot hit the transport's size limit, so
    /// `Rejected` is the only failure it can produce.
    ///
    /// **Takes no [`MainThread`]**, and reaches `effGetChunk` from whichever thread
    /// calls it, behind the backend's lock, through `read_state_off_main_thread`: the one
    /// named exception to the main-thread rule that `tutti-vst2-ffi`'s `PluginInstance`
    /// puts in its types. `HostState::save_state` is shared by every backend and is called
    /// off the main thread on purpose: the health snapshot is a job on the task pool,
    /// because a subprocess save is a blocking round trip with a ten-second timeout. For
    /// this backend that is a chunk read beside the editor and the audio thread, which the
    /// SDK does not promise to be safe; a plugin that cannot take it is the convention
    /// breaking. Still deferred, with the reason: the fix is a main-thread route for the
    /// snapshot (the session asks the main thread for an in-process plugin's state, and
    /// the task pool waits for the answer), which is a change to `tutti-session`,
    /// `bevy-tutti` and the snapshot job's shape, not to the handles; a stale cached
    /// snapshot would be a behaviour change, not a route.
    fn save_state(&self) -> Result<Vec<u8>, StateError> {
        self.inner
            .lock()
            .read_state_off_main_thread()
            .map_err(|e| StateError::Rejected(e.to_string()))
    }

    /// The plugin's own refusal, forwarded rather than dropped.
    ///
    /// This is the whole change on the in-process path: the `Result` was
    /// already in hand and discarded on the previous line.
    fn load_state(&self, main: &MainThread, data: &[u8]) -> Result<(), StateError> {
        Control::load_state(self.inner.lock().controller_mut(), main, data)
            .map_err(|e| StateError::Rejected(e.to_string()))
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
    fn presets(&self, main: &MainThread) -> Vec<Preset> {
        self.inner
            .lock()
            .programs(main)
            .into_iter()
            .map(|(index, name)| Preset::new(PresetId::Number(index), name))
            .collect()
    }

    /// Switch program, bracketed by `effBeginSetProgram` / `effEndSetProgram`.
    ///
    /// `false` for an id this format cannot address — a `Program` or `Location`
    /// belongs to another format and names no VST2 program, so it is refused
    /// rather than coerced into an index.
    fn load_preset(&self, main: &MainThread, id: &PresetId) -> bool {
        match id.number() {
            Some(index) => self.inner.lock().set_program(main, index),
            None => false,
        }
    }

    fn current_preset(&self, main: &MainThread) -> Option<PresetId> {
        Some(PresetId::Number(self.inner.lock().current_program(main)))
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
    /// with it. `super::client::InProcessVst2Client::set_render_mode` writes
    /// the same cell through the same `Arc`, so the two routes cannot drift.
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.inner.lock().set_offline_render(mode.is_offline());
        true
    }
}

impl HostEditor for InProcessVst2Backend {
    /// `_scale` is unused: VST2 has no call that takes a content scale.
    fn open_editor(
        &self,
        main: &MainThread,
        parent_ptr: *mut c_void,
        _scale: ContentScale,
    ) -> std::result::Result<EditorSize, EditorError> {
        // SAFETY: caller supplied a valid native window handle (NSView*,
        // HWND, X11 window id). vst2-host only forwards it to the
        // plugin's effEditOpen.
        let parent = unsafe { tutti_vst2_host::WindowHandle::from_ptr(parent_ptr) };
        let mut instance = self.inner.lock();
        instance
            .open_editor(main, parent)
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    fn close_editor(&self, main: &MainThread) {
        self.inner.lock().close_editor(main);
    }

    fn editor_idle(&self, main: &MainThread) {
        // Before the lock below, not inside it: the drain takes the same lock
        // and would deadlock under that guard. This runs on the main thread,
        // every editor frame, which is what makes it the delivery point for the
        // rate the audio thread had to park.
        super::client::drain_sample_rate(main, &self.pending_sample_rate, &self.inner);

        let (changes, param_changes) = {
            let mut instance = self.inner.lock();
            // Before the lost check: a refusal that lost the plugin is the one to report.
            if let Some(report) = instance.take_rate_report() {
                use crate::handles::PluginInvalidation;
                self.invalidate_sink.fire(match report {
                    Some((rate, reason)) => PluginInvalidation::RateRefused { rate, reason },
                    None => PluginInvalidation::RateRestored,
                });
            }
            if instance.is_lost() {
                return;
            }
            instance.editor_idle(main);
            // What the plugin changed about itself since the last pump: the
            // host's re-reads after the rate drained above, and an
            // `audioMasterIOChanged` the plugin raised. Re-read here, on the
            // main thread, where `effGetTailSize` belongs.
            (instance.poll_changes(main), instance.drain_param_changes())
        };
        // Drain any plugin-internal parameter changes (knob movement on
        // the editor surface) and forward them to the user sink. Fires
        // on the GUI thread — same threading contract as the parameter
        // sink callback for the subprocess backend (which fires on the
        // bridge thread). After the lock is released: a callback may call back.
        for (index, value) in param_changes {
            if let Ok(index) = u32::try_from(index) {
                self.param_sink
                    .fire(ParamId::new(index), Normalized::clamped(f64::from(value)));
            }
        }
        use crate::handles::PluginInvalidation;
        if let Some(samples) = changes.latency {
            self.invalidate_sink
                .fire(PluginInvalidation::Latency { samples });
        }
        if let Some(tail) = changes.tail {
            self.invalidate_sink.fire(PluginInvalidation::Tail { tail });
        }
        if changes.io {
            self.invalidate_sink.fire(PluginInvalidation::Io);
        }
    }
}
