//! Polling host-state flags, opening/closing the plugin editor, context
//! menus, triggers, remote controls, POSIX FDs, timers, and other
//! main-thread host-interaction methods.

use super::ClapLoaded;
use crate::error::{ClapError, Result};
use crate::host::state::TimerEntry;
use crate::host::{HostState, LogRecord};
use crate::types::{ContentScale, EditorCapabilities, EditorSize, WindowHandle};
#[cfg(feature = "clap-extras")]
use crate::types::{
    ContextMenuItem, ContextMenuTarget, RemoteControlsPage, TrackInfo, TransportRequest,
    TriggerInfo,
};
use std::sync::Arc;
use tutti_clap_ffi::GuiError;
#[cfg(unix)]
use tutti_clap_ffi::{Extension, FdPoller};
use tutti_plugin_types::MainThread;

#[cfg(feature = "clap-extras")]
fn menu_target(target: ContextMenuTarget) -> tutti_clap_ffi::MenuTarget {
    match target {
        ContextMenuTarget::Global => tutti_clap_ffi::MenuTarget::Global,
        ContextMenuTarget::Param(id) => tutti_clap_ffi::MenuTarget::Param(id),
    }
}

fn gui_error(error: GuiError) -> ClapError {
    ClapError::EditorError(error.to_string())
}

impl ClapLoaded {
    /// Creates the plugin editor and embed it into the given native `parent`
    /// window, whose backing-store scale is `scale`, returning the editor's
    /// initial size.
    ///
    /// Follows the CLAP embed sequence: `is_api_supported` → `create` →
    /// `set_scale(scale)` → `get_size` → `set_parent` → `show`. See
    /// `embed_editor_sequence` for the ordering rationale and for which
    /// window apis take the `set_scale` step (win32 and x11; never cocoa).
    ///
    /// # Errors
    /// [`ClapError::EditorError`] if the plugin does not expose a GUI, if the
    /// embedded window API is unsupported, or if `create`/`set_parent` fails.
    pub(crate) fn embed_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> Result<EditorSize> {
        self.host_state
            .gui
            .window_destroyed
            .store(false, std::sync::atomic::Ordering::Release);
        self.host_state
            .gui
            .closed
            .store(false, std::sync::atomic::Ordering::Release);

        let outcome = self
            .plugin
            .open_embedded(main, &parent, scale.get())
            .map_err(gui_error)?;

        // Only an editor that was `create`d needs `destroy` later.
        if outcome.did_create {
            self.flags.gui_created = true;
        }

        Ok(EditorSize {
            width: outcome.width,
            height: outcome.height,
        })
    }

    /// Creates the plugin's own top-level window, rather than embedding.
    ///
    /// The path for a plugin that answers `is_api_supported(api, true)` and not
    /// the embedded question — legal CLAP, and the only option on windowing
    /// systems where embedding is unavailable (`ext/gui.h:68` marks Wayland
    /// "embed is currently not supported, use floating windows").
    ///
    /// **Returns no size, deliberately.** An embedded editor's size is a fact
    /// the host needs, because the host lays out the window it owns. A floating
    /// window is the plugin's, so there is nothing here for a caller to apply —
    /// returning a size would imply otherwise, and a caller acting on it would
    /// be fighting the plugin for control of a window it does not own.
    ///
    /// `transient` is the host window the plugin's should stay above; `None`
    /// skips the hint. `title` is advisory — the plugin owns its title bar.
    ///
    /// # Errors
    /// [`ClapError::EditorError`] if the plugin exposes no GUI, if it does not
    /// support a floating window, or if `create` fails.
    pub fn open_floating_editor_with(
        &mut self,
        main: &MainThread,
        transient: Option<WindowHandle>,
        title: &std::ffi::CStr,
    ) -> Result<()> {
        self.host_state
            .gui
            .window_destroyed
            .store(false, std::sync::atomic::Ordering::Release);
        self.host_state
            .gui
            .closed
            .store(false, std::sync::atomic::Ordering::Release);

        let did_create = self
            .plugin
            .open_floating(main, transient.as_ref(), title)
            .map_err(gui_error)?;

        if did_create {
            self.flags.gui_created = true;
        }

        Ok(())
    }

    /// Whether this plugin can present a floating (plugin-owned) window.
    ///
    /// The counterpart to [`has_editor`](tutti_plugin_types::EditorWindow::has_editor), which asks the
    /// embedded question only. A host should consult both: a plugin may support
    /// either, both, or — via [`prefers_floating`](Self::prefers_floating) —
    /// have an opinion about which.
    ///
    /// Side-effect-free, like its embedded sibling: `is_api_supported` is a
    /// predicate, `create` is what allocates.
    #[must_use]
    pub fn has_floating_editor(&self, main: &MainThread) -> bool {
        self.plugin.has_floating_editor(main)
    }

