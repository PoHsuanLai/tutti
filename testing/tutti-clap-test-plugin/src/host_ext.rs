//! HOST-EXTENSION probe: `clap.track-info` and `clap.transport-control`, from
//! the plugin's side.
//!
//! The plugin answers the host's `changed` call and records what it was handed;
//! a latched command makes
//! it call the host's `track-info.get` or one of the transport requests, from
//! `on_main_thread` (where CLAP allows both) or from the next `process` (where
//! it does not, so a host that queues the request there is caught). The
//! expected figures live in the tests, never here.
//!
//! The capture sits behind a mutex the main thread alone takes.

use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

use clap_sys::ext::draft::transport_control::{
    CLAP_EXT_TRANSPORT_CONTROL, clap_host_transport_control,
};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, clap_host_track_info, clap_plugin_track_info, clap_track_info,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

use crate::threading::host_ext;

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

static CAPTURE: Mutex<HostExtCapture> = Mutex::new(empty_capture());

const fn empty_capture() -> HostExtCapture {
    HostExtCapture {
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

/// Clears the capture and any latched command.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_reset() {
    COMMAND.store(0, Ordering::SeqCst);
    PROCESS_COMMAND.store(0, Ordering::SeqCst);
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
    let snapshot = with_capture(|c| *c);
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

/// Latches a command for the plugin's next `on_main_thread`, and has the
/// plugin ask its host for that callback now, so a host that serves the
/// callback on its own tick (an editor instance's idle) runs it.
///
/// # Safety
/// The host recorded at `init` must still be alive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_hostext_command_via_callback(
    cmd: u32,
    a: i64,
    b: i64,
) -> bool {
    unsafe { tutti_test_plugin_hostext_command(cmd, a, b) };
    crate::subprocess::request_callback_now()
}

/// This module's vtable for `id`, or null.
pub(crate) fn get_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_TRACK_INFO {
        return (&raw const TRACK_INFO_EXT).cast::<c_void>();
    }
    std::ptr::null()
}
