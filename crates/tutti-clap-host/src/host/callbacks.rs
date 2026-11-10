//! What a plugin's calls into the host do: [`HostCallbacks`] for
//! [`HostState`].
//!
//! The FFI crate owns the `clap_host` vtable, decodes each call and runs it
//! under the panic guard; every method here is the smallest possible thing:
//! set a latch, push onto a queue, or read a published value. The host's own
//! reaction happens later, on the main thread, when a `take_*` drains what
//! was set. No method calls back into the plugin, which is what keeps a
//! plugin that notifies the host from inside its own `process` from
//! re-entering itself.

#[cfg(unix)]
use super::state::PosixFdEntry;
use super::state::{HostState, TimerEntry};
use crate::types::{AudioPortsRescan, ParamRescan, TrackPortType, TransportRequest, UndoChange};
use clap_sys::ext::log::{
    CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO,
    CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_log_severity,
};
use std::sync::atomic::Ordering;
use std::time::Instant;
use tutti_clap_ffi::{
    HostCallbacks, TrackInfoWriter, TrackPortKind, TransportCommand, TuningInfoWriter,
};
use tutti_plugin_types::{Request, RescanScope};

/// The stderr tag for each CLAP severity. Split out of the log method so the
/// mapping is testable: inline, its seven arms were reachable only through
/// the FFI, with stderr as the sole output.
fn severity_label(severity: clap_log_severity) -> &'static str {
    match severity {
        CLAP_LOG_DEBUG => "DEBUG",
        CLAP_LOG_INFO => "INFO",
        CLAP_LOG_WARNING => "WARN",
        CLAP_LOG_ERROR => "ERROR",
        CLAP_LOG_FATAL => "FATAL",
        CLAP_LOG_HOST_MISBEHAVING => "HOST-MISBEHAVING",
        CLAP_LOG_PLUGIN_MISBEHAVING => "PLUGIN-MISBEHAVING",
        _ => "UNKNOWN",
    }
}

impl HostCallbacks for HostState {
    fn is_main_thread(&self) -> bool {
        HostState::is_main_thread(self)
    }

    fn is_audio_thread(&self) -> bool {
        HostState::is_audio_thread(self)
    }

    /// Routes one line off the audio thread. (An audio-thread line never
    /// gets here: the FFI crate counts it through
    /// [`log_dropped_on_audio_thread`](Self::log_dropped_on_audio_thread)
    /// before decoding it, because building the `String` is itself one of
    /// the allocations that guard exists to prevent.)
    fn log(&self, severity: i32, message: &str) {
        let label = severity_label(severity);
        if label == "UNKNOWN" {
            eprintln!("[clap-plugin ?{severity}] {message}");
        } else {
            eprintln!("[clap-plugin {label}] {message}");
        }
        // Retain the line so a consumer can route it somewhere other than
        // stderr. Unrecognised severities are kept verbatim: see `LogRecord`.
        self.log.push(severity, message.to_owned());
    }

    /// `clap.log` is `[thread-safe]`, audio thread included, but recording a
    /// line allocates, prints and takes a lock that
    /// [`drain_log`](crate::ClapLoaded::drain_log) holds across a copy. So an
    /// audio-thread line is counted, not recorded (visible through
    /// [`log_lines_dropped`](crate::ClapLoaded::log_lines_dropped)). That
    /// loses the message text, a real cost; the eventual answer is a
    /// lock-free queue with pre-allocated slots.
    fn log_dropped_on_audio_thread(&self) {
        self.log.note_audio_thread_drop();
    }

    fn request_restart(&self) {
        Request::Restart.raise(&self.requests);
    }

    fn request_process(&self) {
        self.lifecycle
            .process_requested
            .store(true, Ordering::Release);
    }

    fn request_callback(&self) {
        Request::Callback.raise(&self.requests);
    }

