//! The phaser's sweep mapping and the strip's balance law moved into
//! `tutti-dsp` (doc 024 step 0b, consolidation 10); the nodes call them and
//! render what they always did. Each digest is FNV-1a over the bits of every
//! output sample of a seeded render with a control moved between blocks (the
//! phaser's depth and sweep range, the strip's pan and volume), recorded before
//! the move.
//!
//! The phaser reaches libm (`sin` for the LFO, `tan` in the coefficient), so its
//! digest goes through `assert_golden`. The strip's render is `min`, adds and
//! multiplies of `f32` with no libm call, so its digest is a plain `assert_eq!`.

use tutti_fixtures::{Fnv, Noise};
use tutti_graph::Node;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::testing::assert_golden;
use tutti_nodes::{BusStripNode, PhaserNode};
use tutti_types::{Amplitude, ChannelLayout, Depth, Hz, Mix, Pan, SampleRate};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const BLOCKS: usize = 200;

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    Noise::seeded(seed).take(frames).map(|x| 0.5 * x).collect()
}

/// Renders `BLOCKS` blocks of two seeded noise channels, calling `change` with
/// the node and the block index before each, and returns the FNV-1a digest of
/// the output.
fn digest<N: Node>(node: &mut N, change: impl Fn(&mut N, usize)) -> u64 {
    let ins = [noise(1, BLOCK * BLOCKS), noise(2, BLOCK * BLOCKS)];
    let mut d = Fnv::new();
    for k in 0..BLOCKS {
        change(node, k);
        let r = k * BLOCK..(k + 1) * BLOCK;
        let out = drive(node, SR, &[&ins[0][r.clone()], &ins[1][r]], &[]);
        for ch in &out {
            d.f32s(ch);
        }
    }
    d.finish()
}

/// Mutation (run): the sweep's `min + (max - min) * lfo * depth` with `max`
/// and `min` swapped, or the LFO's `* 0.5 + 0.5` dropped, changes the digest
/// and fails.
#[test]
fn the_phaser_renders_what_it_always_did() {
    let mut node: PhaserNode = prepared(
        PhaserNode::with_channels(ChannelLayout::STEREO, 6),
        SR,
        BLOCK,
    );
    node.set_mix(Mix::new(1.0));
    node.set_rate(Hz(2.0));
    let got = digest(&mut node, |n, k| match k {
        50 => n.set_depth(Depth(0.9)),
        100 => n.set_frequency_range(Hz(300.0), Hz(2_500.0)),
        150 => n.set_depth(Depth(0.2)),
        _ => {}
    });
    println!("PHASER {got:#018x}");
    assert_golden("phaser sweep", got, PHASER_DIGEST);
}

/// Mutation (run): `1.0 - p` -> `1.0 + p` for the left side in the balance
/// law, or the `min(1.0)` dropped, changes the digest and fails.
#[test]
fn the_strip_renders_what_it_always_did() {
    let mut node: BusStripNode = prepared(BusStripNode::new(), SR, BLOCK);
    let got = digest(&mut node, |n, k| match k {
        40 => n.set_pan(Pan(-0.6)),
        80 => n.set_volume(Amplitude::new(0.5)),
        120 => n.set_pan(Pan(0.35)),
        160 => n.set_pan(Pan(2.0)),
        _ => {}
    });
    println!("STRIP {got:#018x}");
    assert_eq!(got, STRIP_DIGEST);
}

const PHASER_DIGEST: u64 = 0x7630_ea29_6734_f7c6;
const STRIP_DIGEST: u64 = 0x634e_f470_49ef_23e3;
