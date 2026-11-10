//! The two graph nodes that fold channels, rendered on seeded multi-channel
//! input and hashed: the digest pins their output across the move from the
//! width-only folds to `tutti_types::Fold` (design 024 step 0b, consolidation 3).
//!
//! The digest is FNV-1a over the bits of every output sample, channel by channel,
//! block by block. A mid-render change of the convolver's wet/dry mix covers the
//! ramp path. The downmix render is adds and multiplies of `f32`, so its digest holds on
//! every platform and is a plain `assert_eq!`. The convolver's FFT reaches `libm`
//! (twiddle factors), so its digest goes through `assert_golden`.
//!
//! Mutation (run): the fold's mono scale `M3DB` -> 0.5 (in `Fold`'s mono row), or
//! a channel read at the wrong index, changes the digests and fails.

use tutti_graph::contract::{drive, prepared};
#[cfg(feature = "convolution")]
use tutti_nodes::ConvolverNode;
use tutti_nodes::DownmixNode;
use tutti_nodes::testing::assert_golden;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

fn noise(seed: u32, len: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn eat(&mut self, samples: &[f32]) {
        for s in samples {
            for byte in s.to_bits().to_le_bytes() {
                self.0 ^= u64::from(byte);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
}

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
            h.eat(&out);
        }
    }
    h.0
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
        all.eat(&[f32::from_bits(d as u32), f32::from_bits((d >> 32) as u32)]);
    }
    println!("DIGEST downmix {:#018x}", all.0);
    assert_eq!(all.0, DOWNMIX);
}

#[cfg(feature = "convolution")]
/// The convolver's mono-to-stereo fold at the widths the tree uses (2, 6), with
/// the mix moved mid-render.
#[test]
fn the_convolver_fold_renders_what_it_always_did() {
    let ir_a = noise(1, 200);
    let ir_b = noise(2, 150);
    let mut all = Fnv::new();
    for width in [2usize, 6] {
        let irs: Vec<&[f32]> = (0..width)
            .map(|c| if c % 2 == 0 { &ir_a[..] } else { &ir_b[..] })
            .collect();
        let mut node = prepared(ConvolverNode::folded(&irs, 32), RATE, BLOCK);
        let handle = node.mix();
        let d = digest(&mut node, &input(width, 1500), |b| {
            if b == 5 {
                handle.store(0.8, std::sync::atomic::Ordering::Relaxed);
            }
        });
        all.eat(&[f32::from_bits(d as u32), f32::from_bits((d >> 32) as u32)]);
    }
    println!("DIGEST convolver {:#018x}", all.0);
    assert_golden("the convolver's folded render", all.0, CONVOLVER);
}

const DOWNMIX: u64 = 0xa683_8aff_5759_c8b4;
#[cfg(feature = "convolution")]
const CONVOLVER: u64 = 0;