    fn params_rescan(&self, flags: u32) {
        // One bit per scope, so the main thread can tell a full rescan, which
        // the spec says is honoured only while the plugin is deactivated, from
        // a value-only one applicable live. Bits accumulate between takes.
        let rescan = ParamRescan::from_flags(true, flags);
        for (set, scope) in [
            (rescan.values, RescanScope::Values),
            (rescan.text, RescanScope::Text),
            (rescan.info, RescanScope::Info),
            (rescan.needs_deactivate(), RescanScope::All),
        ] {
            if set {
                Request::RescanParams(scope).raise(&self.requests);
            }
        }
    }

    fn params_request_flush(&self) {
        self.params.flush_requested.store(true, Ordering::Release);
    }

    fn state_mark_dirty(&self) {
        self.processing.state_dirty.store(true, Ordering::Release);
    }

    fn latency_changed(&self) {
        Request::LatencyChanged.raise(&self.requests);
    }

    fn tail_changed(&self) {
        Request::TailChanged.raise(&self.requests);
    }

    fn gui_resize_hints_changed(&self) {
        self.gui.resize_hints_changed.store(true, Ordering::Release);
    }

    fn gui_request_resize(&self, width: u32, height: u32) -> bool {
        self.gui
            .request_resize_width
            .store(width, Ordering::Release);
        self.gui
            .request_resize_height
            .store(height, Ordering::Release);
        self.gui
            .request_resize_pending
            .store(true, Ordering::Release);
        true
    }

    fn gui_closed(&self, was_destroyed: bool) {
        self.gui.closed.store(true, Ordering::Release);
        // `was_destroyed` reports the plugin's *window* is gone. Record it so
        // `close_editor` skips `gui.hide` (there is no window left to hide)
        // while still calling `gui.destroy`, which `ext/gui.h` requires the
        // host call "to acknowledge the gui destruction" and which releases
        // what `create` allocated.
        if was_destroyed {
            self.gui.window_destroyed.store(true, Ordering::Release);
        }
    }

    fn audio_ports_rescan(&self, flags: u32) {
        // Five of the six flags are `[!active]`: only those change what a
        // buffer looks like, so only they ask for a restart. A cosmetic name
        // change asks for nothing.
        if AudioPortsRescan::from_flags(true, flags).needs_deactivate() {
            Request::IoChanged.raise(&self.requests);
        }
    }

    fn note_ports_rescan(&self) {
        self.notes.ports_changed.store(true, Ordering::Release);
    }

    fn timer_register(&self, period_ms: u32) -> Option<u32> {
        let id = self.timer.next_id.fetch_add(1, Ordering::Relaxed);
        let mut timers = self.timer.timers.lock().ok()?;
        let now = Instant::now();
        timers.push(TimerEntry {
            id,
            period_ms,
            last_fire: now,
        });
        self.timer
            .note_due(now + std::time::Duration::from_millis(u64::from(period_ms)));
        Some(id)
    }

    fn timer_unregister(&self, timer_id: u32) -> bool {
        let Ok(mut timers) = self.timer.timers.lock() else {
            return false;
        };
        let len_before = timers.len();
        timers.retain(|t| t.id != timer_id);
        timers.len() < len_before
    }

    fn note_names_changed(&self) {
        self.notes.names_changed.store(true, Ordering::Release);
    }

    fn voice_info_changed(&self) {
        self.notes.voice_info_changed.store(true, Ordering::Release);
    }

    fn preset_loaded(&self) {
        self.processing.preset_loaded.store(true, Ordering::Release);
    }

    fn audio_ports_config_rescan(&self) {
        self.audio_ports
            .config_changed
            .store(true, Ordering::Release);
    }

    fn remote_controls_changed(&self) {
        self.remote_controls.changed.store(true, Ordering::Release);
    }

    fn remote_controls_suggest_page(&self, page_id: u32) {
        self.remote_controls
            .suggested_page
            .store(page_id, Ordering::Release);
    }

