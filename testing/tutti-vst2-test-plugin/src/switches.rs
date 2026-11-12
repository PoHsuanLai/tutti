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

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};

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
    /// Answer with `can_do_custom_value` verbatim.
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
/// Whether the probe accepts `effSetBypass`. Off by default, matching the
/// plugins that do not implement the opcode at all.
static ACCEPT_SOFT_BYPASS: AtomicBool = AtomicBool::new(false);
/// The last `effSetBypass` value the probe was *told*, regardless of whether
/// it accepted. `-1` until the host sends one, so "never asked" is distinct
/// from "asked with 0".
static LAST_BYPASS: AtomicI32 = AtomicI32::new(-1);
/// How many `audioMasterAutomate` reports each `process` call sends. Zero
/// (the default) sends none.
static AUTOMATE_BURST: AtomicU32 = AtomicU32::new(0);
/// Whether `setParameter` reports the value it was set to back through
/// `audioMasterAutomate`, from inside the call, as some plugins do. Off by
/// default.
static ECHO_SET_PARAMETER: AtomicBool = AtomicBool::new(false);

/// Ordered log of preset-related opcodes the probe received, one bit-packed
/// entry per call. Records *order*, not just arrival: an `effBeginSetProgram`
/// that lands after the change it was meant to bracket is as wrong as one that
/// never arrives, and a plain counter cannot tell those apart.
static PRESET_TRACE: AtomicI32 = AtomicI32::new(0);
/// How many entries `PRESET_TRACE` holds. Saturates at 8 (2 bits each in an
/// i32); a test needing more should assert on a prefix.
static PRESET_TRACE_LEN: AtomicI32 = AtomicI32::new(0);
static SERVICED_MIDI_PROGRAMS: AtomicI32 = AtomicI32::new(0);

/// Latency the probe declares once it knows its sample rate, or 0 for none.
///
/// Models the plugin class that motivates D-4: a linear-phase EQ or an
/// oversampling limiter cannot size its filter until `effSetSampleRate` lands,
/// so `AEffect::initialDelay` is 0 at construction — which is precisely when a
/// host reading `get_info()`'s snapshot takes its copy. A probe that declared
/// its latency up front could not tell a host that re-reads from one that does
/// not; both would see the same number.
static LATE_LATENCY: AtomicI32 = AtomicI32::new(0);

/// Declare `samples` of latency, but only from `effSetSampleRate` onwards.
///
/// Set before `Vst2Loaded::load` and restore afterwards — like every switch
/// here, this mutates the loaded image the whole process shares.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_late_latency(samples: i32) {
    LATE_LATENCY.store(samples, Ordering::SeqCst);
    // Clear the "rate is known" latch with it. Both are process-global and the
    // probe image is shared by every test in a binary, so without this the
    // first load leaves the latch set and every later `PluginInstance::new`
    // snapshot already contains the late figure — which makes a host reading
    // the stale snapshot indistinguishable from one that re-reads. That is
    // exactly the difference these tests exist to detect, and whether it was
    // detectable depended on the order the tests happened to run in.
    RATE_KNOWN.store(false, Ordering::SeqCst);
}

pub(crate) fn late_latency() -> i32 {
    LATE_LATENCY.load(Ordering::SeqCst)
}

/// Whether the probe has been told its sample rate yet.
///
/// The gate on [`late_latency`]: before `effSetSampleRate` the probe reports 0,
/// after it reports the configured figure. That transition is the whole point —
/// it is what a host reading the construction-time snapshot misses.
static RATE_KNOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_rate_known(known: bool) {
    RATE_KNOWN.store(known, Ordering::SeqCst);
}

pub(crate) fn rate_known() -> bool {
    RATE_KNOWN.load(Ordering::SeqCst)
}

/// Whether the probe calls `audioMasterUpdateDisplay` on the next resume.
///
/// A plugin fires that opcode after changing preset or program from its own
/// editor. The probe has no editor, so `effMainsChanged` stands in as a
/// host-driven moment the test controls exactly.
static FIRE_UPDATE_DISPLAY: AtomicBool = AtomicBool::new(false);

