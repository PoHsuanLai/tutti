//! THREADING probe — the plugin side of the host's thread model, timers, log,
//! and the host-callback round trips.
//!
//! Its own module because the threading surface needs its own capture channel,
//! its own extension vtables, and a *command* channel the test drives from
//! outside. `lib.rs` holds only the four call-site hooks this module exports.
//!
//! CLAP's thread model is one answer *per context*, and plugins branch on
//! `clap.thread-check` to pick a locking strategy. So the probe records
//! `(is_main, is_audio)` separately at each [`Site`] the host can call from:
//!
//! | site              | CLAP tag        | expected (is_main, is_audio) |
//! |-------------------|-----------------|------------------------------|
//! | `init`            | `[main-thread]` | (true, false)                |
//! | `activate`        | `[main-thread]` | (true, false)                |
//! | `start_processing`| `[audio-thread]`| (false, true)                |
//! | `process`         | `[audio-thread]`| (false, true)                |
//! | `on_main_thread`  | `[main-thread]` | (true, false)                |
//! | `on_timer`        | `[main-thread]` | (true, false)                |
//!
//! `start_processing` and `process` expect `is_main == false` *even when the
//! host drives them from the OS main thread*, which the test harness does: the
//! two roles are alternatives, so a plugin's own `assert(!is_main_thread())`
//! inside an `[audio-thread]` call has to be able to fail.
//!
//! Every field is a plain atomic rather than a `Mutex`, because the `process`
//! and `start_processing` sites run on the audio thread of the very host being
//! measured.
//!
//! Behaviours that only exist if the plugin *initiates* them — registering a
//! timer, emitting a log line, `request_restart` — are latched via
//! [`tutti_test_plugin_thread_command`] and consumed at the next call site legal
//! for that command, so the test never reaches into the plugin from a thread
//! the CLAP spec does not allow.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use clap_sys::ext::log::{
    clap_host_log, CLAP_EXT_LOG, CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL,
    CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO, CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING,
};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

/// The plugin entry points whose thread identity the probe records. The
/// discriminants are the indices into [`ThreadCapture::sites`], and are part of
/// the `#[repr(C)]` contract with the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Site {
    Init = 0,
    Activate = 1,
    StartProcessing = 2,
    Process = 3,
    OnMainThread = 4,
    OnTimer = 5,
}

/// Number of [`Site`] variants — the length of [`ThreadCapture::sites`].
pub const SITE_COUNT: usize = 6;

/// What the host's `clap.thread-check` answered at one call site.
///
/// `observed` distinguishes "the host said (false, false)" from "this site
/// never ran", which are very different failures and would otherwise be the
/// same zeroed record.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadAnswer {
    /// Whether this site has run at least once since the last reset.
    pub observed: bool,
    /// The host's `is_main_thread()` answer at the most recent visit.
    pub is_main: bool,
    /// The host's `is_audio_thread()` answer at the most recent visit.
    pub is_audio: bool,
    /// Whether the host offered `clap.thread-check` at all when asked here.
    /// A host returning null from `get_extension("clap.thread-check")` leaves
    /// `is_main`/`is_audio` meaningless, so the test checks this first.
    pub ext_present: bool,
    /// How many times this site has run since the last reset. Timer tests
    /// assert on the delta, making "fires" and "stops firing" checkable without
    /// a wall-clock wait.
    pub visits: u32,
}

/// The threading snapshot the conformance test reads across the dlopen seam.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadCapture {
    /// One record per [`Site`], indexed by its discriminant.
    pub sites: [ThreadAnswer; SITE_COUNT],
    /// Timer id the host handed back from `register_timer`, or 0 if none is
    /// registered. CLAP ids are opaque, but a host that never writes the
    /// out-param leaves this 0, which the test rejects.
    pub timer_id: u32,
    /// Whether the host's `register_timer` returned true.
    pub timer_registered: bool,
    /// Whether the host's `unregister_timer` returned true.
    pub timer_unregistered: bool,
    /// The most recent `timer_id` the host passed back into `on_timer`. Must
    /// equal `timer_id`: a wrong or recycled id routes the callback to the
    /// wrong subscriber inside the plugin.
    pub last_fired_timer_id: u32,
    /// Whether the host offered `clap.timer-support` when the plugin asked.
    pub timer_ext_present: bool,
    /// Whether the host offered `clap.log` when the plugin asked.
    pub log_ext_present: bool,
    /// How many log lines the plugin has emitted since the last reset.
    pub log_emitted: u32,
}

