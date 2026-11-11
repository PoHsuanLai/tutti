//! The one-pole recurrence under the limiter, compressor and gate renders
//! what it always did, bit for bit (doc 024 step 0b, consolidation 2): a
//! digest (FNV-1a over the bits of every output sample) of each node's answer
//! to seeded noise with a release/attack change in the middle of the render,
//! recorded before `smooth_envelope` became `Smoother`'s step.
//!
//! The render reaches libm (`powf`, `log10` in the dB conversions), so the
//! digests go through `assert_golden`: asserted on the recorded target only.
//!
//! Mutation (run): `Smoother`'s step as `current * (1 - coeff) + target * coeff`
//! changes the envelope's last bits and fails every case.

use tutti_fixtures::{Fnv, Noise};
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::testing::assert_golden;
use tutti_nodes::{CompressorNode, GateNode, LimiterNode};
use tutti_types::{CompressionRatio, Db, SampleRate, Seconds};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const BLOCKS: usize = 40;

/// Seeded noise under a slow swell.
fn noise(seed: u32, frames: usize) -> Vec<f32> {
    Noise::seeded(seed)
        .take(frames)
        .enumerate()
        .map(|(i, x)| (0.55 + 0.45 * (i as f32 * 0.0007).sin()) * x)
        .collect()
}

/// Renders `BLOCKS` blocks of `inputs` seeded noise channels through `node`,
/// calling `change` once, halfway.
fn render(node: &mut dyn tutti_graph::Node, inputs: usize, change: impl Fn()) -> u64 {
    let planes: Vec<Vec<f32>> = (0..inputs)
        .map(|c| noise(7 + c as u32, BLOCK * BLOCKS))
        .collect();
    let mut digest = Fnv::new();
    for b in 0..BLOCKS {
        if b == BLOCKS / 2 {
            change();
        }
        let block: Vec<&[f32]> = planes
            .iter()
            .map(|p| &p[b * BLOCK..(b + 1) * BLOCK])
            .collect();
        for out in drive(node, SR, &block, &[]) {
            digest.f32s(&out);
        }
    }
    digest.finish()
}

#[test]
fn the_limiter_release_renders_what_it_always_did() {
    let node = LimiterNode::new(Db(-12.0), Db(-1.0));
    let set = node.clone();
    let mut node = prepared(node, SR, BLOCK);
    let digest = render(&mut node, 2, || set.set_release(Seconds(0.02)));
    eprintln!("DIGEST limiter {digest:#018x}");
    assert_golden("limiter", digest, 0xdfc4_0e16_5103_58ea);
}

#[test]
fn the_compressor_follower_renders_what_it_always_did() {
    let node = CompressorNode::stereo(
        Db(-20.0),
        CompressionRatio::new(4.0),
        Seconds(0.005),
        Seconds(0.15),
    );
    let set = node.clone();
    let mut node = prepared(node, SR, BLOCK);
    let digest = render(&mut node, 4, || {
        set.set_attack(Seconds(0.001));
        set.set_release(Seconds(0.05));
    });
    eprintln!("DIGEST compressor {digest:#018x}");
    assert_golden("compressor", digest, 0x76f1_fc00_4f2b_d92b);
}

#[test]
fn the_gate_follower_renders_what_it_always_did() {
    let node = GateNode::stereo(Db(-18.0), Seconds(0.002), Seconds(0.01), Seconds(0.08));
    let set = node.clone();
    let mut node = prepared(node, SR, BLOCK);
    let digest = render(&mut node, 4, || set.set_release(Seconds(0.02)));
    eprintln!("DIGEST gate {digest:#018x}");
    assert_golden("gate", digest, 0x08fe_1784_207b_c738);
}
