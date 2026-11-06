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