    /// The plugin's preferred windowing api and mode, if it states one.
    ///
    /// `ext/gui.h:113` — "The host has no obligation to honor the plugin
    /// preference, this is just a hint." Returned as the `is_floating` flag
    /// alone: the api half is only useful to a host that speaks more than one
    /// per platform, and this one does not — `platform_window_handle` resolves
    /// exactly one api per target, so reporting a preference this host cannot
    /// act on would invite a caller to try.
    ///
    /// `None` means the plugin expressed no preference, which is distinct from
    /// preferring embedded — a caller that conflates them would override a
    /// plugin that deliberately said nothing.
    pub fn prefers_floating(&self, main: &MainThread) -> Option<bool> {
        self.plugin.prefers_floating(main)
    }

    /// Queries the plugin's resize/aspect capabilities.
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
    /// on every call made before `create()`.
    ///
    /// Defaults are the honest answer before create: nothing has been asked, so
    /// nothing is claimed. A caller wanting real hints must create the editor first,
    /// which is the same order the spec requires of it.
    pub fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        if !self.flags.gui_created {
            return EditorCapabilities::default();
        }
        let Some(hints) = self.plugin.gui_hints(main) else {
            return EditorCapabilities::default();
        };
        let mut caps = EditorCapabilities {
            resize: tutti_plugin_types::ResizeHints {
                resizable: hints.resizable,
                can_resize_horizontally: hints.can_resize_horizontally,
                can_resize_vertically: hints.can_resize_vertically,
            },
            aspect: tutti_plugin_types::AspectRatio::default(),
            appkit_autoresize_friendly: false,
        };
        caps.aspect.preserve = hints.preserve_aspect_ratio;
        caps.aspect.ratio = hints.aspect_ratio;
        caps
    }

    /// Asks the plugin to resize its open editor to `requested`, returning the
    /// size it applied after snapping.
    ///
    /// When the plugin implements `adjust_size` the request is snapped first, so
    /// the result may differ from `requested`.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::EditorError`] if the plugin has no GUI extension,
    /// cannot adjust the request to a usable size, lacks `set_size`, or refuses
    /// the adjusted size.
    pub fn resize_editor(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> Result<EditorSize> {
        let (width, height) = self
            .plugin
            .resize_gui(main, requested.width, requested.height)
            .map_err(gui_error)?;
        Ok(EditorSize { width, height })
    }

    /// Consumes the plugin's pending `gui.request_resize`, if any.
    ///
    /// Draining: a second call with no new request answers `None`. The host
    /// is free to ignore or clamp the size — CLAP treats it as a request, not
    /// an instruction.
    pub fn take_editor_resize_request(&self) -> Option<EditorSize> {
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

    /// [`EditorWindow::close_editor`](tutti_plugin_types::EditorWindow::close_editor) without a token, for `Drop`, which
    /// has none: the thread a drop runs on is the owner's. The per-format lane
    /// that gives the plugin a main-thread handle makes this unreachable off the
    /// main thread.
    pub(crate) fn destroy_editor(&mut self, main: &MainThread) {
        if !self.flags.gui_created {
            return;
        }
        // The plugin may have reported its window already gone (`gui.closed`
        // with `was_destroyed`): then there is nothing to hide, but `destroy`
        // still runs.
        let window_destroyed = self
            .host_state
            .gui
            .window_destroyed
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        self.plugin.close_gui(main, window_destroyed);
        self.flags.gui_created = false;
    }

    /// Direct access to the shared [`HostState`] — useful if you need to
    /// read a flag without consuming it or observe a field not wrapped by
    /// the `poll_*` helpers.
    pub fn host_state(&self) -> &Arc<HostState> {
        &self.host_state
    }

    /// Consumes and returns the `process_requested` flag (the plugin wants
    /// `process()` to be called even if the host would otherwise skip it).
    pub fn take_process_requested(&self) -> bool {
        self.host_state
            .take(&self.host_state.lifecycle.process_requested)
    }

    /// Consumes and returns the `params_flush_requested` flag — call
    /// [`Self::flush_params`] or run a process block when this fires.
    pub fn take_params_flush_requested(&self) -> bool {
        self.host_state
            .take(&self.host_state.params.flush_requested)
    }

    /// Consumes and returns the `state_dirty` flag — the plugin's state has
    /// diverged from the last save.
    pub fn take_state_dirty(&self) -> bool {
        self.host_state
            .take(&self.host_state.processing.state_dirty)
    }

    /// Consumes and returns the `notes.ports_changed` flag. The note dialect
    /// ([`ClapProcessor::sends_midi_notes`](crate::ClapProcessor::sends_midi_notes)) is
    /// read only at activation, so on `true` activate again.
    pub fn take_note_ports_changed(&self) -> bool {
        self.host_state.take(&self.host_state.notes.ports_changed)
    }

    /// Consumes and returns the `gui.closed` flag.
    pub fn take_gui_closed(&self) -> bool {
        self.host_state.take(&self.host_state.gui.closed)
    }

    /// Drains every `clap.log` line the plugin has emitted since the last drain,
    /// oldest first.
    ///
    /// Lines are also mirrored to stderr as they arrive; this is the
    /// programmatic route. Draining rather than peeking keeps "have I seen this
    /// line?" out of the caller, and the buffer is bounded by
    /// [`LOG_CAPACITY`](crate::host::LOG_CAPACITY) for a caller that never
    /// drains.
    pub fn drain_log(&self) -> Vec<LogRecord> {
        let mut records = self
            .host_state
            .log
            .records
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        records.drain(..).collect()
    }

    /// How many log lines the host did not keep — cumulative, not consumed on
    /// read, so callers track their own delta.
    ///
    /// Either the buffer was full, or the plugin logged from the audio thread,
    /// where keeping the line would mean allocating and locking inside the
    /// callback (see `host_log`). Counted together: a consumer cannot act
    /// differently on the two.
    pub fn log_lines_dropped(&self) -> u32 {
        self.host_state
            .log
            .dropped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Fires any expired timers the plugin registered via
    /// `CLAP_EXT_TIMER_SUPPORT`. Call periodically from the main thread.
    /// Returns the number of timer callbacks invoked.
    ///
    /// `on_timer` is `[main-thread]` (`ext/timer-support.h`), and a UI
    /// framework's tick need not run on the thread that loaded the plugin.
    pub fn poll_timers(&mut self, main: &MainThread) -> usize {
        self.poll_timers_at(main, std::time::Instant::now())
    }

    /// Like [`poll_timers`](Self::poll_timers), with the current time supplied
    /// by the caller.
    ///
    /// A timer is due once `period_ms` has elapsed since it last fired, and its
    /// period restarts from the fire instant. An instant earlier than a timer's
    /// last fire is treated as not due. Useful for driving timers
    /// deterministically in tests; production code calls `poll_timers`.
    pub fn poll_timers_at(&mut self, main: &MainThread, now: std::time::Instant) -> usize {
        if !self.plugin.has_on_timer() {
            // Nothing can fire, so nothing is due: the per-block pass need not
            // come back for these timers.
            self.host_state.timer.set_next_due(None);
            return 0;
        }

        let plugin = &mut self.plugin;
        fire_due_timers(&self.host_state.timer, now, |id| {
            plugin.fire_timer(main, id);
        })
    }

    /// Consumes and returns the `audio_ports.config_changed` flag.
    pub fn take_audio_ports_config_changed(&self) -> bool {
        self.host_state
            .take(&self.host_state.audio_ports.config_changed)
    }

    /// Consumes and returns the `remote_controls.changed` flag. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn take_remote_controls_changed(&self) -> bool {
        self.host_state
            .take(&self.host_state.remote_controls.changed)
    }

    /// Consumes and returns the page ID the plugin most recently suggested
    /// the host switch to, or `None` if no suggestion is pending. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn take_suggested_remote_page(&self) -> Option<u32> {
        let val = self
            .host_state
            .remote_controls
            .suggested_page
            .swap(u32::MAX, std::sync::atomic::Ordering::AcqRel);
        if val == u32::MAX { None } else { Some(val) }
    }

    /// Drains all pending [`TransportRequest`]s the plugin has emitted, in
    /// arrival order. Requires the `clap-extras` feature.
    ///
    /// Main-thread only: this takes the lock that the plugin-side push
    /// deliberately only *tries*, so calling it from the audio thread
    /// reintroduces the inversion that push avoids.
    ///
    /// A short result is not proof the plugin was quiet — the queue drops when
    /// full or contended. Read
    /// [`TransportState::dropped`](crate::host::state::TransportState::dropped)
    /// when that matters.
    #[cfg(feature = "clap-extras")]
    pub fn drain_transport_requests(&self) -> Vec<TransportRequest> {
        let Ok(mut reqs) = self.host_state.transport.requests.lock() else {
            return Vec::new();
        };
        // `drain`, not `mem::take`: taking the `VecDeque` leaves an empty one
        // with no capacity behind, so the plugin's next push would allocate —
        // on the audio thread, which is the whole thing this queue avoids.
        reqs.drain(..).collect()
    }

    /// Consumes and returns the `notes.names_changed` flag.
    pub fn take_note_names_changed(&self) -> bool {
        self.host_state.take(&self.host_state.notes.names_changed)
    }

    /// Consumes and returns the `notes.voice_info_changed` flag.
    pub fn take_voice_info_changed(&self) -> bool {
        self.host_state
            .take(&self.host_state.notes.voice_info_changed)
    }

    /// Consumes and returns the `preset_loaded` flag. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn take_preset_loaded(&self) -> bool {
        self.host_state
            .take(&self.host_state.processing.preset_loaded)
    }

    /// Invokes the plugin's `on_main_thread` callback — call when
    /// the plugin raised a [`Request::Callback`](tutti_plugin_types::Request::Callback).
    ///
    /// The name states the contract (`plugin.h`'s `on_main_thread` is
    /// `[main-thread]`), and it is the other method an embedder is likely to
    /// drive from a UI tick, so it takes the same guard as
    /// [`Self::poll_timers`]. A plugin reached here off-thread runs whatever
    /// deferred work it queued on the wrong thread, which tends to show up as a
    /// crash somewhere else entirely.
    pub fn on_main_thread(&mut self, main: &MainThread) -> &mut Self {
        self.plugin.on_main_thread(main);
        self
    }

    /// Publishes track metadata for the plugin to read via
    /// `CLAP_EXT_TRACK_INFO`. Call [`Self::notify_track_info_changed`]
    /// afterwards to ping the plugin. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn set_track_info(&self, info: TrackInfo) {
        if let Ok(mut guard) = self.host_state.resources.track_info.lock() {
            *guard = Some(info);
        }
    }

    /// Tells the plugin its track info has changed. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn notify_track_info_changed(&self, main: &MainThread) {
        self.plugin.notify_track_info_changed(main);
    }

    /// Number of remote-control pages the plugin exposes. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn remote_controls_page_count(&self, main: &MainThread) -> usize {
        self.plugin.remote_controls_count(main) as usize
    }

    /// Describes the remote-controls page at `index`. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn remote_controls_page(
        &self,
        main: &MainThread,
        index: usize,
    ) -> Option<RemoteControlsPage> {
        let page = self
            .plugin
            .remote_controls_page(main, u32::try_from(index).ok()?)?;
        Some(RemoteControlsPage {
            section_name: page.section_name,
            page_id: page.page_id,
            page_name: page.page_name,
            param_ids: page.param_ids,
            is_for_preset: page.is_for_preset,
        })
    }

    /// Asks the plugin to supply the context-menu entries for `target`.
    /// Returns `None` if the plugin does not implement context menus.
    /// Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn context_menu_populate(
        &self,
        main: &MainThread,
        target: ContextMenuTarget,
    ) -> Option<Vec<ContextMenuItem>> {
        self.plugin.context_menu_populate(main, menu_target(target))
    }

    /// Invokes a context-menu action the plugin previously reported via
    /// [`Self::context_menu_populate`]. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn context_menu_perform(
        &self,
        main: &MainThread,
        target: ContextMenuTarget,
        action_id: u32,
    ) -> bool {
        self.plugin
            .context_menu_perform(main, menu_target(target), action_id)
    }

    /// Number of trigger "parameters" (stateless momentary actions) the
    /// plugin exposes via the draft `CLAP_EXT_TRIGGERS`. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn trigger_count(&self, main: &MainThread) -> usize {
        self.plugin.trigger_count(main) as usize
    }

    /// Describes the trigger at `index`. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn trigger_info(&self, main: &MainThread, index: usize) -> Option<TriggerInfo> {
        let info = self.plugin.trigger_info(main, u32::try_from(index).ok()?)?;
        Some(TriggerInfo {
            id: info.id,
            flags: info.flags,
            name: info.name,
            module: info.module,
        })
    }

    /// Tells the plugin its tuning table set has changed. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn notify_tuning_changed(&self, main: &MainThread) {
        self.plugin.notify_tuning_changed(main);
    }

    /// Calls `on_fd` for each POSIX descriptor the plugin registered that is
    /// ready now, with the events that are ready among those it asked for
    /// (`ext/posix-fd-support.h`). Never blocks. Returns the number of
    /// callbacks invoked.
    ///
    /// `on_fd` is `[main-thread]`.
    #[cfg(unix)]
    pub fn poll_posix_fds(&mut self, main: &MainThread) -> usize {
        if self.scan_fds().is_none() {
            return 0;
        }
        let ready: Vec<i32> = self.work.ready_fds().iter().map(|&(fd, _)| fd).collect();
        ready
            .into_iter()
            .filter(|&fd| self.fire_fd(main, fd))
            .count()
    }

    /// Polls the registered descriptors without calling the plugin: fills
    /// [`MainThreadWork::ready_fds`]. `None` when the plugin has no
    /// `posix-fd-support`, or its descriptor list is unreadable.
    #[cfg(unix)]
    fn scan_fds(&mut self) -> Option<()> {
        if !self.plugin.supports(Extension::PosixFdSupport) {
            return None;
        }
        let scratch = &mut self.work.fds;
        scratch.watched.clear();
        match self.host_state.resources.posix_fds.lock() {
            Ok(fds) => scratch.watched.extend(fds.iter().map(|e| (e.fd, e.flags))),
            Err(_) => {
                scratch.ready.clear();
                return None;
            }
        }
        scratch.poller.poll(&scratch.watched, &mut scratch.ready);
        Some(())
    }

    /// Calls `on_fd` for descriptor `fd` if the last scan found it ready and it
    /// is still registered (an earlier callback may have unregistered it).
    /// Returns whether the plugin was called.
    #[cfg(unix)]
    pub(crate) fn fire_fd(&mut self, main: &MainThread, fd: i32) -> bool {
        if !self.plugin.has_on_fd() {
            return false;
        }
        let Some(at) = self.work.fds.ready.iter().position(|&(f, _)| f == fd) else {
            return false;
        };
        let (_, ready) = self.work.fds.ready.remove(at);
        let registered = self
            .host_state
            .resources
            .posix_fds
            .lock()
            .is_ok_and(|fds| fds.iter().any(|e| e.fd == fd));
        if registered {
            self.plugin.fire_fd(main, fd, ready);
        }
        registered
    }

    /// Calls the plugin's `on_timer` for timer `id` if it is still registered
    /// (an earlier callback may have unregistered it), and caches the next
    /// deadline. Returns whether the plugin was called.
    pub(crate) fn fire_timer(&mut self, main: &MainThread, id: u32) -> bool {
        let on_timer = self.plugin.has_on_timer();
        let state = &self.host_state.timer;
        let registered = state
            .timers
            .lock()
            .is_ok_and(|timers| timers.iter().any(|t| t.id == id));
        if registered && on_timer {
            self.plugin.fire_timer(main, id);
        }
        cache_next_deadline(state);
        registered && on_timer
    }

    /// Which scans are due, and the timers that are, without calling the
    /// plugin. The clock is read at most once, and only when a timer or a
    /// descriptor is registered.
    pub(crate) fn due_work(
        &mut self,
        clock: impl FnOnce() -> std::time::Instant,
    ) -> (WorkRan, Vec<u32>) {
        let timer = &self.host_state.timer;
        #[cfg(unix)]
        let fds_registered = self
            .host_state
            .resources
            .posix_fd_count
            .load(std::sync::atomic::Ordering::Acquire)
            > 0;
        #[cfg(not(unix))]
        let fds_registered = false;
        let next_timer = timer.next_due_nanos();
        if !work_needs_clock(next_timer, fds_registered) {
            return (WorkRan::default(), Vec::new());
        }
        let now = clock();
        let ran = work_due(
            next_timer,
            timer.epoch(),
            fds_registered,
            self.work.last_fd_scan,
            now,
        );
        let due = if ran.timers {
            collect_due_timers(&self.host_state.timer, now)
        } else {
            Vec::new()
        };
        #[cfg(unix)]
        if ran.fds {
            self.work.last_fd_scan = Some(now);
            self.scan_fds();
        }
        (ran, due)
    }
}