// ---------------------------------------------------------------------------
// Backing storage — atomics, so the audio-thread sites record without locking.
// ---------------------------------------------------------------------------

struct AtomicAnswer {
    observed: AtomicBool,
    is_main: AtomicBool,
    is_audio: AtomicBool,
    ext_present: AtomicBool,
    visits: AtomicU32,
}

impl AtomicAnswer {
    const fn new() -> Self {
        Self {
            observed: AtomicBool::new(false),
            is_main: AtomicBool::new(false),
            is_audio: AtomicBool::new(false),
            ext_present: AtomicBool::new(false),
            visits: AtomicU32::new(0),
        }
    }

    fn snapshot(&self) -> ThreadAnswer {
        ThreadAnswer {
            observed: self.observed.load(Ordering::Acquire),
            is_main: self.is_main.load(Ordering::Acquire),
            is_audio: self.is_audio.load(Ordering::Acquire),
            ext_present: self.ext_present.load(Ordering::Acquire),
            visits: self.visits.load(Ordering::Acquire),
        }
    }

    fn reset(&self) {
        self.observed.store(false, Ordering::Release);
        self.is_main.store(false, Ordering::Release);
        self.is_audio.store(false, Ordering::Release);
        self.ext_present.store(false, Ordering::Release);
        self.visits.store(0, Ordering::Release);
    }
}

struct ThreadGlobals {
    sites: [AtomicAnswer; SITE_COUNT],
    timer_id: AtomicU32,
    timer_registered: AtomicBool,
    timer_unregistered: AtomicBool,
    last_fired_timer_id: AtomicU32,
    timer_ext_present: AtomicBool,
    log_ext_present: AtomicBool,
    log_emitted: AtomicU32,
    /// Latched command word; see [`tutti_test_plugin_thread_command`].
    command: AtomicU32,
}

static THREADING: ThreadGlobals = ThreadGlobals {
    sites: [
        AtomicAnswer::new(),
        AtomicAnswer::new(),
        AtomicAnswer::new(),
        AtomicAnswer::new(),
        AtomicAnswer::new(),
        AtomicAnswer::new(),
    ],
    timer_id: AtomicU32::new(0),
    timer_registered: AtomicBool::new(false),
    timer_unregistered: AtomicBool::new(false),
    last_fired_timer_id: AtomicU32::new(0),
    timer_ext_present: AtomicBool::new(false),
    log_ext_present: AtomicBool::new(false),
    log_emitted: AtomicU32::new(0),
    command: AtomicU32::new(CMD_NONE),
};

// ---------------------------------------------------------------------------
// Commands the test can latch. Consumed at the next legal call site.
// ---------------------------------------------------------------------------

/// No pending command.
pub const CMD_NONE: u32 = 0;
/// Register a `period_ms = 0` timer with the host, from `on_main_thread`.
///
/// Zero is deliberate: the host fires once `elapsed >= period_ms`, so the timer
/// is due on every `poll_timers` call. That makes "the timer fires" one poll,
/// one callback — deterministic, rather than a race a test could only resolve
/// by sleeping.
pub const CMD_REGISTER_TIMER: u32 = 1;
/// Unregister the timer previously registered, from `on_main_thread`.
pub const CMD_UNREGISTER_TIMER: u32 = 2;
/// Emit one log line at each of the seven CLAP severities, from
/// `on_main_thread`.
pub const CMD_LOG_ALL_SEVERITIES: u32 = 3;
/// Call `host.request_restart()` from `process`. `[thread-safe]` per the spec.
pub const CMD_REQUEST_RESTART: u32 = 4;
/// Call `host.request_process()` from `process`. `[thread-safe]` per the spec.
pub const CMD_REQUEST_PROCESS: u32 = 5;

/// Latch a command for the plugin to run at its next legal call site, and
/// return the previous one. `CMD_NONE` clears.
///
/// # Safety
/// Safe to call from any thread; this is a single relaxed swap. Declared
/// `extern "C"` only so the test can reach it across the dlopen seam.
#[no_mangle]
pub extern "C" fn tutti_test_plugin_thread_command(cmd: u32) -> u32 {
    THREADING.command.swap(cmd, Ordering::AcqRel)
}

