//! VST3-specific encoding of the shared [`TransportInfo`] into VST3's
//! `ProcessContext` struct.

use tutti_plugin_types::{is_usable, TransportInfo};
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
/// - `position.samples` → `projectTimeSamples` (always). VST3 has no validity
///   bit for this field, so when the host reports `None` there is nothing
///   honest to send and it is left `0`. TODO: source a real project-time sample
///   clock from the transport — until then a plugin doing sample-accurate math
///   off `projectTimeSamples` sees the project frozen at sample 0. See
///   `TransportPosition::samples`.
/// - `position.continuous_samples` → `continousTimeSamples` (the monotonic
///   clock that does not reset on loop; falls back to `position.samples` when
///   unset, i.e. `0`)
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
                state |= StatesAndFlags_::kCycleActive as u32;
            }
        }
        // `kCycleValid` is deliberately NOT set above beside `kCycleActive`.
        // The two answer different questions and the SDK routes them through
        // different requirement bits: `kNeedTransportState` covers
        // "kPlaying, kCycleActive, kRecording", while `kCycleValid` is listed
        // against `kNeedCycleMusic` (`ivstaudioprocessor.h:446,452`). And the
        // flag's own doc is about the *fields* — "cycleStartMusic and
        // barPositionMusic contain valid information" — not about whether a
        // cycle is running.
        //
        // Setting it from `cycle_active` under the transport gate broke both
        // directions at once: a plugin asking only for transport state was
        // told the cycle bounds were valid while they were still the
        // `mem::zeroed` 0.0, and a plugin asking only for cycle music got the
        // bounds written below with no bit to say so, and ignored them.
        if wants(need::NEED_CYCLE_MUSIC)
            && is_usable(t.loop_region.start_quarters)
            && is_usable(t.loop_region.end_quarters)
        {
            state |= StatesAndFlags_::kCycleValid as u32;
        }
        // Each `*Valid` bit advertises that the matching field below is filled,
        // so it must track the same requirement gate — AND the value must be one
        // a plugin can compute with.
        //
        // The requirement mask alone is not enough: it says what the plugin
        // *asked for*, not what the host *has*. Setting a validity bit over a
        // NaN or an infinity is worse than leaving it clear, because a plugin
        // that trusts the bit propagates the NaN through its timing math into
        // the audio buffer. The VST2 path has always gated on the value this
        // way; this one did not, so the two drifted (`is_usable` now lives in
        // `tutti-plugin-types` so they cannot drift again).
        if wants(need::NEED_PROJECT_TIME_MUSIC) && is_usable(t.position.quarters) {
            state |= StatesAndFlags_::kProjectTimeMusicValid as u32;
        }
        if wants(need::NEED_BAR_POSITION_MUSIC) && is_usable(t.bar.position_quarters) {
            state |= StatesAndFlags_::kBarPositionValid as u32;
        }
        // Tempo carries a domain rule beyond finiteness: zero or negative BPM
        // is not a tempo, and a plugin dividing by it produces an infinity.
        // Matches the VST2 path's `is_usable(..) && > 0.0`.
        if wants(need::NEED_TEMPO) && is_usable(t.timing.tempo) && t.timing.tempo > 0.0 {
            state |= StatesAndFlags_::kTempoValid as u32;
        }
        if wants(need::NEED_TIME_SIGNATURE) {
            state |= StatesAndFlags_::kTimeSigValid as u32;
        }
        // `continousTimeSamples` is filled below whenever the plugin asked for
        // it, so `kContTimeValid` must be set on the same condition. Without the
        // bit a spec-correct plugin ignores the field entirely — the value was
        // being computed and shipped into a dead slot.
        if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
            state |= StatesAndFlags_::kContTimeValid as u32;
        }
        // Deliberately NOT set: `kSystemTimeValid` and `kClockValid`. This host
        // has no source for `systemTime` (a host-clock reading in nanoseconds)
        // or `samplesToNextClock` (distance to the next MIDI 24-ppq clock), so
        // both fields are left zeroed by the `mem::zeroed` above and left
        // unflagged. Writing a literal 0 *and* claiming validity would tell a
        // plugin the transport is pinned at time zero, which is worse than
        // saying nothing. Same for `kSmpteValid` / `kChordValid`, whose fields
        // have no producer either.
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
    // VST3 carries no `kProjectTimeSamplesValid` bit — the field is always read
    // as fact — so an absent project-time clock can only degrade to 0. TODO
    // (see the field doc on `TransportPosition::samples`): give the transport a
    // real project-time sample clock; until then plugins keying sample-accurate
    // math off this see sample 0 forever.
    ctx.projectTimeSamples = t.position.samples.unwrap_or(0);

    if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
        // The continuous clock is monotonic across loop/cycle boundaries and is
        // distinct from `projectTimeSamples`, which jumps back on a loop. Hosts
        // that don't keep a separate continuous counter leave it `0` (unset);
        // fall back to `samples` there so their behaviour is unchanged.
        ctx.continousTimeSamples = if t.position.continuous_samples != 0 {
            t.position.continuous_samples
        } else {
            t.position.samples.unwrap_or(0)
        };
    }
    // `systemTime` is intentionally not written: there is no host-clock source
    // in `TransportInfo`, and `kSystemTimeValid` is correspondingly left clear
    // (see the state block above). It stays 0 from `mem::zeroed`, which a
    // spec-correct plugin ignores.
    // TODO: plumb a monotonic host clock through `TransportInfo` (a
    // tutti-plugin-types change), then fill the field and set the bit here.
    if wants(need::NEED_PROJECT_TIME_MUSIC) {
        ctx.projectTimeMusic = t.position.quarters;
    }
    if wants(need::NEED_BAR_POSITION_MUSIC) {
        ctx.barPositionMusic = t.bar.position_quarters;
    }
    // Same condition as `kCycleValid` above, including the finiteness check —
    // the bit and the fields it advertises have to move together, or a NaN
    // bound ships under a flag saying it is computable.
    if wants(need::NEED_CYCLE_MUSIC)
        && is_usable(t.loop_region.start_quarters)
        && is_usable(t.loop_region.end_quarters)
    {
        ctx.cycleStartMusic = t.loop_region.start_quarters;
        ctx.cycleEndMusic = t.loop_region.end_quarters;
    }
    if wants(need::NEED_TEMPO) {
        ctx.tempo = t.timing.tempo;
    }
    if wants(need::NEED_TIME_SIGNATURE) {
        // Conversion at the boundary, gating unchanged: the `kTimeSigValid` bit
        // above is set from the same `wants` check, so folding the gate into the
        // conversion would split a pair that has to move together.
        ctx.timeSigNumerator = t.timing.signature.beats_per_bar().into();
        ctx.timeSigDenominator = t.timing.signature.note_value().into();
    }
    // `samplesToNextClock` is likewise not written: no MIDI-clock grid is
    // tracked here, so `kClockValid` stays clear and the field stays 0.
    // TODO: derive it from tempo + sample rate + project position (24 ppq) and
    // set `kClockValid` alongside it.
    //
    // `smpteOffsetSubframes` / `frameRate` (kNeedFrameRate) and the chord field
    // (kNeedChord) are not sourced from TransportInfo yet; left zeroed with
    // `kSmpteValid` / `kChordValid` clear. When a producer exists they gate on
    // NEED_FRAME_RATE / NEED_CHORD here and set their own valid bit.
    ctx
}

