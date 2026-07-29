//! Polling host-state flags, opening/closing the plugin editor, context
//! menus, triggers, remote controls, POSIX FDs, timers, and other
//! main-thread host-interaction methods.

use super::ClapLoaded;
#[cfg(feature = "clap-extras")]
use crate::cstr_to_string;
use crate::error::{ClapError, Result};
use crate::host::{HostState, LogRecord};
#[cfg(feature = "clap-extras")]
use crate::types::{
    ContextMenuItem, ContextMenuTarget, RemoteControlsPage, TrackInfo, TransportRequest,
    TriggerInfo,
};
use crate::types::{EditorCapabilities, EditorSize, ParamRescan, WindowHandle};
#[cfg(feature = "clap-extras")]
use clap_sys::ext::context_menu::{
    clap_context_menu_builder, clap_context_menu_check_entry, clap_context_menu_entry,
    clap_context_menu_item_title, clap_context_menu_submenu, clap_context_menu_target,
    CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU, CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_END_SUBMENU, CLAP_CONTEXT_MENU_ITEM_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_SEPARATOR, CLAP_CONTEXT_MENU_ITEM_TITLE,
    CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL, CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
};
#[cfg(feature = "clap-extras")]
use clap_sys::ext::draft::triggers::clap_trigger_info;
use clap_sys::ext::gui::{clap_window, clap_window_handle};
#[cfg(feature = "clap-extras")]
use clap_sys::ext::remote_controls::clap_remote_controls_page;
use std::ffi::c_void;
use std::sync::Arc;

#[cfg(target_os = "macos")]
fn platform_window_handle(parent: *mut c_void) -> (*const i8, clap_window_handle) {
    use clap_sys::ext::gui::CLAP_WINDOW_API_COCOA;
    (
        CLAP_WINDOW_API_COCOA.as_ptr(),
        clap_window_handle { cocoa: parent },
    )
}

#[cfg(target_os = "windows")]
fn platform_window_handle(parent: *mut c_void) -> (*const i8, clap_window_handle) {
    use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
    (
        CLAP_WINDOW_API_WIN32.as_ptr(),
        clap_window_handle { win32: parent },
    )
}

#[cfg(target_os = "linux")]
fn platform_window_handle(parent: *mut c_void) -> (*const i8, clap_window_handle) {
    use clap_sys::ext::gui::CLAP_WINDOW_API_X11;
    (
        CLAP_WINDOW_API_X11.as_ptr(),
        clap_window_handle { x11: parent as u64 },
    )
}

/// Result of running the CLAP editor-embed sequence.
struct EmbedOutcome {
    /// The editor's initial size (from `get_size`, or the 800×600 fallback).
    size: EditorSize,
    /// Whether the plugin's `create` fn ran (so the caller latches
    /// `gui_created`). Floating-only plugins with no `create` fn leave this
    /// `false`.
    did_create: bool,
}

/// Run the CLAP GUI embed sequence against a raw `gui` vtable and `plugin`
/// pointer, in the spec-mandated order:
///
/// `is_api_supported` → `create` → `set_scale` (HiDPI) → `get_size` →
/// `set_parent` → `show`.
///
/// The order matters: `is_api_supported` must gate `create` (so a
/// floating-only plugin is detected before we try to embed), `set_scale` must
/// land before `get_size` (so the reported size already accounts for the DPI
/// factor), and `set_parent` must follow `get_size` but precede `show`. The
/// previous implementation created, set the parent, *then* asked for size,
/// which reported the pre-embed size and skipped both `is_api_supported` and
/// `set_scale`.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin` pointer the `gui` vtable's fns
/// accept, and `window`'s `specific` handle must reference a live native
/// window that outlives the editor.
fn embed_editor_sequence(
    gui: &clap_sys::ext::gui::clap_plugin_gui,
    plugin: *const clap_sys::plugin::clap_plugin,
    api: *const i8,
    window_handle: clap_window_handle,
    scale: f64,
) -> Result<EmbedOutcome> {
    // 1. is_api_supported — confirm the plugin accepts the embedded window API
    //    before we create. If it does not, degrade gracefully: a plugin that
    //    only supports floating windows has no embedded `create` path, which
    //    the existing create-fails handling already covers.
    if let Some(is_api_supported_fn) = gui.is_api_supported {
        if !unsafe { is_api_supported_fn(plugin, api, false) } {
            return Err(ClapError::GuiError(
                "GUI embedded window API not supported".to_string(),
            ));
        }
    }

    // 2. create (embedded, is_floating = false).
    let did_create = if let Some(create_fn) = gui.create {
        if !unsafe { create_fn(plugin, api, false) } {
            return Err(ClapError::GuiError("GUI create failed".to_string()));
        }
        true
    } else {
        false
    };

    // 3. set_scale (HiDPI) — before get_size so the size reflects the factor.
    if let Some(set_scale_fn) = gui.set_scale {
        // A false return means the plugin does not honour host-set scale; that
        // is not an error (it will use its own).
        unsafe { set_scale_fn(plugin, scale) };
    }

    // 4. get_size — now that scale is applied.
    let size = if let Some(get_size_fn) = gui.get_size {
        let mut w: u32 = 0;
        let mut h: u32 = 0;
        if unsafe { get_size_fn(plugin, &mut w, &mut h) } {
            EditorSize {
                width: w,
                height: h,
            }
        } else {
            EditorSize {
                width: 800,
                height: 600,
            }
        }
    } else {
        EditorSize {
            width: 800,
            height: 600,
        }
    };

    // 5. set_parent — embed into the host window.
    if let Some(set_parent_fn) = gui.set_parent {
        let window = clap_window {
            api,
            specific: window_handle,
        };
        if !unsafe { set_parent_fn(plugin, &window) } {
            return Err(ClapError::GuiError("Set parent failed".to_string()));
        }
    }

    // 6. show.
    if let Some(show_fn) = gui.show {
        unsafe { show_fn(plugin) };
    }

    Ok(EmbedOutcome { size, did_create })
}

