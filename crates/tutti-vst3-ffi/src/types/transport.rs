//! VST3-specific encoding of the shared [`TransportInfo`] into VST3's
//! `ProcessContext` struct.

use tutti_plugin_types::{TransportInfo, is_usable};
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

use vst3::Steinberg::Vst::IProcessContextRequirements_::Flags_;

bitflags::bitflags! {
    /// `IProcessContextRequirements` flags, mirroring `Flags_` from the Steinberg
    /// SDK: a plugin returns an OR of these from `getProcessContextRequirements`
    /// to declare which [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
    /// fields it consumes. [`to_process_context`] only fills (and marks valid)
    /// the fields whose bit is set; a plugin without the interface is given
    /// [`all`](Self::all).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ProcessContextRequirements: u32 {
        /// Plugin needs `systemTime`.
        const NEED_SYSTEM_TIME = crate::sdk_enum_u32(Flags_::kNeedSystemTime);
        /// Plugin needs `continousTimeSamples`.
        const NEED_CONTINOUS_TIME_SAMPLES = crate::sdk_enum_u32(Flags_::kNeedContinousTimeSamples);
        /// Plugin needs `projectTimeMusic`.
        const NEED_PROJECT_TIME_MUSIC = crate::sdk_enum_u32(Flags_::kNeedProjectTimeMusic);
        /// Plugin needs `barPositionMusic`.
        const NEED_BAR_POSITION_MUSIC = crate::sdk_enum_u32(Flags_::kNeedBarPositionMusic);
        /// Plugin needs `cycleStartMusic` / `cycleEndMusic`.
        const NEED_CYCLE_MUSIC = crate::sdk_enum_u32(Flags_::kNeedCycleMusic);
        /// Plugin needs `samplesToNextClock`.
        const NEED_SAMPLES_TO_NEXT_CLOCK = crate::sdk_enum_u32(Flags_::kNeedSamplesToNextClock);
        /// Plugin needs `tempo`.
        const NEED_TEMPO = crate::sdk_enum_u32(Flags_::kNeedTempo);
        /// Plugin needs `timeSigNumerator` / `timeSigDenominator`.
        const NEED_TIME_SIGNATURE = crate::sdk_enum_u32(Flags_::kNeedTimeSignature);
        /// Plugin needs the (host-track) chord field.
        const NEED_CHORD = crate::sdk_enum_u32(Flags_::kNeedChord);
        /// Plugin needs `frameRate`.
        const NEED_FRAME_RATE = crate::sdk_enum_u32(Flags_::kNeedFrameRate);
        /// Plugin needs the transport-state bits in `state`.
        const NEED_TRANSPORT_STATE = crate::sdk_enum_u32(Flags_::kNeedTransportState);
        // The plugin's own bits beyond the ones named, and `all()` as the
        // "plugin has no such interface" request for everything.
        const _ = !0;
    }
}

