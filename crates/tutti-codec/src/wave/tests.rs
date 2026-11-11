use super::*;
use tutti_types::SampleRate;
use tutti_types::Seconds;

/// `push_frame` appends to every channel and advances the length once.
#[test]
fn push_frame_appends_one_frame_across_channels() {
    let mut w = Wave::new(2, SampleRate::new(48_000.0));
    w.push_frame(&[0.25, -0.5]);
    assert_eq!(w.len(), 1);
    assert_eq!((w.at(0, 0), w.at(1, 0)), (0.25, -0.5));
}

#[test]
#[should_panic(expected = "frame width")]
fn push_frame_rejects_a_frame_of_the_wrong_width() {
    Wave::new(2, SampleRate::new(48_000.0)).push_frame(&[0.5]);
}

#[test]
fn zero_rounds_the_duration_to_the_nearest_frame() {
    let w = Wave::zero(1, SampleRate::new(2.0), Seconds::new(0.75));
    assert_eq!(w.len(), 2);
    assert!(w.channel(0).iter().all(|&s| s == 0.0));
}

/// A wave built from a count assumes the engine's order where it has one and
/// says "unassigned" where it does not (4 is quad or B-format; 3 and 5 have no
/// order), and one built from a topology keeps it, one channel per speaker.
#[test]
fn a_count_assumes_smpte_or_unassigned_and_a_topology_is_kept() {
    use tutti_types::{ChannelLayout, ChannelTopology, Speaker};
    for n in [1u16, 2, 6, 8, 12] {
        let w = Wave::new(usize::from(n), SampleRate::new(48_000.0));
        assert_eq!(
            Some(w.topology()),
            ChannelTopology::smpte(ChannelLayout::from(n)).as_ref(),
            "{n}"
        );
    }
    for n in [3usize, 4, 5, 7, 16] {
        assert_eq!(
            Wave::new(n, SampleRate::new(48_000.0))
                .topology()
                .positions(),
            vec![Speaker::Unassigned; n],
            "{n}"
        );
    }
    assert!(
        Wave::new(0, SampleRate::new(48_000.0))
            .topology()
            .positions()
            .is_empty()
    );
    assert_eq!(
        Wave::from_samples(SampleRate::new(48_000.0), &[0.5])
            .topology()
            .positions(),
        [Speaker::FrontCenter]
    );

    let quad = Wave::with_topology(ChannelTopology::quad(), SampleRate::new(48_000.0));
    assert_eq!(quad.channels(), 4);
    assert_eq!(quad.topology(), &ChannelTopology::quad());
}

/// A wave wider than a layout holds is refused, not given the wrapped width
/// (65 538 channels would otherwise be a stereo topology over 65 538 vectors).
#[test]
#[should_panic(expected = "at most u16::MAX channels")]
fn a_wave_wider_than_a_layout_is_refused() {
    let _ = Wave::new(usize::from(u16::MAX) + 1, SampleRate::new(48_000.0));
}
