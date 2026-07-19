//! Build a `vst::api::TimeInfo` snapshot from [`TransportInfo`].
//!
//! The snapshot is stored into the host-state `time_info` arc-swap before
//! each process call and served back to the plugin via the
//! `audioMasterGetTime` callback in [`crate::host::HostState`].

use crate::types::TransportInfo;

// VST2 TimeInfo flag constants (from VST2.4 SDK)
mod flags {
    pub const TRANSPORT_CHANGED: i32 = 1 << 0;
    pub const TRANSPORT_PLAYING: i32 = 1 << 1;
    pub const TRANSPORT_CYCLE_ACTIVE: i32 = 1 << 2;
    pub const TRANSPORT_RECORDING: i32 = 1 << 6;
    pub const TEMPO_VALID: i32 = 1 << 9;
    pub const TIME_SIG_VALID: i32 = 1 << 10;
    pub const PPQ_POS_VALID: i32 = 1 << 11;
    pub const BARS_VALID: i32 = 1 << 13;
}

pub(crate) fn build_vst2_time_info(
    transport: &TransportInfo,
    sample_rate: f64,
) -> vst::api::TimeInfo {
    use flags::*;

    let mut flag_bits =
        TRANSPORT_CHANGED | TEMPO_VALID | TIME_SIG_VALID | PPQ_POS_VALID | BARS_VALID;

    if transport.state.playing {
        flag_bits |= TRANSPORT_PLAYING;
    }
    if transport.state.recording {
        flag_bits |= TRANSPORT_RECORDING;
    }
    if transport.state.cycle_active {
        flag_bits |= TRANSPORT_CYCLE_ACTIVE;
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
        flags: flag_bits,
        ..Default::default()
    }
}
