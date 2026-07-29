//! What the host actually handed the plugin, recorded across the AEffect
//! seam and read back by the conformance test.
//!
//! `#[repr(C)]` throughout: the test may either link the `rlib` and use
//! these types directly, or `dlopen` the cdylib and mirror them. Both must
//! agree on layout, so the layout is pinned rather than left to Rust.

use std::sync::Mutex;

/// One MIDI event as the host delivered it, with the sample offset the
/// host claimed. `delta_frames` is the field the routing tests care about:
/// VST 2.4 requires it be relative to the *current* block, and a host that
/// forgets to rebase absolute timestamps gets caught here.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CapturedEvent {
    /// `VstEvent::deltaFrames` — sample offset into the block.
    pub delta_frames: i32,
    /// `VstEvent::type` (1 = `kVstMidiType`, 6 = `kVstSysExType`).
    pub event_type: i32,
    /// `VstEvent::flags`.
    pub flags: i32,
    /// The three MIDI-1 status/data bytes.
    pub midi_data: [u8; 3],
    /// Padding to keep the struct's C layout obvious across the seam.
    pub _pad: u8,
}

/// Cap on events stored per block. `event_count` records the true count
/// even when it exceeds this, so an overflow is visible rather than
/// silently truncated into a passing assertion.
pub const MAX_CAPTURED_EVENTS: usize = 64;

/// Which entry point the host used to render. VST 2.4 deprecated the
/// accumulating `process`, but hosts still exist that call it — and a host
/// that calls it against a plugin *without* `effFlagsCanReplacing` is doing
/// the right thing, while one that calls `processReplacing` regardless is
/// writing through a slot the plugin never promised.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEntry {
    /// No render observed since load.
    None = 0,
    /// Deprecated accumulating `AEffect::process`.
    Accumulating = 1,
    /// `AEffect::processReplacing` (f32).
    Replacing = 2,
    /// `AEffect::processReplacingF64`.
    ReplacingF64 = 3,
}

/// The snapshot the conformance test reads back.
///
/// Everything here is what the *host* chose, not what the probe declared —
/// the point is to compare the two. `valid` stays false until a render has
/// actually happened, so a test that forgets to call `process` fails loudly
/// instead of asserting against a zeroed struct.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessCapture {
    /// False until the host has rendered at least one block.
    pub valid: bool,
    /// Number of render calls observed since load.
    pub process_calls: u32,
    /// `samples` argument of the most recent render call.
    pub block_size: i32,
    /// Channel count the host's input pointer table was read at — i.e. what
    /// the probe declared via `numInputs`, since `AudioBuffer::from_raw` is
    /// built from the AEffect. Mismatches show up as reads of channels the
    /// host never filled.
    pub input_count: i32,
    /// Same for outputs.
    pub output_count: i32,
    /// Which entry point rendered the most recent block.
    pub entry: ProcessEntry,

    /// Last `effSetSampleRate` the host dispatched, or 0.0 if never.
    pub sample_rate: f32,
    /// Last `effSetBlockSize` the host dispatched, or 0 if never.
    pub max_block_size: i64,
    /// Count of `effMainsChanged value=1` dispatches (resume).
    pub resume_count: u32,
    /// Count of `effMainsChanged value=0` dispatches (suspend).
    pub suspend_count: u32,
    /// Whether `effOpen` was dispatched before any render.
    pub initialized: bool,

    /// Whether `audioMasterGetTime` returned a non-null `VstTimeInfo`.
    /// A host that never answers the transport query is a distinct failure
    /// from one that answers with wrong numbers, so both are recorded.
    pub time_info_present: bool,
    /// `VstTimeInfo::samplePos`.
    pub time_sample_pos: f64,
    /// `VstTimeInfo::tempo`.
    pub time_tempo: f64,
    /// `VstTimeInfo::ppqPos`.
    pub time_ppq_pos: f64,
    /// `VstTimeInfo::barStartPos`.
    pub time_bar_start_pos: f64,
    /// `VstTimeInfo::timeSigNumerator`.
    pub time_sig_numerator: i32,
    /// `VstTimeInfo::timeSigDenominator`.
    pub time_sig_denominator: i32,
    /// `VstTimeInfo::flags` — the validity bitmask. A host that fills a
    /// field but leaves its `…Valid` flag clear has told the plugin to
    /// ignore the value it just supplied.
    pub time_flags: i32,

    /// True count of events in the most recent `effProcessEvents`, even
    /// past [`MAX_CAPTURED_EVENTS`].
    pub event_count: u32,
    /// Cumulative events across every `effProcessEvents` since load.
    pub total_event_count: u32,
    /// The first [`MAX_CAPTURED_EVENTS`] of the most recent batch, in the
    /// order the host presented them.
    pub events: [CapturedEvent; MAX_CAPTURED_EVENTS],
}

