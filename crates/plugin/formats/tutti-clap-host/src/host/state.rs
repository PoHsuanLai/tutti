//! Shared host↔plugin state: the latches a plugin's callbacks set and the
//! host's poll methods drain, plus the `[audio-thread]` role guard.
//!
//! A plugin calls host callbacks from its own threads at times the host does
//! not choose, so almost everything here is an atomic flag *set* by a callback
//! and *cleared* by the matching `poll_*` on the main thread. The flags are
//! grouped into one struct per CLAP extension so a caller can see which
//! extension a request came from.

use crate::types::{TrackInfo, TransportRequest, TuningInfo, UndoChange};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::Instant;

pub(crate) struct TimerEntry {
    pub id: u32,
    pub period_ms: u32,
    pub last_fire: Instant,
}

/// One POSIX file descriptor the plugin asked the host to watch
/// (`CLAP_EXT_POSIX_FD_SUPPORT`).
#[cfg(unix)]
pub struct PosixFdEntry {
    /// The descriptor to poll. Owned by the plugin — the host watches it but
    /// must not close it.
    pub fd: i32,
    /// Raw `clap_posix_fd_flags` bits naming which events to watch for.
    pub flags: u32,
}

/// Plugin requests to change its own lifecycle, from `clap_host`'s core
/// callbacks. Each is latched until the matching `poll_*` drains it.
pub struct LifecycleFlags {
    /// The plugin asked to be deactivated and reactivated, typically because
    /// its port layout or sample-rate needs changed.
    pub restart_requested: AtomicBool,
    /// The plugin asked the host to resume calling `process`, having gone
    /// idle earlier.
    pub process_requested: AtomicBool,
    /// The plugin asked for a main-thread callback (`on_main_thread`).
    pub callback_requested: AtomicBool,
}

impl LifecycleFlags {
    fn new() -> Self {
        Self {
            restart_requested: AtomicBool::new(false),
            process_requested: AtomicBool::new(false),
            callback_requested: AtomicBool::new(false),
        }
    }
}

/// Notifications about values the host caches but the plugin owns. Each is
/// latched until the matching `poll_*` drains it; the host then re-reads the
/// value from the plugin.
pub struct ProcessingState {
    /// The plugin's reported latency changed, so PDC compensation is stale.
    pub latency_changed: AtomicBool,
    /// The plugin's reported tail length changed.
    pub tail_changed: AtomicBool,
    /// The plugin's saveable state changed, so a state blob written earlier no
    /// longer matches the instance.
    pub state_dirty: AtomicBool,
    /// The plugin loaded a preset of its own accord.
    pub preset_loaded: AtomicBool,
}

impl ProcessingState {
    fn new() -> Self {
        Self {
            latency_changed: AtomicBool::new(false),
            tail_changed: AtomicBool::new(false),
            state_dirty: AtomicBool::new(false),
            preset_loaded: AtomicBool::new(false),
        }
    }
}

/// Editor-window requests the plugin raised through `CLAP_EXT_GUI`.
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

/// Parameter-side requests from `CLAP_EXT_PARAMS`.
pub struct ParamState {
    /// A `params.rescan` arrived. Tracked apart from
    /// [`rescan_flags`](Self::rescan_flags) because a plugin may legally
    /// rescan with no bits set, and "asked for nothing" must stay distinct
    /// from "never asked".
    pub rescan_requested: AtomicBool,
    /// Accumulated `clap_param_rescan_flags` from every `params.rescan` call
    /// since the last poll (OR-combined). Distinguishes RESCAN_ALL — which the
    /// spec requires the host handle only while the plugin is deactivated —
    /// from value-only (RESCAN_VALUES) rescans that can be applied live.
    pub rescan_flags: AtomicU32,
    /// The plugin asked the host to flush pending parameter changes.
    pub flush_requested: AtomicBool,
}

impl ParamState {
    fn new() -> Self {
        Self {
            rescan_requested: AtomicBool::new(false),
            rescan_flags: AtomicU32::new(0),
            flush_requested: AtomicBool::new(false),
        }
    }
}

