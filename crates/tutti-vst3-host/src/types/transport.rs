//! VST3-specific encoding of the shared [`TransportInfo`] into VST3's
//! `ProcessContext` struct.

use tutti_plugin_types::TransportInfo;
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

/// VST3 `IProcessContextRequirements` flag bits as simple `u32` constants,
/// mirroring `IProcessContextRequirements_::Flags_` from the Steinberg SDK.
///
/// A plugin returns an OR of these from `getProcessContextRequirements` to
/// declare which [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
/// fields it consumes; [`to_process_context`] only fills (and marks valid) the
/// fields whose bit is set. See [`crate::host`]'s query for the
/// "everything when absent" ([`u32::MAX`]) fallback.
///
/// The casts below are `u32 as u32` no-ops on unix and `c_int as u32`
/// conversions on Windows (where the SDK enum is signed); allow the lint that
/// only the former triggers.
#[allow(clippy::unnecessary_cast)]
pub mod process_context_flags {
    use vst3::Steinberg::Vst::IProcessContextRequirements_::Flags_;

    /// Plugin needs `systemTime`.
    pub const NEED_SYSTEM_TIME: u32 = Flags_::kNeedSystemTime as u32;
    /// Plugin needs `continousTimeSamples`.
    pub const NEED_CONTINOUS_TIME_SAMPLES: u32 = Flags_::kNeedContinousTimeSamples as u32;
    /// Plugin needs `projectTimeMusic`.
    pub const NEED_PROJECT_TIME_MUSIC: u32 = Flags_::kNeedProjectTimeMusic as u32;
    /// Plugin needs `barPositionMusic`.
    pub const NEED_BAR_POSITION_MUSIC: u32 = Flags_::kNeedBarPositionMusic as u32;
    /// Plugin needs `cycleStartMusic` / `cycleEndMusic`.
    pub const NEED_CYCLE_MUSIC: u32 = Flags_::kNeedCycleMusic as u32;
    /// Plugin needs `samplesToNextClock`.
    pub const NEED_SAMPLES_TO_NEXT_CLOCK: u32 = Flags_::kNeedSamplesToNextClock as u32;
    /// Plugin needs `tempo`.
    pub const NEED_TEMPO: u32 = Flags_::kNeedTempo as u32;
    /// Plugin needs `timeSigNumerator` / `timeSigDenominator`.
    pub const NEED_TIME_SIGNATURE: u32 = Flags_::kNeedTimeSignature as u32;
    /// Plugin needs the (host-track) chord field.
    pub const NEED_CHORD: u32 = Flags_::kNeedChord as u32;
    /// Plugin needs `frameRate`.
    pub const NEED_FRAME_RATE: u32 = Flags_::kNeedFrameRate as u32;
    /// Plugin needs the transport-state bits in `state`.
    pub const NEED_TRANSPORT_STATE: u32 = Flags_::kNeedTransportState as u32;
}

