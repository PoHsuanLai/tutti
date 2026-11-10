//! The lookahead limiter's render, pinned by a digest across the move of its
//! recursion into `tutti_dsp::LimiterKernel` (doc 024 step 0b, consolidation 9).

use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Transport, TransportChanges};
use tutti_nodes::LimiterNode;
use tutti_nodes::testing::assert_golden;
use tutti_types::{Frame, SampleRate, Samples, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

fn noise(seed: u32, frames: usize, gain: f32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..frames)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            gain * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect()
}

/// A stereo lookahead limiter renders what it always did, bit for bit: a
/// digest (FNV-1a over the bits of every sample, left then right of each
/// 64-frame block) of 24 000 frames of seeded noise at 48 kHz, with the
/// threshold and release changed through the `ParamSet` at block 100, the
/// threshold and ceiling driven by lanes for blocks 200 to 249, and the
/// release driven by a lane (the third param) for blocks 250 to 299. Recorded
/// before the recursion moved to `LimiterKernel`.
///
/// The coefficients reach libm (`exp` in the release's one-pole coefficient,
/// `powf` in the curve's dB conversions), so the committed assert is a golden
/// one, recorded on glibc 2.39.
///
/// Mutation (run): the kernel's release coefficient taken from the attack
/// path (instant release), or `ceiling` and `threshold` swapped in the curve,
/// changes the digest and fails.
#[test]
fn the_limiter_renders_what_it_always_did() {
    let mut node = prepared(LimiterNode::new(-6.0, -0.3), SR, BLOCK);
    let set = node.param_set();
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |samples: &[f32]| {
        for s in samples {
            for byte in s.to_bits().to_le_bytes() {
                digest ^= u64::from(byte);
                digest = digest.wrapping_mul(0x0100_0000_01b3);
            }
        }
    };
    let left = noise(5, 64 * 375, 1.5);
    let right = noise(9, 64 * 375, 1.5);
    let ceiling_lane: Vec<f32> = (0..BLOCK).map(|i| -3.0 - i as f32 * 0.05).collect();
    let threshold_lane: Vec<f32> = (0..BLOCK).map(|i| -12.0 + i as f32 * 0.1).collect();
    let release_lane: Vec<f32> = (0..BLOCK).map(|i| 0.01 + i as f32 * 0.002).collect();
    for (b, (l, r)) in left.chunks(BLOCK).zip(right.chunks(BLOCK)).enumerate() {
        if b == 100 {
            assert!(set.set(UnitParam::Threshold, -10.0));
            assert!(set.set(UnitParam::Release, 0.02));
        }
        let params: Vec<Option<&[f32]>> = match b {
            200..250 => vec![Some(&ceiling_lane), Some(&threshold_lane), None],
            250..300 => vec![None, None, Some(&release_lane)],
            _ => vec![None, None, None],
        };
        let env = Env {
            frame: Frame((b * BLOCK) as u64),
            sample_rate: SR,
            block_len: Samples(BLOCK),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let out = drive_input(&mut node as &mut dyn Node, &env, &[l, r], &params, &[]).audio;
        eat(&out[0]);
        eat(&out[1]);
    }
    println!("DIGEST {digest:#018x}");
    assert_golden("limiter_render", digest, 0x193c_3ddd_3ece_3335);
}
