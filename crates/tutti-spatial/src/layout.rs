//! SMPTE / WAV channel-order conventions: where LFE sits, and how a speaker
//! index maps to a file channel.
//!
//! Interchange-format knowledge, not panner logic — it describes the
//! destination buffer. The VBAP node uses [`speaker_channel_map`] to scatter
//! its gains; the mix builder uses [`lfe_channel`] to route bass management.

use tutti_types::ChannelLayout;

/// `map[i]` = file channel for speaker `i`.
///
/// VBAP presets carry no LFE (5.1/7.1 are literally 5.0/7.0) and run
/// `[L, R, C, surrounds…]`, while the file order is `[FL, FR, C, LFE, SL, SR, …]`
/// with LFE at 3. A straight gain-i → channel-i write would put the surrounds
/// one slot early. LFE is absent here — [`build_vbap_mix`](crate::vbap::build_vbap_mix)
/// feeds it separately. Widths without a preset map identity.
pub(crate) fn speaker_channel_map(layout: ChannelLayout) -> Vec<usize> {
    match layout.count() {
        6 => vec![0, 1, 2, 4, 5],
        8 => vec![0, 1, 2, 4, 5, 6, 7],
        12 => vec![0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11],
        n => (0..n as usize).collect(),
    }
}

/// The LFE channel index (always 3 in 5.1 / 7.1 / 7.1.4 file order), or `None`
/// for layouts without one. Not a panned speaker — see [`speaker_channel_map`].
pub(crate) fn lfe_channel(layout: ChannelLayout) -> Option<usize> {
    match layout.count() {
        6 | 8 | 12 => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