/// Make the probe call `audioMasterUpdateDisplay` from its next `resume`.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_fire_update_display(enable: bool) {
    FIRE_UPDATE_DISPLAY.store(enable, Ordering::SeqCst);
}

pub(crate) fn fire_update_display() -> bool {
    FIRE_UPDATE_DISPLAY.load(Ordering::SeqCst)
}

/// Make the probe answer `effGetParameterProperties`.
///
/// Off by default, because *declining is the realistic behaviour*: every VST2
/// plugin installed on the development machine (TAL-NoiseMaker, TAL-Reverb-4,
/// TDR Nova) answers `0` for every parameter. The default probe therefore
/// models the common case, and a test opts in to the rare plugin that answers.
///
/// Without this switch the host's decode path is unreachable from any test on
/// this machine — the opcode would be implemented and never executed.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_answer_param_properties(enable: bool) {
    ANSWER_PARAM_PROPERTIES.store(enable, Ordering::SeqCst);
}

pub(crate) fn answer_param_properties() -> bool {
    ANSWER_PARAM_PROPERTIES.load(Ordering::SeqCst)
}

/// Make the probe accept `effSetBypass` (44).
///
/// Off by default: a plugin with no soft bypass is the common case, and an
/// unimplemented opcode returns 0 — the same "no" an explicit refusal gives.
/// A host must not read either as success.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_accept_soft_bypass(enable: bool) {
    ACCEPT_SOFT_BYPASS.store(enable, Ordering::SeqCst);
}

/// The last `effSetBypass` value the host sent: `-1` never, `0` resume,
/// `1` bypass. Records what arrived even when the probe refuses it, so a test
/// can tell "the host never dispatched" from "the plugin said no".
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_last_bypass() -> i32 {
    LAST_BYPASS.load(Ordering::SeqCst)
}

pub(crate) fn accept_soft_bypass() -> bool {
    ACCEPT_SOFT_BYPASS.load(Ordering::SeqCst)
}

pub(crate) fn record_bypass(bypass: bool) {
    LAST_BYPASS.store(i32::from(bypass), Ordering::SeqCst);
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
#[unsafe(no_mangle)]
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
#[unsafe(no_mangle)]
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
#[unsafe(no_mangle)]
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
#[unsafe(no_mangle)]
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
#[unsafe(no_mangle)]
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
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_read_extra_input(enable: bool) {
    READ_EXTRA_INPUT.store(enable, Ordering::SeqCst);
}

pub(crate) fn read_extra_input() -> bool {
    READ_EXTRA_INPUT.load(Ordering::SeqCst)
}

/// Send `count` `audioMasterAutomate` reports (parameter 0) from inside each
/// `process` call, as a plugin applying a preset from its audio thread does:
/// a burst the host must queue without allocating, and may have to drop.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_automate_burst(count: u32) {
    AUTOMATE_BURST.store(count, Ordering::SeqCst);
}

pub(crate) fn automate_burst() -> u32 {
    AUTOMATE_BURST.load(Ordering::SeqCst)
}

/// Make `setParameter` echo each value it is set to through
/// `audioMasterAutomate`, on the caller's thread, before it returns.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_echo_set_parameter(on: bool) {
    ECHO_SET_PARAMETER.store(on, Ordering::SeqCst);
}

pub(crate) fn echo_set_parameter() -> bool {
    ECHO_SET_PARAMETER.load(Ordering::SeqCst)
}

/// Restore every switch to its well-behaved default.
///
/// Tests share one loaded image, so a switch left set contaminates every
/// later test. Call it on the way out of any misbehaviour test.
#[unsafe(no_mangle)]
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
    ACCEPT_SOFT_BYPASS.store(false, Ordering::SeqCst);
    LAST_BYPASS.store(-1, Ordering::SeqCst);
    AUTOMATE_BURST.store(0, Ordering::SeqCst);
    ECHO_SET_PARAMETER.store(false, Ordering::SeqCst);
    PRESET_TRACE.store(0, Ordering::SeqCst);
    PRESET_TRACE_LEN.store(0, Ordering::SeqCst);
    EDITOR_OPENS.store(false, Ordering::SeqCst);
    tutti_vst2_probe_reset_trace();
}