/// Read `can_resize` + `get_resize_hints` off a created editor.
///
/// Split out of [`ClapActive::editor_capabilities`] for the same reason as
/// [`embed_editor_sequence`]: the CLAP call order is the contract, and a free
/// function over a vtable can be tested against a logging stub. The caller owns
/// the *precondition* — `create()` must already have run — because that lives in
/// `LifecycleFlags`, not in the vtable.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin` pointer the `gui` vtable's fns accept,
/// and `gui.create` must have already returned `true` for it.
unsafe fn query_editor_capabilities(
    gui: &clap_sys::ext::gui::clap_plugin_gui,
    plugin: *const clap_sys::plugin::clap_plugin,
) -> EditorCapabilities {
    let resizable = gui.can_resize.map(|f| f(plugin)).unwrap_or(false);
    let mut caps = EditorCapabilities {
        resize: tutti_plugin_types::ResizeHints {
            resizable,
            can_resize_horizontally: resizable,
            can_resize_vertically: resizable,
        },
        aspect: tutti_plugin_types::AspectRatio::default(),
        appkit_autoresize_friendly: false,
    };
    if let Some(get_hints) = gui.get_resize_hints {
        let mut hints = clap_sys::ext::gui::clap_gui_resize_hints {
            can_resize_horizontally: false,
            can_resize_vertically: false,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
        if get_hints(plugin, &mut hints) {
            caps.resize.can_resize_horizontally = hints.can_resize_horizontally;
            caps.resize.can_resize_vertically = hints.can_resize_vertically;
            caps.aspect.preserve = hints.preserve_aspect_ratio;
            if hints.preserve_aspect_ratio
                && hints.aspect_ratio_width > 0
                && hints.aspect_ratio_height > 0
            {
                caps.aspect.ratio = Some((hints.aspect_ratio_width, hints.aspect_ratio_height));
            }
        }
    }
    caps
}

impl ClapLoaded {
    /// Whether [`open_editor`](Self::open_editor) can actually embed an editor.
    ///
    /// Not "is there a `clap.gui` vtable?" — that is a different, weaker
    /// question, and answering it here is a bug this project has now shipped
    /// twice. In VST3 the same method asked "is there a controller?" instead of
    /// calling `createView(kEditor)`; it fed `Features::EDITOR` in the plugin
    /// descriptor (`tutti-plugin-server/src/loaders/clap.rs:166` does the same
    /// with this one), so the DAW offered an "open editor" button for plugins
    /// that had no editor to open. The button did nothing, and the failure
    /// surfaced as a user-visible dead control rather than an error.
    ///
    /// Two shapes make the pointer check wrong, and both are legal CLAP:
    ///
    /// - **Floating-only.** `is_api_supported(api, is_floating = false)`
    ///   returns false. The vtable is non-null and `create` exists, but there
    ///   is no *embedded* editor, so [`embed_editor_sequence`] rejects it at
    ///   the first gate.
    /// - **No `create`.** Every `clap_plugin_gui` member is an `Option<fn>`.
    ///   The host already knows this shape exists — `EmbedOutcome::did_create`
    ///   is there precisely because `create` may be absent — but the knowledge
    ///   lived only in the embed path.
    ///
    /// ## Why this does not create and destroy a GUI to find out
    ///
    /// VST3 had no cheaper option: `createView` *is* the query, so the fix
    /// there had to create a view and release it. CLAP is better designed.
    /// `is_api_supported` is documented as "Returns true if the requested gui
    /// api is supported, either in floating or non-floating mode" — a pure
    /// predicate. `create`, by contrast, "allocates all resources necessary for
    /// the gui". Answering a capability question by allocating and freeing a
    /// plugin's entire GUI would be a real side effect on every scan: plugins
    /// spin up OpenGL contexts, worker threads, and font caches in `create`,
    /// and a `destroy` that follows immediately is a path few plugins exercise
    /// and some get wrong. So the query stops at the two facts it can read for
    /// free — the vtable's shape and the plugin's own answer.
    ///
    /// The residual gap is deliberate: a plugin whose `is_api_supported` says
    /// yes and whose `create` then fails still reports `true` here. That is
    /// unknowable without creating, and the honest place to discover it is
    /// [`open_editor`](Self::open_editor)'s error.
    ///
    /// A plugin that omits `is_api_supported` entirely is taken at its word and
    /// treated as embeddable — the same reading [`embed_editor_sequence`] uses,
    /// where an absent query is not a refusal.
    pub fn has_editor(&self) -> bool {
        if self.extensions.gui.gui.is_null() {
            return false;
        }
        // SAFETY: non-null checked above; the cache holds the pointer the
        // plugin returned from `get_extension`, valid for the plugin's life.
        let gui = unsafe { &*self.extensions.gui.gui };
        if gui.create.is_none() {
            return false;
        }
        // Ask about the *embedded* window API for this platform — the same
        // constant `open_editor` will pass, resolved by the same
        // `platform_window_handle` helper, so the two can never disagree about
        // which API is being asked about.
        let (api, _) = platform_window_handle(std::ptr::null_mut());
        match gui.is_api_supported {
            // SAFETY: `plugin` is live for `&self`, and `api` is a 'static C
            // string from the clap-sys constants.
            Some(is_api_supported) => unsafe { is_api_supported(self.plugin.as_ptr(), api, false) },
            None => true,
        }
    }

    /// Create the plugin editor and embed it into the given native `parent`
    /// window, returning the editor's initial size.
    ///
    /// Follows the CLAP embed sequence: `is_api_supported` → `create` →
    /// `set_scale` → `get_size` → `set_parent` → `show`. See
    /// [`embed_editor_sequence`] for the ordering rationale.
    ///
    /// # Errors
    /// [`ClapError::GuiError`] if the plugin does not expose a GUI, if the
    /// embedded window API is unsupported, or if `create`/`set_parent` fails.
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        self.assert_main_thread();
        if self.extensions.gui.gui.is_null() {
            return Err(ClapError::GuiError("No GUI extension".to_string()));
        }
        let gui = unsafe { &*self.extensions.gui.gui };

        let (api, window_handle) = platform_window_handle(parent.as_ptr());

        // TODO: real backing-scale from frontend — the host `WindowHandle`
        // carries no DPI today, so we pass 1.0 and wire the `set_scale` call.
        let scale = 1.0_f64;

        let outcome = embed_editor_sequence(gui, self.plugin.as_ptr(), api, window_handle, scale)?;

        if outcome.did_create {
            self.flags.gui_created = true;
            // H5: a fresh editor exists now — clear any stale "already
            // destroyed" latch from the previous editor's teardown.
            self.host_state
                .gui
                .already_destroyed
                .store(false, std::sync::atomic::Ordering::Release);
        }

        Ok(outcome.size)
    }

    /// Query the plugin's resize/aspect capabilities.
    ///
    /// Requires a created editor, not merely a `gui` extension. The CLAP
    /// spec orders every other `clap_plugin_gui` call after `create()`, and plugins
    /// enforce it — TAL-Reverb-4's validation layer prints
    ///
    /// ```text
    /// [clap-plugin HOST-MISBEHAVING] clap_plugin_gui.can_resize() was called
    /// without a prior call to clap_plugin_gui.create()
    /// ```
    ///
    /// on every call. This guarded only on the extension pointer, so it violated
    /// that on every pre-create query; the diagnostic went to a log nobody read.
    /// `destroy_editor` already gates on `gui_created` — this was the outlier.
    ///
    /// Defaults are the honest answer before create: nothing has been asked, so
    /// nothing is claimed. A caller wanting real hints must create the editor first,
    /// which is the same order the spec requires of it.
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        if self.extensions.gui.gui.is_null() || !self.flags.gui_created {
            return EditorCapabilities::default();
        }
        // SAFETY: non-null checked above, and `gui_created` means `create()` has
        // run — the precondition every other `clap_plugin_gui` call has.
        let gui = unsafe { &*self.extensions.gui.gui };
        unsafe { query_editor_capabilities(gui, self.plugin.as_ptr()) }
    }

    /// Returns the snapped size the plugin applied.
    ///
    /// `adjust_size` is consulted first and its **failure is fatal**, not
    /// ignored. CLAP defines it as "the plugin will calculate the closest
    /// usable size which fits in the given size … Returns true if the plugin
    /// could adjust the given size" — so a `false` return says the plugin could
    /// not produce a usable size for this request, and leaves the out-params
    /// holding whatever they held before (the spec does not promise the plugin
    /// wrote them, and a fixed-size editor has nothing to write).
    ///
    /// This previously read `false` as "no snap to apply" and forwarded the
    /// *unadjusted* request to `set_size`. That inverts the meaning: the one
    /// case where the plugin has explicitly said "I cannot give you a working
    /// size" was the case where the host pushed the raw size through anyway.
    /// A plugin that then accepted it out of politeness would render at a size
    /// it had just declined; one that refused produced a `set_size refused`
    /// error naming the wrong call.
    pub fn resize_editor(&mut self, requested: EditorSize) -> Result<EditorSize> {
        if self.extensions.gui.gui.is_null() {
            return Err(ClapError::GuiError("No GUI extension".to_string()));
        }
        let gui = unsafe { &*self.extensions.gui.gui };
        let mut w = requested.width;
        let mut h = requested.height;
        if let Some(adjust) = gui.adjust_size {
            if !unsafe { adjust(self.plugin.as_ptr(), &mut w, &mut h) } {
                return Err(ClapError::GuiError(
                    "adjust_size: no usable size fits the request".to_string(),
                ));
            }
        }
        let set_size = gui
            .set_size
            .ok_or_else(|| ClapError::GuiError("set_size unsupported".to_string()))?;
        if !unsafe { set_size(self.plugin.as_ptr(), w, h) } {
            return Err(ClapError::GuiError("set_size refused".to_string()));
        }
        Ok(EditorSize {
            width: w,
            height: h,
        })
    }

    pub fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        if !self
            .host_state
            .gui
            .request_resize_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return None;
        }
        Some(EditorSize {
            width: self
                .host_state
                .gui
                .request_resize_width
                .load(std::sync::atomic::Ordering::Acquire),
            height: self
                .host_state
                .gui
                .request_resize_height
                .load(std::sync::atomic::Ordering::Acquire),
        })
    }

    /// Hide and destroy the plugin editor, if one was opened. Idempotent.
    pub fn close_editor(&mut self) {
        self.assert_main_thread();
        if !self.flags.gui_created {
            return;
        }
        // H5: if the plugin already destroyed its own editor (it reported
        // `gui.closed(was_destroyed = true)`), skip hide/destroy entirely —
        // calling `gui.destroy` again would be a double-destroy. Just clear our
        // bookkeeping and consume the latch.
        if self
            .host_state
            .gui
            .already_destroyed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.flags.gui_created = false;
            return;
        }
        // SAFETY: `gui_created` implies the pointer is non-null. The only
        // writer that sets it true is `open_editor`, after its own
        // `gui.is_null()` guard and a successful embed; `ExtensionCache` is
        // built once in `load.rs` and the `gui` field is never reassigned, so
        // the pointer cannot go null underneath a live latch. This runs from
        // `Drop` (lifecycle.rs), where an unwind is not available anyway.
        let gui = unsafe { &*self.extensions.gui.gui };
        if let Some(hide_fn) = gui.hide {
            unsafe { hide_fn(self.plugin.as_ptr()) };
        }
        if let Some(destroy_fn) = gui.destroy {
            unsafe { destroy_fn(self.plugin.as_ptr()) };
        }
        self.flags.gui_created = false;
    }

    /// Direct access to the shared [`HostState`] — useful if you need to
    /// read a flag without consuming it or observe a field not wrapped by
    /// the `poll_*` helpers.
    pub fn host_state(&self) -> &Arc<HostState> {
        &self.host_state
    }

    /// Consume and return the `restart_requested` flag.
    pub fn poll_restart_requested(&self) -> bool {
        self.host_state
            .poll(&self.host_state.lifecycle.restart_requested)
    }

    /// Consume and return the `process_requested` flag (the plugin wants
    /// `process()` to be called even if the host would otherwise skip it).
    pub fn poll_process_requested(&self) -> bool {
        self.host_state
            .poll(&self.host_state.lifecycle.process_requested)
    }

    /// Consume and return the `callback_requested` flag — call
    /// [`Self::on_main_thread`] when this fires.
    pub fn poll_callback_requested(&self) -> bool {
        self.host_state
            .poll(&self.host_state.lifecycle.callback_requested)
    }

    /// Consume and return the `latency_changed` flag; fetch the new value
    /// with [`Self::get_latency`].
    pub fn poll_latency_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.processing.latency_changed)
    }

    /// Consume and return the `tail_changed` flag; fetch the new value with
    /// [`Self::get_tail`].
    pub fn poll_tail_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.processing.tail_changed)
    }

    /// Consume and return the pending parameter-rescan request as a decoded
    /// [`ParamRescan`], clearing both the request flag and the accumulated
    /// flags. `ParamRescan::requested` is `false` when nothing is pending.
    ///
    /// The decoded scope lets the caller honour CLAP's rule that a full rescan
    /// (`ParamRescan::all` / [`ParamRescan::needs_deactivate`]) is applied only
    /// while the plugin is deactivated, while a value-only rescan is safe live.
    pub fn poll_params_rescan(&self) -> ParamRescan {
        let requested = self
            .host_state
            .poll(&self.host_state.params.rescan_requested);
        let flags = self
            .host_state
            .params
            .rescan_flags
            .swap(0, std::sync::atomic::Ordering::AcqRel);
        ParamRescan::from_flags(requested, flags)
    }

    /// Consume and return the `params_flush_requested` flag — call
    /// [`Self::flush_params`] or run a process block when this fires.
    pub fn poll_params_flush_requested(&self) -> bool {
        self.host_state
            .poll(&self.host_state.params.flush_requested)
    }

    /// Consume and return the `state_dirty` flag — the plugin's state has
    /// diverged from the last save.
    pub fn poll_state_dirty(&self) -> bool {
        self.host_state
            .poll(&self.host_state.processing.state_dirty)
    }

    /// Consume and return the `audio_ports.changed` flag.
    pub fn poll_audio_ports_changed(&self) -> bool {
        self.host_state.poll(&self.host_state.audio_ports.changed)
    }

    /// Consume and return the `notes.ports_changed` flag.
    pub fn poll_note_ports_changed(&self) -> bool {
        self.host_state.poll(&self.host_state.notes.ports_changed)
    }

    /// Consume and return the `gui.closed` flag.
    pub fn poll_gui_closed(&self) -> bool {
        self.host_state.poll(&self.host_state.gui.closed)
    }

    /// Non-consuming peek at the restart flag. Unlike
    /// [`Self::poll_restart_requested`] (which clears the flag on read),
    /// this returns the current value without resetting it.
    pub fn needs_restart(&self) -> bool {
        self.host_state
            .lifecycle
            .restart_requested
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Drain every `clap.log` line the plugin has emitted since the last drain,
    /// oldest first.
    ///
    /// The host also mirrors each line to stderr as it arrives; this is the
    /// programmatic route, for a consumer that wants them in a UI console or a
    /// structured log. Draining is deliberate — a peek would make "have I seen
    /// this line already?" the caller's problem, and the buffer is bounded
    /// ([`LOG_CAPACITY`](crate::host::LOG_CAPACITY)) precisely so a caller that
    /// never drains cannot grow it without limit.
    pub fn drain_log(&self) -> Vec<LogRecord> {
        let mut records = self
            .host_state
            .log
            .records
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        records.drain(..).collect()
    }

    /// How many log lines were dropped because the buffer was full while the
    /// consumer was not draining. Cumulative, not consumed on read: a caller
    /// tracks its own delta, and a caller that never asks is not silently told
    /// the history is complete.
    pub fn log_lines_dropped(&self) -> u32 {
        self.host_state
            .log
            .dropped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Fire any expired timers the plugin registered via
    /// `CLAP_EXT_TIMER_SUPPORT`. Call periodically from the main thread.
    /// Returns the number of timer callbacks invoked.
    pub fn poll_timers(&mut self) -> usize {
        if self.extensions.system.timer_support.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.system.timer_support };
        let on_timer = match ext.on_timer {
            Some(f) => f,
            None => return 0,
        };

        let now = std::time::Instant::now();
        let mut fired = 0usize;
        let mut expired_ids = Vec::new();

        if let Ok(mut timers) = self.host_state.timer.timers.lock() {
            for timer in timers.iter_mut() {
                let elapsed = now.duration_since(timer.last_fire);
                if elapsed.as_millis() >= timer.period_ms as u128 {
                    expired_ids.push(timer.id);
                    timer.last_fire = now;
                }
            }
        }

        for id in expired_ids {
            unsafe { on_timer(self.plugin.as_ptr(), id) };
            fired += 1;
        }

        fired
    }

    /// Consume and return the `audio_ports.config_changed` flag.
    pub fn poll_audio_ports_config_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.audio_ports.config_changed)
    }

    /// Consume and return the `remote_controls.changed` flag. Speculative —
    /// gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn poll_remote_controls_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.remote_controls.changed)
    }

    /// Consume and return the page ID the plugin most recently suggested
    /// the host switch to, or `None` if no suggestion is pending. Speculative —
    /// gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn poll_suggested_remote_page(&self) -> Option<u32> {
        let val = self
            .host_state
            .remote_controls
            .suggested_page
            .swap(u32::MAX, std::sync::atomic::Ordering::AcqRel);
        if val == u32::MAX {
            None
        } else {
            Some(val)
        }
    }

    /// Drain all pending [`TransportRequest`]s the plugin has emitted.
    /// Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn drain_transport_requests(&self) -> Vec<TransportRequest> {
        if let Ok(mut reqs) = self.host_state.transport.requests.lock() {
            std::mem::take(&mut *reqs)
        } else {
            Vec::new()
        }
    }

    /// Consume and return the `notes.names_changed` flag.
    pub fn poll_note_names_changed(&self) -> bool {
        self.host_state.poll(&self.host_state.notes.names_changed)
    }

    /// Consume and return the `notes.voice_info_changed` flag.
    pub fn poll_voice_info_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.notes.voice_info_changed)
    }

    /// Consume and return the `preset_loaded` flag. Speculative — gated behind
    /// `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn poll_preset_loaded(&self) -> bool {
        self.host_state
            .poll(&self.host_state.processing.preset_loaded)
    }

    /// Invoke the plugin's `on_main_thread` callback — call when
    /// [`Self::poll_callback_requested`] fires.
    pub fn on_main_thread(&mut self) -> &mut Self {
        let plugin_ref = unsafe { &*self.plugin.as_ptr() };
        if let Some(f) = plugin_ref.on_main_thread {
            unsafe { f(self.plugin.as_ptr()) };
        }
        self
    }

    /// Publish track metadata for the plugin to read via
    /// `CLAP_EXT_TRACK_INFO`. Call [`Self::notify_track_info_changed`]
    /// afterwards to ping the plugin. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn set_track_info(&self, info: TrackInfo) {
        if let Ok(mut guard) = self.host_state.resources.track_info.lock() {
            *guard = Some(info);
        }
    }

    /// Tell the plugin its track info has changed. Speculative — gated behind
    /// `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn notify_track_info_changed(&self) {
        if self.extensions.system.track_info.is_null() {
            return;
        }
        let ext = unsafe { &*self.extensions.system.track_info };
        if let Some(f) = ext.changed {
            unsafe { f(self.plugin.as_ptr()) };
        }
    }

    /// Number of remote-control pages the plugin exposes. Speculative — gated
    /// behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn remote_controls_page_count(&self) -> usize {
        if self.extensions.params.remote_controls.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.params.remote_controls };
        match ext.count {
            Some(f) => (unsafe { f(self.plugin.as_ptr()) }) as usize,
            None => 0,
        }
    }

    /// Describe the remote-controls page at `index`. Speculative — gated behind
    /// `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn get_remote_controls_page(&self, index: usize) -> Option<RemoteControlsPage> {
        if self.extensions.params.remote_controls.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.params.remote_controls };
        let get_fn = ext.get?;
        let mut page: clap_remote_controls_page = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index as u32, &mut page) } {
            return None;
        }
        Some(RemoteControlsPage {
            section_name: unsafe { cstr_to_string(page.section_name.as_ptr()) },
            page_id: page.page_id,
            page_name: unsafe { cstr_to_string(page.page_name.as_ptr()) },
            param_ids: page.param_ids,
            is_for_preset: page.is_for_preset,
        })
    }

    /// Ask the plugin to supply the context-menu entries for `target`.
    /// Returns `None` if the plugin does not implement context menus.
    /// Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn context_menu_populate(&self, target: ContextMenuTarget) -> Option<Vec<ContextMenuItem>> {
        if self.extensions.gui.context_menu.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.gui.context_menu };
        let populate_fn = ext.populate?;

        let clap_target = match target {
            ContextMenuTarget::Global => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL,
                id: 0,
            },
            ContextMenuTarget::Param(id) => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
                id,
            },
        };

        let mut items: Vec<ContextMenuItem> = Vec::new();
        let items_ptr = &mut items as *mut Vec<ContextMenuItem> as *mut c_void;

        let builder = clap_context_menu_builder {
            ctx: items_ptr,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };

        if unsafe { populate_fn(self.plugin.as_ptr(), &clap_target, &builder) } {
            Some(items)
        } else {
            None
        }
    }

    /// Invoke a context-menu action the plugin previously reported via
    /// [`Self::context_menu_populate`]. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn context_menu_perform(&self, target: ContextMenuTarget, action_id: u32) -> bool {
        if self.extensions.gui.context_menu.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.gui.context_menu };
        let perform_fn = match ext.perform {
            Some(f) => f,
            None => return false,
        };
        let clap_target = match target {
            ContextMenuTarget::Global => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL,
                id: 0,
            },
            ContextMenuTarget::Param(id) => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
                id,
            },
        };
        unsafe { perform_fn(self.plugin.as_ptr(), &clap_target, action_id) }
    }

    /// Number of trigger "parameters" (stateless momentary actions) the
    /// plugin exposes via the draft `CLAP_EXT_TRIGGERS`. Speculative — gated
    /// behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn trigger_count(&self) -> usize {
        if self.extensions.system.triggers.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.system.triggers };
        match ext.count {
            Some(f) => (unsafe { f(self.plugin.as_ptr()) }) as usize,
            None => 0,
        }
    }

    /// Describe the trigger at `index`. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn get_trigger_info(&self, index: usize) -> Option<TriggerInfo> {
        if self.extensions.system.triggers.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.system.triggers };
        let get_fn = ext.get_info?;
        let mut info: clap_trigger_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index as u32, &mut info) } {
            return None;
        }
        Some(TriggerInfo {
            id: info.id,
            flags: info.flags,
            name: unsafe { cstr_to_string(info.name.as_ptr()) },
            module: unsafe { cstr_to_string(info.module.as_ptr()) },
        })
    }

    /// Run a task that the plugin enqueued via `CLAP_EXT_THREAD_POOL`.
    /// Call from a worker thread. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn thread_pool_exec(&self, task_index: u32) {
        if self.extensions.system.thread_pool.is_null() {
            return;
        }
        let ext = unsafe { &*self.extensions.system.thread_pool };
        if let Some(f) = ext.exec {
            unsafe { f(self.plugin.as_ptr(), task_index) };
        }
    }

    /// Tell the plugin its tuning table set has changed. Speculative — gated
    /// behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn notify_tuning_changed(&self) {
        if self.extensions.system.tuning.is_null() {
            return;
        }
        let ext = unsafe { &*self.extensions.system.tuning };
        if let Some(f) = ext.changed {
            unsafe { f(self.plugin.as_ptr()) };
        }
    }

    /// Fire `on_fd` for every POSIX FD the plugin has registered.
    /// Returns the number of callbacks invoked. Speculative — gated behind
    /// `clap-extras`.
    #[cfg(all(unix, feature = "clap-extras"))]
    pub fn poll_posix_fds(&mut self) -> usize {
        if self.extensions.system.posix_fd_support.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.system.posix_fd_support };
        let on_fd = match ext.on_fd {
            Some(f) => f,
            None => return 0,
        };

        let fds: Vec<(i32, u32)> = if let Ok(guard) = self.host_state.resources.posix_fds.lock() {
            guard.iter().map(|e| (e.fd, e.flags)).collect()
        } else {
            return 0;
        };

        let mut fired = 0;
        for (fd, flags) in fds {
            unsafe { on_fd(self.plugin.as_ptr(), fd, flags) };
            fired += 1;
        }
        fired
    }
}