impl ProcessCapture {
    /// The all-zero starting state. A `const fn` rather than `Default`
    /// because the static below needs it in const context.
    pub const fn empty() -> Self {
        Self {
            valid: false,
            process_calls: 0,
            block_size: 0,
            input_count: 0,
            output_count: 0,
            entry: ProcessEntry::None,
            sample_rate: 0.0,
            max_block_size: 0,
            resume_count: 0,
            suspend_count: 0,
            initialized: false,
            time_info_present: false,
            time_sample_pos: 0.0,
            time_tempo: 0.0,
            time_ppq_pos: 0.0,
            time_bar_start_pos: 0.0,
            time_sig_numerator: 0,
            time_sig_denominator: 0,
            time_flags: 0,
            event_count: 0,
            total_event_count: 0,
            events: [CapturedEvent {
                delta_frames: 0,
                event_type: 0,
                flags: 0,
                midi_data: [0; 3],
                _pad: 0,
            }; MAX_CAPTURED_EVENTS],
        }
    }
}

impl Default for ProcessCapture {
    fn default() -> Self {
        Self::empty()
    }
}

/// Process-global capture. One loaded image per path (both `dlopen` and the
/// host's own load resolve to the same image), so a single global is the
/// simplest correct channel between the plugin and the test — and it is
/// strictly test-only code.
///
/// A `Mutex` rather than atomics: the capture is written on the audio
/// thread, which would normally forbid a lock, but there is no audio thread
/// here — the conformance test drives `process` synchronously. The
/// no-alloc regression tests deliberately do not use this probe.
pub(crate) static CAPTURE: Mutex<ProcessCapture> = Mutex::new(ProcessCapture::empty());

/// Run `f` against the global capture, recovering from a poisoned lock so a
/// panic in one test cannot cascade into every later one.
pub(crate) fn with_capture<R>(f: impl FnOnce(&mut ProcessCapture) -> R) -> R {
    let mut guard = CAPTURE.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut guard)
}

/// Copy the latest capture out to `out`; returns whether a render has been
/// observed since load.
///
/// This is the read side of the dlopen seam: the conformance test resolves
/// this symbol out of the same image the host loaded, so it sees exactly
/// what the host's `process` call produced.
///
/// # Safety
/// `out` must point to a valid, writable [`ProcessCapture`].
#[no_mangle]
pub unsafe extern "C" fn tutti_vst2_probe_capture(out: *mut ProcessCapture) -> bool {
    if out.is_null() {
        return false;
    }
    with_capture(|cap| {
        *out = *cap;
        cap.valid
    })
}

/// Reset the capture to its starting state.
///
/// Tests run in parallel threads against one shared global, so a test that
/// wants to assert "the host called `process` exactly once" must be able to
/// start from a known zero. The conformance harness pairs this with a mutex
/// around the whole drive→read sequence.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_reset_capture() {
    with_capture(|cap| *cap = ProcessCapture::empty());
}