    fn track_info(&self, out: &mut TrackInfoWriter<'_>) -> bool {
        let Ok(guard) = self.resources.track_info.lock() else {
            return false;
        };
        let Some(track) = guard.as_ref() else {
            return false;
        };
        if let Some(name) = &track.name {
            out.set_name(name);
        }
        if let Some(color) = track.color {
            out.set_color(color.alpha, color.red, color.green, color.blue);
        }
        if let Some(audio) = track.audio {
            out.set_audio(
                i32::from(audio.layout.count()),
                audio.port_type.map(|kind| match kind {
                    TrackPortType::Mono => TrackPortKind::Mono,
                    TrackPortType::Stereo => TrackPortKind::Stereo,
                    TrackPortType::Surround => TrackPortKind::Surround,
                    TrackPortType::Ambisonic => TrackPortKind::Ambisonic,
                }),
            );
        }
        if track.is_return_track {
            out.set_return_track();
        }
        if track.is_bus {
            out.set_bus();
        }
        if track.is_master {
            out.set_master();
        }
        true
    }

    fn transport_request(&self, command: TransportCommand) {
        let request = match command {
            TransportCommand::Start => TransportRequest::Start,
            TransportCommand::Stop => TransportRequest::Stop,
            TransportCommand::Continue => TransportRequest::Continue,
            TransportCommand::Pause => TransportRequest::Pause,
            TransportCommand::TogglePlay => TransportRequest::TogglePlay,
            TransportCommand::Jump { position_beats } => TransportRequest::Jump { position_beats },
            TransportCommand::LoopRegion {
                start_beats,
                duration_beats,
            } => TransportRequest::LoopRegion {
                start_beats,
                duration_beats,
            },
            TransportCommand::ToggleLoop => TransportRequest::ToggleLoop,
            TransportCommand::EnableLoop(on) => TransportRequest::EnableLoop(on),
            TransportCommand::Record(on) => TransportRequest::Record(on),
            TransportCommand::ToggleRecord => TransportRequest::ToggleRecord,
        };
        self.transport.push(request);
    }

    fn ambisonic_changed(&self) {
        self.audio_ports
            .ambisonic_changed
            .store(true, Ordering::Release);
    }

    fn surround_changed(&self) {
        self.audio_ports
            .surround_changed
            .store(true, Ordering::Release);
    }

    fn triggers_rescan(&self) {
        self.resources
            .triggers_rescan_requested
            .store(true, Ordering::Release);
    }

    fn tuning_count(&self) -> u32 {
        self.resources
            .tuning_infos
            .lock()
            .map(|infos| u32::try_from(infos.len()).unwrap_or(u32::MAX))
            .unwrap_or(0)
    }

    fn tuning_info(&self, index: u32, out: &mut TuningInfoWriter<'_>) -> bool {
        let Ok(infos) = self.resources.tuning_infos.lock() else {
            return false;
        };
        let Some(tuning) = infos.get(index as usize) else {
            return false;
        };
        out.set(tuning.tuning_id, &tuning.name, tuning.is_dynamic);
        true
    }