#[cfg(test)]
// `StatesAndFlags_::*` is `u32` on unix and `c_int` (i32) on Windows, so the
// `as u32` normalisations below are redundant on one target and load-bearing on
// the other — the same reason `to_process_context` carries this allow. Taking
// clippy's suggestion here would compile on macOS and break Windows.
#[allow(clippy::unnecessary_cast)]
mod tests {
    use super::process_context_flags as need;
    use super::*;
    use tutti_plugin_types::{BeatsPerBar, NoteValue, TimeSignature};
    use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

    /// A TransportInfo with every field set to a recognisable non-zero value,
    /// so a "field was populated" check is unambiguous.
    ///
    /// Built through the `with_*` constructors, **not** struct literals. A
    /// literal hand-filling `LoopRegion { start_quarters, end_quarters }`
    /// directly would let `with_loop` ship without setting the `_quarters` pair
    /// and still pass: the test would assert this host reads the fields, never
    /// that the constructor writes them.
    fn populated_transport() -> TransportInfo {
        TransportInfo::new()
            .with_sample_rate(48_000.0)
            .with_position_samples(1_234)
            .with_position_quarters(4.0)
            .with_bar(8.0, Default::default())
            .with_loop(true, 2.0, 6.0)
            .with_tempo(128.0)
            .with_time_signature(TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH))
            .with_playing(true)
            .with_recording(true)
    }

    /// `u32::MAX` (the "plugin didn't implement IProcessContextRequirements"
    /// sentinel) fills every field this host knows how to source — i.e. the
    /// behaviour before the interface was wired.
    /// An unusable value must not be advertised as valid, however loudly the
    /// plugin asked for it.
    ///
    /// The requirement mask says what the plugin *wants*, not what the host
    /// *has*. Gating the `*Valid` bits on the mask alone let a NaN tempo reach
    /// the plugin flagged valid — and a plugin that trusts the flag divides by
    /// it, propagating NaN or an infinity straight into the audio buffer. The
    /// VST2 path has always gated on the value; this one had drifted.
    #[test]
    fn a_nan_or_zero_field_is_not_advertised_as_valid() {
        // Everything requested, so only the value can clear a bit.
        let all = u32::MAX;

        let mut t = populated_transport();
        t.timing.tempo = f64::NAN;
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & StatesAndFlags_::kTempoValid as u32,
            0,
            "a NaN tempo must not be flagged valid — the plugin will compute \
             with it"
        );

        // Zero is finite but not a tempo: a plugin dividing by it gets an
        // infinity. Matches the VST2 path's `> 0.0` rule.
        let mut t = populated_transport();
        t.timing.tempo = 0.0;
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & StatesAndFlags_::kTempoValid as u32,
            0,
            "a zero tempo must not be flagged valid"
        );

        let mut t = populated_transport();
        t.position.quarters = f64::INFINITY;
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & StatesAndFlags_::kProjectTimeMusicValid as u32,
            0,
            "an infinite musical position must not be flagged valid"
        );

        // The gate must not fire on good values — otherwise it would pass by
        // clearing every bit unconditionally.
        let ctx = to_process_context(&populated_transport(), all);
        assert_ne!(ctx.state & StatesAndFlags_::kTempoValid as u32, 0);
        assert_ne!(
            ctx.state & StatesAndFlags_::kProjectTimeMusicValid as u32,
            0
        );
    }

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
        assert_ne!(
            s & StatesAndFlags_::kContTimeValid,
            0,
            "continousTimeSamples is filled, so kContTimeValid must be set"
        );

        // Fields with no producer are neither written nor flagged. Writing a
        // literal 0 while leaving the bit clear was dead code; claiming
        // validity for it would be a lie.
        assert_eq!(ctx.systemTime, 0);
        assert_eq!(s & StatesAndFlags_::kSystemTimeValid, 0);
        assert_eq!(ctx.samplesToNextClock, 0);
        assert_eq!(s & StatesAndFlags_::kClockValid, 0);
        assert_eq!(s & StatesAndFlags_::kSmpteValid, 0);
        assert_eq!(s & StatesAndFlags_::kChordValid, 0);
    }

    /// Per `ivstprocesscontext.h`, each `*Valid` bit must be set iff its field
    /// carries meaning. This pins the pairing for every field this host fills:
    /// requesting exactly one field sets exactly that field's valid bit, and
    /// not requesting it leaves both the field and the bit clear.
    #[test]
    fn each_valid_bit_tracks_its_own_field() {
        let t = populated_transport();

        // (requirement bit, valid bit, "is the field non-zero?" probe)
        #[allow(clippy::type_complexity)]
        let cases: &[(
            &str,
            u32,
            u32,
            fn(&vst3::Steinberg::Vst::ProcessContext) -> bool,
        )] = &[
            (
                "projectTimeMusic",
                need::NEED_PROJECT_TIME_MUSIC,
                StatesAndFlags_::kProjectTimeMusicValid,
                |c| c.projectTimeMusic != 0.0,
            ),
            (
                "barPositionMusic",
                need::NEED_BAR_POSITION_MUSIC,
                StatesAndFlags_::kBarPositionValid,
                |c| c.barPositionMusic != 0.0,
            ),
            (
                "tempo",
                need::NEED_TEMPO,
                StatesAndFlags_::kTempoValid,
                |c| c.tempo != 0.0,
            ),
            (
                "timeSig",
                need::NEED_TIME_SIGNATURE,
                StatesAndFlags_::kTimeSigValid,
                |c| c.timeSigNumerator != 0,
            ),
            (
                "continousTimeSamples",
                need::NEED_CONTINOUS_TIME_SAMPLES,
                StatesAndFlags_::kContTimeValid,
                |c| c.continousTimeSamples != 0,
            ),
            // `cycleStartMusic` was the one filled field missing from this
            // table, and its absence is why `kCycleValid` could be set from
            // `cycle_active` under the transport gate for so long: no case
            // here paired the bit with the field, so nothing contradicted it.
            (
                "cycleStartMusic",
                need::NEED_CYCLE_MUSIC,
                StatesAndFlags_::kCycleValid,
                |c| c.cycleStartMusic != 0.0,
            ),
        ];

        for (name, requirement, valid_bit, field_filled) in cases {
            let on = to_process_context(&t, *requirement);
            assert!(field_filled(&on), "{name}: requested but not filled");
            assert_ne!(
                on.state & valid_bit,
                0,
                "{name}: filled but its *Valid bit is clear — a spec-correct \
                 plugin ignores the field, so the feature is dead"
            );

            // The inverse: nothing requested → field untouched, bit clear.
            let off = to_process_context(&t, 0);
            assert!(
                !field_filled(&off),
                "{name}: filled without being requested"
            );
            assert_eq!(
                off.state & valid_bit,
                0,
                "{name}: valid bit set with no data"
            );
        }
    }

    /// `kCycleActive` and `kCycleValid` answer different questions and ride
    /// different requirement bits.
    ///
    /// The SDK maps them separately: `kNeedTransportState` covers
    /// "kPlaying, kCycleActive, kRecording" while `kCycleValid` is listed
    /// against `kNeedCycleMusic` (`ivstaudioprocessor.h:446,452`), and the
    /// flag itself is documented as "cycleStartMusic and barPositionMusic
    /// contain valid information" — a claim about fields, not about whether a
    /// cycle is running.
    ///
    /// Setting `kCycleValid` from `cycle_active` broke both directions at
    /// once, and each half needs its own assertion because either alone still
    /// passes with the bug half-fixed.
    #[test]
    fn cycle_active_and_cycle_valid_are_gated_separately() {
        let t = populated_transport();

        // A plugin that asked only for transport state gets the running flag
        // but no claim about the bounds — which it never asked for, and which
        // are still `mem::zeroed` here.
        let transport_only = to_process_context(&t, need::NEED_TRANSPORT_STATE);
        assert_ne!(
            transport_only.state & StatesAndFlags_::kCycleActive,
            0,
            "a cycling transport must still report kCycleActive"
        );
        assert_eq!(
            transport_only.state & StatesAndFlags_::kCycleValid,
            0,
            "kCycleValid claims cycleStartMusic/cycleEndMusic are meaningful, \
             but nothing asked for them and they are still zero"
        );
        assert_eq!(transport_only.cycleStartMusic, 0.0);
        assert_eq!(transport_only.cycleEndMusic, 0.0);

        // The converse: a plugin that asked only for cycle music gets the
        // bounds AND the bit that makes them readable. Without the bit a
        // spec-correct plugin ignores the fields, so the loop points are
        // computed and thrown away.
        let cycle_only = to_process_context(&t, need::NEED_CYCLE_MUSIC);
        assert_ne!(
            cycle_only.state & StatesAndFlags_::kCycleValid,
            0,
            "cycle bounds were filled but not flagged — the plugin ignores them"
        );
        assert_eq!(cycle_only.cycleStartMusic, 2.0);
        assert_eq!(cycle_only.cycleEndMusic, 6.0);
    }

    /// A non-finite cycle bound is not flagged valid, matching every other
    /// field's gate. A plugin trusting the bit would propagate the NaN into
    /// its own timing math.
    #[test]
    fn a_non_finite_cycle_bound_is_not_flagged_valid() {
        let mut t = populated_transport();
        t.loop_region.end_quarters = f64::NAN;

        let ctx = to_process_context(&t, need::NEED_CYCLE_MUSIC);
        assert_eq!(
            ctx.state & StatesAndFlags_::kCycleValid,
            0,
            "a NaN bound must not be advertised as valid"
        );
        assert_eq!(
            ctx.cycleStartMusic, 0.0,
            "neither bound is written when the pair is unusable — a good start \
             beside a NaN end is still an uncomputable region"
        );
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

    /// A loop scenario: the continuous clock has advanced past where the
    /// project clock jumped back to, so `continousTimeSamples` must reflect the
    /// monotonic counter, distinct from `projectTimeSamples`.
    #[test]
    fn continuous_samples_distinct_on_loop() {
        let mut t = populated_transport();
        // Project time looped back to 1_234; the monotonic clock kept counting.
        t.position.continuous_samples = 100_000;
        let ctx = to_process_context(&t, need::NEED_CONTINOUS_TIME_SAMPLES);

        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.continousTimeSamples, 100_000);
        assert_ne!(ctx.projectTimeSamples, ctx.continousTimeSamples);
        assert_ne!(
            ctx.state & StatesAndFlags_::kContTimeValid,
            0,
            "without kContTimeValid the plugin ignores continousTimeSamples"
        );
    }

    /// A host with no separate continuous clock leaves `continuous_samples`
    /// unset (`0`); the continuous field then mirrors `projectTimeSamples`, so
    /// nothing regresses for such hosts.
    #[test]
    fn continuous_samples_falls_back_when_unset() {
        let t = populated_transport(); // continuous_samples defaults to 0
        assert_eq!(t.position.continuous_samples, 0);
        let ctx = to_process_context(&t, need::NEED_CONTINOUS_TIME_SAMPLES);

        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.continousTimeSamples, 1_234);
        assert_eq!(ctx.projectTimeSamples, ctx.continousTimeSamples);
        assert_ne!(ctx.state & StatesAndFlags_::kContTimeValid, 0);
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