/// One preset opcode, as recorded in `PRESET_TRACE`. Mirrored in
/// `tutti-vst2-host`'s tests.
pub mod preset_event {
    /// `effBeginSetProgram` (67).
    pub const BEGIN: i32 = 1;
    /// `effProgramChange` (2).
    pub const CHANGE: i32 = 2;
    /// `effEndSetProgram` (68).
    pub const END: i32 = 3;
}

pub(crate) fn record_preset_event(event: i32) {
    let len = PRESET_TRACE_LEN.load(Ordering::SeqCst);
    if len < 8 {
        let packed = PRESET_TRACE.load(Ordering::SeqCst) | (event << (len * 2));
        PRESET_TRACE.store(packed, Ordering::SeqCst);
        PRESET_TRACE_LEN.store(len + 1, Ordering::SeqCst);
    }
}

/// The packed preset-opcode trace. Decode with [`preset_event`]'s constants,
/// two bits per entry, oldest first.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_preset_trace() -> i32 {
    PRESET_TRACE.load(Ordering::SeqCst)
}

/// How many entries [`tutti_vst2_probe_preset_trace`] holds.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_preset_trace_len() -> i32 {
    PRESET_TRACE_LEN.load(Ordering::SeqCst)
}

pub(crate) fn set_resumed(resumed: bool) {
    RESUMED.store(resumed, Ordering::SeqCst);
}

pub(crate) fn is_resumed() -> bool {
    RESUMED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// effClose accounting
// ---------------------------------------------------------------------------

/// How many `effClose` dispatches this image has received since it was mapped.
///
/// Lives in the probe's image rather than in the host, which is the whole
/// point: it is readable *after* the host has dropped its instance, and it
/// survives exactly as long as the image stays mapped. Those two properties are
/// what let one test separate the two events that
/// `tutti_vst2_host::handle`'s module docs insist are different — `effClose`
/// (release this instance; mandatory) and `dlclose` (unload the module; the
/// step hosts must not take).
///
/// A host that skipped `effClose` leaves this at 0. A host that unloaded the
/// module resets it to 0 *and* loses every other switch write with it, which is
/// the failure mode that once read as flakiness.
static CLOSE_COUNT: AtomicU32 = AtomicU32::new(0);

pub(crate) fn record_close() {
    CLOSE_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Read the `effClose` count out of the loaded image.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_close_count() -> u32 {
    CLOSE_COUNT.load(Ordering::SeqCst)
}

/// Reset the `effClose` count, so a test can start from a known zero.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_reset_close_count() {
    CLOSE_COUNT.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Lifecycle trace
// ---------------------------------------------------------------------------

/// The host's lifecycle opcodes in the order they arrived: `effSetSampleRate`,
/// `effSetBlockSize`, `effMainsChanged`, `effStartProcess`, `effStopProcess`,
/// `effSetProcessPrecision` and `effEditIdle`, each as `opcode << 32 | value`
/// (the value's low 32 bits; the rate, an `f32`, travels in `opt` and is not
/// recorded). Order is the point: the VST 2.4 bracket (stop, suspend, set,
/// resume, start) is a sequence, and a count cannot tell a balanced pair from
/// two unbalanced ones.
static LIFECYCLE: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn record_lifecycle(opcode: i32, value: isize) {
    let packed = (i64::from(opcode) << 32) | i64::from(value as u32);
    LIFECYCLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(packed);
}

/// How many entries the lifecycle trace holds.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_trace_len() -> u32 {
    LIFECYCLE.lock().unwrap_or_else(|p| p.into_inner()).len() as u32
}

/// Entry `index` of the lifecycle trace (`opcode << 32 | value`), or `-1`.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_trace_at(index: u32) -> i64 {
    LIFECYCLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(index as usize)
        .copied()
        .unwrap_or(-1)
}

/// Clear the lifecycle trace.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_reset_trace() {
    LIFECYCLE.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

