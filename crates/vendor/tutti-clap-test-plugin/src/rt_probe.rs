//! RT-HAZARD probe — the plugin-side switches that make the host's audio
//! thread do the things it must not do.
//!
//! The other probe modules ask "did the host build a *correct* call?". This one
//! asks a different question: "when the plugin exercises a legal-but-awkward
//! corner, does the host allocate on the audio thread?". Every switch here
//! corresponds to a confirmed hazard in `tutti-clap-host`'s `do_process` path,
//! and the matching test lives in `tests/clap_process_no_alloc.rs`.
//!
//! | switch                         | host hazard it reaches                        |
//! |--------------------------------|-----------------------------------------------|
//! | [`StatusMode`]                 | H1 — `eprintln!` on a process-status *transition* |
//! | [`StatusMode::Error`]          | H2 — `format!`/`to_string` building a `ClapError` |
//! | [`tutti_test_plugin_set_sysex_output_bytes`] | H3 — per-event `to_vec()` in `output_events_try_push` |
//! | [`tutti_test_plugin_set_audio_thread_log_lines`] | H4 — `String` + stderr lock + `Mutex` in `clap.log` |
//! | [`WideLayout`]                 | H7 — `SmallVec<[*mut T; 16]>` spilling past its inline capacity |
//!
//! ## Why these are process-global switches rather than parameters
//!
//! Same reason as [`crate::PortLayoutMode`]: the host reads the port layout
//! once at load time, and a `clap_process_status` is a return value, not a
//! parameter. A test sets the switch, then drives blocks; there is one plugin
//! image per test process, so a global is the whole channel.
//!
//! ## Why the status modes *alternate*
//!
//! H1 is not "the host logs under SLEEP". The host already guards its
//! `eprintln!` behind `status != prev_status`, so a plugin parked on one status
//! prints once and never again — which is exactly why the hazard survived
//! review. A plugin that alternates CONTINUE/TAIL transitions on *every* block,
//! and so logs on every block. [`StatusMode::AlternateContinueTail`] and
//! [`StatusMode::AlternateContinueGarbage`] exist to produce that, and they are
//! ordinary plugin behaviour: a reverb whose tail decays below the noise floor
//! and is re-excited by input flips between CONTINUE and TAIL naturally.

use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use clap_sys::events::{clap_event_header, clap_event_midi_sysex, CLAP_EVENT_MIDI_SYSEX};
use clap_sys::ext::log::{clap_host_log, CLAP_EXT_LOG, CLAP_LOG_WARNING};
use clap_sys::host::clap_host;
use clap_sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
    CLAP_PROCESS_ERROR, CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL,
};

// ---------------------------------------------------------------------------
// Process status
// ---------------------------------------------------------------------------

/// A `clap_process_status` value CLAP does not define.
///
/// The host's status `match` has an `other =>` arm that heap-formats the `i32`
/// into an `eprintln!`. Reaching it needs a value outside the defined set, and
/// CLAP explicitly leaves the space open — a plugin built against a newer
/// version of the header can legitimately return a status this host has never
/// heard of, which is precisely the case the host must survive without
/// allocating.
pub const GARBAGE_STATUS: clap_process_status = 0x7EED_BEEF;

/// The CLAP-defined statuses, re-exported so a test can assert against them
/// without taking its own `clap-sys` dependency.
///
/// Worth the two lines: a test that hardcodes these gets them wrong. CLAP
/// numbers `ERROR = 0` and `CONTINUE = 1`, so the natural guess — that the
/// success value is 0, as in almost every other C API — is off by one and
/// silently compares against ERROR. That mistake was made while writing the
/// suite these serve.
pub mod status {
    use super::clap_process_status;

    pub const ERROR: clap_process_status = super::CLAP_PROCESS_ERROR;
    pub const CONTINUE: clap_process_status = super::CLAP_PROCESS_CONTINUE;
    pub const CONTINUE_IF_NOT_QUIET: clap_process_status =
        super::CLAP_PROCESS_CONTINUE_IF_NOT_QUIET;
    pub const TAIL: clap_process_status = super::CLAP_PROCESS_TAIL;
    pub const SLEEP: clap_process_status = super::CLAP_PROCESS_SLEEP;
}

