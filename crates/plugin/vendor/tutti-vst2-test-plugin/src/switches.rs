//! Runtime behaviour switches, flipped by the test across the dlopen seam.
//!
//! Everything here is something a *loaded, running* plugin can change its
//! mind about, so unlike `config.rs` these need not be in place before
//! `VSTPluginMain` runs. The test resolves the symbol out of the same image
//! the host loaded and calls it directly.
//!
//! Prefer a switch to an env var wherever the timing permits: an env var is
//! process-global *and* sticky for the life of the image, so one a test
//! forgets to unset silently poisons every later test in the binary.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};

/// What the probe answers to `effCanDo`.
///
/// VST 2.4 defines three answers and hosts routinely collapse them into two:
/// `1` = yes, `0` = "don't know, assume the default", `-1` = **explicitly
/// no**. Treating `-1` as truthy (it is non-zero) or as equal to `0` (it is
/// not "unknown") is the bug this hunts. `Custom` exists because real plugins
/// have returned other integers, which must not read as success either.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanDoAnswer {
    /// `1` — supported.
    Yes = 0,
    /// `0` — unknown / not answered.
    Maybe = 1,
    /// `-1` — explicitly unsupported.
    No = 2,
    /// Answer with [`can_do_custom_value`] verbatim.
    Custom = 3,
}

impl CanDoAnswer {
    fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Maybe,
            2 => Self::No,
            3 => Self::Custom,
            _ => Self::Yes,
        }
    }
}

static CAN_DO_ANSWER: AtomicI32 = AtomicI32::new(CanDoAnswer::Yes as i32);
static CAN_DO_CUSTOM: AtomicIsize = AtomicIsize::new(0);
static REFUSE_RESUME: AtomicBool = AtomicBool::new(false);
static SILENT_PROCESS: AtomicBool = AtomicBool::new(false);
static WRITE_EXTRA_OUTPUT: AtomicBool = AtomicBool::new(false);
static READ_EXTRA_INPUT: AtomicBool = AtomicBool::new(false);
static RESUMED: AtomicBool = AtomicBool::new(false);
static ANSWER_PARAM_PROPERTIES: AtomicBool = AtomicBool::new(false);
static ANSWER_MIDI_METADATA: AtomicBool = AtomicBool::new(false);
static SERVICED_MIDI_PROGRAMS: AtomicI32 = AtomicI32::new(0);

/// Make the probe answer `effGetParameterProperties`.
///
/// Off by default, because *declining is the realistic behaviour*: every VST2
/// plugin installed on the development machine (TAL-NoiseMaker, TAL-Reverb-4,
/// TDR Nova) answers `0` for every parameter. The default probe therefore
/// models the common case, and a test opts in to the rare plugin that answers.
///
/// Without this switch the host's decode path is unreachable from any test on
/// this machine — the opcode would be implemented and never executed.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_answer_param_properties(enable: bool) {
    ANSWER_PARAM_PROPERTIES.store(enable, Ordering::SeqCst);
}

pub(crate) fn answer_param_properties() -> bool {
    ANSWER_PARAM_PROPERTIES.load(Ordering::SeqCst)
}

/// Make the probe answer the MIDI-metadata family (`effGetMidiProgramName`,
/// `effGetCurrentMidiProgram`, `effGetMidiProgramCategory`,
/// `effHasMidiProgramsChanged`, `effGetMidiKeyName`).
///
/// `serviced_programs` is the count `effGetMidiProgramName` reports. Setting it
/// *above* the number of programs the probe will actually name reproduces the
/// enumeration hole the parameter/preset switches already model on their axes:
/// a host that trusts the advertised count and walks it reads names the plugin
/// never had.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_answer_midi_metadata(enable: bool, serviced_programs: i32) {
    ANSWER_MIDI_METADATA.store(enable, Ordering::SeqCst);
    SERVICED_MIDI_PROGRAMS.store(serviced_programs, Ordering::SeqCst);
}

pub(crate) fn answer_midi_metadata() -> bool {
    ANSWER_MIDI_METADATA.load(Ordering::SeqCst)
}