#[cfg(feature = "clap-extras")]
pub(super) unsafe extern "C" fn context_menu_builder_add_item(
    builder: *const clap_context_menu_builder,
    item_kind: u32,
    item_data: *const c_void,
) -> bool {
    if builder.is_null() || (*builder).ctx.is_null() {
        return false;
    }
    let items = &mut *((*builder).ctx as *mut Vec<ContextMenuItem>);
    let item = match item_kind {
        CLAP_CONTEXT_MENU_ITEM_ENTRY => {
            if item_data.is_null() {
                return false;
            }
            let entry = &*(item_data as *const clap_context_menu_entry);
            ContextMenuItem::Entry {
                label: cstr_to_string(entry.label),
                is_enabled: entry.is_enabled,
                action_id: entry.action_id,
            }
        }
        CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY => {
            if item_data.is_null() {
                return false;
            }
            let entry = &*(item_data as *const clap_context_menu_check_entry);
            ContextMenuItem::CheckEntry {
                label: cstr_to_string(entry.label),
                is_enabled: entry.is_enabled,
                is_checked: entry.is_checked,
                action_id: entry.action_id,
            }
        }
        CLAP_CONTEXT_MENU_ITEM_SEPARATOR => ContextMenuItem::Separator,
        CLAP_CONTEXT_MENU_ITEM_TITLE => {
            if item_data.is_null() {
                return false;
            }
            let title = &*(item_data as *const clap_context_menu_item_title);
            ContextMenuItem::Title {
                title: cstr_to_string(title.title),
                is_enabled: title.is_enabled,
            }
        }
        CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU => {
            if item_data.is_null() {
                return false;
            }
            let sub = &*(item_data as *const clap_context_menu_submenu);
            ContextMenuItem::BeginSubmenu {
                label: cstr_to_string(sub.label),
                is_enabled: sub.is_enabled,
            }
        }
        CLAP_CONTEXT_MENU_ITEM_END_SUBMENU => ContextMenuItem::EndSubmenu,
        _ => return false,
    };
    items.push(item);
    true
}

