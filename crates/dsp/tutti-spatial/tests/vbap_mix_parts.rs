//! `vbap_mix_parts` builds the same mix as `build_vbap_mix` when a graph
//! wires the parts its own way (doc 013 Phase 3 PR 8).
//!
//! `build_vbap_mix` is `vbap_mix_parts(..).insert_into(g, ..)`, so its graph
//! is one description of the mix by construction. What that cannot show is
//! that a *second* wiring of [`VbapMixParts::edges`] — a host inserting the
//! units through its own graph, as bevy-tutti does — gets the same mix: every
//! edge resolved, no port left to read silence that the builder wires. So
//! each case builds the mix twice from the same sources, once with
//! `build_vbap_mix` and once from the parts through this file's own
//! [`insert`], and asserts the renders are **bit-identical**. (The first side
//! was a `Net` until the LFE low-pass became a native node, which a `Net`
//! cannot hold.)
//!
//! Exact equality is portable: both sides run the same unit code on the same
//! machine, in the same blocks.

use tutti_core::{AudioUnit, SampleRate};
use tutti_graph::{GraphBuilder, Prepare};
use tutti_nodes::testing::Osc;
use tutti_spatial::{build_vbap_mix, vbap_mix_parts, VbapMixNode, VbapMixParts, VbapSource};
use tutti_types::{Amplitude, ChannelLayout, Hz, NodeKey, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
/// 14 462 frames: neither side's last block is full.
const FRAMES: usize = 14_462;

/// Insert `parts` into `g`, wiring every edge, and return the sum's key — an
/// independent counterpart of `VbapMixParts::insert_into`, written here.
fn insert(g: &mut GraphBuilder, parts: VbapMixParts, sources: &[NodeKey]) -> NodeKey {
    let edges = parts.edges().to_vec();
    let panners: Vec<NodeKey> = parts
        .panners
        .into_iter()
        .map(|p| g.add_with_controls(p).0)
        .collect();
    let lfe = parts
        .lfe
        .map(|send| (g.add(send.sum), g.add_with_controls(send.lowpass).0));
    let sum = g.add(parts.sum);
    let key = |n: VbapMixNode| match n {
        VbapMixNode::Source(i) => sources[i],
        VbapMixNode::Panner(i) => panners[i],
        VbapMixNode::LfeSum => lfe.expect("an LFE edge implies the send").0,
        VbapMixNode::LfeLowpass => lfe.expect("an LFE edge implies the send").1,
        VbapMixNode::Sum => sum,
    };
    for e in edges {
        g.connect(key(e.from), e.from_port, key(e.to), e.to_port);
    }
    sum
}

/// A stereo tone per source, so every source is distinguishable and the LFE
/// low-pass has something above its cutoff to take out.
fn tone(i: usize) -> Box<dyn AudioUnit> {
    Box::new(
        Osc::sine(Hz(200.0 + 150.0 * i as f32))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO),
    )
}

/// Render the mix of `positions` at `layout` both ways; returns
/// `(build_vbap_mix, parts)` planes.
fn both(layout: ChannelLayout, positions: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let render = |g: GraphBuilder| {
        g.renderer(Prepare::new(RATE, Samples(1024)))
            .expect("builds")
            .render(FRAMES)
    };

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    let keys: Vec<NodeKey> = (0..positions.len()).map(|i| g.add_unit(tone(i))).collect();
    let sources: Vec<VbapSource> = keys
        .iter()
        .zip(positions)
        .map(|(&k, &az)| VbapSource::at(k, az))
        .collect();
    let mix = build_vbap_mix(&mut g, layout, &sources).expect("a preset layout");
    g.pipe_output(mix);
    let a = render(g);

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    let keys: Vec<NodeKey> = (0..positions.len()).map(|i| g.add_unit(tone(i))).collect();
    let sources: Vec<VbapSource<NodeKey>> = keys
        .iter()
        .zip(positions)
        .map(|(&k, &az)| VbapSource::at(k, az))
        .collect();
    let parts = vbap_mix_parts(layout, &sources).expect("a preset layout");
    let mix = insert(&mut g, parts, &keys);
    g.pipe_output(mix);
    (a, render(g))
}