/// What the per-block pass keeps between calls.
#[derive(Default)]
pub(crate) struct MainThreadWork {
    /// When the descriptors were last polled.
    last_fd_scan: Option<std::time::Instant>,
    /// Room the descriptor scan reuses.
    #[cfg(unix)]
    fds: FdScratch,
}

impl MainThreadWork {
    /// The descriptors the last scan found ready, with the events ready.
    #[cfg(unix)]
    pub(crate) fn ready_fds(&self) -> &[(i32, u32)] {
        &self.fds.ready
    }
}

/// Which scans a pass ran.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkRan {
    pub timers: bool,
    pub fds: bool,
}

/// The least time between two polls of the registered descriptors.
const FD_SCAN_PERIOD: std::time::Duration = std::time::Duration::from_millis(5);

/// Whether the pass needs to read the clock at all: a timer is registered or a
/// descriptor is.
fn work_needs_clock(next_timer_nanos: u64, fds_registered: bool) -> bool {
    next_timer_nanos != crate::host::state::TimerState::NONE || fds_registered
}

/// Which scans are due at `now`: the timers once the earliest deadline has
/// passed, the descriptors once [`FD_SCAN_PERIOD`] has passed since the last.
fn work_due(
    next_timer_nanos: u64,
    epoch: std::time::Instant,
    fds_registered: bool,
    last_fd_scan: Option<std::time::Instant>,
    now: std::time::Instant,
) -> WorkRan {
    let since_epoch = now.saturating_duration_since(epoch).as_nanos();
    WorkRan {
        timers: next_timer_nanos != crate::host::state::TimerState::NONE
            && since_epoch >= u128::from(next_timer_nanos),
        fds: fds_registered
            && last_fd_scan
                .is_none_or(|last| now.saturating_duration_since(last) >= FD_SCAN_PERIOD),
    }
}

