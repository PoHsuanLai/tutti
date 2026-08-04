//! Reference VST2 plugin — a host-conformance probe.
//!
//! **Not** a usable audio effect. It is a test oracle:
//! `out[ch][i] = in[ch][i] + channel_tag(ch)`, a distinct DC offset per
//! channel (see [`config::channel_tag`]), so a host that swaps channels,
//! duplicates one across both, or writes into the wrong output slot produces
//! arithmetically wrong samples. Structural checks — non-null pointers,
//! agreeing counts — pass just as happily on misrouted audio, which is why
//! the tag exists. Alongside that it records what the host handed it across
//! the `AEffect` FFI into a process-global [`ProcessCapture`], read back
//! through [`tutti_vst2_probe_capture`]. The loader dedupes images by path,
//! so the host's load and a test's `dlopen` share one image and one global.
//!
//! On demand it also misbehaves — `effCanDo` answering `-1`, counts larger
//! than it services, a refused resume, tail sizes of 0 / 1 / large, a missing
//! `effFlagsCanReplacing`, channel counts inconsistent with what it reads and
//! writes. A corpus of well-behaved plugins proves little about host
//! robustness. See `README.md` for the table and the constraints on adding
//! to it.
//!
//! Construction-time metadata comes from `TUTTI_VST2_PROBE_*` env vars
//! (`config.rs`), runtime behaviour from `extern "C"` switches
//! (`switches.rs`); each file documents why its half falls on that side.

extern crate vst_tutti as vst;

pub mod capture;
pub mod config;
pub mod switches;

use std::sync::Arc;

use num_traits::Float;
use vst::api::{self, AEffect, HostCallbackProc, Supported};
use vst::buffer::AudioBuffer;
use vst::editor::Editor;
use vst::host::Host as _;
use vst::plugin::{CanDo, HostCallback, Info, Plugin, PluginParameters};

pub use capture::{
    tutti_vst2_probe_capture, tutti_vst2_probe_reset_capture, CapturedEvent, ProcessCapture,
    ProcessEntry, MAX_CAPTURED_EVENTS,
};
pub use config::{channel_tag, ProbeConfig};
pub use switches::CanDoAnswer;

/// `AEffect::uniqueId` the probe advertises; the host derives its plugin id
/// (`vst2.<unique_id>`) from it. Spells "TPRB" in VST2's four-character-code
/// convention.
pub const PROBE_UNIQUE_ID: i32 = i32::from_be_bytes(*b"TPRB");

/// `AEffect::version`, and the string the host reports.
pub const PROBE_VERSION: i32 = 1000;

/// Plugin name the probe reports through `effGetEffectName`.
pub const PROBE_NAME: &str = "Tutti VST2 Probe";

/// Vendor string the probe reports.
pub const PROBE_VENDOR: &str = "Tutti";

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Parameter store.
///
/// `serviced` is the count the store will actually answer for, which may be
/// *below* what the AEffect advertises. That gap is the enumeration hole: a
/// host that walks `0..numParams` and trusts every answer reads names and
/// values the plugin never had. Out-of-range indices answer safely (empty
/// name, `None` value) rather than panicking — a crash *inside the plugin*
/// reads as a host bug.
///
/// Mutex rather than atomics because `PluginParameters` takes `&self` and
/// there is no audio thread here to keep lock-free.
struct ProbeParameters {
    values: std::sync::Mutex<Vec<f32>>,
    serviced: i32,
    serviced_programs: i32,
    current_program: std::sync::atomic::AtomicI32,
    /// Backing store for `effGetChunk` / `effSetChunk` round-trips.
    chunk: std::sync::Mutex<Vec<u8>>,
}