#[cfg(feature = "clap-extras")]
pub(super) unsafe extern "C" fn context_menu_builder_supports(
    _builder: *const clap_context_menu_builder,
    item_kind: u32,
) -> bool {
    matches!(
        item_kind,
        CLAP_CONTEXT_MENU_ITEM_ENTRY
            | CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY
            | CLAP_CONTEXT_MENU_ITEM_SEPARATOR
            | CLAP_CONTEXT_MENU_ITEM_TITLE
            | CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU
            | CLAP_CONTEXT_MENU_ITEM_END_SUBMENU
    )
}

#[cfg(test)]
mod embed_sequence_tests {
    use super::*;
    use clap_sys::ext::gui::clap_plugin_gui;
    use clap_sys::plugin::clap_plugin;

    // The stub gui vtable's fns log their own name into a `Vec<&str>` reached
    // through the stub plugin's `plugin_data`, so the test can assert the
    // CLAP-mandated call order.
    unsafe fn log(plugin: *const clap_plugin, name: &'static str) {
        let vec = &mut *((*plugin).plugin_data as *mut Vec<&'static str>);
        vec.push(name);
    }

    unsafe extern "C" fn stub_is_api_supported(
        plugin: *const clap_plugin,
        _api: *const i8,
        _is_floating: bool,
    ) -> bool {
        log(plugin, "is_api_supported");
        true
    }

    unsafe extern "C" fn stub_create(
        plugin: *const clap_plugin,
        _api: *const i8,
        _is_floating: bool,
    ) -> bool {
        log(plugin, "create");
        true
    }

    unsafe extern "C" fn stub_set_scale(plugin: *const clap_plugin, _scale: f64) -> bool {
        log(plugin, "set_scale");
        true
    }

    unsafe extern "C" fn stub_get_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        log(plugin, "get_size");
        *width = 640;
        *height = 480;
        true
    }

