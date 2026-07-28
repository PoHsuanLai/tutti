//! Build a `vst::api::TimeInfo` snapshot from [`TransportInfo`].
//!
//! The snapshot is stored into the host-state `time_info` arc-swap before
//! each process call and served back to the plugin via the
//! `audioMasterGetTime` callback in [`crate::host::HostState`].
//!
//! Flag bits come straight from `vst::api::TimeInfoFlags` (the vst-rs
//! transcription of the VST2.4 SDK). We never hand-roll the bit positions:
//! VST2.4 clusters four `*_VALID` bits at non-adjacent positions
//! (`PPQ=9, TEMPO=10, BARS=11, CYCLE=12, TIME_SIG=13`) and
//! `TRANSPORT_RECORDING` sits at bit 3 — mistranscribing any of them
//! silently mis-signals the plugin's transport state.
//!
//! # Two flag families, two different rules
//!
//! **`*_VALID` bits are claims about *this* snapshot.** Setting one tells the
//! plugin "the matching field holds a real number, use it". A `f64` has no
//! absent value, so a non-`Option` field is *not* evidence that the field was
//! populated: a zeroed [`TransportInfo`] hands over `tempo = 0.0`, and a plugin
//! told `kVstTempoValid` computes `60.0 / 0.0` and puts an infinity into the
//! master bus. Each bit is therefore gated on the field genuinely being usable
//! (JUCE's `setFromOptional` is the same shape). The one exception is
//! `TIME_SIG_VALID`, and it is an exception because the *type* does the work —
//! see [`build_vst2_time_info`].
//!
//! **`TRANSPORT_CHANGED` is a claim about the *transition*.** Per the SDK it
//! means the transport state just changed, and plugins use it as a reset
//! trigger: a tempo-synced delay or arpeggiator retriggers on it, a reverb
//! flushes its tail. Asserting it every block means those plugins reset every
//! 64 samples and never advance. It is a strict edge, computed against the
//! previously served snapshot exactly as JUCE and Ardour do.

use crate::types::TransportInfo;
// Shared with the VST3 path so the two cannot drift again.
use tutti_plugin_types::is_usable;

/// The transport bits whose transition defines `TRANSPORT_CHANGED`.
///
/// Ardour compares playing/recording/cycle; JUCE compares playing/recording.
/// We take the wider set — a cycle being switched on or off is a transport
/// state change by any reading of the SDK, and a plugin that resets on it is
/// resetting for a real reason.
fn transport_bits(flags: vst::api::TimeInfoFlags) -> i32 {
    use vst::api::TimeInfoFlags as F;
    (flags & (F::TRANSPORT_PLAYING | F::TRANSPORT_RECORDING | F::TRANSPORT_CYCLE_ACTIVE)).bits()
}

