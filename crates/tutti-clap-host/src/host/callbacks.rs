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

use super::state::{HostState, PosixFdEntry, TRANSPORT_QUEUE, TimerEntry};
use crate::types::{AudioPortsRescan, ParamRescan};
use std::sync::atomic::Ordering;
use std::time::Instant;
use tutti_clap_ffi::{AudioPortsRescanFlags, HostCallbacks, LogSeverity, ParamsRescanFlags};
use tutti_plugin_types::{Request, RescanScope, TrackInfo, TransportRequest};

/// The stderr tag for each CLAP severity. Split out of the log method so the
/// mapping is testable: inline, its seven arms were reachable only through
/// the FFI, with stderr as the sole output.
fn severity_label(severity: LogSeverity) -> &'static str {
    match severity {
        LogSeverity::Debug => "DEBUG",
        LogSeverity::Info => "INFO",
        LogSeverity::Warning => "WARN",
        LogSeverity::Error => "ERROR",
        LogSeverity::Fatal => "FATAL",
        LogSeverity::HostMisbehaving => "HOST-MISBEHAVING",
        LogSeverity::PluginMisbehaving => "PLUGIN-MISBEHAVING",
        LogSeverity::Unknown(_) => "UNKNOWN",
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
    fn log(&self, severity: LogSeverity, message: &str) {
        let label = severity_label(severity);
        if let LogSeverity::Unknown(raw) = severity {
            eprintln!("[clap-plugin ?{raw}] {message}");
        } else {
            eprintln!("[clap-plugin {label}] {message}");
        }
        // Retain the line so a consumer can route it somewhere other than
        // stderr. Unrecognised severities are kept verbatim: see `LogRecord`.
        self.log.push(severity.raw(), message.to_owned());
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
        if let Some(wake) = self.process_wake.get() {
            wake.raise();
        }
    }

    fn request_callback(&self) {
        Request::Callback.raise(&self.requests);
    }

    fn params_rescan(&self, flags: ParamsRescanFlags) {
        // One bit per scope, so the main thread can tell a full rescan, which
        // the spec says is honoured only while the plugin is deactivated, from
        // a value-only one applicable live. Bits accumulate between takes.
        let rescan = ParamRescan::from_flags(true, flags.bits());
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

    fn audio_ports_rescan(&self, flags: AudioPortsRescanFlags) {
        // Five of the six flags are `[!active]`: only those change what a
        // buffer looks like, so only they ask for a restart. A cosmetic name
        // change asks for nothing.
        if AudioPortsRescan::from_flags(true, flags.bits()).needs_deactivate() {
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
        let entry = TimerEntry {
            id,
            period_ms,
            last_fire: now,
        };
        self.timer.note_due(entry.wake_at());
        timers.push(entry);
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

    fn track_info(&self) -> Option<TrackInfo> {
        self.track.info.lock().ok()?.clone()
    }

    fn transport_request(&self, request: TransportRequest) {
        // A refused request is the plugin's flood, so it lands in the log a
        // consumer reads, as the plugin misbehaving.
        if !self.transport.push(request) {
            self.log(
                LogSeverity::PluginMisbehaving,
                &format!("transport request {request:?} refused: {TRANSPORT_QUEUE} already wait"),
            );
        }
    }

    fn posix_fd_register(&self, fd: i32, flags: u32) -> bool {
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
            return false;
        };
        if fds.iter().any(|e| e.fd == fd) {
            return false;
        }
        fds.push(PosixFdEntry { fd, flags });
        true
    }

    fn posix_fd_modify(&self, fd: i32, flags: u32) -> bool {
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
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

    fn posix_fd_unregister(&self, fd: i32) -> bool {
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
            return false;
        };
        let len_before = fds.len();
        fds.retain(|e| e.fd != fd);
        fds.len() < len_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An audio-ports rescan asks for a restart only when a flag that changes a
    /// buffer's shape is set; a cosmetic name change asks for nothing.
    #[test]
    fn a_ports_rescan_asks_for_a_restart_only_for_a_change_of_shape() {
        let state = HostState::new(
            &tutti_plugin_types::MainThread::claim().expect("one test, one process"),
        );

        state.audio_ports_rescan(AudioPortsRescanFlags::NAMES);
        assert_eq!(tutti_plugin_types::taken(&state.requests).count(), 0);

        state.audio_ports_rescan(AudioPortsRescanFlags::CHANNEL_COUNT);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::IoChanged]);
    }

    /// Two rescans between takes keep both requests, each once.
    #[test]
    fn rescans_between_takes_accumulate_rather_than_overwrite() {
        let state = HostState::new(
            &tutti_plugin_types::MainThread::claim().expect("one test, one process"),
        );

        state.params_rescan(ParamsRescanFlags::VALUES);
        state.params_rescan(ParamsRescanFlags::TEXT | ParamsRescanFlags::VALUES);
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
    #[test]
    fn each_severity_has_its_label() {
        assert_eq!(
            severity_label(LogSeverity::from_raw(clap_sys::ext::log::CLAP_LOG_WARNING)),
            "WARN"
        );
        assert_eq!(
            severity_label(LogSeverity::from_raw(
                clap_sys::ext::log::CLAP_LOG_PLUGIN_MISBEHAVING
            )),
            "PLUGIN-MISBEHAVING"
        );
        assert_eq!(severity_label(LogSeverity::from_raw(99)), "UNKNOWN");
    }
}
