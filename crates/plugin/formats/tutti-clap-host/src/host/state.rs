use crate::types::{TrackInfo, TransportRequest, TuningInfo, UndoChange};
use arc_swap::ArcSwapOption;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Mutex;
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
            resize_hints_changed: AtomicBool::new(false),
            request_resize_width: AtomicU32::new(0),
            request_resize_height: AtomicU32::new(0),
            request_resize_pending: AtomicBool::new(false),
        }
    }
}

pub struct ParamState {
    pub rescan_requested: AtomicBool,
    pub flush_requested: AtomicBool,
}

impl ParamState {
    fn new() -> Self {
        Self {
            rescan_requested: AtomicBool::new(false),
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
    pub audio_thread_id: ArcSwapOption<ThreadId>,
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

impl HostState {
    pub fn new() -> Self {
        Self {
            main_thread_id: std::thread::current().id(),
            audio_thread_id: ArcSwapOption::from(None),
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
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}
