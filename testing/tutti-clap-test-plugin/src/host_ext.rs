//! HOST-EXTENSION probe: `clap.audio-ports-activation`, `clap.track-info` and
//! `clap.transport-control`, from the plugin's side.
//!
//! The plugin answers the host's `set_active` and `changed` calls and records
//! what it was handed (which thread, which sample size); a latched command makes
//! it call the host's `track-info.get` or one of the transport requests, from
//! `on_main_thread` (where CLAP allows both) or from the next `process` (where
//! it does not, so a host that queues the request there is caught). The
//! expected figures live in the tests, never here.
//!
//! The activation records are lock-free (a `set_active` may arrive on the audio
//! thread); the rest sits behind a mutex the main thread alone takes.

use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, clap_plugin_audio_ports_activation,
};
use clap_sys::ext::draft::transport_control::{
    CLAP_EXT_TRANSPORT_CONTROL, clap_host_transport_control,
};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, clap_host_track_info, clap_plugin_track_info, clap_track_info,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

use crate::threading::host_ext;

/// The most `set_active` calls recorded.
pub const MAX_CAPTURED_ACTIVATIONS: usize = 16;

/// Command: call `track-info.get` and record the answer.
pub const HOSTEXT_CMD_TRACK_GET: u32 = 1;
/// Command: `transport-control.request_start`.
pub const HOSTEXT_CMD_START: u32 = 10;
/// Command: `request_stop`.
pub const HOSTEXT_CMD_STOP: u32 = 11;
/// Command: `request_continue`.
pub const HOSTEXT_CMD_CONTINUE: u32 = 12;
/// Command: `request_pause`.
pub const HOSTEXT_CMD_PAUSE: u32 = 13;
/// Command: `request_toggle_play`.
pub const HOSTEXT_CMD_TOGGLE_PLAY: u32 = 14;
/// Command: `request_jump(a)`, `a` a CLAP fixed-point beat time.
pub const HOSTEXT_CMD_JUMP: u32 = 15;
/// Command: `request_loop_region(a, b)`, both fixed-point beat times.
pub const HOSTEXT_CMD_LOOP_REGION: u32 = 16;
/// Command: `request_toggle_loop`.
pub const HOSTEXT_CMD_TOGGLE_LOOP: u32 = 17;
/// Command: `request_enable_loop(a != 0)`.
pub const HOSTEXT_CMD_ENABLE_LOOP: u32 = 18;
/// Command: `request_record(a != 0)`.
pub const HOSTEXT_CMD_RECORD: u32 = 19;
/// Command: `request_toggle_record`.
pub const HOSTEXT_CMD_TOGGLE_RECORD: u32 = 20;
/// Command: `request_start`, `a` times in a row.
pub const HOSTEXT_CMD_START_BURST: u32 = 21;

/// One `audio-ports-activation.set_active` call as the plugin received it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapturedActivation {
    /// The port's direction.
    pub is_input: bool,
    /// The state asked for.
    pub is_active: bool,
    /// Whether the host's thread-check called the thread the audio thread.
    pub on_audio_thread: bool,
    /// Whether the host's thread-check called the thread the main thread.
    pub on_main_thread: bool,
    /// The sample size the host named: 32, 64, or 0 for unspecified.
    pub sample_size: u32,
    /// The port.
    pub port_index: u32,
}

/// A `clap_track_info` as the host filled it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturedTrack {
    /// The `CLAP_TRACK_INFO_*` flags.
    pub flags: u64,
    /// The name, nul-terminated.
    pub name: [u8; 256],
    /// Red, green, blue, alpha.
    pub color: [u8; 4],
    /// The audio channel count.
    pub audio_channel_count: i32,
    /// The port type, nul-terminated; empty when the host gave none.
    pub port_type: [u8; 16],
    /// Whether the host gave a port type.
    pub has_port_type: bool,
}

impl CapturedTrack {
    const EMPTY: Self = Self {
        flags: 0,
        name: [0; 256],
        color: [0; 4],
        audio_channel_count: 0,
        port_type: [0; 16],
        has_port_type: false,
    };

    /// The name as text.
    #[must_use]
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(256);
        std::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    /// The port type as text.
    #[must_use]
    pub fn port_type_str(&self) -> &str {
        let end = self.port_type.iter().position(|&b| b == 0).unwrap_or(16);
        std::str::from_utf8(&self.port_type[..end]).unwrap_or("")
    }
}

