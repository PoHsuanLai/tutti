//! The two graph nodes that fold channels, rendered on seeded multi-channel
//! input and hashed: the digest pins their output across the move from the
//! width-only folds to `tutti_types::Fold` (design 024 step 0b, consolidation 3).
//!
//! The digest is FNV-1a over the bits of every output sample, channel by channel,
//! block by block. A mid-render change of the convolver's mix (the graph's:
//! the digest was recorded when the node blended internally, and is unchanged
//! by the move) covers the ramp path. The downmix render is adds and multiplies of `f32`, so its digest holds on
//! every platform and is a plain `assert_eq!`. The convolver's FFT reaches `libm`
//! (twiddle factors), so its digest goes through `assert_golden`.
//!
//! Mutation (run): the fold's mono scale `M3DB` -> 0.5 (in `Fold`'s mono row), or
//! a channel read at the wrong index, changes the digests and fails.

use tutti_fixtures::{Fnv, noise};
use tutti_graph::contract::{drive, prepared};
#[cfg(feature = "convolution")]
use tutti_graph::{GraphBuilder, Prepare};
#[cfg(feature = "convolution")]
use tutti_nodes::ConvolverNode;
use tutti_nodes::DownmixNode;
#[cfg(feature = "convolution")]
use tutti_nodes::testing::assert_golden;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};
#[cfg(feature = "convolution")]
use tutti_types::{Mix, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

/// `width` channels of distinct seeded noise, `frames` long.
fn input(width: usize, frames: usize) -> Vec<Vec<f32>> {
    (0..width).map(|c| noise(100 + c as u32, frames)).collect()
}

/// Renders `node` over `x` in blocks, calling `between(block_index)` before each.
fn digest(node: &mut dyn tutti_graph::Node, x: &[Vec<f32>], mut between: impl FnMut(usize)) -> u64 {
    let mut h = Fnv::new();
    let frames = x[0].len();
    for (b, at) in (0..frames).step_by(BLOCK).enumerate() {
        between(b);
        let n = BLOCK.min(frames - at);
        let block: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
        for out in drive(node, RATE, &block, &[]) {
            h.f32s(&out);
        }
    }
    h.finish()
}

/// Every width the engine has a matrix for, to stereo and to mono.
#[test]
fn the_downmix_node_renders_what_the_width_folds_did() {
    let mut all = Fnv::new();
    for (src, dst) in [
        (1u16, 2u16),
        (2, 1),
        (4, 2),
        (4, 1),
        (6, 2),
        (6, 1),
        (8, 2),
        (8, 1),
        (12, 2),
        (12, 1),
    ] {
        let dst_topology = ChannelTopology::smpte(ChannelLayout::from(dst)).unwrap();
        // Quad has no SMPTE order: `ChannelTopology::quad`.
        let src_topology = if src == 4 {
            ChannelTopology::quad()
        } else {
            ChannelTopology::smpte(ChannelLayout::from(src)).unwrap()
        };
        let mut node = prepared(
            DownmixNode::between(&src_topology, &dst_topology).unwrap(),
            RATE,
            BLOCK,
        );
        let d = digest(&mut node, &input(usize::from(src), 1000), |_| {});
        all.u64(d);
    }
    println!("DIGEST downmix {:#018x}", all.finish());
    assert_eq!(all.finish(), DOWNMIX);
}

#[cfg(feature = "convolution")]
/// The convolver's mono-to-stereo fold at the widths the tree uses (2, 6), in a
/// graph at a mix of 0.5 moved to 0.8 mid-render.
///
/// Mutation (run): the graph's mix set to 0.6 instead of 0.5 fails.
#[test]
fn the_convolver_fold_renders_what_it_always_did() {
    let ir_a = noise(1, 200);
    let ir_b = noise(2, 150);
    let mut all = Fnv::new();
    for width in [2usize, 6] {
        let irs: Vec<&[f32]> = (0..width)
            .map(|c| if c % 2 == 0 { &ir_a[..] } else { &ir_b[..] })
            .collect();
        let x = input(width, 1500);
        let mut g = GraphBuilder::new(
            ChannelLayout::from(width as u16),
            ChannelLayout::from(width as u16),
        );
        let (key, _) = g.add_with_controls(ConvolverNode::folded(&irs, 32));
        for c in 0..width {
            g.connect_input(c, key, c).connect_output(key, c, c);
        }
        g.spec_mut().set_mix(key, Mix::new(0.5));
        let mut r = g.renderer(Prepare::new(RATE, Samples(BLOCK))).unwrap();
        let mut h = Fnv::new();
        for (b, at) in (0..1500).step_by(BLOCK).enumerate() {
            if b == 5 {
                r.editor_mut().set_mix(key, Mix::new(0.8)).unwrap();
            }
            let n = BLOCK.min(1500 - at);
            let block: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
            for out in r.render_input(&block) {
                h.f32s(&out);
            }
        }
        all.u64(h.finish());
    }
    println!("DIGEST convolver {:#018x}", all.finish());
    assert_golden("the convolver's folded render", all.finish(), CONVOLVER);
}

const DOWNMIX: u64 = 0xa683_8aff_5759_c8b4;
#[cfg(feature = "convolution")]
const CONVOLVER: u64 = 0x16de_0f44_0e66_cf81;