    fn resource_directory_request(&self, is_shared: bool) -> bool {
        let lock = if is_shared {
            &self.resources.directory_shared
        } else {
            &self.resources.directory_private
        };
        lock.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    fn resource_directory_release(&self, is_shared: bool) {
        let lock = if is_shared {
            &self.resources.directory_shared
        } else {
            &self.resources.directory_private
        };
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }

    fn undo_begin_change(&self) {
        self.undo.in_progress.store(true, Ordering::Release);
    }

    fn undo_cancel_change(&self) {
        self.undo.in_progress.store(false, Ordering::Release);
    }

    fn undo_change_made(&self, name: &str, delta: &[u8], delta_can_undo: bool) {
        self.undo.in_progress.store(false, Ordering::Release);
        if let Ok(mut changes) = self.undo.changes.lock() {
            changes.push(UndoChange {
                name: name.to_owned(),
                delta: delta.to_vec(),
                delta_can_undo,
            });
        }
    }

    fn undo_request_undo(&self) {
        self.undo.requested.store(true, Ordering::Release);
    }

    fn undo_request_redo(&self) {
        self.undo.redo_requested.store(true, Ordering::Release);
    }

    fn undo_wants_context_updates(&self, wanted: bool) {
        self.undo.wants_context.store(wanted, Ordering::Release);
    }

    #[cfg(unix)]
    fn posix_fd_register(&self, fd: i32, flags: u32) -> bool {
        let Ok(mut fds) = self.resources.posix_fds.lock() else {
            return false;
        };
        if fds.iter().any(|e| e.fd == fd) {
            return false;
        }
        fds.push(PosixFdEntry { fd, flags });
        self.resources
            .posix_fd_count
            .store(fds.len(), Ordering::Release);
        true
    }

    #[cfg(unix)]
    fn posix_fd_modify(&self, fd: i32, flags: u32) -> bool {
        let Ok(mut fds) = self.resources.posix_fds.lock() else {
            return false;
        };
        match fds.iter_mut().find(|e| e.fd == fd) {
            Some(entry) => {
                entry.flags = flags;
                true
            }
            None => false,
        }
    }

    #[cfg(unix)]
    fn posix_fd_unregister(&self, fd: i32) -> bool {
        let Ok(mut fds) = self.resources.posix_fds.lock() else {
            return false;
        };
        let len_before = fds.len();
        fds.retain(|e| e.fd != fd);
        self.resources
            .posix_fd_count
            .store(fds.len(), Ordering::Release);
        fds.len() < len_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::ext::audio_ports::{
        CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_NAMES,
    };

    /// An audio-ports rescan asks for a restart only when a flag that changes a
    /// buffer's shape is set; a cosmetic name change asks for nothing.
    ///
    /// Mutation (run): `needs_deactivate` replaced by `true` in the callback →
    /// the names-only rescan raises `IoChanged` and the first assertion fails;
    /// by `false` → the channel-count rescan raises nothing and the second
    /// fails.
    #[test]
    fn a_ports_rescan_asks_for_a_restart_only_for_a_change_of_shape() {
        let state = HostState::new(
            &tutti_plugin_types::MainThread::claim().expect("one test, one process"),
        );

        state.audio_ports_rescan(CLAP_AUDIO_PORTS_RESCAN_NAMES);
        assert_eq!(tutti_plugin_types::taken(&state.requests).count(), 0);

        state.audio_ports_rescan(CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::IoChanged]);
    }

    /// Two rescans between takes keep both requests, each once.
    ///
    /// Mutation (run): the params callback raising only the first scope it
    /// finds set → the `Text` request is lost and the assertion fails.
    #[test]
    fn rescans_between_takes_accumulate_rather_than_overwrite() {
        let state = HostState::new(
            &tutti_plugin_types::MainThread::claim().expect("one test, one process"),
        );

        state.params_rescan(clap_sys::ext::params::CLAP_PARAM_RESCAN_VALUES);
        state.params_rescan(
            clap_sys::ext::params::CLAP_PARAM_RESCAN_TEXT
                | clap_sys::ext::params::CLAP_PARAM_RESCAN_VALUES,
        );
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::RescanParams(RescanScope::Values),
                Request::RescanParams(RescanScope::Text),
            ]
        );
    }

    /// A restart, a callback, a latency and a tail change each reach the word.
    ///
    /// Mutation (run): `tail_changed` raising `LatencyChanged` → the tail
    /// request is missing and the assertion fails.
    #[test]
    fn the_plain_callbacks_raise_their_requests() {
        let state = HostState::new(
            &tutti_plugin_types::MainThread::claim().expect("one test, one process"),
        );
        state.request_restart();
        state.request_callback();
        state.latency_changed();
        state.tail_changed();
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::Restart,
                Request::LatencyChanged,
                Request::TailChanged,
                Request::Callback,
            ]
        );
    }

    /// Every CLAP severity has its tag, and one the host does not know is
    /// `UNKNOWN` rather than a wrong label.
    ///
    /// Mutation: map `CLAP_LOG_WARNING` to `"INFO"`: the first assertion
    /// fails.
    #[test]
    fn each_severity_has_its_label() {
        assert_eq!(severity_label(CLAP_LOG_WARNING), "WARN");
        assert_eq!(
            severity_label(CLAP_LOG_PLUGIN_MISBEHAVING),
            "PLUGIN-MISBEHAVING"
        );
        assert_eq!(severity_label(99), "UNKNOWN");
    }
}