/// Takes the timers due at `now`, each restarting its period, and caches the
/// next deadline. They are fired one at a time by `fire_timer`.
fn collect_due_timers(state: &crate::host::state::TimerState, now: std::time::Instant) -> Vec<u32> {
    let due = match state.timers.lock() {
        Ok(mut timers) => take_due_timers(&mut timers, now),
        Err(_) => Vec::new(),
    };
    cache_next_deadline(state);
    due
}

/// Stores the earliest deadline among the registered timers, exactly.
/// Stored under the lock, so a registration's `note_due` is not overwritten.
fn cache_next_deadline(state: &crate::host::state::TimerState) {
    if let Ok(timers) = state.timers.lock() {
        let next = timers
            .iter()
            .map(|t| t.last_fire + std::time::Duration::from_millis(u64::from(t.period_ms)))
            .min();
        state.set_next_due(next);
    }
}

/// Fires each due timer through `fire`, skipping one an earlier callback in
/// the same pass unregistered, and caches the next deadline. Returns how many
/// fired.
fn fire_due_timers(
    state: &crate::host::state::TimerState,
    now: std::time::Instant,
    mut fire: impl FnMut(u32),
) -> usize {
    let due = match state.timers.lock() {
        Ok(mut timers) => take_due_timers(&mut timers, now),
        Err(_) => Vec::new(),
    };
    let mut fired = 0usize;
    for id in due {
        // An earlier `on_timer` may have unregistered this one.
        let registered = state
            .timers
            .lock()
            .is_ok_and(|timers| timers.iter().any(|t| t.id == id));
        if registered {
            fire(id);
            fired += 1;
        }
    }
    // After the callbacks, which may have registered or unregistered timers.
    cache_next_deadline(state);
    fired
}

