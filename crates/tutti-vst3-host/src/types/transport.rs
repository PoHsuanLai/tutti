//! VST3-specific encoding of the shared [`TransportInfo`] into VST3's
//! `ProcessContext` struct.

use tutti_plugin_types::TransportInfo;
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

/// Build a VST3 [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
/// from the shared [`TransportInfo`], encoding the validity flags the
/// VST3 spec requires.
///
/// Field mapping:
/// - `position.samples` → `projectTimeSamples` + `continousTimeSamples`
/// - `position.quarters` → `projectTimeMusic`
/// - `bar.position_quarters` → `barPositionMusic`
/// - `loop_region.{start,end}_quarters` → `cycleStartMusic`/`cycleEndMusic`
/// - `sample_rate` → `sampleRate`
pub fn to_process_context(t: &TransportInfo) -> vst3::Steinberg::Vst::ProcessContext {
    // `StatesAndFlags_::*` is `u32` on unix, `c_int` (i32) on Windows — the casts
    // below are no-ops on one target and sign-changes on the other. Cleanest is
    // to normalise to `u32` at the edge.
    #[allow(clippy::unnecessary_cast)]
    let state = {
        let mut state = 0u32;
        if t.state.playing {
            state |= StatesAndFlags_::kPlaying as u32;
        }
        if t.state.recording {
            state |= StatesAndFlags_::kRecording as u32;
        }
        if t.state.cycle_active {
            state |= (StatesAndFlags_::kCycleActive as u32) | (StatesAndFlags_::kCycleValid as u32);
        }
        state |= (StatesAndFlags_::kProjectTimeMusicValid as u32)
            | (StatesAndFlags_::kBarPositionValid as u32)
            | (StatesAndFlags_::kTempoValid as u32)
            | (StatesAndFlags_::kTimeSigValid as u32);
        state
    };

    let mut ctx: vst3::Steinberg::Vst::ProcessContext = unsafe { std::mem::zeroed() };
    ctx.state = state;
    ctx.sampleRate = if t.sample_rate > 0.0 {
        t.sample_rate
    } else {
        44100.0
    };
    ctx.projectTimeSamples = t.position.samples;
    ctx.systemTime = 0;
    ctx.continousTimeSamples = t.position.samples;
    ctx.projectTimeMusic = t.position.quarters;
    ctx.barPositionMusic = t.bar.position_quarters;
    ctx.cycleStartMusic = t.loop_region.start_quarters;
    ctx.cycleEndMusic = t.loop_region.end_quarters;
    ctx.tempo = t.timing.tempo;
    ctx.timeSigNumerator = t.timing.time_sig_numerator;
    ctx.timeSigDenominator = t.timing.time_sig_denominator;
    ctx.smpteOffsetSubframes = 0;
    ctx.samplesToNextClock = 0;
    ctx
}
