//! Spread reaches the stereo-width path of `VbapPannerNode`.
//!
//! The unit-power law of the gains, the stereo pan law and the rear arc are
//! `SpeakerLayout::gains_into`'s and are pinned there, directly, in
//! `tutti-dsp`'s `spatial` tests. What only the node can show is that its two
//! branches (the two-source width path and the mono fold) both reach
//! `apply_spread`.

use tutti_graph::Node;
use tutti_graph::contract::drive;
use tutti_nodes::VbapPannerNode;
use tutti_types::{Azimuth, Elevation, SampleRate, Spread, StereoWidth};

/// One block of one frame, written to `out`.
///
/// The node is not prepared: it runs at the 48 kHz it is built at, which is
/// the rate this drives it at, and a one-frame block needs no sizing.
fn tick(node: &mut VbapPannerNode, input: [f32; 2], out: &mut [f32]) {
    let rendered = drive(node, SampleRate(48_000.0), &[&input[..1], &input[1..]], &[]);
    for (o, c) in out.iter_mut().zip(rendered) {
        *o = c[0];
    }
}

/// Spread is applied on the stereo-width path, not silently dropped.
///
/// The width>0 branch of `VbapKernel::gains` must reach `apply_spread` too, or a
/// spread set on a node fed any non-zero width would do nothing at all. Width
/// and spread are orthogonal
/// controls (width separates two virtual sources, spread smears each across the
/// speaker field), so one must not suppress the other.
///
/// Checked on 5.1, where full spread has somewhere to spread *to*: it must
/// light up more speakers than a point source at the same bearing does.
///
/// Mutation: skip `apply_spread` in `SpeakerLayout::gains_into`, which both branches share (the
/// width branch included) → fails, because both readings light the same speakers.
#[test]
fn spread_reaches_the_stereo_width_path() {
    let mut node = VbapPannerNode::surround_5_1();
    node.controls().set_width(StereoWidth::new(1.0f32)); // the width>0 branch
    node.controls()
        .set_position(Azimuth::new(0.0f32), Elevation::new(0.0f32));

    let mut out = vec![0.0f32; node.num_channels()];

    node.controls().set_spread(Spread::new(0.0f32));
    Node::reset(&mut node);
    tick(&mut node, [1.0, 1.0], &mut out);
    let point_lit = out.iter().filter(|g| g.abs() > 0.05).count();

    node.controls().set_spread(Spread::new(1.0f32));
    Node::reset(&mut node);
    tick(&mut node, [1.0, 1.0], &mut out);
    let diffuse_lit = out.iter().filter(|g| g.abs() > 0.05).count();

    assert!(
        diffuse_lit > point_lit,
        "full spread lit {diffuse_lit} speakers and a point source lit \
         {point_lit} — spread is being dropped on the width>0 path"
    );
}