/// Room for one scan of the registered descriptors.
#[cfg(unix)]
#[derive(Default)]
struct FdScratch {
    watched: Vec<(i32, u32)>,
    poller: FdPoller,
    ready: Vec<(i32, u32)>,
}

/// Which registered timers are due at `now`, marking each as fired.
///
/// Split out of [`ClapLoaded::poll_timers_at`] because it is the whole of the
/// decision and none of the FFI: every input is a plain value, so it can be
/// tested directly, whereas the poll needs a live plugin on the other end of a
/// function pointer. That split is what makes the comparison below coverable at
/// all — the suite that drove it through the FFI could only reach it with
/// `period_ms = 0`, where every possible comparison returns the same answer.
///
/// A timer is due when at least its full period has elapsed since it last
/// fired. `>=` rather than `>`: a 10 ms timer polled at exactly 10 ms has
/// waited its period, and requiring a strictly greater instant would make a
/// caller ticking on an exact period never fire at all.
///
/// `last_fire` advances to `now`, not by `period_ms`. The difference shows when
/// a poll is late: advancing by the period would leave the timer immediately
/// due again and fire it repeatedly to "catch up", which for a UI heartbeat
/// means a burst of redraws after every stall. CLAP's timers are a periodic
/// wake-up, not a budget to be reconciled.
fn take_due_timers(timers: &mut [TimerEntry], now: std::time::Instant) -> Vec<u32> {
    let mut due = Vec::new();
    for timer in timers.iter_mut() {
        // `checked_duration_since` rather than `duration_since`: the latter
        // panics (debug) or saturates (release) when `now` precedes
        // `last_fire`, which a caller passing its own instants can do. A timer
        // whose last fire is in the future is simply not due yet.
        let elapsed = now.checked_duration_since(timer.last_fire);
        if elapsed.is_some_and(|e| e.as_millis() >= timer.period_ms as u128) {
            due.push(timer.id);
            timer.last_fire = now;
        }
    }
    due
}