fn assert_bit_identical(layout: ChannelLayout, positions: &[f32]) -> Vec<Vec<f32>> {
    let (a, b) = both(layout, positions);
    assert_eq!(a.len(), b.len());
    for (c, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(x.len(), y.len());
        if let Some(i) = x
            .iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits())
        {
            panic!(
                "{}-wide: channel {c} differs first at frame {i}: builder {} parts {}",
                a.len(),
                x[i],
                y[i]
            );
        }
    }
    a
}

fn energy(plane: &[f32]) -> f32 {
    plane.iter().map(|s| s * s).sum()
}

/// Quad (no LFE): two sources, front-left and rear-left.
///
/// Mutation (run): `insert` skipping the source→panner edges → channel 0
/// differs (the graph's panners read silence), here and in both cases below.
#[test]
fn a_quad_mix_from_parts_renders_as_build_vbap_mix_does() {
    let planes = assert_bit_identical(ChannelLayout::QUAD, &[45.0, 135.0]);
    // Not vacuous: the rear source reached the rear-left channel.
    assert!(energy(&planes[2]) > 1.0, "no rear energy");
}

/// 5.1, which has the LFE send: the sum's extra input group and the mono sum
/// feeding the low-pass are the edges a hand-written port is most likely to
/// miss.
///
/// Mutations (run): `insert` skipping the low-pass→sum edge → channel 3
/// differs (silent from the parts), here and for 7.1.4. The same edge
/// dropped from the parts themselves reaches the builder too, so both sides
/// agree on a silent LFE: the `energy` assertion is what fails then (as does
/// `lfe_channel_receives_bass_managed_send` in `src/vbap/mix.rs`).
#[test]
fn a_5_1_mix_with_its_lfe_send_renders_as_build_vbap_mix_does() {
    let planes = assert_bit_identical(ChannelLayout::from(6u16), &[0.0, 110.0]);
    assert!(energy(&planes[3]) > 1.0, "the LFE send is silent");
}

/// 7.1.4, the widest preset, with three sources.
#[test]
fn a_7_1_4_mix_from_parts_renders_as_build_vbap_mix_does() {
    let planes = assert_bit_identical(ChannelLayout::from(12u16), &[30.0, 150.0, -150.0]);
    assert!(energy(&planes[3]) > 1.0, "the LFE send is silent");
}

/// The parts describe every port the mix wires and nothing else: for 5.1 with
/// one source, two edges into the panner, one into the LFE sum, one into the
/// low-pass, six from the panner into the sum and one from the low-pass into
/// the sum's LFE slot of the second group.
///
/// Mutations (run): the LFE edge into the sum at `send.channel` instead of
/// `panners.len() * channels + send.channel` → the last assertion fails (the
/// renders above still agree, both sides reading the same wrong slot); the
/// LFE edge dropped → the count fails.
#[test]
fn the_parts_list_every_edge_of_the_mix() {
    let parts = vbap_mix_parts(ChannelLayout::from(6u16), &[VbapSource::at((), 0.0)])
        .expect("5.1 is a preset");
    let edges = parts.edges();
    assert_eq!(edges.len(), 11, "{edges:#?}");
    assert_eq!(parts.panners.len(), 1);
    let lfe = parts.lfe.as_ref().expect("5.1 has an LFE channel");
    assert_eq!(lfe.channel, 3);
    assert!(edges
        .iter()
        .all(|e| !matches!(e.to, VbapMixNode::Source(_))));
    let into_sum_lfe = edges
        .iter()
        .find(|e| e.from == VbapMixNode::LfeLowpass)
        .expect("the send reaches the sum");
    assert_eq!(
        (into_sum_lfe.to, into_sum_lfe.to_port),
        (VbapMixNode::Sum, 6 + 3)
    );
}

/// An empty source list is still a valid mix: the sum alone, with no edges
/// (quad has no LFE send), and silent.
#[test]
fn an_empty_mix_is_a_silent_sum() {
    let parts = vbap_mix_parts::<()>(ChannelLayout::QUAD, &[]).expect("quad");
    assert!(parts.panners.is_empty());
    assert!(parts.edges().is_empty());
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let mix = insert(&mut g, parts, &[]);
    g.pipe_output(mix);
    let out = g
        .renderer(Prepare::new(RATE, Samples(64)))
        .expect("builds")
        .render(100);
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}