impl ProbeParameters {
    fn new(serviced: i32, serviced_programs: i32) -> Self {
        let n = serviced.max(0) as usize;
        Self {
            // Distinct, recognisable starting values: parameter i starts at
            // i/16, so a host that returns the wrong index is visible in the
            // value rather than only in the name.
            values: std::sync::Mutex::new((0..n).map(|i| i as f32 / 16.0).collect()),
            serviced,
            serviced_programs,
            current_program: std::sync::atomic::AtomicI32::new(0),
            chunk: std::sync::Mutex::new(b"tutti-vst2-probe-chunk-v1".to_vec()),
        }
    }

    fn in_range(&self, index: i32) -> bool {
        index >= 0 && index < self.serviced
    }
}

impl PluginParameters for ProbeParameters {
    fn get_parameter(&self, index: i32) -> Option<f32> {
        if !self.in_range(index) {
            // Not `Some(0.0)` — the fork made this an `Option` so "no such
            // parameter" and "the value is zero" cannot collapse.
            //
            // But the `None` does not survive to the host: `getParameter`
            // returns a bare `float`, so vst-rs flattens it to `0.0` and the
            // host reads `Some(0.0)` across the whole hole. Tests must assert
            // on the string opcodes instead — out-of-range
            // `get_parameter_name` / `_label` answer empty below and
            // `can_be_automated` answers false.
            return None;
        }
        let values = self.values.lock().unwrap_or_else(|p| p.into_inner());
        values.get(index as usize).copied()
    }

    fn set_parameter(&self, index: i32, value: f32) -> bool {
        if !self.in_range(index) {
            return false;
        }
        let mut values = self.values.lock().unwrap_or_else(|p| p.into_inner());
        match values.get_mut(index as usize) {
            Some(slot) => {
                *slot = value.clamp(0.0, 1.0);
                true
            }
            None => false,
        }
    }

    fn get_parameter_name(&self, index: i32) -> String {
        if !self.in_range(index) {
            return String::new();
        }
        format!("Probe {index}")
    }

    fn get_parameter_label(&self, index: i32) -> String {
        if !self.in_range(index) {
            return String::new();
        }
        // Non-uniform, so a host handing back one shared buffer for every
        // index is caught.
        ["dB", "Hz", "%", "ms"][(index as usize) % 4].to_string()
    }

    fn get_parameter_text(&self, index: i32) -> String {
        match self.get_parameter(index) {
            Some(v) => format!("{v:.3}"),
            None => String::new(),
        }
    }

    fn can_be_automated(&self, index: i32) -> bool {
        self.in_range(index)
    }

