//! A VBAP mix is a panner per source, wired to the output channels: the graph
//! sums the fan-in. These cases were the `build_vbap_mix` builder's tests and
//! pin what the panners do through that wiring.

use tutti_graph::{GraphBuilder, NodeKey, Prepare};
use tutti_nodes::testing::Const;
use tutti_nodes::{VbapError, VbapPannerNode};
use tutti_types::{Azimuth, ChannelLayout, SampleRate, Samples};

/// A panner per `(azimuth, elevation)` source (a DC `[1.0, 1.0]` each), every
/// panner channel `c` wired to output `c`. Renders 49 blocks of 256 frames,
/// past the panners' ~0.05 s position smoother, and returns each output
/// channel's energy over the last block.
fn settled_energy(layout: ChannelLayout, bearings: &[f32]) -> Vec<f32> {
    let width = layout.count() as usize;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    for &az in bearings {
        let src: NodeKey = g.add(Const::frame(&[1.0, 1.0]));
        let panner = VbapPannerNode::for_layout(layout).expect("a preset");
        panner
            .controls()
            .set_position(Azimuth::new(az), tutti_types::Elevation::LEVEL);
        let (pan, _) = g.add_with_controls(panner);
        g.connect(src, 0, pan, 0).connect(src, 1, pan, 1);
        for c in 0..width {
            g.connect_output(pan, c, c);
        }
    }
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(256)))
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
/// Mutation (run): the 5.1 `speaker_channel_map` as `[0, 1, 2, 3, 4]` (the
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

/// Each supported width builds a panner of that width; one with no preset is
/// refused rather than falling back.
#[test]
fn for_layout_dispatches_and_rejects_unsupported() {
    for (width, want) in [(2u16, 2usize), (4, 4), (6, 6)] {
        let layout = if width == 2 {
            ChannelLayout::STEREO
        } else if width == 4 {
            ChannelLayout::QUAD
        } else {
            ChannelLayout::from(width)
        };
        assert_eq!(
            VbapPannerNode::for_layout(layout).unwrap().num_channels(),
            want
        );
    }
    assert!(matches!(
        VbapPannerNode::for_layout(ChannelLayout::from(3u16)),
        Err(VbapError::UnsupportedSpeakerLayout(3))
    ));
}