/// What the probe saw. `#[repr(C)]` so the test reads it across the `dlopen`
/// seam.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostExtCapture {
    /// `set_active` calls, which may exceed [`MAX_CAPTURED_ACTIVATIONS`];
    /// `activations` holds the first ones in call order.
    pub activation_calls: u32,
    /// The recorded calls.
    pub activations: [CapturedActivation; MAX_CAPTURED_ACTIVATIONS],
    /// `track-info.changed` calls.
    pub track_changed_calls: u32,
    /// What `get` answered inside the last `changed`.
    pub track_at_changed: CapturedTrack,
    /// `track-info.get` calls made by a command.
    pub track_queries: u32,
    /// Whether the last queried `get` returned `true`.
    pub track_query_ok: bool,
    /// What the last queried `get` filled.
    pub track_queried: CapturedTrack,
    /// Whether the host offered `clap.transport-control` at all.
    pub transport_offered: bool,
}

static CAN_WHILE_PROCESSING: AtomicBool = AtomicBool::new(false);
static ACTIVATION_CALLS: AtomicU32 = AtomicU32::new(0);
static ACTIVATIONS: [AtomicU64; MAX_CAPTURED_ACTIVATIONS] =
    [const { AtomicU64::new(0) }; MAX_CAPTURED_ACTIVATIONS];
static CAPTURE: Mutex<HostExtCapture> = Mutex::new(empty_capture());

const fn empty_capture() -> HostExtCapture {
    HostExtCapture {
        activation_calls: 0,
        activations: [CapturedActivation {
            is_input: false,
            is_active: false,
            on_audio_thread: false,
            on_main_thread: false,
            sample_size: 0,
            port_index: 0,
        }; MAX_CAPTURED_ACTIVATIONS],
        track_changed_calls: 0,
        track_at_changed: CapturedTrack::EMPTY,
        track_queries: 0,
        track_query_ok: false,
        track_queried: CapturedTrack::EMPTY,
        transport_offered: false,
    }
}

fn with_capture<R>(f: impl FnOnce(&mut HostExtCapture) -> R) -> R {
    f(&mut CAPTURE.lock().unwrap_or_else(|p| p.into_inner()))
}

// ---------------------------------------------------------------------------
// The plugin's side of `audio-ports-activation`.
// ---------------------------------------------------------------------------

static ACTIVATION_EXT: clap_plugin_audio_ports_activation = clap_plugin_audio_ports_activation {
    can_activate_while_processing: Some(can_activate_while_processing),
    set_active: Some(set_active),
};

unsafe extern "C" fn can_activate_while_processing(_plugin: *const clap_plugin) -> bool {
    CAN_WHILE_PROCESSING.load(Ordering::SeqCst)
}

unsafe extern "C" fn set_active(
    plugin: *const clap_plugin,
    is_input: bool,
    port_index: u32,
    is_active: bool,
    sample_size: u32,
) -> bool {
    // SAFETY: a CLAP plugin entry point: the host passes the plugin pointer it
    // was handed.
    let host = unsafe { crate::plugin_host(plugin) };
    // SAFETY: the host this plugin was created with, live until `destroy`.
    let tc = unsafe { host_ext::<clap_host_thread_check>(host, CLAP_EXT_THREAD_CHECK) };
    // SAFETY: the host's own thread-check entries, called with its host.
    let ask = |f: Option<unsafe extern "C" fn(*const clap_host) -> bool>| {
        f.is_some_and(|f| unsafe { f(host) })
    };
    let audio = ask(tc.and_then(|t| t.is_audio_thread));
    let main = ask(tc.and_then(|t| t.is_main_thread));
    let n = ACTIVATION_CALLS.fetch_add(1, Ordering::SeqCst) as usize;
    if let Some(slot) = ACTIVATIONS.get(n) {
        let word = u64::from(is_input)
            | u64::from(is_active) << 1
            | u64::from(audio) << 2
            | u64::from(main) << 3
            | u64::from(sample_size & 0xff) << 8
            | u64::from(port_index) << 16;
        // The word is never zero once written (the sample size or index may
        // be, the call is marked by bit 63).
        slot.store(word | 1 << 63, Ordering::SeqCst);
    }
    // A port past the ones the plugin has is an invalid parameter.
    port_index < unsafe { crate::audio_ports_count(plugin, is_input) }
}

