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

use crate::types::TransportInfo;

pub(crate) fn build_vst2_time_info(
    transport: &TransportInfo,
    sample_rate: f64,
) -> vst::api::TimeInfo {
    use vst::api::TimeInfoFlags as F;

    // All four numeric fields (ppq_pos, tempo, bar_start_pos, time signature)
    // are always populated below, so their VALID bits are set unconditionally,
    // alongside TRANSPORT_CHANGED.
    let mut flags = F::TRANSPORT_CHANGED
        | F::PPQ_POS_VALID
        | F::TEMPO_VALID
        | F::TIME_SIG_VALID
        | F::BARS_VALID;

    if transport.state.playing {
        flags |= F::TRANSPORT_PLAYING;
    }
    if transport.state.recording {
        flags |= F::TRANSPORT_RECORDING;
    }
    if transport.state.cycle_active {
        // Signal both that the cycle is active AND that the cycle_start_pos /
        // cycle_end_pos fields are populated — without CYCLE_POS_VALID the
        // plugin ignores the filled cycle boundaries.
        flags |= F::TRANSPORT_CYCLE_ACTIVE | F::CYCLE_POS_VALID;
    }

    vst::api::TimeInfo {
        sample_rate,
        sample_pos: transport.position.samples.max(0) as f64,
        ppq_pos: transport.position.quarters,
        tempo: transport.timing.tempo,
        bar_start_pos: transport.bar.position_quarters,
        cycle_start_pos: transport.loop_region.start_quarters,
        cycle_end_pos: transport.loop_region.end_quarters,
        time_sig_numerator: transport.timing.time_sig_numerator,
        time_sig_denominator: transport.timing.time_sig_denominator,
        flags: flags.bits(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst::api::TimeInfoFlags as F;

    #[test]
    fn flags_recording_and_cycle_active() {
        // Recording + cycle-active + playing: every conditional bit should
        // fire alongside the unconditional VALID set.
        let transport = TransportInfo::new()
            .with_playing(true)
            .with_recording(true)
            .with_loop(true, 0.0, 8.0);

        let info = build_vst2_time_info(&transport, 48_000.0);

        let expected = (F::TRANSPORT_CHANGED
            | F::PPQ_POS_VALID
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
        // Stopped, not recording, no cycle: only the unconditional VALID set.
        let transport = TransportInfo::new();
        let info = build_vst2_time_info(&transport, 48_000.0);

        let expected = (F::TRANSPORT_CHANGED
            | F::PPQ_POS_VALID
            | F::TEMPO_VALID
            | F::TIME_SIG_VALID
            | F::BARS_VALID)
            .bits();

        assert_eq!(info.flags, expected);
        assert_eq!(info.flags & F::TRANSPORT_PLAYING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_RECORDING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_eq!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
    }
}