/// Copy the threading snapshot out to `out`.
///
/// # Safety
/// `out` must point to a valid, writable [`ThreadCapture`].
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_thread_capture(out: *mut ThreadCapture) -> bool {
    if out.is_null() {
        return false;
    }
    let mut cap = ThreadCapture {
        timer_id: THREADING.timer_id.load(Ordering::Acquire),
        timer_registered: THREADING.timer_registered.load(Ordering::Acquire),
        timer_unregistered: THREADING.timer_unregistered.load(Ordering::Acquire),
        last_fired_timer_id: THREADING.last_fired_timer_id.load(Ordering::Acquire),
        timer_ext_present: THREADING.timer_ext_present.load(Ordering::Acquire),
        log_ext_present: THREADING.log_ext_present.load(Ordering::Acquire),
        log_emitted: THREADING.log_emitted.load(Ordering::Acquire),
        ..ThreadCapture::default()
    };
    for (dst, src) in cap.sites.iter_mut().zip(THREADING.sites.iter()) {
        *dst = src.snapshot();
    }
    *out = cap;
    true
}

/// Clear every recorded site and counter, so `visits` deltas mean "since this
/// scenario started" — the whole test binary shares one loaded image.
///
/// Does **not** clear `timer_id`: a timer registered in one scenario is still
/// registered with the host, and the test needs the id to assert on the
/// `on_timer` routing afterwards.
#[no_mangle]
pub extern "C" fn tutti_test_plugin_thread_reset() {
    for site in THREADING.sites.iter() {
        site.reset();
    }
    THREADING.timer_registered.store(false, Ordering::Release);
    THREADING.timer_unregistered.store(false, Ordering::Release);
    THREADING.last_fired_timer_id.store(0, Ordering::Release);
    THREADING.log_emitted.store(0, Ordering::Release);
    THREADING.command.store(CMD_NONE, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Call-site hooks — what `lib.rs` calls.
// ---------------------------------------------------------------------------

/// Ask the host's `clap.thread-check` where we are and record the answer under
/// `site`.
///
/// # Safety
/// `host` must be null or a valid `clap_host` pointer.
pub unsafe fn record_thread_roles(site: Site, host: *const clap_host) {
    let slot = &THREADING.sites[site as usize];
    slot.visits.fetch_add(1, Ordering::AcqRel);

    let tc = host_ext::<clap_host_thread_check>(host, CLAP_EXT_THREAD_CHECK);
    if let Some(tc) = tc {
        slot.ext_present.store(true, Ordering::Release);
        let is_main = tc.is_main_thread.map(|f| f(host)).unwrap_or(false);
        let is_audio = tc.is_audio_thread.map(|f| f(host)).unwrap_or(false);
        slot.is_main.store(is_main, Ordering::Release);
        slot.is_audio.store(is_audio, Ordering::Release);
    } else {
        slot.ext_present.store(false, Ordering::Release);
    }
    // Released last so a reader that sees `observed` also sees the answers.
    slot.observed.store(true, Ordering::Release);
}

/// Run whatever command the test latched, if this site may run it. Consumes the
/// command so it fires exactly once.
///
/// # Safety
/// `host` must be null or a valid `clap_host` pointer, and `site` must
/// accurately name the CLAP call this is running inside — the site is what
/// decides which commands are legal here.
pub unsafe fn run_pending_command(site: Site, host: *const clap_host) {
    if host.is_null() {
        return;
    }
    let cmd = THREADING.command.load(Ordering::Acquire);
    if cmd == CMD_NONE {
        return;
    }
    // Only run a command at a site CLAP allows it from. Timer registration and
    // logging are `[main-thread]`; the two request_* calls are `[thread-safe]`
    // but driven from `process`, where a real plugin reaches the decision.
    let legal = match cmd {
        CMD_REGISTER_TIMER | CMD_UNREGISTER_TIMER | CMD_LOG_ALL_SEVERITIES => {
            site == Site::OnMainThread
        }
        CMD_REQUEST_RESTART | CMD_REQUEST_PROCESS => site == Site::Process,
        _ => false,
    };
    if !legal {
        return;
    }
    // Claim the command before running it so a re-entrant or concurrent visit
    // cannot run it twice.
    if THREADING
        .command
        .compare_exchange(cmd, CMD_NONE, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    match cmd {
        CMD_REGISTER_TIMER => register_timer(host),
        CMD_UNREGISTER_TIMER => unregister_timer(host),
        CMD_LOG_ALL_SEVERITIES => log_all_severities(host),
        CMD_REQUEST_RESTART => {
            if let Some(f) = (*host).request_restart {
                f(host);
            }
        }
        CMD_REQUEST_PROCESS => {
            if let Some(f) = (*host).request_process {
                f(host);
            }
        }
        _ => {}
    }
}

/// The threading extensions this probe implements, for `plugin_get_extension`
/// to fall through to. Returns null for anything else.
///
/// # Safety
/// `id` must be a valid nul-terminated C string.
pub unsafe fn get_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_TIMER_SUPPORT {
        return &TIMER_SUPPORT as *const _ as *const c_void;
    }
    ptr::null()
}

// ---------------------------------------------------------------------------
// timer-support: the plugin side.
// ---------------------------------------------------------------------------

static TIMER_SUPPORT: clap_plugin_timer_support = clap_plugin_timer_support {
    on_timer: Some(plugin_on_timer),
};

unsafe extern "C" fn plugin_on_timer(plugin: *const clap_plugin, timer_id: u32) {
    THREADING
        .last_fired_timer_id
        .store(timer_id, Ordering::Release);
    let host = crate::plugin_host(plugin);
    record_thread_roles(Site::OnTimer, host);
}

unsafe fn register_timer(host: *const clap_host) {
    let Some(ts) = host_ext::<clap_sys::ext::timer_support::clap_host_timer_support>(
        host,
        CLAP_EXT_TIMER_SUPPORT,
    ) else {
        THREADING.timer_ext_present.store(false, Ordering::Release);
        return;
    };
    THREADING.timer_ext_present.store(true, Ordering::Release);
    let Some(register) = ts.register_timer else {
        return;
    };
    let mut id: u32 = 0;
    // Period 0: due on every host poll, so the test never waits on a clock.
    let ok = register(host, 0, &mut id);
    THREADING.timer_id.store(id, Ordering::Release);
    THREADING.timer_registered.store(ok, Ordering::Release);
}

unsafe fn unregister_timer(host: *const clap_host) {
    let Some(ts) = host_ext::<clap_sys::ext::timer_support::clap_host_timer_support>(
        host,
        CLAP_EXT_TIMER_SUPPORT,
    ) else {
        return;
    };
    let Some(unregister) = ts.unregister_timer else {
        return;
    };
    let id = THREADING.timer_id.load(Ordering::Acquire);
    let ok = unregister(host, id);
    THREADING.timer_unregistered.store(ok, Ordering::Release);
}

// ---------------------------------------------------------------------------
// log: emit one line at each severity.
// ---------------------------------------------------------------------------

/// The seven CLAP severities, paired with the message the plugin sends at each.
/// The messages are distinct so the host-side record is checkable
/// severity-by-severity — a host routing every line through one arm would
/// otherwise pass on count alone.
const LOG_LINES: [(i32, &CStr); 7] = [
    (CLAP_LOG_DEBUG, c"tutti-probe severity debug"),
    (CLAP_LOG_INFO, c"tutti-probe severity info"),
    (CLAP_LOG_WARNING, c"tutti-probe severity warning"),
    (CLAP_LOG_ERROR, c"tutti-probe severity error"),
    (CLAP_LOG_FATAL, c"tutti-probe severity fatal"),
    (
        CLAP_LOG_HOST_MISBEHAVING,
        c"tutti-probe severity host-misbehaving",
    ),
    (
        CLAP_LOG_PLUGIN_MISBEHAVING,
        c"tutti-probe severity plugin-misbehaving",
    ),
];

unsafe fn log_all_severities(host: *const clap_host) {
    let Some(log_ext) = host_ext::<clap_host_log>(host, CLAP_EXT_LOG) else {
        THREADING.log_ext_present.store(false, Ordering::Release);
        return;
    };
    THREADING.log_ext_present.store(true, Ordering::Release);
    let Some(log) = log_ext.log else {
        return;
    };
    for (severity, msg) in LOG_LINES {
        log(host, severity, msg.as_ptr());
        THREADING.log_emitted.fetch_add(1, Ordering::AcqRel);
    }
}

/// The `(severity, message)` pairs [`CMD_LOG_ALL_SEVERITIES`] emits, exposed so
/// the conformance test asserts against the same list the plugin sends rather
/// than a hand-copied duplicate that could drift.
pub fn log_lines() -> [(i32, &'static CStr); 7] {
    LOG_LINES
}

// ---------------------------------------------------------------------------
// Shared helper.
// ---------------------------------------------------------------------------

/// Fetch a host extension vtable by id, or `None` if the host does not offer it.
///
/// # Safety
/// `host` must be null or valid, and `T` must be the vtable type CLAP defines
/// for `id`.
unsafe fn host_ext<'a, T>(host: *const clap_host, id: &CStr) -> Option<&'a T> {
    if host.is_null() {
        return None;
    }
    let get_ext = (*host).get_extension?;
    let ptr = get_ext(host, id.as_ptr() as *const c_char);
    if ptr.is_null() {
        None
    } else {
        Some(&*(ptr as *const T))
    }
}