/// Build a VST3 [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
/// from the shared [`TransportInfo`], encoding the validity flags the
/// VST3 spec requires.
///
/// `requirements` is the bitmask the plugin returned from
/// `IProcessContextRequirements::getProcessContextRequirements` (or [`u32::MAX`]
/// when the plugin doesn't implement that interface — see
/// [`process_context_flags`]). Only the fields whose requirement bit is set are
/// populated and marked valid; the rest are left zeroed. Passing [`u32::MAX`]
/// fills everything, reproducing the unconditional behaviour this host had
/// before the interface was wired.
///
/// Field mapping:
/// - `position.samples` → `projectTimeSamples` (always) + `continousTimeSamples`
/// - `position.quarters` → `projectTimeMusic`
/// - `bar.position_quarters` → `barPositionMusic`
/// - `loop_region.{start,end}_quarters` → `cycleStartMusic`/`cycleEndMusic`
/// - `sample_rate` → `sampleRate` (always)
pub fn to_process_context(
    t: &TransportInfo,
    requirements: u32,
) -> vst3::Steinberg::Vst::ProcessContext {
    use process_context_flags as need;

    let wants = |flag: u32| requirements & flag != 0;

    // `StatesAndFlags_::*` is `u32` on unix, `c_int` (i32) on Windows — the casts
    // below are no-ops on one target and sign-changes on the other. Cleanest is
    // to normalise to `u32` at the edge.
    #[allow(clippy::unnecessary_cast)]
    let state = {
        let mut state = 0u32;
        // The play/record/cycle bits are part of the transport state the plugin
        // reads only when it asked for kNeedTransportState.
        if wants(need::NEED_TRANSPORT_STATE) {
            if t.state.playing {
                state |= StatesAndFlags_::kPlaying as u32;
            }
            if t.state.recording {
                state |= StatesAndFlags_::kRecording as u32;
            }
            if t.state.cycle_active {
                state |=
                    (StatesAndFlags_::kCycleActive as u32) | (StatesAndFlags_::kCycleValid as u32);
            }
        }
        // Each `*Valid` bit advertises that the matching field below is filled,
        // so it must track the same requirement gate.
        if wants(need::NEED_PROJECT_TIME_MUSIC) {
            state |= StatesAndFlags_::kProjectTimeMusicValid as u32;
        }
        if wants(need::NEED_BAR_POSITION_MUSIC) {
            state |= StatesAndFlags_::kBarPositionValid as u32;
        }
        if wants(need::NEED_TEMPO) {
            state |= StatesAndFlags_::kTempoValid as u32;
        }
        if wants(need::NEED_TIME_SIGNATURE) {
            state |= StatesAndFlags_::kTimeSigValid as u32;
        }
        state
    };

    let mut ctx: vst3::Steinberg::Vst::ProcessContext = unsafe { std::mem::zeroed() };
    ctx.state = state;
    // Always-on: not gated by any requirement flag in the spec.
    ctx.sampleRate = if t.sample_rate > 0.0 {
        t.sample_rate
    } else {
        44100.0
    };
    ctx.projectTimeSamples = t.position.samples;

    if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
        ctx.continousTimeSamples = t.position.samples;
    }
    if wants(need::NEED_SYSTEM_TIME) {
        ctx.systemTime = 0;
    }
    if wants(need::NEED_PROJECT_TIME_MUSIC) {
        ctx.projectTimeMusic = t.position.quarters;
    }
    if wants(need::NEED_BAR_POSITION_MUSIC) {
        ctx.barPositionMusic = t.bar.position_quarters;
    }
    if wants(need::NEED_CYCLE_MUSIC) {
        ctx.cycleStartMusic = t.loop_region.start_quarters;
        ctx.cycleEndMusic = t.loop_region.end_quarters;
    }
    if wants(need::NEED_TEMPO) {
        ctx.tempo = t.timing.tempo;
    }
    if wants(need::NEED_TIME_SIGNATURE) {
        ctx.timeSigNumerator = t.timing.time_sig_numerator;
        ctx.timeSigDenominator = t.timing.time_sig_denominator;
    }
    if wants(need::NEED_SAMPLES_TO_NEXT_CLOCK) {
        ctx.samplesToNextClock = 0;
    }
    // `smpteOffsetSubframes` / `frameRate` (kNeedFrameRate) and the chord field
    // (kNeedChord) are not sourced from TransportInfo yet; left zeroed. When a
    // producer exists they gate on NEED_FRAME_RATE / NEED_CHORD here.
    ctx
}

#[cfg(test)]
mod tests {
    use super::process_context_flags as need;
    use super::*;
    use tutti_plugin_types::{BarInfo, LoopRegion, MusicalTiming, TransportPosition, TransportState};
    use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

