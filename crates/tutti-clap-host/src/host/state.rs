//! Shared host↔plugin state: the latches a plugin's callbacks set and the
//! host's poll methods drain, plus the `[audio-thread]` role guard.
//!
//! A plugin calls host callbacks from its own threads at times the host does
//! not choose, so almost everything here is an atomic flag *set* by a callback
//! and *cleared* by the matching `poll_*` on the main thread. The flags are
//! grouped into one struct per CLAP extension so a caller can see which
//! extension a request came from.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tutti_clap_ffi::{AudioClaim, ThreadRoles};
use tutti_plugin_types::{MainThread, ProcessWake, RequestWord, TrackInfo, TransportRequest};

pub(crate) struct TimerEntry {
    pub id: u32,
    pub period_ms: u32,
    pub last_fire: Instant,
}

/// The least a timer's deadline is put after its last fire, whatever period it
/// asked for. `timer-support.h` lets the host adjust a period it finds too
/// short (30 Hz must be allowed), and a deadline at "now" for a period of zero
/// would have the main loop wake again at once, for ever. A timer that asks for
/// less still fires at its own period, whenever the loop is woken by anything.
pub(crate) const TIMER_FLOOR: std::time::Duration = std::time::Duration::from_millis(10);

impl TimerEntry {
    /// When the loop should wake for this timer: its period after the last
    /// fire, at least [`TIMER_FLOOR`].
    pub(crate) fn wake_at(&self) -> Instant {
        self.last_fire
            + std::time::Duration::from_millis(u64::from(self.period_ms)).max(TIMER_FLOOR)
    }
}

/// One POSIX file descriptor the plugin asked the host to watch
/// (`CLAP_EXT_POSIX_FD_SUPPORT`).
#[derive(Debug)]
pub struct PosixFdEntry {
    /// The descriptor to poll. Owned by the plugin — the host watches it but
    /// must not close it.
    pub fd: i32,
    /// Raw `clap_posix_fd_flags` bits naming which events to watch for.
    pub flags: u32,
}

/// Notifications about values the host caches but the plugin owns. Each is
/// latched until the matching `take_*` drains it; the host then re-reads the
/// value from the plugin.
#[derive(Debug)]
pub struct ProcessingState {
    /// The plugin's saveable state changed, so a state blob written earlier no
    /// longer matches the instance.
    pub state_dirty: AtomicBool,
}

impl ProcessingState {
    fn new() -> Self {
        Self {
            state_dirty: AtomicBool::new(false),
        }
    }
}

/// Editor-window requests the plugin raised through `CLAP_EXT_GUI`.
#[derive(Debug)]
pub struct GuiState {
    /// The plugin reported its editor closed. Says nothing about whether the
    /// window survived — [`window_destroyed`](Self::window_destroyed) carries
    /// that, and the two differ in whether `gui.hide` still has a target.
    pub closed: AtomicBool,
    /// Set when the plugin reported `gui.closed(was_destroyed = true)` — its
    /// **window** is gone. `close_editor` reads this to skip `gui.hide`, which
    /// has no window left to act on, and still calls `gui.destroy`: `ext/gui.h`
    /// requires the host call `destroy()` to acknowledge the destruction, and
    /// `destroy` releases what `create` allocated rather than the window.
    /// Latched until the next editor is opened.
    pub window_destroyed: AtomicBool,
    /// The plugin's resize constraints (aspect ratio, step size) changed, so
    /// cached hints must be re-read before the next user resize.
    pub resize_hints_changed: AtomicBool,
    /// Width in pixels the plugin asked to be resized to. Only meaningful
    /// while [`request_resize_pending`](Self::request_resize_pending) is set.
    pub request_resize_width: AtomicU32,
    /// Height in pixels the plugin asked to be resized to. Only meaningful
    /// while [`request_resize_pending`](Self::request_resize_pending) is set.
    pub request_resize_height: AtomicU32,
    /// Distinguishes a fresh request from stale width/height values.
    pub request_resize_pending: AtomicBool,
}

impl GuiState {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            window_destroyed: AtomicBool::new(false),
            resize_hints_changed: AtomicBool::new(false),
            request_resize_width: AtomicU32::new(0),
            request_resize_height: AtomicU32::new(0),
            request_resize_pending: AtomicBool::new(false),
        }
    }
}