/// What [`plugin_process`](crate) returns.
///
/// Discriminants cross the dlopen seam as a bare `u32`, so they are pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum StatusMode {
    /// `CLAP_PROCESS_CONTINUE` on every block — the default, and the only mode
    /// under which the host's transition log stays quiet after the first block.
    Continue = 0,
    /// `CLAP_PROCESS_CONTINUE_IF_NOT_QUIET` on every block.
    ContinueIfNotQuiet = 1,
    /// `CLAP_PROCESS_TAIL` on every block. The host logs on the *first* block
    /// only, so this alone does not reach H1 in steady state.
    Tail = 2,
    /// `CLAP_PROCESS_SLEEP` on every block. As with `Tail`, one log line.
    Sleep = 3,
    /// `CLAP_PROCESS_ERROR` on every block. Reaches H2: the host builds an
    /// owned `String` for the `ClapError` it returns.
    Error = 4,
    /// [`GARBAGE_STATUS`] on every block — the host's unknown-status arm.
    Garbage = 5,
    /// CONTINUE, TAIL, CONTINUE, TAIL, … — a transition on **every** block.
    /// This is the mode that reaches H1: the host's transition guard passes
    /// each time, so it logs (and, in the `other` arm, formats) per block.
    AlternateContinueTail = 6,
    /// CONTINUE, [`GARBAGE_STATUS`], … — as above, but landing in the host's
    /// `other =>` arm, which additionally heap-formats the status integer.
    AlternateContinueGarbage = 7,
    /// CONTINUE, SLEEP, … — the third transition pair, so a fix that special-
    /// cases only TAIL is still caught.
    AlternateContinueSleep = 8,
}

impl StatusMode {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::ContinueIfNotQuiet,
            2 => Self::Tail,
            3 => Self::Sleep,
            4 => Self::Error,
            5 => Self::Garbage,
            6 => Self::AlternateContinueTail,
            7 => Self::AlternateContinueGarbage,
            8 => Self::AlternateContinueSleep,
            _ => Self::Continue,
        }
    }

    /// The status for block number `block` (0-based since the last reset).
    fn status_for_block(self, block: u64) -> clap_process_status {
        let odd = block % 2 == 1;
        match self {
            Self::Continue => CLAP_PROCESS_CONTINUE,
            Self::ContinueIfNotQuiet => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
            Self::Tail => CLAP_PROCESS_TAIL,
            Self::Sleep => CLAP_PROCESS_SLEEP,
            Self::Error => CLAP_PROCESS_ERROR,
            Self::Garbage => GARBAGE_STATUS,
            Self::AlternateContinueTail => {
                if odd {
                    CLAP_PROCESS_TAIL
                } else {
                    CLAP_PROCESS_CONTINUE
                }
            }
            Self::AlternateContinueGarbage => {
                if odd {
                    GARBAGE_STATUS
                } else {
                    CLAP_PROCESS_CONTINUE
                }
            }
            Self::AlternateContinueSleep => {
                if odd {
                    CLAP_PROCESS_SLEEP
                } else {
                    CLAP_PROCESS_CONTINUE
                }
            }
        }
    }
}

static STATUS_MODE: AtomicU32 = AtomicU32::new(StatusMode::Continue as u32);

/// Blocks processed since the last [`tutti_test_plugin_reset_rt_probe`]. Drives
/// the alternating modes; a free-running counter rather than a toggle so a test
/// can reason about which status a given block index produced.
static BLOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Select the `clap_process_status` the plugin returns. Takes the discriminant
/// of [`StatusMode`] as a bare `u32` because this crosses the dlopen seam.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_status_mode(mode: u32) {
    STATUS_MODE.store(mode, Ordering::SeqCst);
}

/// The status this block should return, advancing the block counter.
///
/// Called once per `process`, from `plugin_process`.
pub(crate) fn next_status() -> clap_process_status {
    let block = BLOCK_COUNTER.fetch_add(1, Ordering::SeqCst);
    StatusMode::from_u32(STATUS_MODE.load(Ordering::SeqCst)).status_for_block(block)
}

// ---------------------------------------------------------------------------
// SysEx output
// ---------------------------------------------------------------------------

/// Largest SysEx payload the probe emits, and the size of its static buffer.
///
/// 256 bytes is comfortably past any inline/small-buffer optimisation a host
/// might use, so a host that "handles" SysEx by stashing short payloads inline
/// still has to face a real one.
pub const MAX_SYSEX_BYTES: usize = 256;