/// Builds a VST3 [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
/// from the shared [`TransportInfo`], encoding the validity flags the
/// VST3 spec requires.
///
/// `requirements` is what the plugin returned from
/// `IProcessContextRequirements::getProcessContextRequirements` (or
/// [`all`](ProcessContextRequirements::all) when the plugin doesn't implement
/// that interface). Only the fields whose requirement bit is set are
/// populated and marked valid; the rest are left zeroed. Passing `all()`
/// fills everything, reproducing the unconditional behaviour this host had
/// before the interface was wired.
///
/// Field mapping:
/// - `position.samples` → `projectTimeSamples` (always). VST3 has no validity
///   bit for this field, so a snapshot without one (`None`) sends `0`; the
///   engine's plugin node reports one whenever the tempo is usable. See
///   `TransportPosition::samples`.
/// - `position.continuous_samples` → `continousTimeSamples` (the monotonic
///   clock that does not reset on loop; falls back to `position.samples` when
///   unset, i.e. `0`)
/// - `position.quarters` → `projectTimeMusic`
/// - `bar.position_quarters` → `barPositionMusic`
/// - `loop_region.{start,end}_quarters` → `cycleStartMusic`/`cycleEndMusic`
/// - `system_time_ns` → `systemTime`, under `kSystemTimeValid`, when present
/// - [`TransportInfo::samples_to_nearest_clock`] → `samplesToNextClock`,
///   under `kClockValid`, when derivable
/// - `sample_rate` → `sampleRate` (always)
#[must_use]
pub fn to_process_context(
    t: &TransportInfo,
    requirements: ProcessContextRequirements,
) -> vst3::Steinberg::Vst::ProcessContext {
    use ProcessContextRequirements as need;

    let wants = |flag: ProcessContextRequirements| requirements.intersects(flag);
    // The MIDI clock grid under the playhead, at the snapshot's own rate: a
    // guessed rate (the 44.1 kHz fallback below) is not a fact to flag.
    let to_clock = t.samples_to_nearest_clock();

    // `sdk_enum_u32` normalises the platform-varying SDK constants to `u32`.
    let state = {
        let mut state = 0u32;
        // The play/record/cycle bits are part of the transport state the plugin
        // reads only when it asked for kNeedTransportState.
        if wants(need::NEED_TRANSPORT_STATE) {
            if t.state.playing {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kPlaying);
            }
            if t.state.recording {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kRecording);
            }
            if t.state.cycle_active {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kCycleActive);
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
            state |= crate::sdk_enum_u32(StatesAndFlags_::kCycleValid);
        }
        // Each `*Valid` bit advertises that the matching field below is filled,
        // so it must track the same requirement gate — AND the value must be one
        // a plugin can compute with.
        //
        // The requirement mask alone is not enough: it says what the plugin
        // *asked for*, not what the host *has*. Setting a validity bit over a
        // NaN or an infinity is worse than leaving it clear, because a plugin
        // that trusts the bit propagates the NaN through its timing math into
        // the audio buffer. The VST2 path gates on the value the same way, via
        // the shared `is_usable` in `tutti-plugin-types`.
        if wants(need::NEED_PROJECT_TIME_MUSIC) && is_usable(t.position.quarters) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid);
        }
        if wants(need::NEED_BAR_POSITION_MUSIC) && is_usable(t.bar.position_quarters) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid);
        }
        // Tempo carries a domain rule beyond finiteness: zero or negative BPM
        // is not a tempo, and a plugin dividing by it produces an infinity.
        // Matches the VST2 path's `is_usable(..) && > 0.0`.
        if wants(need::NEED_TEMPO) && is_usable(t.timing.tempo) && t.timing.tempo > 0.0 {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kTempoValid);
        }
        if wants(need::NEED_TIME_SIGNATURE) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid);
        }
        // `continousTimeSamples` is filled below whenever the plugin asked for
        // it, so `kContTimeValid` must be set on the same condition. Without the
        // bit a spec-correct plugin ignores the field entirely — the value was
        // being computed and shipped into a dead slot.
        if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid);
        }
        // The host clock and the MIDI clock: flagged only when the snapshot
        // has them (an offline render has no host clock; an unusable tempo
        // or rate leaves no clock grid), on the same condition that fills
        // the field below. `kSmpteValid` / `kChordValid` stay clear: their
        // fields have no producer.
        if wants(need::NEED_SYSTEM_TIME) && t.system_time_ns.is_some() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid);
        }
        if wants(need::NEED_SAMPLES_TO_NEXT_CLOCK) && to_clock.is_some() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kClockValid);
        }
        state
    };

    let mut ctx: vst3::Steinberg::Vst::ProcessContext = crate::sdk::zeroed();
    ctx.state = state;
    // Always-on: not gated by any requirement flag in the spec.
    ctx.sampleRate = if t.sample_rate > 0.0 {
        t.sample_rate
    } else {
        44100.0
    };
    // VST3 carries no `kProjectTimeSamplesValid` bit — the field is always read
    // as fact — so a snapshot without a project-time position can only send
    // 0 (see `TransportPosition::samples`).
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
    if wants(need::NEED_SYSTEM_TIME)
        && let Some(ns) = t.system_time_ns
    {
        ctx.systemTime = ns;
    }
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
    if wants(need::NEED_SAMPLES_TO_NEXT_CLOCK)
        && let Some(frames) = to_clock
    {
        ctx.samplesToNextClock = frames;
    }
    // `smpteOffsetSubframes` / `frameRate` (kNeedFrameRate) and the chord field
    // (kNeedChord) are not sourced from TransportInfo yet; left zeroed with
    // `kSmpteValid` / `kChordValid` clear. When a producer exists they gate on
    // NEED_FRAME_RATE / NEED_CHORD here and set their own valid bit.
    ctx
}