/// Parameter-side requests from `CLAP_EXT_PARAMS`. (A rescan is a bit of
/// [`HostState::requests`].)
#[derive(Debug)]
pub struct ParamState {
    /// The plugin asked the host to flush pending parameter changes.
    pub flush_requested: AtomicBool,
}

impl ParamState {
    fn new() -> Self {
        Self {
            flush_requested: AtomicBool::new(false),
        }
    }
}

/// Note-side change notifications, from the note-ports extension.
#[derive(Debug)]
pub struct NoteState {
    /// The note-port layout changed and must be re-enumerated.
    pub ports_changed: AtomicBool,
}

impl NoteState {
    fn new() -> Self {
        Self {
            ports_changed: AtomicBool::new(false),
        }
    }
}

/// One routed `clap.log` line: the CLAP severity the plugin passed and the
/// decoded message.
///
/// `severity` stays a bare `clap_log_severity` (`i32`): CLAP leaves room for
/// severities a host does not recognise, and an enum would have to bucket those
/// away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The raw `clap_log_severity` the plugin passed, unmapped.
    pub severity: i32,
    /// The decoded message text.
    pub message: String,
}

/// The last [`LOG_CAPACITY`] lines the plugin logged.
///
/// Bounded on purpose: a plugin can log per audio block, so an unbounded buffer
/// behind a host that never drains is a leak, not a diagnostic. Oldest lines go
/// first, and `dropped` counts them so a consumer does not read a truncated
/// history as a complete one.
#[derive(Debug)]
pub struct LogState {
    pub(crate) records: Mutex<std::collections::VecDeque<LogRecord>>,
    pub(crate) dropped: AtomicU32,
}

/// How many log lines the host retains before dropping the oldest.
pub const LOG_CAPACITY: usize = 256;

impl LogState {
    fn new() -> Self {
        Self {
            records: Mutex::new(std::collections::VecDeque::new()),
            dropped: AtomicU32::new(0),
        }
    }

    /// Counts a line refused because it arrived on the audio thread (see
    /// `host_log`). Shares the capacity-eviction counter — both are "a line the
    /// host did not keep", and no caller can act differently on the two.
    ///
    /// Audio-thread safe: one relaxed increment, no lock, no allocation.
    pub(crate) fn note_audio_thread_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one routed line, evicting the oldest if at capacity.
    ///
    /// **Not audio-thread safe:** takes a `Mutex` that `drain_log` holds across
    /// a copy, so an audio-thread call risks a priority-inversion stall.
    /// `host_log` diverts those to
    /// [`note_audio_thread_drop`](Self::note_audio_thread_drop) first.
    ///
    /// A poisoned lock is recovered, not propagated — a panic here would take a
    /// caller down over a diagnostic.
    pub(crate) fn push(&self, severity: i32, message: String) {
        let mut records = self.records.lock().unwrap_or_else(|p| p.into_inner());
        if records.len() == LOG_CAPACITY {
            records.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        records.push_back(LogRecord { severity, message });
    }
}

/// Periodic timers the plugin registered through `CLAP_EXT_TIMER_SUPPORT`.
///
/// The host is responsible for firing them; nothing here drives a clock.
pub struct TimerState {
    pub(crate) timers: Mutex<Vec<TimerEntry>>,
    pub(crate) next_id: AtomicU32,
    /// What `next_due` counts from.
    epoch: Instant,
    /// The earliest moment a timer can be due, in nanoseconds after `epoch`,
    /// or [`Self::NONE`] with no timer. Never later than the true earliest: a
    /// registration lowers it, an unregistration leaves it (a stale early
    /// value costs one pass, which recomputes it), and a pass over the timers
    /// sets it exactly. It lets the per-block pass skip the timers lock.
    next_due: AtomicU64,
}

impl std::fmt::Debug for TimerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerState")
            .field("next_id", &self.next_id)
            .field("epoch", &self.epoch)
            .field("next_due", &self.next_due)
            .finish_non_exhaustive()
    }
}

impl TimerState {
    /// [`Self::next_due_nanos`] when no timer is registered.
    pub(crate) const NONE: u64 = u64::MAX;