/// Payload size for each emitted SysEx event, or 0 to emit none.
static SYSEX_BYTES: AtomicU32 = AtomicU32::new(0);

/// How many SysEx events to push per block.
static SYSEX_COUNT: AtomicU32 = AtomicU32::new(0);

/// The payload the probe pushes. Contents are irrelevant to the hazard (the
/// host copies bytes regardless), so a fixed pattern serves — but it is a
/// *valid* SysEx frame (`F0 … F7`) so a host that validates before copying
/// still takes the copy path.
static SYSEX_PAYLOAD: [u8; MAX_SYSEX_BYTES] = {
    let mut buf = [0x42u8; MAX_SYSEX_BYTES];
    buf[0] = 0xF0;
    buf[MAX_SYSEX_BYTES - 1] = 0xF7;
    buf
};

/// Emit `count` SysEx output events of `bytes` bytes each, on every block.
///
/// `bytes` is clamped to [`MAX_SYSEX_BYTES`]. Either argument being 0 disables
/// SysEx emission.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_sysex_output_bytes(count: u32, bytes: u32) {
    SYSEX_COUNT.store(count, Ordering::SeqCst);
    SYSEX_BYTES.store(bytes.min(MAX_SYSEX_BYTES as u32), Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// clap.log from the audio thread
// ---------------------------------------------------------------------------

/// How many `clap.log` lines to emit per block from inside `process`.
static LOG_LINES_PER_BLOCK: AtomicU32 = AtomicU32::new(0);

/// The line the probe logs from the audio thread. Fixed and distinctive so a
/// host-side test can tell it from the threading probe's main-thread lines.
static AUDIO_THREAD_LOG_LINE: &CStr = c"tutti-probe logging from the audio thread";

/// Emit `count` `clap.log` lines per block, from inside `process`.
///
/// CLAP marks `clap.log` `[thread-safe]`, which includes the audio thread —
/// deliberately, because a plugin detecting a denormal storm or a dropped
/// buffer has nothing else to report it with. A host that records such a line
/// the way it records a main-thread one allocates a `String`, takes the stderr
/// lock, and takes a `Mutex` its own `drain_log` holds across a copy: three
/// things forbidden in an audio callback, the last of them a priority
/// inversion.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_audio_thread_log_lines(count: u32) {
    LOG_LINES_PER_BLOCK.store(count, Ordering::SeqCst);
}

/// Log the configured number of lines through the host's `clap.log`.
///
/// Called from `plugin_process`, so these arrive on the host's audio thread
/// while it is inside its own `process`.
///
/// # Safety
/// `host` must be null or the live `clap_host` the plugin was created with.
pub(crate) unsafe fn emit_audio_thread_logs(host: *const clap_host) {
    let count = LOG_LINES_PER_BLOCK.load(Ordering::Acquire);
    if count == 0 {
        return;
    }
    // Resolved inline rather than through a shared helper: `gui` and
    // `threading` each keep their own private `host_ext`, and a third copy
    // would be one more than the two that already exist.
    if host.is_null() {
        return;
    }
    let Some(get_extension) = (*host).get_extension else {
        return;
    };
    let ptr = get_extension(host, CLAP_EXT_LOG.as_ptr()) as *const clap_host_log;
    if ptr.is_null() {
        return;
    }
    let Some(log) = (*ptr).log else {
        return;
    };
    for _ in 0..count {
        log(host, CLAP_LOG_WARNING, AUDIO_THREAD_LOG_LINE.as_ptr());
    }
}

/// Push the configured SysEx events into the host's output event list.
///
/// Called from `plugin_process` — i.e. from inside the host's `process`, on the
/// host's audio thread, which is what makes this reach the hazard. A host that
/// copies each payload into a fresh `Vec` allocates here, per event, per block.
///
/// # Safety
/// `p` must be the live `clap_process` the host passed to `process`.
pub(crate) unsafe fn emit_sysex_output(p: &clap_process) {
    let count = SYSEX_COUNT.load(Ordering::SeqCst);
    let bytes = SYSEX_BYTES.load(Ordering::SeqCst) as usize;
    if count == 0 || bytes == 0 || p.out_events.is_null() {
        return;
    }
    let Some(try_push) = (*p.out_events).try_push else {
        return;
    };

    for _ in 0..count {
        let ev = clap_event_midi_sysex {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_midi_sysex>() as u32,
                time: 0,
                space_id: clap_sys::events::CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: SYSEX_PAYLOAD.as_ptr(),
            size: bytes as u32,
        };
        // CLAP: the buffer is owned by the *caller* and is only valid for the
        // duration of this call, so a host that wants to keep it must copy.
        // That copy is the hazard.
        try_push(
            p.out_events,
            &ev as *const clap_event_midi_sysex as *const clap_event_header,
        );
    }
}

