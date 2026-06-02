//! Polling host-state flags, opening/closing the plugin editor, context
//! menus, triggers, remote controls, POSIX FDs, timers, and other
//! main-thread host-interaction methods.

use super::ClapInstance;
use crate::cstr_to_string;
use crate::error::{ClapError, Result};
use crate::host::HostState;
use crate::types::{
    ContextMenuItem, ContextMenuTarget, EditorCapabilities, EditorSize, RemoteControlsPage,
    TrackInfo, TransportRequest, TriggerInfo, WindowHandle,
};
use clap_sys::ext::context_menu::{
    clap_context_menu_builder, clap_context_menu_check_entry, clap_context_menu_entry,
    clap_context_menu_item_title, clap_context_menu_submenu, clap_context_menu_target,
    CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU, CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_END_SUBMENU, CLAP_CONTEXT_MENU_ITEM_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_SEPARATOR, CLAP_CONTEXT_MENU_ITEM_TITLE,
    CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL, CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
};
use clap_sys::ext::draft::triggers::clap_trigger_info;
use clap_sys::ext::gui::{clap_window, clap_window_handle};
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

impl ClapInstance {
    /// Whether the plugin implements `CLAP_EXT_GUI` and can open an editor.
    pub fn has_editor(&self) -> bool {
        !self.extensions.gui.gui.is_null()
    }

    /// Create the plugin editor and embed it into the given native `parent`
    /// window, returning the editor's initial size.
    ///
    /// # Errors
    /// [`ClapError::GuiError`] if the plugin does not expose a GUI, or if
    /// `create`/`set_parent` fails.
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        self.assert_main_thread();
        if self.extensions.gui.gui.is_null() {
            return Err(ClapError::GuiError("No GUI extension".to_string()));
        }
        let gui = unsafe { &*self.extensions.gui.gui };

        let (api, window_handle) = platform_window_handle(parent.as_ptr());

        if let Some(create_fn) = gui.create {
            if !unsafe { create_fn(self.plugin.as_ptr(), api, false) } {
                return Err(ClapError::GuiError("GUI create failed".to_string()));
            }
            self.flags.gui_created = true;
        }

        if let Some(set_parent_fn) = gui.set_parent {
            let window = clap_window {
                api,
                specific: window_handle,
            };
            if !unsafe { set_parent_fn(self.plugin.as_ptr(), &window) } {
                return Err(ClapError::GuiError("Set parent failed".to_string()));
            }
        }

        let size = if let Some(get_size_fn) = gui.get_size {
            let mut w: u32 = 0;
            let mut h: u32 = 0;
            if unsafe { get_size_fn(self.plugin.as_ptr(), &mut w, &mut h) } {
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

        if let Some(show_fn) = gui.show {
            unsafe { show_fn(self.plugin.as_ptr()) };
        }

        Ok(size)
    }

    pub fn editor_capabilities(&self) -> EditorCapabilities {
        if self.extensions.gui.gui.is_null() {
            return EditorCapabilities::default();
        }
        let gui = unsafe { &*self.extensions.gui.gui };
        let resizable = gui
            .can_resize
            .map(|f| unsafe { f(self.plugin.as_ptr()) })
            .unwrap_or(false);
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
            if unsafe { get_hints(self.plugin.as_ptr(), &mut hints) } {
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

    /// Returns the snapped size the plugin applied.
    pub fn resize_editor(&mut self, requested: EditorSize) -> Result<EditorSize> {
        if self.extensions.gui.gui.is_null() {
            return Err(ClapError::GuiError("No GUI extension".to_string()));
        }
        let gui = unsafe { &*self.extensions.gui.gui };
        let mut w = requested.width;
        let mut h = requested.height;
        if let Some(adjust) = gui.adjust_size {
            // false return just means "no snap to apply".
            unsafe { adjust(self.plugin.as_ptr(), &mut w, &mut h) };
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

    /// Consume and return the `params_rescan_requested` flag — re-read
    /// parameter metadata when this fires.
    pub fn poll_params_rescan(&self) -> bool {
        self.host_state
            .poll(&self.host_state.params.rescan_requested)
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

    /// Consume and return the `remote_controls.changed` flag.
    pub fn poll_remote_controls_changed(&self) -> bool {
        self.host_state
            .poll(&self.host_state.remote_controls.changed)
    }

    /// Consume and return the page ID the plugin most recently suggested
    /// the host switch to, or `None` if no suggestion is pending.
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

    /// Consume and return the `preset_loaded` flag.
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
    /// afterwards to ping the plugin.
    pub fn set_track_info(&self, info: TrackInfo) {
        if let Ok(mut guard) = self.host_state.resources.track_info.lock() {
            *guard = Some(info);
        }
    }

    /// Tell the plugin its track info has changed.
    pub fn notify_track_info_changed(&self) {
        if self.extensions.system.track_info.is_null() {
            return;
        }
        let ext = unsafe { &*self.extensions.system.track_info };
        if let Some(f) = ext.changed {
            unsafe { f(self.plugin.as_ptr()) };
        }
    }

    /// Number of remote-control pages the plugin exposes.
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

    /// Describe the remote-controls page at `index`.
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
    /// [`Self::context_menu_populate`].
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
    /// plugin exposes via the draft `CLAP_EXT_TRIGGERS`.
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

    /// Describe the trigger at `index`.
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
    /// Call from a worker thread.
    pub fn thread_pool_exec(&self, task_index: u32) {
        if self.extensions.system.thread_pool.is_null() {
            return;
        }
        let ext = unsafe { &*self.extensions.system.thread_pool };
        if let Some(f) = ext.exec {
            unsafe { f(self.plugin.as_ptr(), task_index) };
        }
    }

    /// Tell the plugin its tuning table set has changed.
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
    /// Returns the number of callbacks invoked.
    #[cfg(unix)]
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