/// Build the snapshot the plugin will read via `audioMasterGetTime`.
///
/// `previous` is the snapshot last served to this plugin (`None` before the
/// first block). It is used only to compute the `TRANSPORT_CHANGED` edge; every
/// other bit is a function of `transport` alone.
pub(crate) fn build_vst2_time_info(
    transport: &TransportInfo,
    sample_rate: f64,
    previous: Option<&vst::api::TimeInfo>,
) -> vst::api::TimeInfo {
    use vst::api::TimeInfoFlags as F;

    // TIME_SIG_VALID is unconditional, and it is the only one that is. Not
    // because the field is non-`Option` — that argument is what set every other
    // VALID bit here unconditionally and shipped `tempo = 0, kVstTempoValid` to
    // plugins — but because `TimeSignature` is a validated newtype whose
    // constructors clamp both halves into their legal ranges. There is no
    // representable invalid signature, so the claim is always true.
    let mut flags = F::TIME_SIG_VALID;

    if is_usable(transport.position.quarters) {
        flags |= F::PPQ_POS_VALID;
    }
    // Tempo additionally has to be non-zero: plugins divide by it.
    if is_usable(transport.timing.tempo) && transport.timing.tempo > 0.0 {
        flags |= F::TEMPO_VALID;
    }
    if is_usable(transport.bar.position_quarters) {
        flags |= F::BARS_VALID;
    }

    if transport.state.playing {
        flags |= F::TRANSPORT_PLAYING;
    }
    if transport.state.recording {
        flags |= F::TRANSPORT_RECORDING;
    }
    if transport.state.cycle_active {
        // Signal both that the cycle is active AND that the cycle_start_pos /
        // cycle_end_pos fields are populated — without CYCLE_POS_VALID the
        // plugin ignores the filled cycle boundaries. Same finiteness rule as
        // the other position fields.
        flags |= F::TRANSPORT_CYCLE_ACTIVE;
        if is_usable(transport.loop_region.start_quarters)
            && is_usable(transport.loop_region.end_quarters)
        {
            flags |= F::CYCLE_POS_VALID;
        }
    }

    // The edge. Set only when the transport bits differ from the snapshot the
    // plugin last saw; on the very first block there is no previous state, so
    // there is no transition to report either.
    if let Some(prev) = previous {
        let prev_bits = transport_bits(F::from_bits_truncate(prev.flags));
        if transport_bits(flags) != prev_bits {
            flags |= F::TRANSPORT_CHANGED;
        }
    }

    vst::api::TimeInfo {
        sample_rate,
        // VST2.4 has no `*_VALID` bit for `sample_pos` — the plugin always reads
        // it as fact — so an absent project-time clock can only degrade to 0.
        // TODO (see `TransportPosition::samples`): source a real project-time
        // sample clock; until then a plugin doing sample-accurate math off
        // `samplePos` sees the project frozen at sample 0.
        sample_pos: transport.position.samples.unwrap_or(0).max(0) as f64,
        ppq_pos: transport.position.quarters,
        tempo: transport.timing.tempo,
        bar_start_pos: transport.bar.position_quarters,
        cycle_start_pos: transport.loop_region.start_quarters,
        cycle_end_pos: transport.loop_region.end_quarters,
        time_sig_numerator: transport.timing.signature.beats_per_bar().into(),
        time_sig_denominator: transport.timing.signature.note_value().into(),
        flags: flags.bits(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst::api::TimeInfoFlags as F;

    fn playing_transport() -> TransportInfo {
        TransportInfo::new()
            .with_playing(true)
            .with_recording(true)
            .with_loop(true, 0.0, 8.0)
    }

    #[test]
    fn flags_recording_and_cycle_active() {
        // Recording + cycle-active + playing, with real tempo/position values,
        // so every VALID bit is genuinely earned.
        let transport = playing_transport()
            .with_tempo(120.0)
            .with_position_quarters(4.0)
            .with_bar(4.0, Default::default());

        // First block: no previous snapshot, so no transition to report.
        let info = build_vst2_time_info(&transport, 48_000.0, None);

        let expected = (F::PPQ_POS_VALID
            | F::TEMPO_VALID
            | F::TIME_SIG_VALID
            | F::BARS_VALID
            | F::TRANSPORT_PLAYING
            | F::TRANSPORT_RECORDING
            | F::TRANSPORT_CYCLE_ACTIVE
            | F::CYCLE_POS_VALID)
            .bits();

        assert_eq!(info.flags, expected);

        // Spot-check the historically-mistranscribed bits are actually set.
        assert_ne!(info.flags & F::TRANSPORT_RECORDING.bits(), 0);
        assert_ne!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
        // AUTOMATION_WRITING (bit 6) must NOT be set — the old hand-rolled
        // table put TRANSPORT_RECORDING there by mistake.
        assert_eq!(info.flags & F::AUTOMATION_WRITING.bits(), 0);
    }

    #[test]
    fn flags_stopped_no_cycle() {
        // Stopped, not recording, no cycle. `TransportInfo::default()` carries a
        // real 120 BPM tempo and 4/4, and 0.0 is a perfectly usable position, so
        // the position/tempo VALID bits stand.
        let transport = TransportInfo::new();
        let info = build_vst2_time_info(&transport, 48_000.0, None);

        let expected =
            (F::PPQ_POS_VALID | F::TEMPO_VALID | F::TIME_SIG_VALID | F::BARS_VALID).bits();

        assert_eq!(info.flags, expected);
        assert_eq!(info.flags & F::TRANSPORT_PLAYING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_RECORDING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_eq!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
    }

    /// `kVstTransportChanged` means the transport state *just
    /// changed*, not "a transport snapshot exists". The previous version of
    /// this function asserted it on every block — and the previous version of
    /// this test asserted that as correct — so a tempo-synced delay or
    /// arpeggiator retriggered every 64 samples and never advanced, and a
    /// reverb flushed its tail every block.
    #[test]
    fn transport_changed_is_an_edge_not_a_level() {
        let stopped = TransportInfo::new();
        let playing = TransportInfo::new().with_playing(true);

        // Stopped → playing: an edge.
        let first = build_vst2_time_info(&stopped, 48_000.0, None);
        let start = build_vst2_time_info(&playing, 48_000.0, Some(&first));
        assert_ne!(start.flags & F::TRANSPORT_CHANGED.bits(), 0);

        // Playing → still playing, over and over: no edge. This is the block
        // that used to (wrongly) keep asserting the bit.
        let mut prev = start;
        for _ in 0..8 {
            let next = build_vst2_time_info(&playing, 48_000.0, Some(&prev));
            assert_eq!(
                next.flags & F::TRANSPORT_CHANGED.bits(),
                0,
                "TRANSPORT_CHANGED must not repeat while the transport is steady"
            );
            prev = next;
        }

        // Playing → stopped: an edge again.
        let stop = build_vst2_time_info(&stopped, 48_000.0, Some(&prev));
        assert_ne!(stop.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// The first snapshot has nothing to compare against, so there is no
    /// transition to announce.
    #[test]
    fn transport_changed_is_clear_on_the_first_snapshot() {
        let info = build_vst2_time_info(&playing_transport(), 48_000.0, None);
        assert_eq!(info.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// Recording and cycle transitions are transport changes too, even while
    /// `playing` never moves.
    #[test]
    fn transport_changed_fires_for_recording_and_cycle_transitions() {
        let playing = TransportInfo::new().with_playing(true);
        let base = build_vst2_time_info(&playing, 48_000.0, None);

        let armed = playing.with_recording(true);
        let rec = build_vst2_time_info(&armed, 48_000.0, Some(&base));
        assert_ne!(rec.flags & F::TRANSPORT_CHANGED.bits(), 0);

        let looped = armed.with_loop(true, 0.0, 8.0);
        let cyc = build_vst2_time_info(&looped, 48_000.0, Some(&rec));
        assert_ne!(cyc.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// A pure position advance is not a transport state change.
    #[test]
    fn moving_the_playhead_is_not_a_transport_change() {
        let a = TransportInfo::new()
            .with_playing(true)
            .with_position_quarters(1.0);
        let b = TransportInfo::new()
            .with_playing(true)
            .with_position_quarters(2.0);

        let first = build_vst2_time_info(&a, 48_000.0, None);
        let second = build_vst2_time_info(&b, 48_000.0, Some(&first));
        assert_eq!(second.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// A zeroed transport carries `tempo = 0.0`; advertising
    /// `kVstTempoValid` alongside it hands the plugin a divisor of zero, and
    /// `60.0 / tempo` puts an infinity on the master bus.
    #[test]
    fn tempo_valid_is_withheld_for_a_zero_tempo() {
        let transport = TransportInfo::new().with_tempo(0.0);
        let info = build_vst2_time_info(&transport, 48_000.0, None);
        assert_eq!(
            info.flags & F::TEMPO_VALID.bits(),
            0,
            "a zero tempo must not be advertised as valid"
        );
        // The other bits are unaffected — this is a per-field gate.
        assert_ne!(info.flags & F::PPQ_POS_VALID.bits(), 0);
        assert_ne!(info.flags & F::TIME_SIG_VALID.bits(), 0);
    }

    #[test]
    fn non_finite_fields_are_withheld() {
        let transport = TransportInfo::new()
            .with_tempo(f64::NAN)
            .with_position_quarters(f64::INFINITY)
            .with_bar(f64::NAN, Default::default());
        let info = build_vst2_time_info(&transport, 48_000.0, None);

        assert_eq!(info.flags & F::TEMPO_VALID.bits(), 0);
        assert_eq!(info.flags & F::PPQ_POS_VALID.bits(), 0);
        assert_eq!(info.flags & F::BARS_VALID.bits(), 0);
        // The signature is a validated newtype, so its claim always holds.
        assert_ne!(info.flags & F::TIME_SIG_VALID.bits(), 0);
    }

    /// A negative tempo is as unusable as a zero one.
    #[test]
    fn negative_tempo_is_withheld() {
        let info = build_vst2_time_info(&TransportInfo::new().with_tempo(-120.0), 48_000.0, None);
        assert_eq!(info.flags & F::TEMPO_VALID.bits(), 0);
    }

    /// `CYCLE_POS_VALID` is gated on the boundaries, not just on the cycle
    /// being switched on.
    #[test]
    fn cycle_pos_valid_requires_usable_boundaries() {
        let bad = TransportInfo::new().with_loop(true, 0.0, f64::NAN);
        let info = build_vst2_time_info(&bad, 48_000.0, None);
        assert_ne!(info.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_eq!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
    }
}