pub(crate) fn serviced_midi_programs() -> i32 {
    SERVICED_MIDI_PROGRAMS.load(Ordering::SeqCst)
}

/// Set the `effCanDo` answer. `answer` is a [`CanDoAnswer`] discriminant;
/// `custom` is the raw value used only when `answer` is `Custom`.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_can_do(answer: i32, custom: isize) {
    CAN_DO_ANSWER.store(answer, Ordering::SeqCst);
    CAN_DO_CUSTOM.store(custom, Ordering::SeqCst);
}

pub(crate) fn can_do_answer() -> CanDoAnswer {
    CanDoAnswer::from_i32(CAN_DO_ANSWER.load(Ordering::SeqCst))
}

pub(crate) fn can_do_custom_value() -> isize {
    CAN_DO_CUSTOM.load(Ordering::SeqCst)
}

/// Make the probe refuse to enter the resumed state.
///
/// `effMainsChanged` has no failure return — every host ignores the
/// dispatcher's answer, and vst-rs's `Plugin::resume` returns `()`
/// accordingly — so the refusal is expressed *in substance*: stay suspended
/// and render silence, as a plugin whose device or licence claim failed
/// does. `resume_count` still increments, so a test can tell "the host never
/// called resume" from "the plugin declined it".
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_refuse_resume(refuse: bool) {
    REFUSE_RESUME.store(refuse, Ordering::SeqCst);
}

pub(crate) fn refuse_resume() -> bool {
    REFUSE_RESUME.load(Ordering::SeqCst)
}

/// Return from `process` without touching the output buffers — the
/// stale-buffer leak. A host that does not zero its output scratch between
/// blocks replays the previous block's audio; well-behaved plugins never
/// expose it, because they always write.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_silent_process(silent: bool) {
    SILENT_PROCESS.store(silent, Ordering::SeqCst);
}

pub(crate) fn silent_process() -> bool {
    SILENT_PROCESS.load(Ordering::SeqCst)
}

/// Write one channel *past* the declared `numOutputs`.
///
/// The host sizes its pointer table from the AEffect, so this is a genuine
/// out-of-bounds write: it will corrupt or crash a host that trusts the
/// plugin, which is the finding. Off by default.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_write_extra_output(enable: bool) {
    WRITE_EXTRA_OUTPUT.store(enable, Ordering::SeqCst);
}

pub(crate) fn write_extra_output() -> bool {
    WRITE_EXTRA_OUTPUT.load(Ordering::SeqCst)
}

/// Read one channel past the declared `numInputs`. The read counterpart of
/// [`tutti_vst2_probe_set_write_extra_output`]; the value read is folded
/// into the output so the test can observe whether the host supplied a
/// readable (zeroed) slot or garbage.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_read_extra_input(enable: bool) {
    READ_EXTRA_INPUT.store(enable, Ordering::SeqCst);
}

pub(crate) fn read_extra_input() -> bool {
    READ_EXTRA_INPUT.load(Ordering::SeqCst)
}

/// Restore every switch to its well-behaved default.
///
/// Tests share one loaded image, so a switch left set contaminates every
/// later test. Call it on the way out of any misbehaviour test.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_reset_switches() {
    CAN_DO_ANSWER.store(CanDoAnswer::Yes as i32, Ordering::SeqCst);
    CAN_DO_CUSTOM.store(0, Ordering::SeqCst);
    REFUSE_RESUME.store(false, Ordering::SeqCst);
    SILENT_PROCESS.store(false, Ordering::SeqCst);
    WRITE_EXTRA_OUTPUT.store(false, Ordering::SeqCst);
    READ_EXTRA_INPUT.store(false, Ordering::SeqCst);
    ANSWER_PARAM_PROPERTIES.store(false, Ordering::SeqCst);
    ANSWER_MIDI_METADATA.store(false, Ordering::SeqCst);
    SERVICED_MIDI_PROGRAMS.store(0, Ordering::SeqCst);
}

pub(crate) fn set_resumed(resumed: bool) {
    RESUMED.store(resumed, Ordering::SeqCst);
}

pub(crate) fn is_resumed() -> bool {
    RESUMED.load(Ordering::SeqCst)
}