    unsafe extern "C" fn stub_set_parent(
        plugin: *const clap_plugin,
        _window: *const clap_window,
    ) -> bool {
        log(plugin, "set_parent");
        true
    }

    unsafe extern "C" fn stub_show(plugin: *const clap_plugin) -> bool {
        log(plugin, "show");
        true
    }

    unsafe extern "C" fn stub_is_api_supported_false(
        plugin: *const clap_plugin,
        _api: *const i8,
        _is_floating: bool,
    ) -> bool {
        log(plugin, "is_api_supported");
        false
    }

    unsafe extern "C" fn stub_can_resize(plugin: *const clap_plugin) -> bool {
        log(plugin, "can_resize");
        true
    }

    unsafe extern "C" fn stub_get_resize_hints(
        plugin: *const clap_plugin,
        hints: *mut clap_sys::ext::gui::clap_gui_resize_hints,
    ) -> bool {
        log(plugin, "get_resize_hints");
        if !hints.is_null() {
            (*hints).can_resize_horizontally = true;
            (*hints).can_resize_vertically = true;
        }
        true
    }

    fn stub_gui() -> clap_plugin_gui {
        // SAFETY: clap_plugin_gui is all Option<fn ptr> fields; zeroed = None.
        let mut gui: clap_plugin_gui = unsafe { std::mem::zeroed() };
        gui.is_api_supported = Some(stub_is_api_supported);
        gui.create = Some(stub_create);
        gui.set_scale = Some(stub_set_scale);
        gui.get_size = Some(stub_get_size);
        gui.set_parent = Some(stub_set_parent);
        gui.show = Some(stub_show);
        gui.can_resize = Some(stub_can_resize);
        gui.get_resize_hints = Some(stub_get_resize_hints);
        gui
    }