// ---------------------------------------------------------------------------
// The plugin's side of `track-info`.
// ---------------------------------------------------------------------------

static TRACK_INFO_EXT: clap_plugin_track_info = clap_plugin_track_info {
    changed: Some(track_changed),
};

unsafe extern "C" fn track_changed(plugin: *const clap_plugin) {
    // SAFETY: a CLAP plugin entry point; the host this plugin was created with
    // is live until `destroy`.
    let host = unsafe { crate::plugin_host(plugin) };
    let seen = unsafe { read_track(host) }.map(|(_, t)| t);
    with_capture(|c| {
        c.track_changed_calls += 1;
        if let Some(t) = seen {
            c.track_at_changed = t;
        }
    });
}

/// The host's answer to `track-info.get`, with whether it said `true`.
///
/// # Safety
/// `host` must be null or live.
unsafe fn read_track(host: *const clap_host) -> Option<(bool, CapturedTrack)> {
    let ext = unsafe { host_ext::<clap_host_track_info>(host, CLAP_EXT_TRACK_INFO) }?;
    let get = ext.get?;
    // SAFETY: all-zero is a valid `clap_track_info`.
    let mut info: clap_track_info = unsafe { std::mem::zeroed() };
    let ok = unsafe { get(host, &mut info) };
    let mut out = CapturedTrack::EMPTY;
    out.flags = info.flags;
    for (d, s) in out.name.iter_mut().zip(info.name.iter()) {
        *d = *s as u8;
    }
    out.color = [
        info.color.red,
        info.color.green,
        info.color.blue,
        info.color.alpha,
    ];
    out.audio_channel_count = info.audio_channel_count;
    if !info.audio_port_type.is_null() {
        out.has_port_type = true;
        // SAFETY: a nul-terminated string of the host's, valid for the call.
        let text = unsafe { CStr::from_ptr(info.audio_port_type) }.to_bytes();
        let n = text.len().min(out.port_type.len() - 1);
        out.port_type[..n].copy_from_slice(&text[..n]);
    }
    Some((ok, out))
}

// ---------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------

static COMMAND: AtomicU32 = AtomicU32::new(0);
static ARG_A: AtomicI64 = AtomicI64::new(0);
static ARG_B: AtomicI64 = AtomicI64::new(0);
static PROCESS_COMMAND: AtomicU32 = AtomicU32::new(0);
static PROCESS_ARG_A: AtomicI64 = AtomicI64::new(0);
static PROCESS_ARG_B: AtomicI64 = AtomicI64::new(0);

/// Latches a command for the plugin's next `on_main_thread`.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_command(cmd: u32, a: i64, b: i64) {
    ARG_A.store(a, Ordering::SeqCst);
    ARG_B.store(b, Ordering::SeqCst);
    COMMAND.store(cmd, Ordering::SeqCst);
}

/// Latches a command for the plugin's next `process`, the audio thread's
/// block, where the transport requests are not allowed.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_process_command(cmd: u32, a: i64, b: i64) {
    PROCESS_ARG_A.store(a, Ordering::SeqCst);
    PROCESS_ARG_B.store(b, Ordering::SeqCst);
    PROCESS_COMMAND.store(cmd, Ordering::SeqCst);
}

/// Whether the plugin says it can activate a port while processing. Set
/// before the host asks.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_set_can_activate_while_processing(can: bool) {
    CAN_WHILE_PROCESSING.store(can, Ordering::SeqCst);
}

/// Clears the capture and any latched command.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_reset() {
    COMMAND.store(0, Ordering::SeqCst);
    PROCESS_COMMAND.store(0, Ordering::SeqCst);
    CAN_WHILE_PROCESSING.store(false, Ordering::SeqCst);
    ACTIVATION_CALLS.store(0, Ordering::SeqCst);
    for slot in &ACTIVATIONS {
        slot.store(0, Ordering::SeqCst);
    }
    with_capture(|c| *c = empty_capture());
}