    pub(crate) fn new() -> Self {
        Self {
            timers: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
            epoch: Instant::now(),
            next_due: AtomicU64::new(Self::NONE),
        }
    }

    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    fn nanos(&self, at: Instant) -> u64 {
        // Saturating, and one short of `NONE`, which a real deadline never is.
        at.saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u128::from(Self::NONE - 1)) as u64
    }

    /// Lowers the cached deadline to `due`, for a timer just registered.
    pub(crate) fn note_due(&self, due: Instant) {
        self.next_due.fetch_min(self.nanos(due), Ordering::AcqRel);
    }

    /// Sets the cached deadline exactly: `None` when no timer is left.
    pub(crate) fn set_next_due(&self, due: Option<Instant>) {
        let nanos = due.map_or(Self::NONE, |d| self.nanos(d));
        self.next_due.store(nanos, Ordering::Release);
    }

    /// The cached deadline in nanoseconds after [`Self::epoch`]; see `next_due`.
    pub(crate) fn next_due_nanos(&self) -> u64 {
        self.next_due.load(Ordering::Acquire)
    }
}

/// File descriptors the plugin asked the host to watch
/// (`CLAP_EXT_POSIX_FD_SUPPORT`).
#[derive(Debug)]
pub struct PosixFdState {
    /// Descriptors the plugin asked the host to watch, in registration order.
    pub entries: Mutex<Vec<PosixFdEntry>>,
}

impl PosixFdState {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

/// How many transport requests wait between two takes. A plugin asks from the
/// main thread, one request per user action, so a take every pass leaves the
/// queue near empty; past this the newest are refused, which keeps the
/// queue's memory fixed against a plugin that asks in a loop.
pub(crate) const TRANSPORT_QUEUE: usize = 64;

/// What the plugin asked the transport to do (`CLAP_EXT_TRANSPORT_CONTROL`),
/// in the order it asked. The extension is `[main-thread]`, and the FFI crate
/// refuses a call from anywhere else, so the queue has one writer and one
/// reader, both the main thread: the lock is never contended and the audio
/// thread never reaches it.
#[derive(Debug)]
pub struct TransportState {
    queue: Mutex<Vec<TransportRequest>>,
}

impl TransportState {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::with_capacity(TRANSPORT_QUEUE)),
        }
    }

    /// Queues `request`; `false` when it was refused, because
    /// [`TRANSPORT_QUEUE`] are waiting or the queue's lock is poisoned.
    #[must_use]
    pub(crate) fn push(&self, request: TransportRequest) -> bool {
        let Ok(mut queue) = self.queue.lock() else {
            return false;
        };
        if queue.len() < TRANSPORT_QUEUE {
            queue.push(request);
            true
        } else {
            false
        }
    }

    /// Moves what is queued to `out`, oldest first.
    pub(crate) fn drain_into(&self, out: &mut Vec<TransportRequest>) {
        if let Ok(mut queue) = self.queue.lock() {
            out.append(&mut queue);
        }
    }
}

/// What the host says of the plugin's track (`CLAP_EXT_TRACK_INFO`), read by
/// the plugin's `get`, which is `[main-thread]`.
#[derive(Debug)]
pub struct TrackState {
    pub(crate) info: Mutex<Option<TrackInfo>>,
}

impl TrackState {
    fn new() -> Self {
        Self {
            info: Mutex::new(None),
        }
    }
}

/// Shared state for host↔plugin communication via atomic flags.
#[derive(Debug)]
pub struct HostState {
    /// Which thread plays which role, and the audio claim: shared with the
    /// FFI crate's plugin handle, which takes the claim around every
    /// audio-side entry.
    pub(crate) roles: Arc<ThreadRoles>,
    /// What the plugin asked of the host, by any callback on any thread: a
    /// restart, a main-thread callback, a latency or tail change, an
    /// audio-ports or parameter rescan. The main thread takes it once per
    /// pass ([`Control::take_requests`](tutti_plugin_types::Control::take_requests)).
    pub requests: RequestWord,
    /// What `request_process` raises: the host's wake for a plugin it parked.
    /// Set once, by [`Control::bind_wake`](tutti_plugin_types::Control::bind_wake);
    /// a request before then has nobody to wake.
    pub process_wake: OnceLock<ProcessWake>,
    /// State-dirty notification.
    pub processing: ProcessingState,
    /// Editor close and resize requests.
    pub gui: GuiState,
    /// Parameter rescan and flush requests.
    pub params: ParamState,
    /// Note-port change notifications.
    pub notes: NoteState,
    /// The bounded ring of routed `clap.log` lines.
    pub log: LogState,
    /// Timers the plugin registered for the host to fire.
    pub timer: TimerState,
    /// Descriptors the plugin registered for the host to watch.
    pub posix_fds: PosixFdState,
    /// Transport requests the plugin made, waiting for the host's pass.
    pub transport: TransportState,
    /// What the host says of the plugin's track.
    pub track: TrackState,
}

