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
        if let Some(tail) = config.raw_tail_size {
            RAW_TAIL_SIZE = Some(tail);
            INNER_DISPATCHER = (*effect).dispatcher;
            (*effect).dispatcher = Some(probe_dispatch);
        }
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