// ---------------------------------------------------------------------------
// Wide channel layouts (H7)
// ---------------------------------------------------------------------------

/// A port layout wide enough to spill the host's per-side `SmallVec<[*mut T;
/// 16]>` of caller channel pointers.
///
/// These are not contrived widths. 7.1.4 Dolby Atmos is 12 channels; two 8-
/// channel ports is 16; third-order ambisonic is 16. The host advertises
/// surround and ambisonic port types precisely so it can be handed these, so a
/// layout at or past the inline bound is a supported configuration, not an
/// abuse.
///
/// The interesting boundary is **17**, not 16: a `SmallVec<[T; 16]>` holding
/// exactly 16 elements is still inline. A test that only reached 16 would pass
/// against an unfixed host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WideLayout {
    /// Not wide — defer to [`crate::PortLayoutMode`]. The default.
    Off = 0,
    /// One 16-channel port per side: 3rd-order ambisonic, or two 7.1 buses
    /// worth. Exactly at the inline bound, so an unfixed host does **not**
    /// spill — this is the control case that proves the test is measuring the
    /// boundary rather than the mere presence of many channels.
    Exactly16 = 1,
    /// One 20-channel port per side. Past the bound: an unfixed host heap-
    /// allocates two `SmallVec` backing buffers every block.
    Wide20 = 2,
    /// Two ports per side, 12 + 12 — 7.1.4 Atmos twice over. Reaches the same
    /// spill through *port count* rather than a single wide port, so a fix that
    /// only widens the single-port case is still caught.
    Split12Plus12 = 3,
}

impl WideLayout {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Exactly16,
            2 => Self::Wide20,
            3 => Self::Split12Plus12,
            _ => Self::Off,
        }
    }

    /// Per-port channel counts on either side, or `None` when this layout is
    /// not selected (the caller then falls back to [`crate::PortLayoutMode`]).
    pub(crate) fn ports(self) -> Option<&'static [u32]> {
        match self {
            Self::Off => None,
            Self::Exactly16 => Some(&[16]),
            Self::Wide20 => Some(&[20]),
            Self::Split12Plus12 => Some(&[12, 12]),
        }
    }
}

static WIDE_LAYOUT: AtomicU32 = AtomicU32::new(WideLayout::Off as u32);

/// Select a wide channel layout. Call **before** the host loads the plugin —
/// like [`crate::tutti_test_plugin_set_port_layout`], the host reads the audio
/// ports once during load.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_wide_layout(mode: u32) {
    WIDE_LAYOUT.store(mode, Ordering::SeqCst);
}

/// The selected wide layout's per-port channel counts, if any.
pub(crate) fn wide_ports() -> Option<&'static [u32]> {
    WideLayout::from_u32(WIDE_LAYOUT.load(Ordering::SeqCst)).ports()
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------

/// Return every RT-probe switch to its inert default and zero the block
/// counter.
///
/// The switches are process-global and the suites run in one process, so a test
/// that set a mode and did not clear it would silently change the meaning of
/// every later test. Each test in `clap_process_no_alloc.rs` calls this on
/// entry rather than trusting its predecessors.
///
/// Does **not** reset the port/wide layout, because that is read at load time:
/// a test that resets it after loading would be describing a layout the host is
/// no longer using. Layout selection is the loading test's own responsibility.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_reset_rt_probe() {
    STATUS_MODE.store(StatusMode::Continue as u32, Ordering::SeqCst);
    BLOCK_COUNTER.store(0, Ordering::SeqCst);
    SYSEX_COUNT.store(0, Ordering::SeqCst);
    SYSEX_BYTES.store(0, Ordering::SeqCst);
    LOG_LINES_PER_BLOCK.store(0, Ordering::SeqCst);
}