#[cfg(test)]
mod timer_due_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn timer(id: u32, period_ms: u32, last_fire: Instant) -> TimerEntry {
        TimerEntry {
            id,
            period_ms,
            last_fire,
        }
    }

    /// The headline case, and the one no previous test could express: a 10 ms
    /// timer ticked at 5, 10, 15 and 20 ms fires at 10 and 20 only.
    ///
    /// With `period_ms = 0`, "always due", `>=` and `<=` are
    /// indistinguishable. Here they are not: an inverted comparison fires
    /// at 5 and nowhere else, and an always-due implementation fires four
    /// times.
    ///
    /// 15 ms is in the list on purpose, and it is the tick that separates two
    /// plausible designs. The period restarts **from the fire instant**, so
    /// after firing at 10 the timer is next due at 20, not at 15 — a
    /// fixed-schedule timer, one that keeps its original 0/10/20 grid by
    /// advancing `last_fire` by `period_ms`, would fire here. See
    /// [`take_due_timers`] for why the restarting reading is the one CLAP
    /// wants.
    #[test]
    fn a_ten_ms_timer_is_due_at_ten_and_twenty_but_not_between() {
        let start = Instant::now();
        let mut timers = [timer(1, 10, start)];
        let at = |ms| start + Duration::from_millis(ms);

        assert_eq!(
            take_due_timers(&mut timers, at(5)),
            Vec::<u32>::new(),
            "5 ms into a 10 ms period the timer has not waited its period"
        );
        assert_eq!(
            take_due_timers(&mut timers, at(10)),
            vec![1],
            "at exactly one full period the timer is due; a `>` comparison would \
             make a caller ticking on the period never fire"
        );
        assert_eq!(
            take_due_timers(&mut timers, at(15)),
            Vec::<u32>::new(),
            "only 5 ms after firing at 10 ms; the period restarts from the fire \
             instant rather than staying on the original grid"
        );
        assert_eq!(
            take_due_timers(&mut timers, at(20)),
            vec![1],
            "a full period after the fire at 10 ms, the timer is due again"
        );
    }

    /// Firing resets the period rather than consuming a backlog.
    ///
    /// Polled at 10 and then at 12, a 10 ms timer fires once: the second poll
    /// is only 2 ms after the first fire. An implementation that advanced
    /// `last_fire` by `period_ms` instead of to `now` would still be at 10 and
    /// fire again immediately.
    #[test]
    fn firing_restarts_the_period_from_the_fire_instant() {
        let start = Instant::now();
        let mut timers = [timer(1, 10, start)];

        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(10)),
            vec![1]
        );
        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(12)),
            Vec::<u32>::new(),
            "2 ms after firing, a 10 ms timer is not due again"
        );
    }

    /// A late poll fires once, not once per period missed.
    ///
    /// 100 ms into a 10 ms period is ten periods' worth, and a host that
    /// "caught up" would deliver ten `on_timer` calls in a row — for a UI
    /// heartbeat, a burst of redraws after every stall.
    #[test]
    fn a_late_poll_does_not_fire_once_per_missed_period() {
        let start = Instant::now();
        let mut timers = [timer(1, 10, start)];

        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(100)),
            vec![1],
            "ten periods late must still be one fire"
        );
        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(101)),
            Vec::<u32>::new(),
            "and the backlog must not be waiting on the next poll either"
        );
    }

    /// Timers with different periods are judged independently, and each
    /// answer names its own id.
    ///
    /// A single shared deadline, or a loop that fired every timer once any was
    /// due, passes the single-timer tests above and fails here.
    #[test]
    fn timers_with_different_periods_expire_independently() {
        let start = Instant::now();
        let mut timers = [timer(1, 10, start), timer(2, 50, start)];

        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(10)),
            vec![1],
            "only the 10 ms timer is due at 10 ms"
        );
        assert_eq!(
            take_due_timers(&mut timers, start + Duration::from_millis(50)),
            vec![1, 2],
            "at 50 ms both are due: the 50 ms timer for the first time, the              10 ms one 40 ms after its last fire"
        );
    }

    /// The zero period every previous test used: due on every poll.
    ///
    /// Kept, because a plugin may legitimately register one and "fire as often
    /// as you poll" is the right answer — but pinned deliberately rather than
    /// relied on as the only case, which is what made the comparison
    /// uncoverable.
    #[test]
    fn a_zero_period_timer_is_due_on_every_poll() {
        let start = Instant::now();
        let mut timers = [timer(1, 0, start)];

        assert_eq!(take_due_timers(&mut timers, start), vec![1]);
        assert_eq!(take_due_timers(&mut timers, start), vec![1]);
    }

    /// An instant before `last_fire` is not due, and must not panic.
    ///
    /// `Instant::duration_since` panics in debug builds when the argument is
    /// later than the receiver. Since the clock is a caller's value rather than
    /// always `Instant::now()`, that is reachable.
    #[test]
    fn a_clock_that_went_backwards_is_not_due_and_does_not_panic() {
        let start = Instant::now();
        let mut timers = [timer(1, 10, start + Duration::from_millis(100))];

        assert_eq!(
            take_due_timers(&mut timers, start),
            Vec::<u32>::new(),
            "a timer whose last fire is in the future is not yet due"
        );
    }
}

