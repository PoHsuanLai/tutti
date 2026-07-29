//! Runtime behaviour switches, flipped by the test across the dlopen seam.
//!
//! Everything here is something a *loaded, running* plugin can change its
//! mind about, so unlike `config.rs` these do not have to be in place before
//! `VSTPluginMain` runs. They are `#[no_mangle] extern "C"` functions over
//! process-global atomics, matching the CLAP probe's idiom: the test
//! resolves the symbol out of the same image the host loaded and calls it
//! directly.
//!
//! Prefer a switch to an env var wherever the timing permits. An env var is
//! process-global *and* sticky for the life of the image — a test that
//! forgets to unset one silently poisons every later test in the same
//! binary, which is precisely the failure mode `probe_path.rs` documents for
//! stale artifacts.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};

/// What the probe answers to `effCanDo`.
///
/// VST 2.4 defines three answers and hosts routinely collapse them into
/// two: `1` = yes, `0` = "don't know, ask something else / assume the
/// default", `-1` = **explicitly no**. Treating `-1` as truthy (it is
/// non-zero) or as equal to `0` (it is not "unknown") is the VST2 shape of
/// the return-code-misinterpretation bug class that accounted for four of
/// VST3's five host bugs and four of CLAP's nine. `Yes`/`Maybe`/`No` are
/// the three legal answers; `Custom` exists because real plugins have
/// returned other integers and the host must not read them as success.
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
/// VST 2.4's `effMainsChanged` has no failure return — the dispatcher's
/// answer is ignored by every host, and vst-rs's `Plugin::resume` returns
/// `()` accordingly. So "refuse to resume" cannot be signalled the way
/// VST3's `setActive` returning `kResultFalse` can. What the probe does
/// instead is refuse *in substance*: it stays suspended and renders silence,
/// which is what a plugin whose device/licence claim failed actually does.
/// The capture's `resume_count` still increments, so a test can tell "the
/// host never called resume" apart from "the plugin declined it".
///
/// This is a fork limitation, recorded here rather than worked around: the
/// honest fix is a raw-dispatcher override, and there is nothing on the host
/// side that would observe a different `effMainsChanged` return value.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_refuse_resume(refuse: bool) {
    REFUSE_RESUME.store(refuse, Ordering::SeqCst);
}

pub(crate) fn refuse_resume() -> bool {
    REFUSE_RESUME.load(Ordering::SeqCst)
}

/// Return from `process` without touching the output buffers.
///
/// The stale-buffer leak: a host that does not zero its output scratch
/// between blocks replays the previous block's audio. Well-behaved plugins
/// never expose it because they always write.
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_set_silent_process(silent: bool) {
    SILENT_PROCESS.store(silent, Ordering::SeqCst);
}

pub(crate) fn silent_process() -> bool {
    SILENT_PROCESS.load(Ordering::SeqCst)
}

/// Write one channel *past* the declared `numOutputs`.
///
/// A plugin lying about its channel count is the VST2 shape of the VST3
/// `getBusCount` overreport. The host is expected to size its pointer table
/// from the AEffect, so this is a genuine out-of-bounds write — it will
/// corrupt or crash a host that trusts the plugin, which is the finding.
/// Off by default and never enabled by the smoke test.
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
/// Tests share one loaded image, so a switch left set is a cross-test
/// contamination channel. Every misbehaviour test must call this on the way
/// out (and the harness should call it on the way in).
#[no_mangle]
pub extern "C" fn tutti_vst2_probe_reset_switches() {
    CAN_DO_ANSWER.store(CanDoAnswer::Yes as i32, Ordering::SeqCst);
    CAN_DO_CUSTOM.store(0, Ordering::SeqCst);
    REFUSE_RESUME.store(false, Ordering::SeqCst);
    SILENT_PROCESS.store(false, Ordering::SeqCst);
    WRITE_EXTRA_OUTPUT.store(false, Ordering::SeqCst);
    READ_EXTRA_INPUT.store(false, Ordering::SeqCst);
}

pub(crate) fn set_resumed(resumed: bool) {
    RESUMED.store(resumed, Ordering::SeqCst);
}

pub(crate) fn is_resumed() -> bool {
    RESUMED.load(Ordering::SeqCst)
}