impl HostState {
    /// Builds an unclaimed `HostState`. The thread `main` proves is taken as
    /// the `[main-thread]` role for the instance's whole life; it is recorded
    /// once, in the instance's [`ThreadRoles`], which every answer reads.
    #[must_use]
    pub fn new(main: &MainThread) -> Self {
        Self {
            roles: Arc::new(ThreadRoles::new(main)),
            requests: RequestWord::new(),
            process_wake: OnceLock::new(),
            processing: ProcessingState::new(),
            gui: GuiState::new(),
            params: ParamState::new(),
            notes: NoteState::new(),
            log: LogState::new(),
            timer: TimerState::new(),
            posix_fds: PosixFdState::new(),
            transport: TransportState::new(),
            track: TrackState::new(),
        }
    }

    /// Reads a latch and clears it in one atomic step, returning whether it was
    /// set. Draining is the point: two consecutive takes of one unrepeated
    /// request answer `true` then `false`.
    pub fn take(&self, flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::AcqRel)
    }

    /// Claims the `[audio-thread]` role for the calling OS thread, blocking
    /// until any other claim has been released.
    ///
    /// Wrap **every** `[audio-thread]` plugin call in this: `process`,
    /// `start_processing`, `stop_processing`, and an active `params.flush`.
    /// The returned guard releases the role on drop.
    ///
    /// A poisoned lock is recovered rather than propagated: the data is `()`,
    /// so there is no invariant a panicking claimant could have broken, and
    /// panicking here would take down the audio thread.
    ///
    /// Not re-entrant — the mutex is not recursive, so a claim held on this
    /// thread must be threaded into the inner call, not re-taken. That is why
    /// `ensure_processing` / `stop_processing_claimed` take `&AudioClaim`.
    pub fn claim_audio_thread(&self) -> AudioClaim<'_> {
        self.roles.claim()
    }

    /// Whether the calling thread is currently acting as the audio thread.
    pub fn is_audio_thread(&self) -> bool {
        self.roles.is_audio_thread()
    }

    /// Whether the calling thread is currently acting as the main thread.
    ///
    /// **Exclusive with [`is_audio_thread`](Self::is_audio_thread)**: the
    /// spec lets a host mark the OS main thread as the audio thread, but the
    /// two symbolic roles are alternatives, not simultaneous identities. While
    /// an [`AudioClaim`] is held on this thread, this answers `false`,
    /// so a plugin asserting `!is_main_thread()` inside an `[audio-thread]`
    /// call is not silently defeated by a host that claims to be both.
    ///
    /// Note the exclusion is per-thread, not global: `is_audio_thread` compares
    /// against *this* thread, so a claim held by the real audio thread does not
    /// demote a concurrent main-thread caller. Only the main thread's own claim
    /// — the OS main thread running an `[audio-thread]` call — makes it answer
    /// `false` here.
    ///
    /// One consequence is worth knowing when reading plugin bug reports: an
    /// active `flush_params` claims the audio-thread role on whatever thread
    /// calls it, so if the *main* thread drives it, `[main-thread]` callbacks a
    /// plugin makes from inside `flush` (`request_callback`, `request_restart`,
    /// `params.rescan`) see `is_main_thread() == false`. That is the spec's
    /// own framing — `flush` on an active instance *is* `[audio-thread]` — but
    /// a plugin asserting `is_main_thread()` there will trip. Routing param
    /// changes through the next `process` block avoids it entirely, which is
    /// what `flush_params`' own docs already recommend.
    pub fn is_main_thread(&self) -> bool {
        self.roles.is_main_thread()
    }
}
