//! A VBAP mix is a panner per source, wired to the output channels: the graph
//! sums the fan-in. These cases were the `build_vbap_mix` builder's tests and
//! pin what the panners do through that wiring.

use tutti_graph::{GraphBuilder, Prepare};
use tutti_nodes::testing::{Const, vbap_mix};
use tutti_nodes::{VbapError, VbapPannerNode};
use tutti_types::MaxBlock;
use tutti_types::{Azimuth, ChannelLayout, ChannelTopology, SampleRate, Samples};

/// A panner per bearing (a DC `[1.0, 1.0]` source each), wired by
/// [`vbap_mix`]. Renders 49 blocks of 256 frames, past the panners' ~0.05 s
/// position smoother, and returns each output channel's energy over the last
/// block.
fn settled_energy(layout: ChannelLayout, bearings: &[f32]) -> Vec<f32> {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    let sources: Vec<_> = bearings
        .iter()
        .map(|&az| (g.add(Const::frame(&[1.0, 1.0])), Azimuth::new(az)))
        .collect();
    vbap_mix(&mut g, layout, &sources).expect("a preset");
    let mut r = g
        .renderer(Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(Samples(256)),
        ))
        .expect("builds");
    let mut last = Vec::new();
    for _ in 0..49 {
        last = r.render(256);
    }
    last.iter().map(|c| c.iter().map(|s| s * s).sum()).collect()
}

/// Two sources, one at a front and one at a rear speaker of a quad field: the
/// fan-in keeps each in its own channel, and the render does not collapse to
/// the front pair. Quad: ch0 FL 45, ch1 FR -45, ch2 RL 135, ch3 RR -135.
///
/// Mutation (run): the rear panner's `connect_output` skipped for channel 2
/// -> the rear assertion fails.
#[test]
fn fanned_in_panners_place_front_and_rear_sources() {
    let energy = settled_energy(ChannelLayout::QUAD, &[45.0, 135.0]);
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "silent");
    assert!(
        energy[0] > total * 0.2,
        "front-left should be lit ({energy:?})"
    );
    assert!(
        energy[2] > total * 0.2,
        "rear-left should be lit: no collapse to stereo ({energy:?})"
    );
}

/// A dead-centre source lands in the centre channel (2 of 5.1 file order), not
/// the surrounds, and a left-surround source lands in channel 4 with the LFE
/// channel (3) exactly silent: the panner never writes it.
///
/// Mutation (run): the 5.1 `speaker_of_channel` as `[0, 1, 2, 3, 4]` (the
/// surrounds one slot early) -> the surround case fails on the LFE assertion.
#[test]
fn a_centre_source_lands_in_the_centre_and_never_in_the_lfe() {
    let energy = settled_energy(ChannelLayout::from(6u16), &[0.0]);
    let total: f32 = energy.iter().sum();
    assert!(energy[2] > total * 0.4, "centre dominates ({energy:?})");
    assert!(
        energy[4] < total * 0.05 && energy[5] < total * 0.05,
        "surrounds stay quiet ({energy:?})"
    );
    assert_eq!(
        energy[3], 0.0,
        "the panner leaves the LFE silent ({energy:?})"
    );

    let surround = settled_energy(ChannelLayout::from(6u16), &[110.0]);
    assert!(surround[4] > 0.0, "left surround is lit ({surround:?})");
    assert_eq!(surround[3], 0.0, "the LFE stays silent ({surround:?})");
}

/// Each supported width builds a panner of that width; one with no bed is
/// refused rather than falling back.
#[test]
fn for_topology_dispatches_and_rejects_unsupported() {
    for width in [2u16, 4, 6] {
        let topology = ChannelTopology::for_width(ChannelLayout::from(width));
        assert_eq!(
            VbapPannerNode::for_topology(&topology)
                .unwrap()
                .channels()
                .count() as usize,
            usize::from(width)
        );
    }
    let three = ChannelTopology::for_width(ChannelLayout::from(3u16));
    assert!(matches!(
        VbapPannerNode::for_topology(&three),
        Err(VbapError::NoBed(_))
    ));
}