    fn stub_plugin(log: &mut Vec<&'static str>) -> clap_plugin {
        // SAFETY: clap_plugin is POD (pointers + Option<fn>); zeroed is a valid
        // all-null/None instance. We only ever read `plugin_data`.
        let mut plugin: clap_plugin = unsafe { std::mem::zeroed() };
        plugin.plugin_data = log as *mut Vec<&'static str> as *mut std::ffi::c_void;
        plugin
    }

    #[test]
    fn embed_sequence_calls_in_spec_order() {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let gui = stub_gui();
        let handle = clap_window_handle {
            ptr: std::ptr::null_mut(),
        };

        let outcome = embed_editor_sequence(
            &gui,
            &plugin as *const clap_plugin,
            std::ptr::null(),
            handle,
            1.0,
        )
        .expect("embed sequence succeeds");

        assert!(outcome.did_create, "create ran");
        assert_eq!(outcome.size.width, 640);
        assert_eq!(outcome.size.height, 480);
        // The exact CLAP embed order: is_api_supported → create → set_scale →
        // get_size → set_parent → show.
        assert_eq!(
            order,
            vec![
                "is_api_supported",
                "create",
                "set_scale",
                "get_size",
                "set_parent",
                "show",
            ]
        );
    }

    #[test]
    fn embed_sequence_rejects_unsupported_api() {
        // is_api_supported == false must short-circuit before create — a
        // floating-only plugin degrades gracefully instead of being embedded.
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let mut gui = stub_gui();
        gui.is_api_supported = Some(stub_is_api_supported_false);
        let handle = clap_window_handle {
            ptr: std::ptr::null_mut(),
        };

        let result = embed_editor_sequence(
            &gui,
            &plugin as *const clap_plugin,
            std::ptr::null(),
            handle,
            1.0,
        );

        assert!(result.is_err(), "unsupported api errors");
        assert_eq!(order, vec!["is_api_supported"], "stops before create");
    }

