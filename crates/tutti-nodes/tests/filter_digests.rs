//! The filter nodes' renders, pinned by digests (doc 024 step 2): what the SVF,
//! the ladder, the crossover and the EQ band do with seeded noise, a cell moved
//! between blocks (the block-rate ramp) and lanes driving a cutoff and a Q inside
//! a block (the 16-frame solve schedule).
//!
//! Each digest is FNV-1a over the bits of every output sample, channel by channel
//! of each 64-frame block. The SVF, the bell and the EQ band **moved
//! once**, in step 2b's PR3, the ladder once, in PR4, and the crossover once, in PR5,
//! when they went to the stream grid (Q27: a solve at the first frame of each
//! 16-frame segment counted from the last reset, the coefficients ramping to it
//! over the segment, or for the crossover held over it; the ladder's drive joins
//! the grid, a lane sampled per segment and a cell glided like the others);
//! The coefficient
//! solves reach `libm` (`tan`, `powf`), so every assert goes through
//! `assert_golden`: asserted on the recorded target only.
//!
//! The first render of a lane block is the one that pins the schedule: a lane
//! sampled at each segment's first frame with the coefficients interpolated over
//! the segment, so a node that solved at another interval, or interpolated another
//! way, changes the digest.
//!
//! Mutations (run): `COEFF_INTERVAL` 16 -> 8 in `tutti-dsp`'s `control.rs` changes
//! the lane digests; `Ramp::at` taking `step * i` changes the cell digests; the
//! crossover's `end as f64 / n as f64` -> `(end - 1) as f64 / n as f64` changes
//! the crossover digest. `Cadence::steps` solving a stream step at its end instead of
//! `frames.start` changes the lane digests and the cell ones;
//! `DesignNode::render_steps` reading with the controls of the step before
//! (`applied: self.applied` in its `At`) instead of the block's start restarts the
//! cell glide at each step and changes the cell digests. (Locally `assert_golden`
//! only prints on another libm: compare the `DIGEST` lines.)

use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Transport, TransportChanges};
use tutti_nodes::testing::assert_golden;
use tutti_nodes::{
    CrossoverNode, EqBandNode, LadderFilterNode, LadderType, SvfControls, SvfFilterNode, SvfType,
};
use tutti_types::{
    ChannelLayout, Db, Frame, Hz, Q, Resonance, SampleRate, Samples, UnitAddr, UnitParam,
};

const BLOCK: usize = 64;
const BLOCKS: usize = 300;

fn noise(seed: u32, frames: usize, gain: f32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..frames)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            gain * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
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

/// A lane that rises from `lo` to `hi` across the block (a sweep inside it).
fn sweep(lo: f32, hi: f32) -> Vec<f32> {
    (0..BLOCK)
        .map(|i| lo + (hi - lo) * i as f32 / (BLOCK - 1) as f32)
        .collect()
}

/// Renders `BLOCKS` blocks of seeded noise (one signal per input channel) through
/// `node` at `rate` and digests the output. `act(b, set)` runs before block `b`
/// (to move a cell) and `lanes(b)` gives the lanes of block `b`, in port order.
fn run<N: Node + ParamNode + 'static>(
    node: N,
    rate: SampleRate,
    act: impl Fn(usize, &tutti_graph::ParamSet),
    lanes: impl Fn(usize) -> Vec<Option<Vec<f32>>>,
) -> u64 {
    let mut node = prepared(node, rate, BLOCK);
    let set = node.param_set();
    let width = usize::from(node.shape().audio_in.count());
    let inputs: Vec<Vec<f32>> = (0..width)
        .map(|c| noise(3 + c as u32, BLOCK * BLOCKS, 0.8))
        .collect();
    let mut digest = Fnv::new();
    for b in 0..BLOCKS {
        act(b, &set);
        let ins: Vec<&[f32]> = inputs
            .iter()
            .map(|c| &c[b * BLOCK..(b + 1) * BLOCK])
            .collect();
        let owned = lanes(b);
        let params: Vec<Option<&[f32]>> = owned.iter().map(|p| p.as_deref()).collect();
        let env = Env {
            frame: Frame((b * BLOCK) as u64),
            sample_rate: rate,
            block_len: Samples(BLOCK),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let out = drive_input(&mut node as &mut dyn Node, &env, &ins, &params, &[]).audio;
        for ch in &out {
            digest.eat(ch);
        }
    }
    digest.0
}

/// A mono f64 low-pass: the cutoff and Q cells move at block 40 (the block-rate
/// ramp), a cutoff lane sweeps blocks 80 to 119, a Q lane blocks 120 to 139, and
/// both together 140 to 159.
#[test]
fn the_svf_renders_what_it_always_did() {
    let digest = run(
        SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(800.0), Q(1.2)),
        SampleRate(48_000.0),
        |b, set| {
            if b == 40 {
                assert!(set.set(UnitParam::Cutoff, 3_000.0));
                assert!(set.set(UnitParam::Q, 3.0));
            }
        },
        |b| match b {
            80..120 => vec![Some(sweep(300.0, 9_000.0)), None, None],
            120..140 => vec![None, Some(sweep(0.5, 6.0)), None],
            140..160 => vec![Some(sweep(9_000.0, 200.0)), Some(sweep(4.0, 0.7)), None],
            _ => vec![None, None, None],
        },
    );
    println!("DIGEST svf {digest:#018x}");
    assert_golden("svf_render", digest, 0xb59c_d87c_585f_e3d9);
}