    /// A TransportInfo with every field set to a recognisable non-zero value,
    /// so a "field was populated" check is unambiguous.
    fn populated_transport() -> TransportInfo {
        TransportInfo {
            sample_rate: 48_000.0,
            position: TransportPosition {
                samples: 1_234,
                quarters: 4.0,
                ..Default::default()
            },
            bar: BarInfo {
                position_quarters: 8.0,
                ..Default::default()
            },
            loop_region: LoopRegion {
                start_quarters: 2.0,
                end_quarters: 6.0,
                ..Default::default()
            },
            timing: MusicalTiming {
                tempo: 128.0,
                time_sig_numerator: 7,
                time_sig_denominator: 8,
            },
            state: TransportState {
                playing: true,
                recording: true,
                cycle_active: true,
            },
        }
    }

    /// `u32::MAX` (the "plugin didn't implement IProcessContextRequirements"
    /// sentinel) fills every field this host knows how to source — i.e. the
    /// behaviour before the interface was wired.
    #[test]
    fn all_bits_fills_everything() {
        let t = populated_transport();
        let ctx = to_process_context(&t, u32::MAX);

        assert_eq!(ctx.sampleRate, 48_000.0);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.continousTimeSamples, 1_234);
        assert_eq!(ctx.projectTimeMusic, 4.0);
        assert_eq!(ctx.barPositionMusic, 8.0);
        assert_eq!(ctx.cycleStartMusic, 2.0);
        assert_eq!(ctx.cycleEndMusic, 6.0);
        assert_eq!(ctx.tempo, 128.0);
        assert_eq!(ctx.timeSigNumerator, 7);
        assert_eq!(ctx.timeSigDenominator, 8);

        // All the validity flags + transport-state bits set.
        let s = ctx.state;
        assert_ne!(s & StatesAndFlags_::kPlaying, 0);
        assert_ne!(s & StatesAndFlags_::kRecording, 0);
        assert_ne!(s & StatesAndFlags_::kCycleActive, 0);
        assert_ne!(s & StatesAndFlags_::kProjectTimeMusicValid, 0);
        assert_ne!(s & StatesAndFlags_::kBarPositionValid, 0);
        assert_ne!(s & StatesAndFlags_::kTempoValid, 0);
        assert_ne!(s & StatesAndFlags_::kTimeSigValid, 0);
    }

    /// Requesting only tempo fills `tempo` + `kTempoValid` and nothing else
    /// gated — time signature stays zeroed and its valid bit stays clear.
    #[test]
    fn tempo_only_trims_the_rest() {
        let t = populated_transport();
        let ctx = to_process_context(&t, need::NEED_TEMPO);

        // Requested.
        assert_eq!(ctx.tempo, 128.0);
        assert_ne!(ctx.state & StatesAndFlags_::kTempoValid, 0);

        // Not requested → left zeroed, valid bit clear.
        assert_eq!(ctx.timeSigNumerator, 0);
        assert_eq!(ctx.timeSigDenominator, 0);
        assert_eq!(ctx.state & StatesAndFlags_::kTimeSigValid, 0);
        assert_eq!(ctx.barPositionMusic, 0.0);
        assert_eq!(ctx.state & StatesAndFlags_::kBarPositionValid, 0);
        assert_eq!(ctx.continousTimeSamples, 0);

        // Transport-state not requested → no play/record/cycle bits.
        assert_eq!(ctx.state & StatesAndFlags_::kPlaying, 0);
        assert_eq!(ctx.state & StatesAndFlags_::kCycleActive, 0);
    }

    /// Always-on fields (sampleRate, projectTimeSamples) are populated even
    /// when the plugin requested literally nothing.
    #[test]
    fn always_on_fields_survive_empty_requirements() {
        let t = populated_transport();
        let ctx = to_process_context(&t, 0);

        assert_eq!(ctx.sampleRate, 48_000.0);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        // Everything gated is gone.
        assert_eq!(ctx.tempo, 0.0);
        assert_eq!(ctx.state, 0);
    }
}