#[cfg(test)]
mod tests {
    use super::ProcessContextRequirements as need;
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
            .with_time_signature(TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH))
            .with_playing(true)
            .with_recording(true)
            .with_system_time_ns(9_000_000_000)
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
        let all = need::all();

        let mut t = populated_transport();
        t.timing.tempo = f64::NAN;
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
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
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
            0,
            "a zero tempo must not be flagged valid"
        );

        let mut t = populated_transport();
        t.position.quarters = f64::INFINITY;
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
            0,
            "an infinite musical position must not be flagged valid"
        );

        // The gate must not fire on good values — otherwise it would pass by
        // clearing every bit unconditionally.
        let ctx = to_process_context(&populated_transport(), all);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
            0
        );
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
            0
        );
    }

    #[test]
    fn all_bits_fills_everything() {
        let t = populated_transport();
        let ctx = to_process_context(&t, need::all());

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
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kPlaying), 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kRecording), 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kCycleActive), 0);
        assert_ne!(
            s & crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
            0
        );
        assert_ne!(
            s & crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
            0
        );
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid), 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid), 0);
        assert_ne!(
            s & crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid),
            0,
            "continousTimeSamples is filled, so kContTimeValid must be set"
        );

        // The host clock, and the MIDI clock: 4.0 quarters is on a clock.
        assert_eq!(ctx.systemTime, 9_000_000_000);
        assert_ne!(
            s & crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
            0
        );
        assert_eq!(ctx.samplesToNextClock, 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kClockValid), 0);

        // Fields with no producer are neither written nor flagged.
        assert_eq!(s & crate::sdk_enum_u32(StatesAndFlags_::kSmpteValid), 0);
        assert_eq!(s & crate::sdk_enum_u32(StatesAndFlags_::kChordValid), 0);
    }

    /// Per `ivstprocesscontext.h`, each `*Valid` bit must be set iff its field
    /// carries meaning. This pins the pairing for every field this host fills:
    /// requesting exactly one field sets exactly that field's valid bit, and
    /// not requesting it leaves both the field and the bit clear.
    #[test]
    fn each_valid_bit_tracks_its_own_field() {
        let t = populated_transport();

        // (requirement bit, valid bit, "is the field non-zero?" probe)
        let cases: &[(
            &str,
            need,
            u32,
            fn(&vst3::Steinberg::Vst::ProcessContext) -> bool,
        )] = &[
            (
                "projectTimeMusic",
                need::NEED_PROJECT_TIME_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
                |c| c.projectTimeMusic != 0.0,
            ),
            (
                "barPositionMusic",
                need::NEED_BAR_POSITION_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
                |c| c.barPositionMusic != 0.0,
            ),
            (
                "tempo",
                need::NEED_TEMPO,
                crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
                |c| c.tempo != 0.0,
            ),
            (
                "timeSig",
                need::NEED_TIME_SIGNATURE,
                crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid),
                |c| c.timeSigNumerator != 0,
            ),
            (
                "continousTimeSamples",
                need::NEED_CONTINOUS_TIME_SAMPLES,
                crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid),
                |c| c.continousTimeSamples != 0,
            ),
            // `cycleStartMusic` was the one filled field missing from this
            // table, and its absence is why `kCycleValid` could be set from
            // `cycle_active` under the transport gate for so long: no case
            // here paired the bit with the field, so nothing contradicted it.
            (
                "systemTime",
                need::NEED_SYSTEM_TIME,
                crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
                |c| c.systemTime != 0,
            ),
            (
                "cycleStartMusic",
                need::NEED_CYCLE_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
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
            let off = to_process_context(&t, need::empty());
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
    /// Setting `kCycleValid` from `cycle_active` breaks both directions at
    /// once, and each half needs its own assertion because either alone still
    /// passes when only one direction is right.
    #[test]
    fn cycle_active_and_cycle_valid_are_gated_separately() {
        let t = populated_transport();

        // A plugin that asked only for transport state gets the running flag
        // but no claim about the bounds — which it never asked for, and which
        // are still `mem::zeroed` here.
        let transport_only = to_process_context(&t, need::NEED_TRANSPORT_STATE);
        assert_ne!(
            transport_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleActive),
            0,
            "a cycling transport must still report kCycleActive"
        );
        assert_eq!(
            transport_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
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
            cycle_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
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
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
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
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
            0
        );

        // Not requested → left zeroed, valid bit clear.
        assert_eq!(ctx.timeSigNumerator, 0);
        assert_eq!(ctx.timeSigDenominator, 0);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid),
            0
        );
        assert_eq!(ctx.barPositionMusic, 0.0);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
            0
        );
        assert_eq!(ctx.continousTimeSamples, 0);

        // Transport-state not requested → no play/record/cycle bits.
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kPlaying),
            0
        );
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleActive),
            0
        );
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
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid),
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
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid),
            0
        );
    }

    /// Always-on fields (sampleRate, projectTimeSamples) are populated even
    /// when the plugin requested literally nothing.
    #[test]
    fn always_on_fields_survive_empty_requirements() {
        let t = populated_transport();
        let ctx = to_process_context(&t, need::empty());

        assert_eq!(ctx.sampleRate, 48_000.0);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        // Everything gated is gone.
        assert_eq!(ctx.tempo, 0.0);
        assert_eq!(ctx.state, 0);
    }

    /// **`samplesToNextClock` is the signed distance to the nearest 24-ppq
    /// MIDI clock, under `kClockValid`**, and a snapshot with no host clock
    /// or no usable tempo claims neither field. 128 BPM at 48 kHz: a quarter
    /// is 22 500 frames, a clock 937.5; a playhead 0.2 clock past one is
    /// 187.5 frames after it, sent as -188 (rounded half away from zero).
    #[test]
    fn the_midi_clock_and_host_time_are_flagged_only_when_known() {
        let wants = need::NEED_SAMPLES_TO_NEXT_CLOCK | need::NEED_SYSTEM_TIME;
        let t = populated_transport().with_position_quarters(4.0 + 0.2 / 24.0);
        let ctx = to_process_context(&t, wants);
        assert_eq!(ctx.samplesToNextClock, -188);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kClockValid),
            0
        );
        assert_eq!(ctx.systemTime, 9_000_000_000);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
            0
        );

        let mut bare = populated_transport().with_tempo(0.0);
        bare.system_time_ns = None;
        let ctx = to_process_context(&bare, wants);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kClockValid),
            0
        );
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
            0
        );
        assert_eq!(ctx.samplesToNextClock, 0);
        assert_eq!(ctx.systemTime, 0);
    }
}