/// A stereo f32 bell at 44.1 kHz: the gain moves at block 30 and the cutoff by
/// a cell at block 60; a cutoff lane sweeps blocks 100 to 139.
#[test]
fn the_svf_bell_renders_what_it_always_did() {
    let digest = run(
        SvfFilterNode::<f32>::with_controls(
            ChannelLayout::STEREO,
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz(1_500.0),
                q: Q(2.0),
                gain: Db(6.0),
            },
        ),
        SampleRate(44_100.0),
        |b, set| {
            if b == 30 {
                assert!(set.set(UnitParam::GainDb, -9.0));
            }
            if b == 60 {
                assert!(set.set(UnitParam::Cutoff, 500.0));
            }
        },
        |b| match b {
            100..140 => vec![Some(sweep(400.0, 6_000.0)), None, None],
            _ => vec![None, None, None],
        },
    );
    println!("DIGEST svf_bell {digest:#018x}");
    assert_golden("svf_bell_render", digest, 0xb197_e19b_d353_0dfe);
}

/// The EQ band: a mono high shelf, its gain and cutoff cells moved at block 50, a
/// cutoff lane at blocks 90 to 109.
#[test]
fn the_eq_band_renders_what_it_always_did() {
    let digest = run(
        EqBandNode::<f64>::new(SvfType::HighShelf, Hz(4_000.0), Q(0.8), Db(8.0)),
        SampleRate(48_000.0),
        |b, set| {
            if b == 50 {
                assert!(set.set(UnitParam::GainDb, -5.0));
                assert!(set.set(UnitParam::Cutoff, 2_000.0));
            }
        },
        |b| match b {
            90..110 => vec![Some(sweep(1_000.0, 8_000.0)), None, None],
            _ => vec![None, None, None],
        },
    );
    println!("DIGEST eq_band {digest:#018x}");
    assert_golden("eq_band_render", digest, 0xa4d0_46fb_d4ba_0ef2);
}

/// A stereo 24 dB ladder: the resonance and cutoff cells move at block 40, the
/// drive at block 70, a cutoff lane sweeps blocks 100 to 139.
#[test]
fn the_ladder_renders_what_it_always_did() {
    let digest = run(
        LadderFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            LadderType::LP24,
            Hz(1_200.0),
            Resonance::new_clamped(0.6),
        ),
        SampleRate(48_000.0),
        |b, set| {
            if b == 40 {
                assert!(set.set(UnitParam::Cutoff, 2_500.0));
                assert!(set.set(UnitParam::Q, 0.8));
            }
            if b == 70 {
                assert!(set.set(UnitParam::Drive, 2.0));
            }
        },
        |b| match b {
            100..140 => vec![Some(sweep(300.0, 7_000.0)), None, None],
            _ => vec![None, None, None],
        },
    );
    println!("DIGEST ladder {digest:#018x}");
    assert_golden("ladder_render", digest, 0xc146_444c_b50a_5bee);
}

/// A stereo three-band crossover: the first split moves by its cell at block 40
/// (glided across the block), the second is driven by a lane at blocks 100 to
/// 139.
#[test]
fn the_crossover_renders_what_it_always_did() {
    let digest = run(
        CrossoverNode::new(ChannelLayout::STEREO, &[Hz(300.0), Hz(3_000.0)]).unwrap(),
        SampleRate(48_000.0),
        |b, set| {
            if b == 40 {
                assert!(set.set(UnitAddr::new(UnitParam::Cutoff, 0), 450.0));
            }
        },
        |b| match b {
            100..140 => vec![None, Some(sweep(2_000.0, 5_000.0))],
            _ => vec![None, None],
        },
    );
    println!("DIGEST crossover {digest:#018x}");
    // Moved in step 2b PR5 (the stream grid); was 0xe487_a6a1_7b13_6f8d. Recorded on glibc 2.39.
    assert_golden("crossover_render", digest, 0x333a_7595_b2e8_7939);
}