/// Copies the capture to `out`.
///
/// # Safety
/// `out` must be null or writable for one [`HostExtCapture`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_capture(out: *mut HostExtCapture) -> bool {
    if out.is_null() {
        return false;
    }
    let mut snapshot = with_capture(|c| *c);
    snapshot.activation_calls = ACTIVATION_CALLS.load(Ordering::SeqCst);
    for (dst, slot) in snapshot.activations.iter_mut().zip(&ACTIVATIONS) {
        let word = slot.load(Ordering::SeqCst);
        *dst = CapturedActivation {
            is_input: word & 1 != 0,
            is_active: word & 2 != 0,
            on_audio_thread: word & 4 != 0,
            on_main_thread: word & 8 != 0,
            sample_size: ((word >> 8) & 0xff) as u32,
            port_index: ((word >> 16) & 0xffff_ffff) as u32,
        };
    }
    // SAFETY: non-null and writable per the contract above.
    unsafe { out.write(snapshot) };
    true
}

/// Runs the command latched for `on_main_thread`, once.
///
/// # Safety
/// `host` must be null or live, and this must run on the host's main thread.
pub(crate) unsafe fn run_pending_command(host: *const clap_host) {
    let cmd = COMMAND.swap(0, Ordering::SeqCst);
    if cmd != 0 {
        unsafe {
            run(
                cmd,
                ARG_A.load(Ordering::SeqCst),
                ARG_B.load(Ordering::SeqCst),
                host,
            );
        }
    }
}

/// Runs the command latched for `process`, once.
///
/// # Safety
/// `host` must be null or live.
pub(crate) unsafe fn on_process(host: *const clap_host) {
    let cmd = PROCESS_COMMAND.swap(0, Ordering::SeqCst);
    if cmd != 0 {
        unsafe {
            run(
                cmd,
                PROCESS_ARG_A.load(Ordering::SeqCst),
                PROCESS_ARG_B.load(Ordering::SeqCst),
                host,
            );
        }
    }
}

unsafe fn run(cmd: u32, a: i64, b: i64, host: *const clap_host) {
    if cmd == HOSTEXT_CMD_TRACK_GET {
        let read = unsafe { read_track(host) };
        with_capture(|c| {
            c.track_queries += 1;
            if let Some((ok, track)) = read {
                c.track_query_ok = ok;
                c.track_queried = track;
            }
        });
        return;
    }
    let ext = unsafe { host_ext::<clap_host_transport_control>(host, CLAP_EXT_TRANSPORT_CONTROL) };
    with_capture(|c| c.transport_offered |= ext.is_some());
    let Some(ext) = ext else {
        return;
    };
    // SAFETY (each call): the host's own transport-control entry, called with
    // the host it belongs to.
    unsafe {
        match cmd {
            HOSTEXT_CMD_START => call_if(ext.request_start, |f| f(host)),
            HOSTEXT_CMD_STOP => call_if(ext.request_stop, |f| f(host)),
            HOSTEXT_CMD_CONTINUE => call_if(ext.request_continue, |f| f(host)),
            HOSTEXT_CMD_PAUSE => call_if(ext.request_pause, |f| f(host)),
            HOSTEXT_CMD_TOGGLE_PLAY => call_if(ext.request_toggle_play, |f| f(host)),
            HOSTEXT_CMD_JUMP => call_if(ext.request_jump, |f| f(host, a)),
            HOSTEXT_CMD_LOOP_REGION => call_if(ext.request_loop_region, |f| f(host, a, b)),
            HOSTEXT_CMD_TOGGLE_LOOP => call_if(ext.request_toggle_loop, |f| f(host)),
            HOSTEXT_CMD_ENABLE_LOOP => call_if(ext.request_enable_loop, |f| f(host, a != 0)),
            HOSTEXT_CMD_RECORD => call_if(ext.request_record, |f| f(host, a != 0)),
            HOSTEXT_CMD_TOGGLE_RECORD => call_if(ext.request_toggle_record, |f| f(host)),
            HOSTEXT_CMD_START_BURST => call_if(ext.request_start, |f| {
                for _ in 0..a {
                    f(host);
                }
            }),
            _ => {}
        }
    }
}

/// Runs `call` on `entry` when the host has it.
fn call_if<F: Copy>(entry: Option<F>, call: impl FnOnce(F)) {
    if let Some(f) = entry {
        call(f);
    }
}

/// This module's vtable for `id`, or null.
pub(crate) fn get_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_AUDIO_PORTS_ACTIVATION {
        return (&raw const ACTIVATION_EXT).cast::<c_void>();
    }
    if id == CLAP_EXT_TRACK_INFO {
        return (&raw const TRACK_INFO_EXT).cast::<c_void>();
    }
    std::ptr::null()
}