#[cfg(test)]
mod main_thread_work_tests {
    use super::*;
    use crate::host::state::TimerState;
    use std::time::{Duration, Instant};

    /// **With no timer and no descriptor the pass does not read the clock**,
    /// and a pass over registered ones does.
    #[test]
    fn nothing_registered_needs_no_clock() {
        assert!(!work_needs_clock(TimerState::NONE, false));
        assert!(work_needs_clock(0, false));
        assert!(work_needs_clock(TimerState::NONE, true));
    }

    /// **The timers are scanned only once the earliest deadline has passed.**
    #[test]
    fn timers_wait_for_the_earliest_deadline() {
        let epoch = Instant::now();
        let at = |ms| epoch + Duration::from_millis(ms);
        let due = |now| work_due(10_000_000, epoch, false, None, now);
        assert!(!due(at(9)).timers, "before the deadline");
        assert!(due(at(10)).timers, "at the deadline");
        assert!(due(at(50)).timers, "after it");
        assert!(!work_due(TimerState::NONE, epoch, false, None, at(50)).timers);
    }

    /// **The descriptors are polled at most once per scan period, and only
    /// while one is registered.**
    #[test]
    fn descriptors_are_polled_once_per_period() {
        let epoch = Instant::now();
        let last = epoch + Duration::from_millis(100);
        let fds = |reg, last_scan, ms| {
            work_due(
                TimerState::NONE,
                epoch,
                reg,
                last_scan,
                last + Duration::from_millis(ms),
            )
            .fds
        };
        assert!(fds(true, None, 0), "never scanned");
        assert!(!fds(true, Some(last), 1), "just scanned");
        assert!(fds(true, Some(last), 5), "a period later");
        assert!(!fds(false, None, 50), "none registered");
    }