// ---------------------------------------------------------------------------
// Plugin -> host calls
// ---------------------------------------------------------------------------

/// The host's `audioMaster` callback and the `AEffect` it was handed with,
/// stored at `VSTPluginMain` so an `extern "C"` switch can make the calls a
/// plugin makes on its own initiative. The latest instance wins.
static HOST_CALLBACK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HOST_EFFECT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn remember_host(callback: usize, effect: usize) {
    HOST_CALLBACK.store(callback, Ordering::SeqCst);
    HOST_EFFECT.store(effect, Ordering::SeqCst);
}

/// One `audioMaster` call from the plugin, `None` before the plugin has been
/// loaded.
///
/// # Safety
/// `ptr` must satisfy the opcode's contract.
unsafe fn call_host(opcode: i32, ptr: *mut std::ffi::c_void) -> Option<isize> {
    let callback = HOST_CALLBACK.load(Ordering::SeqCst);
    if callback == 0 {
        return None;
    }
    let callback: vst::api::HostCallbackProc = unsafe { std::mem::transmute(callback) };
    let effect = HOST_EFFECT.load(Ordering::SeqCst) as *mut vst::api::AEffect;
    Some(unsafe { callback(effect, opcode, 0, 0, ptr, 0.0) })
}

/// What a plugin does when its configuration changes under it: write the new
/// `initialDelay` and `numInputs` into its `AEffect` and call
/// `audioMasterIOChanged`. Returns the host's answer (`1` = handled), or `-1`
/// before a plugin is loaded.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_fire_io_changed(latency: i32, inputs: i32) -> isize {
    let effect = HOST_EFFECT.load(Ordering::SeqCst) as *mut vst::api::AEffect;
    if effect.is_null() {
        return -1;
    }
    // SAFETY: the host holds the `AEffect` this pointer names for as long as
    // the plugin is loaded, and the test that calls this keeps it so.
    unsafe {
        (*effect).initialDelay = latency;
        (*effect).numInputs = inputs;
        // `audioMasterIOChanged` is opcode 13.
        call_host(13, std::ptr::null_mut()).unwrap_or(-1)
    }
}

/// What a plugin with a size switch does: ask the host to resize its editor window,
/// `audioMasterSizeWindow` (`[index]` width, `[value]` height). Returns the host's answer
/// (`1` = supported), or `-1` before a plugin is loaded.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_request_size_window(width: i32, height: i32) -> isize {
    let effect = HOST_EFFECT.load(Ordering::SeqCst) as *mut vst::api::AEffect;
    if effect.is_null() {
        return -1;
    }
    let callback = HOST_CALLBACK.load(Ordering::SeqCst);
    if callback == 0 {
        return -1;
    }
    // SAFETY: the callback is the host's `audioMaster` pointer stored at load, and the effect
    // is the one it was handed; `audioMasterSizeWindow` (opcode 15) takes no pointer.
    unsafe {
        let callback: vst::api::HostCallbackProc = std::mem::transmute(callback);
        callback(
            effect,
            15,
            width,
            height as isize,
            std::ptr::null_mut(),
            0.0,
        )
    }
}

/// Ask the host `audioMasterCanDo` for `name` (a NUL-terminated string), as a
/// plugin gating a feature does. The host's raw answer: `1`, `0` or `-1`.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_vst2_probe_ask_host_can_do(
    name: *const std::os::raw::c_char,
) -> isize {
    // `audioMasterCanDo` is opcode 37.
    unsafe { call_host(37, name as *mut std::ffi::c_void).unwrap_or(-2) }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

static EDITOR_OPENS: AtomicBool = AtomicBool::new(false);

/// Let the probe's editor (`TUTTI_VST2_PROBE_EDITOR=1`) accept `effEditOpen`.
/// Off by default: it has no window to embed.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_vst2_probe_set_editor_opens(opens: bool) {
    EDITOR_OPENS.store(opens, Ordering::SeqCst);
}

pub(crate) fn editor_opens() -> bool {
    EDITOR_OPENS.load(Ordering::SeqCst)
}
