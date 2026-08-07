//! SMPTE / WAV channel-order conventions: where LFE sits, and how a speaker
//! index maps to a file channel.
//!
//! Interchange-format knowledge, not panner logic — it describes the
//! destination buffer. The VBAP node uses [`speaker_channel_map`] to scatter
//! its gains; the mix builder uses [`lfe_channel`] to route bass management.

use tutti_core::ChannelLayout;

/// `map[i]` = file channel for speaker `i`.
///
/// VBAP presets carry no LFE (5.1/7.1 are literally 5.0/7.0) and run
/// `[L, R, C, surrounds…]`, while the file order is `[FL, FR, C, LFE, SL, SR, …]`
/// with LFE at 3. A straight gain-i → channel-i write would put the surrounds
/// one slot early. LFE is absent here — [`build_surround_mix`](crate::vbap::build_surround_mix)
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
mod tests {
    use super::*;

    /// A panner writing into LFE is the bug this module exists to prevent, and
    /// it is silent: the file plays, with bass wrong and a surround missing.
    #[test]
    fn speaker_map_never_targets_the_lfe_channel() {
        for count in [6u16, 8, 12] {
            let layout = ChannelLayout::from(count);
            let lfe = lfe_channel(layout).expect("these layouts have an LFE");
            let map = speaker_channel_map(layout);
            assert!(
                !map.contains(&lfe),
                "{count}ch: speaker map {map:?} writes into LFE channel {lfe}"
            );
        }
    }

    /// A duplicate would drop a speaker; a gap would leave one silent.
    #[test]
    fn speaker_map_covers_every_non_lfe_channel_exactly_once() {
        for count in [2u16, 4, 6, 8, 12] {
            let layout = ChannelLayout::from(count);
            let map = speaker_channel_map(layout);
            let expected: Vec<usize> = (0..count as usize)
                .filter(|c| Some(*c) != lfe_channel(layout))
                .collect();
            let mut sorted = map.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted, expected,
                "{count}ch: map {map:?} is not a bijection"
            );
        }
    }

    /// The other branch: no `.1`, so nothing to route around.
    #[test]
    fn layouts_without_lfe_map_identity() {
        for count in [2u16, 4] {
            let layout = ChannelLayout::from(count);
            assert_eq!(lfe_channel(layout), None);
            let map = speaker_channel_map(layout);
            assert_eq!(map, (0..count as usize).collect::<Vec<_>>());
        }
    }
}
