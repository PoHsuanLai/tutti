//! `vbap_mix_parts` builds the same mix as `build_vbap_mix`, in a graph that
//! is not a `Net` (doc 013 Phase 3 PR 8).
//!
//! `build_vbap_mix` is `vbap_mix_parts(..).insert_into(net, ..)`, so the `Net`
//! side is one description of the mix by construction. What that cannot show is
//! that a *second* graph wiring [`VbapMixParts::edges`] its own way gets the
//! same mix — every edge resolved, no port left to read silence that the `Net`
//! wires. So each case builds the mix twice from the same sources, once with
//! `build_vbap_mix` into a `Net` and once from the parts into a
//! `tutti_graph::GraphBuilder`, and asserts the renders are **bit-identical**.
//!
//! Exact equality is portable: both sides run the same unit code on the same
//! machine. The graph renders 1024-frame blocks, which a `Legacy` unit runs as
//! 64-frame chunks from each block's start, so its chunks fall where the
//! `Net`'s 64-frame blocks do — the VBAP panner ramps its gains across each call
//! and would otherwise differ (doc 013, PR 7 notes).

use tutti_core::dsp::{Net, NodeId};
use tutti_core::{AudioUnit, BufferRef, BufferVec, SampleRate, MAX_BUFFER_SIZE};
use tutti_graph::{GraphBuilder, Prepare};
use tutti_nodes::testing::Osc;
use tutti_spatial::{build_vbap_mix, vbap_mix_parts, VbapMixNode, VbapMixParts, VbapSource};
use tutti_types::{Amplitude, ChannelLayout, Hz, NodeKey, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
/// 14 462 frames: neither side's last block is full.
const FRAMES: usize = 14_462;

/// Insert `parts` into `g`, wiring every edge, and return the sum's key — the
/// builder's counterpart of `VbapMixParts::insert_into`.
fn insert(g: &mut GraphBuilder, parts: VbapMixParts, sources: &[NodeKey]) -> NodeKey {
    let edges = parts.edges().to_vec();
    let panners: Vec<NodeKey> = parts
        .panners
        .into_iter()
        .map(|p| g.add_unit(Box::new(p)))
        .collect();
    let lfe = parts.lfe.map(|send| {
        (
            g.add_unit(Box::new(send.sum)),
            g.add_unit(Box::new(send.lowpass)),
        )
    });
    let sum = g.add_unit(Box::new(parts.sum));
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

/// Render the mix of `positions` at `layout` through both graphs; returns
/// `(net, graph)` planes.
fn both(layout: ChannelLayout, positions: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let width = layout.count() as usize;

    let mut net = Net::new(0, width);
    let ids: Vec<NodeId> = (0..positions.len()).map(|i| net.push(tone(i))).collect();
    let sources: Vec<VbapSource> = ids
        .iter()
        .zip(positions)
        .map(|(&id, &az)| VbapSource::at(id, az))
        .collect();
    let mix = build_vbap_mix(&mut net, layout, &sources).expect("a preset layout");
    net.pipe_output(mix);
    net.set_sample_rate(RATE);
    let mut a = vec![Vec::with_capacity(FRAMES); width];
    let mut buf = BufferVec::new(width);
    let mut done = 0;
    while done < FRAMES {
        let n = (FRAMES - done).min(MAX_BUFFER_SIZE);
        let mut out = buf.buffer_mut();
        net.process(n, &BufferRef::new(&[]), &mut out);
        for (c, plane) in a.iter_mut().enumerate() {
            plane.extend_from_slice(&out.channel_f32(c)[..n]);
        }
        done += n;
    }

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
    let b = g
        .renderer(Prepare::new(RATE, Samples(1024)))
        .expect("builds")
        .render(FRAMES);
    (a, b)
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
                "{}-wide: channel {c} differs first at frame {i}: net {} graph {}",
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
/// differs (silent in the graph), here and for 7.1.4. The same edge dropped
/// from the parts themselves reaches the `Net` too, so both sides agree on a
/// silent LFE: the `energy` assertion is what fails then (as does
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