    /// Querying capabilities on a created editor calls the vtable; the
    /// `gui_created` gate in `editor_capabilities` is what keeps it from happening
    /// before that.
    ///
    /// The bug was a missing precondition, not a wrong call sequence: the vtable
    /// calls below are correct *once `create()` has run*. CLAP orders every
    /// `clap_plugin_gui` method after `create`, and plugins check — TAL-Reverb-4
    /// printed `[clap-plugin HOST-MISBEHAVING] clap_plugin_gui.can_resize() was
    /// called without a prior call to clap_plugin_gui.create()` on every query,
    /// because the guard tested only the extension pointer.
    #[test]
    fn capability_query_reads_both_gui_fns() {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let gui = stub_gui();

        let caps = unsafe { query_editor_capabilities(&gui, &plugin as *const clap_plugin) };

        assert_eq!(
            order,
            vec!["can_resize", "get_resize_hints"],
            "hints must refine can_resize, not replace it"
        );
        assert!(caps.resize.resizable);
        assert!(caps.resize.can_resize_horizontally);
        assert!(caps.resize.can_resize_vertically);
    }

    /// A plugin advertising `gui` but exposing neither resize fn is reported as
    /// non-resizable rather than defaulting to resizable — the safe direction, since
    /// a host that resizes a fixed-size editor corrupts its layout.
    #[test]
    fn capability_query_defaults_to_not_resizable() {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        // SAFETY: all-Option fields; zeroed = every fn absent.
        let gui: clap_plugin_gui = unsafe { std::mem::zeroed() };

        let caps = unsafe { query_editor_capabilities(&gui, &plugin as *const clap_plugin) };

        assert!(order.is_empty(), "nothing to call");
        assert!(!caps.resize.resizable);
        assert!(!caps.aspect.preserve);
        assert!(caps.aspect.ratio.is_none());
    }
}
