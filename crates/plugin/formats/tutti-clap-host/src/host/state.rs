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

#[cfg(unix)]
pub struct PosixFdEntry {
    pub fd: i32,
    pub flags: u32,
}

pub struct LifecycleFlags {
    pub restart_requested: AtomicBool,
    pub process_requested: AtomicBool,
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

pub struct ProcessingState {
    pub latency_changed: AtomicBool,
    pub tail_changed: AtomicBool,
    pub state_dirty: AtomicBool,
    pub preset_loaded: AtomicBool,
    pub thread_pool_pending: AtomicU32,
}

impl ProcessingState {
    fn new() -> Self {
        Self {
            latency_changed: AtomicBool::new(false),
            tail_changed: AtomicBool::new(false),
            state_dirty: AtomicBool::new(false),
            preset_loaded: AtomicBool::new(false),
            thread_pool_pending: AtomicU32::new(0),
        }
    }
}

pub struct GuiState {
    pub closed: AtomicBool,
    /// Set when the plugin reported `gui.closed(was_destroyed = true)` — it
    /// already tore its own editor down, so `close_editor` must NOT call
    /// `gui.destroy` again (double-destroy). Latched until the next editor is
    /// opened. (H5)
    pub already_destroyed: AtomicBool,
    pub resize_hints_changed: AtomicBool,
    pub request_resize_width: AtomicU32,
    pub request_resize_height: AtomicU32,
    /// Distinguishes a fresh request from stale width/height values.
    pub request_resize_pending: AtomicBool,
}

impl GuiState {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            already_destroyed: AtomicBool::new(false),
            resize_hints_changed: AtomicBool::new(false),
            request_resize_width: AtomicU32::new(0),
            request_resize_height: AtomicU32::new(0),
            request_resize_pending: AtomicBool::new(false),
        }
    }
}

pub struct ParamState {
    pub rescan_requested: AtomicBool,
    /// Accumulated `clap_param_rescan_flags` from every `params.rescan` call
    /// since the last poll (OR-combined). Distinguishes RESCAN_ALL — which the
    /// spec requires the host handle only while the plugin is deactivated —
    /// from value-only (RESCAN_VALUES) rescans that can be applied live.
    pub rescan_flags: AtomicU32,
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

pub struct AudioPortState {
    pub changed: AtomicBool,
    pub config_changed: AtomicBool,
    pub ambisonic_changed: AtomicBool,
    pub surround_changed: AtomicBool,
}

impl AudioPortState {
    fn new() -> Self {
        Self {
            changed: AtomicBool::new(false),
            config_changed: AtomicBool::new(false),
            ambisonic_changed: AtomicBool::new(false),
            surround_changed: AtomicBool::new(false),
        }
    }
}

pub struct NoteState {
    pub ports_changed: AtomicBool,
    pub names_changed: AtomicBool,
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

pub struct UndoState {
    pub in_progress: AtomicBool,
    pub requested: AtomicBool,
    pub redo_requested: AtomicBool,
    pub wants_context: AtomicBool,
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
/// `severity` stays a bare `clap_log_severity` (`i32`) rather than an enum:
/// this is a C ABI value the plugin chose, and CLAP explicitly leaves room for
/// severities a host does not recognise. Widening it into a host enum would
/// have to invent a bucket for those, which is exactly the information the
/// consumer wants preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub severity: i32,
    pub message: String,
}

/// The last [`LOG_CAPACITY`] lines the plugin logged.
///
/// Bounded on purpose. A misbehaving plugin can log per audio block; an
/// unbounded `Vec` behind a host that never drains would grow without limit,
/// which is a leak in a long session rather than a diagnostic. Oldest lines are
/// dropped, and `dropped` counts them so a consumer can see that it happened
/// instead of silently reading a truncated history.
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

    /// Count a line refused because it arrived on the audio thread.
    ///
    /// Folded into the same counter as a capacity eviction: from the
    /// consumer's side both are "a line the host did not keep", and splitting
    /// them would imply a caller can act differently on the two, which it
    /// cannot. See `host_log` for why such a line is refused rather than
    /// recorded.
    ///
    /// Audio-thread safe: one relaxed atomic increment, no lock, no
    /// allocation.
    pub(crate) fn note_audio_thread_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one routed line, evicting the oldest if at capacity.
    ///
    /// **Not audio-thread safe.** This takes a `Mutex` that `drain_log` holds
    /// across a copy, so calling it from the audio thread risks a
    /// priority-inversion stall; `host_log` diverts audio-thread lines to
    /// [`note_audio_thread_drop`](Self::note_audio_thread_drop) before
    /// reaching here.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic here would
    /// take a caller down over a diagnostic.
    pub(crate) fn push(&self, severity: i32, message: String) {
        let mut records = self.records.lock().unwrap_or_else(|p| p.into_inner());
        if records.len() == LOG_CAPACITY {
            records.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        records.push_back(LogRecord { severity, message });
    }
}

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

pub struct RemoteControlState {
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

pub struct ResourceState {
    pub(crate) track_info: Mutex<Option<TrackInfo>>,
    pub(crate) event_spaces: Mutex<HashMap<String, u16>>,
    pub(crate) next_event_space: AtomicU16,
    pub(crate) tuning_infos: Mutex<Vec<TuningInfo>>,
    pub(crate) directory_shared: Mutex<Option<std::path::PathBuf>>,
    pub(crate) directory_private: Mutex<Option<std::path::PathBuf>>,
    pub triggers_rescan_requested: AtomicBool,
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
    pub main_thread_id: ThreadId,
    /// Current audio-thread identity, as a hash of the claiming [`ThreadId`].
    /// [`NO_AUDIO_THREAD`] means no claim is outstanding, so a thread holds the
    /// role only *during* an `[audio-thread]` call.
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
    /// the hash is the portable way to fit it in an atomic. Collisions are
    /// possible in principle; see [`thread_id_hash`] for why that is sound
    /// here.
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
    pub lifecycle: LifecycleFlags,
    pub processing: ProcessingState,
    pub gui: GuiState,
    pub params: ParamState,
    pub audio_ports: AudioPortState,
    pub notes: NoteState,
    pub undo: UndoState,
    pub log: LogState,
    pub timer: TimerState,
    pub transport: TransportState,
    pub remote_controls: RemoteControlState,
    pub resources: ResourceState,
}

/// RAII claim on the `[audio-thread]` role for one plugin instance (C1/C2).
///
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
    /// an [`AudioThreadClaim`] is held on this thread we answer `false` here,
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