/// Port-topology change notifications, from the audio-ports family of
/// extensions.
pub struct AudioPortState {
    /// An `audio-ports.rescan` arrived. Tracked apart from
    /// [`rescan_flags`](Self::rescan_flags) for the same reason as on
    /// [`ParamState`]: a bit-less rescan is still a rescan.
    pub changed: AtomicBool,
    /// Accumulated `clap_audio_ports_rescan_flags` from every
    /// `audio-ports.rescan` call since the last poll (OR-combined).
    ///
    /// Five of the six flags are `[!active]` in the spec, so a consumer has to
    /// tell a live-applicable name change from one that requires
    /// deactivate→re-enumerate→re-activate. OR so multiple rescans between
    /// polls don't lose bits.
    pub rescan_flags: AtomicU32,
    /// The set of selectable port configurations changed
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG`).
    pub config_changed: AtomicBool,
    /// The plugin's ambisonic ordering/normalization changed.
    pub ambisonic_changed: AtomicBool,
    /// The plugin's surround channel map changed.
    pub surround_changed: AtomicBool,
}

impl AudioPortState {
    fn new() -> Self {
        Self {
            changed: AtomicBool::new(false),
            rescan_flags: AtomicU32::new(0),
            config_changed: AtomicBool::new(false),
            ambisonic_changed: AtomicBool::new(false),
            surround_changed: AtomicBool::new(false),
        }
    }
}

/// Note-side change notifications, from the note-ports, note-names and
/// voice-info extensions.
pub struct NoteState {
    /// The note-port layout changed and must be re-enumerated.
    pub ports_changed: AtomicBool,
    /// The plugin's per-key names changed (a drum kit swapping its mapping).
    pub names_changed: AtomicBool,
    /// The plugin's reported voice count or capacity changed.
    pub voice_info_changed: AtomicBool,
}

impl NoteState {
    fn new() -> Self {
        Self {
            ports_changed: AtomicBool::new(false),
            names_changed: AtomicBool::new(false),
            voice_info_changed: AtomicBool::new(false),
        }
    }
}

/// Undo/redo traffic from `CLAP_EXT_UNDO`, in both directions: steps the
/// plugin recorded, and requests it made of the host's undo stack.
pub struct UndoState {
    /// A change the plugin opened with `begin_change` has neither been
    /// committed via `change_made` nor withdrawn via `cancel_change`. Not a
    /// latch — it tracks a span, and both endpoints clear it.
    pub in_progress: AtomicBool,
    /// The plugin asked the host to undo one step of the *host's* stack.
    pub requested: AtomicBool,
    /// The plugin asked the host to redo one step of the *host's* stack.
    pub redo_requested: AtomicBool,
    /// The plugin subscribed to undo-context updates, so the host should keep
    /// it informed about what the next undo/redo step would be.
    pub wants_context: AtomicBool,
    /// Steps the plugin has committed, oldest first. Unbounded — a host that
    /// never drains it grows it without limit.
    pub changes: Mutex<Vec<UndoChange>>,
}

impl UndoState {
    fn new() -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            requested: AtomicBool::new(false),
            redo_requested: AtomicBool::new(false),
            wants_context: AtomicBool::new(false),
            changes: Mutex::new(Vec::new()),
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

    /// Count a line refused because it arrived on the audio thread (see
    /// `host_log`). Shares the capacity-eviction counter — both are "a line the
    /// host did not keep", and no caller can act differently on the two.
    ///
    /// Audio-thread safe: one relaxed increment, no lock, no allocation.
    pub(crate) fn note_audio_thread_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one routed line, evicting the oldest if at capacity.
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
}

impl TimerState {
    fn new() -> Self {
        Self {
            timers: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
        }
    }
}

/// Transport requests the plugin issued via `CLAP_EXT_TRANSPORT_CONTROL`,
/// queued in arrival order for the host to drain and act on (or ignore).
pub struct TransportState {
    pub(crate) requests: Mutex<Vec<TransportRequest>>,
}

impl TransportState {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

/// Remote-control page state from `CLAP_EXT_REMOTE_CONTROLS`.
pub struct RemoteControlState {
    /// The plugin's set of remote-control pages changed and must be
    /// re-enumerated.
    pub changed: AtomicBool,
    pub(crate) suggested_page: AtomicU32,
}

impl RemoteControlState {
    fn new() -> Self {
        Self {
            changed: AtomicBool::new(false),
            suggested_page: AtomicU32::new(u32::MAX),
        }
    }
}

/// Host-owned resources a plugin can query: track metadata, registered event
/// spaces, tuning tables, storage directories and watched descriptors.
///
/// Unlike the latch groups above, most of this is state the *host* publishes
/// for the plugin to read rather than a request the plugin made.
pub struct ResourceState {
    pub(crate) track_info: Mutex<Option<TrackInfo>>,
    pub(crate) event_spaces: Mutex<HashMap<String, u16>>,
    pub(crate) next_event_space: AtomicU16,
    pub(crate) tuning_infos: Mutex<Vec<TuningInfo>>,
    pub(crate) directory_shared: Mutex<Option<std::path::PathBuf>>,
    pub(crate) directory_private: Mutex<Option<std::path::PathBuf>>,
    /// The plugin's trigger list changed and must be re-enumerated.
    pub triggers_rescan_requested: AtomicBool,
    /// Descriptors the plugin asked the host to watch, in registration order.
    #[cfg(unix)]
    pub posix_fds: Mutex<Vec<PosixFdEntry>>,
}

impl ResourceState {
    fn new() -> Self {
        Self {
            track_info: Mutex::new(None),
            event_spaces: Mutex::new(HashMap::new()),
            next_event_space: AtomicU16::new(512),
            tuning_infos: Mutex::new(Vec::new()),
            directory_shared: Mutex::new(None),
            directory_private: Mutex::new(None),
            triggers_rescan_requested: AtomicBool::new(false),
            #[cfg(unix)]
            posix_fds: Mutex::new(Vec::new()),
        }
    }
}

/// Shared state for host↔plugin communication via atomic flags.
pub struct HostState {
    /// The thread that constructed this `HostState`, taken as the
    /// `[main-thread]` role for the instance's whole life. Fixed at
    /// construction, so a `HostState` must be built on the thread that will
    /// drive the plugin's main-thread calls.
    pub main_thread_id: ThreadId,
    /// Current audio-thread identity, as a hash of the claiming [`ThreadId`].
    /// Zero means no claim is outstanding, so a thread holds the role only
    /// *during* an `[audio-thread]` call.
    ///
    /// Read from the audio thread on every CLAP callback that queries
    /// `is_audio_thread`, and written on entry to and exit from every
    /// `[audio-thread]` call — so this is a per-block RT write, not a
    /// start/stop-of-processing one.
    ///
    /// **A plain atomic, deliberately.** This was an `ArcSwapOption<ThreadId>`
    /// with a one-slot `Arc` cache to avoid allocating per block. Both halves
    /// were wrong on the audio thread:
    ///
    /// - `ArcSwapOption::store` is `drop(self.swap(val))`, and `swap` calls
    ///   `wait_for_readers` before returning the old `Arc`. So publishing a
    ///   claim could *block* on the audio thread, and releasing one could run
    ///   the retired `Arc`'s deallocation there. That is the hazard
    ///   `RtPublish` exists to prevent, arrived at through a different door.
    /// - The cache held exactly one slot, so it only helped while the same
    ///   thread claimed repeatedly. A GUI thread calling `flush_params` between
    ///   two audio blocks evicts it, and the audio thread allocates again on
    ///   its next block — precisely the interleaving a DAW produces when a user
    ///   touches a control during playback.
    ///
    /// Hashing sidesteps both: a `u64` needs no allocation, no retirement, and
    /// no reader coordination. `ThreadId` is opaque (`as_u64` is unstable), so
    /// the hash is the portable way to fit it in an atomic.
    ///
    /// Collisions are possible in principle, and are sound here: the property
    /// CLAP requires — one OS thread inside an `[audio-thread]` call at a time
    /// — is enforced by [`audio_thread_lock`](Self::audio_thread_lock), not by
    /// this value. A collision could mislead a plugin's own thread-check
    /// assertion, never admit a second thread.
    pub audio_thread_id: AtomicU64,
    /// The `[audio-thread]` concurrency guard (C1/C2).
    ///
    /// CLAP defines the audio-thread as a *symbolic* thread: "the host may
    /// mark any OS thread, including the main-thread, as the audio-thread, as
    /// long as it can guarantee that only one OS thread is the audio-thread at
    /// a time in a plugin instance. The audio-thread can be seen as a
    /// concurrency guard for all functions marked with [audio-thread]"
    /// (`clap/ext/thread-check.h`). This mutex *is* that guard: every
    /// `[audio-thread]` entry point (`process`, `start_processing`,
    /// `stop_processing`, an active `params.flush`) holds it for the duration
    /// of the plugin call, so those calls can never overlap even when driven
    /// from different OS threads.
    ///
    /// It is uncontended in the steady state (one audio thread, no setup
    /// traffic), so the RT path pays an uncontended lock/unlock and never
    /// blocks. Setup-time callers (`set_sample_rate`, `set_max_block_size`,
    /// `deactivate`, `Drop`) block on it, which is exactly the intended
    /// serialization.
    pub audio_thread_lock: Mutex<()>,
    /// Restart / resume-processing / main-thread-callback requests.
    pub lifecycle: LifecycleFlags,
    /// Latency, tail, state-dirty and preset-loaded notifications.
    pub processing: ProcessingState,
    /// Editor close and resize requests.
    pub gui: GuiState,
    /// Parameter rescan and flush requests.
    pub params: ParamState,
    /// Audio-port topology change notifications.
    pub audio_ports: AudioPortState,
    /// Note-port, note-name and voice-info change notifications.
    pub notes: NoteState,
    /// Undo steps the plugin recorded and undo/redo it requested.
    pub undo: UndoState,
    /// The bounded ring of routed `clap.log` lines.
    pub log: LogState,
    /// Timers the plugin registered for the host to fire.
    pub timer: TimerState,
    /// Queued transport requests awaiting a drain.
    pub transport: TransportState,
    /// Remote-control page change notification and suggested page.
    pub remote_controls: RemoteControlState,
    /// Host-published resources the plugin may read.
    pub resources: ResourceState,
}

/// Sentinel for "no thread currently holds the `[audio-thread]` role".
///
/// Zero because that is what a freshly constructed `AtomicU64` holds, so a
/// `HostState` starts unclaimed without an explicit initialiser. A real thread
/// hashing to 0 would be indistinguishable from "unclaimed" — see
/// [`thread_id_hash`], which folds that case away.
const NO_AUDIO_THREAD: u64 = 0;

/// A [`ThreadId`] as a `u64`, so the audio-thread identity fits in one atomic.
///
/// `ThreadId` is deliberately opaque and its `as_u64` is unstable, so hashing is
/// the portable route. `DefaultHasher` is not stable across releases, which does
/// not matter here: the value never leaves the process and is only ever compared
/// against another hash produced by this same function in this same run.
///
/// **On collisions.** Two live threads could in principle hash alike, which
/// would let a non-claiming thread answer `is_audio_thread() == true`. That is
/// tolerable because the property CLAP actually requires — that only one OS
/// thread is inside an `[audio-thread]` call at a time — is enforced by
/// `audio_thread_lock`, not by this value. The hash answers "who am I?" for
/// plugin-facing thread-check queries; the mutex answers "may I enter?". A
/// collision could mislead a plugin's assertion, never admit a second thread.
/// At 64 bits with a handful of threads, it is also not a practical concern.
fn thread_id_hash(id: ThreadId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // Fold the sentinel away so a real thread can never be read as "unclaimed".
    match hasher.finish() {
        NO_AUDIO_THREAD => 1,
        h => h,
    }
}

/// RAII claim on the `[audio-thread]` role for one plugin instance (C1/C2).
///
/// While alive it holds [`HostState::audio_thread_lock`] and has published the
/// claiming OS thread into [`HostState::audio_thread_id`], so:
/// - `is_audio_thread()` answers `true` on this thread and `false` everywhere else;
/// - `is_main_thread()` answers `false` on this thread even when it *is* the
///   OS main thread — the two roles are mutually exclusive, so a plugin
///   asserting `!is_main_thread()` inside `start_processing` sees the truth;
/// - no second OS thread can enter any `[audio-thread]` plugin call meanwhile.
///
/// On drop the identity is cleared, so outside an `[audio-thread]` call no
/// thread claims the role — and the OS main thread goes back to answering
/// `is_main_thread() == true`.
///
/// Borrows the [`HostState`], not the instance. Callers that need `&mut self`
/// while the claim is alive clone the `Arc<HostState>` into a local first and
/// claim off that local, so the borrow does not reach back into `self`.
pub struct AudioThreadClaim<'a> {
    state: &'a HostState,
    // Held for the claim's lifetime; declared last so it releases only after
    // `Drop` has cleared the published identity (fields drop in declaration
    // order, and the `Drop` impl runs before any field drops).
    _guard: MutexGuard<'a, ()>,
}

impl Drop for AudioThreadClaim<'_> {
    fn drop(&mut self) {
        // One relaxed-ordered store of a `u64`. Nothing to free, so this cannot
        // deallocate on the audio thread; `Release` pairs with the `Acquire` in
        // `is_audio_thread`.
        self.state
            .audio_thread_id
            .store(NO_AUDIO_THREAD, Ordering::Release);
    }
}

impl HostState {
    /// Build an unclaimed `HostState`, taking the calling thread as the
    /// `[main-thread]` role for the instance's whole life.
    pub fn new() -> Self {
        Self {
            main_thread_id: std::thread::current().id(),
            audio_thread_id: AtomicU64::new(NO_AUDIO_THREAD),
            audio_thread_lock: Mutex::new(()),
            lifecycle: LifecycleFlags::new(),
            processing: ProcessingState::new(),
            gui: GuiState::new(),
            params: ParamState::new(),
            audio_ports: AudioPortState::new(),
            notes: NoteState::new(),
            undo: UndoState::new(),
            log: LogState::new(),
            timer: TimerState::new(),
            transport: TransportState::new(),
            remote_controls: RemoteControlState::new(),
            resources: ResourceState::new(),
        }
    }

    /// Read a latch and clear it in one atomic step, returning whether it was
    /// set. Draining is the point: two consecutive polls of one unrepeated
    /// request answer `true` then `false`.
    pub fn poll(&self, flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::AcqRel)
    }

    /// Claim the `[audio-thread]` role for the calling OS thread, blocking
    /// until any other claim has been released (C1/C2).
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
    /// `ensure_processing` / `stop_processing_claimed` take `&AudioThreadClaim`.
    pub fn claim_audio_thread(&self) -> AudioThreadClaim<'_> {
        let guard = self
            .audio_thread_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // RT: a hash and one `Release` store. No allocation, no reader
        // coordination, and nothing retired that could be freed here — see the
        // note on `audio_thread_id` for what this replaced and why.
        self.audio_thread_id.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        AudioThreadClaim {
            state: self,
            _guard: guard,
        }
    }

    /// Whether the calling thread is currently acting as the audio thread.
    pub fn is_audio_thread(&self) -> bool {
        let claimed = self.audio_thread_id.load(Ordering::Acquire);
        claimed != NO_AUDIO_THREAD && claimed == thread_id_hash(std::thread::current().id())
    }

    /// Whether the calling thread is currently acting as the main thread.
    ///
    /// **Exclusive with [`is_audio_thread`](Self::is_audio_thread)** (C1): the
    /// spec lets a host mark the OS main thread as the audio thread, but the
    /// two symbolic roles are alternatives, not simultaneous identities. While
    /// an [`AudioThreadClaim`] is held on this thread, this answers `false`,
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
        std::thread::current().id() == self.main_thread_id && !self.is_audio_thread()
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}
