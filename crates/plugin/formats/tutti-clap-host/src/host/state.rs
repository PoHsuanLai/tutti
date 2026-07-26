use crate::types::{TrackInfo, TransportRequest, TuningInfo, UndoChange};
use arc_swap::ArcSwapOption;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
    /// Current audio-thread identity. Read from the audio thread on every
    /// CLAP callback that queries `is_audio_thread`, and written at
    /// start/stop of processing. Lock-free ([`ArcSwapOption`]: a single
    /// atomic pointer load on the read side).
    ///
    /// Written only through [`HostState::claim_audio_thread`], which also
    /// takes [`audio_thread_lock`](Self::audio_thread_lock) — the two are one
    /// unit, so an OS thread is never published here without also holding the
    /// `[audio-thread]` concurrency guard. It is `None` whenever no claim is
    /// outstanding, so a thread is the audio thread only *during* an
    /// `[audio-thread]` call.
    pub audio_thread_id: ArcSwapOption<ThreadId>,
    /// Cached `Arc<ThreadId>` for the thread that most recently held the
    /// claim, so a repeat claim by the same OS thread republishes without
    /// hitting the allocator. Purely an RT optimisation
    /// (`clap_process_no_alloc` pins the no-allocation property); it is only
    /// ever read/written under [`audio_thread_lock`](Self::audio_thread_lock).
    audio_thread_cache: ArcSwapOption<ThreadId>,
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
    pub timer: TimerState,
    pub transport: TransportState,
    pub remote_controls: RemoteControlState,
    pub resources: ResourceState,
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
        // Hand the Arc back to the cache instead of freeing it, so the next
        // claim by this same thread is allocation-free.
        let released = self.state.audio_thread_id.swap(None);
        if released.is_some() {
            self.state.audio_thread_cache.store(released);
        }
    }
}

impl HostState {
    pub fn new() -> Self {
        Self {
            main_thread_id: std::thread::current().id(),
            audio_thread_id: ArcSwapOption::from(None),
            audio_thread_cache: ArcSwapOption::from(None),
            audio_thread_lock: Mutex::new(()),
            lifecycle: LifecycleFlags::new(),
            processing: ProcessingState::new(),
            gui: GuiState::new(),
            params: ParamState::new(),
            audio_ports: AudioPortState::new(),
            notes: NoteState::new(),
            undo: UndoState::new(),
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
        let current = std::thread::current().id();
        // RT: the steady state (same audio thread claiming every block) must
        // not touch the allocator — `clap_process_no_alloc` pins this. Reuse
        // the Arc the previous claim handed back whenever it names this same
        // thread; only a genuine thread change mints a new one.
        let cached = self.audio_thread_cache.swap(None);
        let id = match cached {
            Some(arc) if *arc == current => arc,
            _ => Arc::new(current),
        };
        self.audio_thread_id.store(Some(id));
        AudioThreadClaim {
            state: self,
            _guard: guard,
        }
    }

    /// Whether the calling thread is currently acting as the audio thread.
    pub fn is_audio_thread(&self) -> bool {
        self.audio_thread_id
            .load()
            .as_deref()
            .is_some_and(|id| *id == std::thread::current().id())
    }

    /// Whether the calling thread is currently acting as the main thread.
    ///
    /// **Exclusive with [`is_audio_thread`](Self::is_audio_thread)** (C1): the
    /// spec lets a host mark the OS main thread as the audio thread, but the
    /// two symbolic roles are alternatives, not simultaneous identities. While
    /// an [`AudioThreadClaim`] is held on this thread we answer `false` here,
    /// so a plugin asserting `!is_main_thread()` inside a `[audio-thread]`
    /// call is not silently defeated by a host that claims to be both.
    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id && !self.is_audio_thread()
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}