    /// A state holding the timers `(id, period_ms)`, each last fired at the
    /// state's epoch.
    fn state_with(timers: &[(u32, u32)]) -> TimerState {
        let state = TimerState::new();
        let at = state.epoch();
        let mut list = state.timers.lock().unwrap();
        for &(id, period_ms) in timers {
            list.push(TimerEntry {
                id,
                period_ms,
                last_fire: at,
            });
        }
        drop(list);
        state
    }

    /// **A timer an earlier callback of the same pass unregistered is not
    /// fired**, as a descriptor is not.
    #[test]
    fn a_timer_unregistered_earlier_in_the_pass_does_not_fire() {
        let state = state_with(&[(1, 0), (2, 0)]);
        let start = state.epoch();
        let mut fired = Vec::new();
        let n = fire_due_timers(&state, start, |id| {
            fired.push(id);
            // Timer 1's callback unregisters timer 2.
            state.timers.lock().unwrap().retain(|t| t.id != 2);
        });
        assert_eq!(fired, [1]);
        assert_eq!(n, 1);
    }

    /// **The pass leaves the cached deadline at the earliest timer's**, and at
    /// none once none is left.
    #[test]
    fn the_pass_caches_the_next_deadline() {
        let state = state_with(&[(1, 30), (2, 10)]);
        let t0 = state.epoch();
        fire_due_timers(&state, t0, |_| {});
        assert_eq!(state.next_due_nanos(), 10_000_000);
        state.timers.lock().unwrap().clear();
        fire_due_timers(&state, t0, |_| {});
        assert_eq!(state.next_due_nanos(), TimerState::NONE);
    }
}