    fn get_preset_num(&self) -> i32 {
        self.current_program
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn change_preset(&self, preset: i32) {
        if preset >= 0 && preset < self.serviced_programs {
            self.current_program
                .store(preset, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn get_preset_name(&self, preset: i32) -> String {
        if preset < 0 || preset >= self.serviced_programs {
            return String::new();
        }
        format!("Probe Program {preset}")
    }

    fn get_bank_data(&self) -> Vec<u8> {
        self.chunk.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn load_bank_data(&self, data: &[u8]) -> bool {
        // Reject an empty blob rather than storing it: `effSetChunk` returns
        // 1 on success, and the probe needs at least one input whose refusal
        // a host can be seen ignoring.
        if data.is_empty() {
            return false;
        }
        *self.chunk.lock().unwrap_or_else(|p| p.into_inner()) = data.to_vec();
        true
    }

    fn get_preset_data(&self) -> Vec<u8> {
        self.get_bank_data()
    }

    fn load_preset_data(&self, data: &[u8]) -> bool {
        self.load_bank_data(data)
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// A do-nothing editor. Its only job is to exist, because `vst::main` keys
/// `effFlagsHasEditor` off `get_editor()` returning `Some`. It never opens a
/// window: the suite runs headless, and realising an X11 surface would fail
/// on CI for reasons unrelated to the host.
struct ProbeEditor;

impl Editor for ProbeEditor {
    fn size(&self) -> (i32, i32) {
        (320, 240)
    }

    fn position(&self) -> (i32, i32) {
        (0, 0)
    }

    fn open(&mut self, _parent: *mut std::os::raw::c_void) -> bool {
        // Refuse rather than pretend: a host that reads this as success and
        // sizes a window around a nonexistent surface is a finding.
        false
    }

    fn close(&mut self) {}

    fn is_open(&mut self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

struct ProbePlugin {
    host: HostCallback,
    config: ProbeConfig,
    params: Arc<ProbeParameters>,
}

impl ProbePlugin {
    /// Snapshot the host's `audioMasterGetTime` answer into the capture.
    ///
    /// Asked with every validity flag set, so the capture records which ones
    /// the host *returns* — otherwise a host that fills only what it was
    /// asked for is indistinguishable from one that fills nothing.
    fn capture_time_info(&self) {
        let mask = api::TimeInfoFlags::all().bits();
        let info = self.host.get_time_info(mask);
        capture::with_capture(|cap| match info {
            Some(t) => {
                cap.time_info_present = true;
                cap.time_sample_pos = t.sample_pos;
                cap.time_tempo = t.tempo;
                cap.time_ppq_pos = t.ppq_pos;
                cap.time_bar_start_pos = t.bar_start_pos;
                cap.time_sig_numerator = t.time_sig_numerator;
                cap.time_sig_denominator = t.time_sig_denominator;
                cap.time_flags = t.flags;
            }
            None => cap.time_info_present = false,
        });
    }

    /// Record the geometry of a render call and return whether the probe
    /// should actually write output.
    fn begin_process(&self, entry: ProcessEntry, samples: usize) -> bool {
        capture::with_capture(|cap| {
            cap.valid = true;
            cap.process_calls = cap.process_calls.saturating_add(1);
            cap.block_size = samples as i32;
            cap.input_count = self.config.inputs;
            cap.output_count = self.config.outputs;
            cap.entry = entry;
        });
        self.capture_time_info();

        // A refused resume renders silence — see
        // `tutti_vst2_probe_set_refuse_resume` for why in substance rather
        // than by return code.
        !switches::silent_process() && switches::is_resumed()
    }
}

impl Plugin for ProbePlugin {
    fn new(host: HostCallback) -> Self {
        let config = ProbeConfig::from_env();
        let params = Arc::new(ProbeParameters::new(
            config.serviced_parameters,
            config.serviced_programs,
        ));
        Self {
            host,
            config,
            params,
        }
    }

    fn get_info(&self) -> Info {
        Info {
            name: PROBE_NAME.to_string(),
            vendor: PROBE_VENDOR.to_string(),
            unique_id: PROBE_UNIQUE_ID,
            version: PROBE_VERSION,
            // The *declared* counts, which may exceed what `ProbeParameters`
            // services: that gap is the enumeration hole.
            parameters: self.config.parameters,
            presets: self.config.programs,
            inputs: self.config.inputs,
            outputs: self.config.outputs,
            midi_inputs: self.config.midi_inputs,
            midi_outputs: self.config.midi_outputs,
            category: self.config.category,
            // `get_info` is the in-process path, where the host reads this
            // struct directly rather than dispatching `effGetPlugCategory`, so
            // the code must agree with the enum beside it.
            category_code: self.config.category as i32,
            initial_delay: self.config.initial_delay,
            preset_chunks: self.config.preset_chunks,
            f64_precision: self.config.f64_precision,
            silent_when_stopped: false,
        }
    }

    fn init(&mut self) {
        capture::with_capture(|cap| cap.initialized = true);
    }

    fn set_sample_rate(&mut self, rate: f32) {
        capture::with_capture(|cap| cap.sample_rate = rate);
    }

    fn set_block_size(&mut self, size: i64) {
        capture::with_capture(|cap| cap.max_block_size = size);
    }

    fn set_precision(&mut self, double: bool) {
        capture::with_capture(|cap| {
            cap.set_precision_count = cap.set_precision_count.saturating_add(1);
            cap.set_precision_value = double as i32;
        });
    }

    fn resume(&mut self) {
        capture::with_capture(|cap| cap.resume_count = cap.resume_count.saturating_add(1));
        // `effMainsChanged` has no failure return, so a refusal is expressed
        // in behaviour: stay suspended, render silence. The count above still
        // increments, separating "host never resumed" from "plugin declined".
        switches::set_resumed(!switches::refuse_resume());
    }

    fn suspend(&mut self) {
        capture::with_capture(|cap| cap.suspend_count = cap.suspend_count.saturating_add(1));
        switches::set_resumed(false);
    }

    fn can_do(&self, can_do: CanDo) -> Supported {
        // Answers every query alike, including the receiveVstMidiEvent /
        // sendVstMidiEvent pair the host asks during `Vst2Instance::load`,
        // so a test can watch `PluginInfo::receives_midi` flip.
        //
        // VST 2.4: 1 = yes, 0 = don't know, -1 = explicitly no. Reading `No`
        // as non-zero-so-truthy, or as equal to "don't know", is the bug.
        let _ = can_do;
        match switches::can_do_answer() {
            CanDoAnswer::Yes => Supported::Yes,
            CanDoAnswer::Maybe => Supported::Maybe,
            CanDoAnswer::No => Supported::No,
            CanDoAnswer::Custom => Supported::Custom(switches::can_do_custom_value()),
        }
    }

    fn get_tail_size(&self) -> isize {
        // Only reached when `raw_tail_size` is unset; otherwise the raw
        // dispatcher installed by `VSTPluginMain` intercepts `effGetTailSize`
        // before it gets here (vst-rs rewrites a trait-reported 0 into 1,
        // which erases the distinction the test needs).
        0
    }

    fn get_parameter_object(&mut self) -> Arc<dyn PluginParameters> {
        Arc::clone(&self.params) as Arc<dyn PluginParameters>
    }

    fn get_editor(&mut self) -> Option<Box<dyn Editor>> {
        if self.config.has_editor {
            Some(Box::new(ProbeEditor))
        } else {
            None
        }
    }

    fn process_events(&mut self, events: &api::Events) {
        let n = events.num_events.max(0) as usize;
        capture::with_capture(|cap| {
            cap.event_count = n as u32;
            cap.total_event_count = cap.total_event_count.saturating_add(n as u32);
            let stored = n.min(MAX_CAPTURED_EVENTS);
            for slot in cap.events.iter_mut() {
                *slot = CapturedEvent::default();
            }
            for i in 0..stored {
                // SAFETY: `Events` is a flexible-array header — declared
                // `[*mut Event; 2]`, but the host allocates `num_events`
                // entries contiguously after it, so reading
                // `i < num_events` off the base is the defined access. No
                // clamp: a host that lies about `num_events` is a finding,
                // and clamping would hide it.
                let ev_ptr = unsafe { *events.events.as_ptr().add(i) };
                if ev_ptr.is_null() {
                    continue;
                }
                let ev = unsafe { &*ev_ptr };
                let mut captured = CapturedEvent {
                    delta_frames: ev.delta_frames,
                    event_type: ev.event_type as i32,
                    flags: ev._flags,
                    ..CapturedEvent::default()
                };
                if matches!(ev.event_type, api::EventType::Midi) {
                    let midi = unsafe { &*(ev_ptr as *const api::MidiEvent) };
                    captured.midi_data = midi.midi_data;
                    captured.flags = midi.flags;
                }
                cap.events[i] = captured;
            }
        });
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        let samples = buffer.samples();
        if !self.begin_process(ProcessEntry::Replacing, samples) {
            return;
        }
        write_tagged(buffer, samples, self.config.outputs as usize);
    }

    fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        let samples = buffer.samples();
        if !self.begin_process(ProcessEntry::ReplacingF64, samples) {
            return;
        }
        write_tagged(buffer, samples, self.config.outputs as usize);
    }
}

/// The oracle: `out[ch][i] = in[ch][i] + channel_tag(ch)`.
///
/// Generic over the sample type so the f32 and f64 entry points cannot drift
/// — a divergence would look like a host bug in whichever was tested second.
///
/// Output channels with no matching input get the tag alone, so an
/// asymmetric layout is still checkable.
fn write_tagged<S: Float>(buffer: &mut AudioBuffer<S>, samples: usize, declared_outputs: usize) {
    let (inputs, mut outputs) = buffer.split();
    let in_count = inputs.len();
    let out_count = outputs.len();

    // Read one channel past `numInputs` when the switch is on. Guarded by
    // `in_count` so the honest configuration never does it.
    let read_channels = if switches::read_extra_input() {
        in_count + 1
    } else {
        in_count
    };
    let write_channels = if switches::write_extra_output() {
        out_count + 1
    } else {
        out_count.min(declared_outputs.max(out_count))
    };

    for ch in 0..write_channels {
        // Unreachable — every tag is small and exactly representable. Zero
        // over a panic because a render call has no `catch_unwind` above it;
        // a disarmed oracle fails visibly in the test's assertions instead.
        let tag = S::from(channel_tag(ch)).unwrap_or_else(S::zero);
        // `get_mut` panics past the end, which is what the write-extra switch
        // asks for. A render call has no `catch_unwind`, so that switch is
        // crash-capable and off by default.
        let out = outputs.get_mut(ch);
        if ch < read_channels && ch < in_count {
            let inp = inputs.get(ch);
            for i in 0..samples {
                out[i] = inp[i] + tag;
            }
        } else {
            out[..samples].fill(tag);
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Raw dispatcher installed over vst-rs's, for opcodes the `Plugin` trait
/// cannot express. Today that is `effGetTailSize` only: vst-rs rewrites a
/// trait-reported `0` into `1`, collapsing two distinct VST 2.4 answers —
/// `0` = "no tail information, host must decide", `1` = "no tail at all, stop
/// rendering immediately". A host conflating them either truncates reverb
/// tails or renders silence forever.
///
/// Everything else falls through to the original pointer, so the probe stays
/// a vst-rs plugin rather than a hand-rolled AEffect.
static mut INNER_DISPATCHER: Option<api::DispatcherProc> = None;
static mut RAW_TAIL_SIZE: Option<isize> = None;

/// `effGetTailSize`. Mirrored here rather than imported because vst-rs's
/// `plugin::OpCode` is `#[doc(hidden)]` and its discriminants are the wire
/// contract regardless.
const EFF_GET_TAIL_SIZE: i32 = 52;

/// `effGetParameterProperties`. vst-rs names it `GetParamInfo` and leaves the
/// struct unimplemented, so the probe answers it raw.
const EFF_GET_PARAMETER_PROPERTIES: i32 = 56;

/// The MIDI-metadata family. vst-rs has all five as `//TODO: Implement`, so
/// there is no trait path for any of them.
const EFF_GET_MIDI_PROGRAM_NAME: i32 = 62;
const EFF_GET_CURRENT_MIDI_PROGRAM: i32 = 63;
const EFF_GET_MIDI_PROGRAM_CATEGORY: i32 = 64;
const EFF_HAS_MIDI_PROGRAMS_CHANGED: i32 = 65;
const EFF_GET_MIDI_KEY_NAME: i32 = 66;

/// The parameter index the probe reports an integer range for, so a test can
/// distinguish "answered with a range" from "answered without one" on the same
/// plugin. Any other index answers with only the float-step group set.
pub const PROBE_INT_STEP_PARAM: i32 = 1;

/// Integer range the probe reports for [`PROBE_INT_STEP_PARAM`]. Chosen as a
/// MIDI-ish 0..127 with a 12-step coarse increment so a wrong field pairing
/// (min/max swapped, step read from large_step) yields visibly wrong numbers.
pub const PROBE_INT_RANGE: (i32, i32, i32, i32) = (0, 127, 1, 12);

/// Category the probe reports for [`PROBE_INT_STEP_PARAM`]. 1-based, matching
/// VST2 — `2` rather than `1` so a decoder that returns a hardcoded or
/// off-by-one category is caught.
pub const PROBE_PARAM_CATEGORY: i16 = 2;

/// MIDI program-change / bank pair the probe reports for program 0. The bank is
/// a real pair so the sentinel handling can be tested against a live value.
pub const PROBE_MIDI_PROGRAM: (u8, u8, u8) = (32, 1, 3);

/// The key the probe names, and the name. 36 is GM's kick drum.
pub const PROBE_NAMED_KEY: i32 = 36;
pub const PROBE_KEY_NAME: &str = "Kick";

/// Write a `&str` into a plugin-side NUL-padded fixed field.
///
/// Truncates rather than panicking on an over-long name: the probe must not
/// abort inside a dispatch, because a crash *in the plugin* reads as a host bug.
fn write_field(dst: &mut [u8], text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    if n < dst.len() {
        dst[n] = 0;
    }
}

/// Write `prefix` followed by `n` into a fixed field, without allocating.
///
/// `format!` would be the obvious way to build these names, but it allocates —
/// and these run inside `extern "C" fn probe_dispatch`, which has no
/// `catch_unwind` above it. An allocation failure there unwinds across the C
/// ABI, which is undefined behaviour rather than a test failure. Formatting
/// into a fixed stack buffer removes the possibility instead of guarding it.
fn write_numbered_field(dst: &mut [u8], prefix: &str, n: i32) {
    // Enough for any prefix used here plus a full i32 and the NUL.
    let mut buf = [0u8; 64];
    let mut len = 0;

    for &b in prefix.as_bytes() {
        if len == buf.len() {
            break;
        }
        buf[len] = b;
        len += 1;
    }

    // Decimal digits, most significant first. `abs()` on i32::MIN would
    // overflow, so go through i64.
    let value = n as i64;
    if value < 0 && len < buf.len() {
        buf[len] = b'-';
        len += 1;
    }
    let magnitude = value.unsigned_abs();
    let mut digits = [0u8; 20];
    let mut ndigits = 0;
    let mut rest = magnitude;
    loop {
        digits[ndigits] = b'0' + (rest % 10) as u8;
        ndigits += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    while ndigits > 0 && len < buf.len() {
        ndigits -= 1;
        buf[len] = digits[ndigits];
        len += 1;
    }

    // SAFETY-free: every byte written is ASCII, so the slice is valid UTF-8.
    write_field(dst, std::str::from_utf8(&buf[..len]).unwrap_or(""));
}

/// Answer `effGetParameterProperties` for `index`.
///
/// # Safety
/// `ptr` must be a valid, writable `*mut api::ParameterProperties` the host
/// owns — which is what the opcode's contract requires.
unsafe fn answer_parameter_properties(index: i32, ptr: *mut std::os::raw::c_void) -> isize {
    if ptr.is_null() {
        return 0;
    }
    let props = &mut *(ptr as *mut api::ParameterProperties);

    write_numbered_field(&mut props.label, "Probe Param ", index);
    write_numbered_field(&mut props.short_label, "P", index);

    if index == PROBE_INT_STEP_PARAM {
        let (min, max, step, large) = PROBE_INT_RANGE;
        props.min_integer = min;
        props.max_integer = max;
        props.step_integer = step;
        props.large_step_integer = large;
        props.category = PROBE_PARAM_CATEGORY;
        props.num_parameters_in_category = 2;
        write_field(&mut props.category_label, "Filter");
        props.display_index = 3;
        props.flags = (api::ParameterFlags::USES_INT_STEP
            | api::ParameterFlags::USES_CATEGORY
            | api::ParameterFlags::USES_INDEX)
            .bits();
    } else {
        // Deliberately leave the integer fields at values a host must NOT
        // read: the flag says they are invalid, and a host that ignores the
        // gate picks these up as a real range.
        props.min_integer = 999;
        props.max_integer = -999;
        props.step_float = 0.25;
        props.small_step_float = 0.05;
        props.large_step_float = 0.5;
        props.flags = api::ParameterFlags::USES_FLOAT_STEP.bits();
    }

    1
}

/// Answer `effGetMidiProgramName` / `effGetCurrentMidiProgram`.
///
/// # Safety
/// `ptr` must be a valid, writable `*mut api::MidiProgramName`.
unsafe fn answer_midi_program_name(ptr: *mut std::os::raw::c_void) -> Option<i32> {
    if ptr.is_null() {
        return None;
    }
    let name = &mut *(ptr as *mut api::MidiProgramName);

    // The host wrote the query here; the probe honours it rather than always
    // describing program 0, so a host that forgets to write it is detectable.
    let requested = name.this_program_index;

    // Name only the programs actually serviced. `serviced_midi_programs` may
    // advertise more than this — that gap is the enumeration hole.
    if !(0..NAMED_MIDI_PROGRAMS).contains(&requested) {
        return None;
    }

    write_numbered_field(&mut name.name, "Probe Program ", requested);
    let (program, msb, lsb) = PROBE_MIDI_PROGRAM;
    name.midi_program = program.wrapping_add(requested as u8);
    name.midi_bank_msb = msb;
    name.midi_bank_lsb = lsb;
    name.parent_category_index = -1;
    // Mark program 1 a drum kit so the key-name path has a reason to be
    // queried, and program 0 not, so the flag is proven to vary.
    name.flags = if requested == 1 {
        api::MidiProgramFlags::IS_OMNI.bits()
    } else {
        0
    };

    Some(requested)
}

/// How many MIDI programs the probe will actually *name*, regardless of the
/// count it advertises. Two, so a test can walk more than one and still have a
/// hole above them.
pub const NAMED_MIDI_PROGRAMS: i32 = 2;

/// Answer `effGetMidiProgramCategory`.
///
/// # Safety
/// `ptr` must be a valid, writable `*mut api::MidiProgramCategory`.
unsafe fn answer_midi_program_category(ptr: *mut std::os::raw::c_void) -> isize {
    if ptr.is_null() {
        return 0;
    }
    let cat = &mut *(ptr as *mut api::MidiProgramCategory);
    let requested = cat.this_category_index;
    if requested != 0 {
        return 0;
    }
    write_field(&mut cat.name, "Probe Category");
    cat.parent_category_index = -1;
    1
}

/// Answer `effGetMidiKeyName`, naming exactly one key.
///
/// Naming one key rather than all 128 is the point: a host must omit the
/// unnamed ones rather than filling them with blanks.
///
/// # Safety
/// `ptr` must be a valid, writable `*mut api::MidiKeyName`.
unsafe fn answer_midi_key_name(ptr: *mut std::os::raw::c_void) -> isize {
    if ptr.is_null() {
        return 0;
    }
    let key = &mut *(ptr as *mut api::MidiKeyName);
    if key.this_key_number != PROBE_NAMED_KEY {
        return 0;
    }
    write_field(&mut key.keyname, PROBE_KEY_NAME);
    1
}

extern "C" fn probe_dispatch(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut std::os::raw::c_void,
    opt: f32,
) -> isize {
    // SAFETY: both statics are written exactly once, in `VSTPluginMain`,
    // before the host has any pointer it could dispatch through. Reads
    // afterwards are on host threads against immutable data.
    if opcode == EFF_GET_TAIL_SIZE {
        if let Some(tail) = unsafe { RAW_TAIL_SIZE } {
            return tail;
        }
    }

    // Parameter properties and the MIDI-metadata family have no `Plugin` trait
    // path in vst-rs (all six are `//TODO: Implement`), so they are answered
    // here or not at all. Each is behind a switch that is OFF by default, so
    // the probe's default behaviour matches the measured real-world one:
    // decline everything.
    //
    // SAFETY (all arms): `ptr` is the host-owned buffer the opcode's contract
    // requires, and each helper null-checks before writing.
    if opcode == EFF_GET_PARAMETER_PROPERTIES && switches::answer_param_properties() {
        return unsafe { answer_parameter_properties(index, ptr) };
    }

    if switches::answer_midi_metadata() {
        match opcode {
            EFF_GET_MIDI_PROGRAM_NAME => {
                return match unsafe { answer_midi_program_name(ptr) } {
                    // The advertised count, which may exceed what is named.
                    Some(_) => switches::serviced_midi_programs() as isize,
                    None => 0,
                };
            }
            EFF_GET_CURRENT_MIDI_PROGRAM => {
                // Report program 0 as current. The return value is the index,
                // not a boolean — a host testing `!= 0` reads this as failure.
                return match unsafe { answer_midi_program_name(ptr) } {
                    Some(index) => index as isize,
                    None => -1,
                };
            }
            EFF_GET_MIDI_PROGRAM_CATEGORY => {
                return unsafe { answer_midi_program_category(ptr) };
            }
            EFF_HAS_MIDI_PROGRAMS_CHANGED => return 1,
            EFF_GET_MIDI_KEY_NAME => return unsafe { answer_midi_key_name(ptr) },
            _ => {}
        }
    }

    match unsafe { INNER_DISPATCHER } {
        Some(inner) => inner(effect, opcode, index, value, ptr, opt),
        None => 0,
    }
}

/// The VST2 entry point. Hand-written rather than `plugin_main!` because two
/// misbehaviours live *outside* the `Plugin` trait:
///
/// - `effFlagsCanReplacing`: `vst::main` sets it unconditionally, so the bit
///   can only be cleared after the AEffect is built. Asks whether the host
///   falls back to the deprecated accumulating `process` or calls
///   `processReplacing` anyway, through a slot the plugin never promised.
/// - `effGetTailSize`: see [`probe_dispatch`].
///
/// Both edits land before the host receives the AEffect, so the intermediate
/// state is unobservable.
///
/// # Safety
/// `callback` must be the host's `audioMaster` function pointer.
#[allow(non_snake_case)]
#[no_mangle]
pub extern "C" fn VSTPluginMain(callback: HostCallbackProc) -> *mut AEffect {
    let effect = vst::main::<ProbePlugin>(callback);
    if effect.is_null() {
        return effect;
    }

    let config = ProbeConfig::from_env();

    // SAFETY: `vst::main` returned a live `Box::into_raw` AEffect that no
    // other thread has seen yet — the host receives it only when this
    // function returns.
    unsafe {
        if config.omit_can_replacing {
            (*effect).flags &= !api::PluginFlags::CAN_REPLACING.bits();
        }

        RAW_TAIL_SIZE = config.raw_tail_size;

        // Install the raw dispatcher unconditionally.
        //
        // It used to go in only when `raw_tail_size` was set, which was fine
        // while `effGetTailSize` was its only job — that answer comes from a
        // construction-time env var. The parameter-properties and
        // MIDI-metadata answers are behind *runtime* switches flipped after
        // load, so a dispatcher installed conditionally at construction can
        // never see them: the switch would flip and nothing would read it.
        // Chaining to `INNER_DISPATCHER` keeps every other opcode on vst-rs's
        // path, so this is transparent when no switch is set.
        INNER_DISPATCHER = (*effect).dispatcher;
        (*effect).dispatcher = Some(probe_dispatch);
    }

    effect
}

/// macOS's historical entry-point name. Kept in step with `VSTPluginMain`
/// by delegating rather than duplicating.
#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "system" fn main_macho(callback: HostCallbackProc) -> *mut AEffect {
    VSTPluginMain(callback)
}

/// Windows's historical entry-point name.
#[cfg(target_os = "windows")]
#[allow(non_snake_case)]
#[no_mangle]
pub extern "system" fn MAIN(callback: HostCallbackProc) -> *mut AEffect {
    VSTPluginMain(callback)
}
