//! Compressor and gate: pinned renders, and the per-block param read.
//!
//! The goldens were captured from the implementation that loaded threshold,
//! knee, ratio and makeup (compressor) / threshold and range (gate) **per
//! sample**, before those reads moved to once per block. With every param held
//! constant the two must agree, so the pinned values are what proves the move
//! changed cost and not sound. Captured with `cargo nextest run -p tutti-nodes
//! -E 'test(print_goldens)' --run-ignored only --no-capture`.
//!
//! The tolerance is 1e-6 absolute rather than bit equality only so a libm
//! `exp`/`log10`/`powf` differing in the last ulp on another C runtime does not
//! fail a render the refactor did not touch (see CLAUDE.md, "One live platform
//! difference remains").

use tutti_core::{AudioUnit, BufferVec, SampleRate};
use tutti_nodes::{CompressorNode, GateNode};

const BLOCK: usize = 64;
const BLOCKS: usize = 8;
const FRAMES: usize = BLOCK * BLOCKS;
/// Pin every `STRIDE`th frame: dense enough that a changed envelope anywhere in
/// the render lands on a pinned frame, sparse enough to paste.
const STRIDE: usize = 23;
const TOL: f32 = 1e-6;

/// Audio on every channel: a slightly different tone per channel so a
/// channel-mixup in the planar gain loop is visible.
fn audio(c: usize, n: usize) -> f32 {
    0.5 * ((n as f32) * (0.031 + 0.007 * c as f32)).sin()
}

/// Sidechain: a slow swell that crosses both thresholds used below, so the
/// detector attacks, holds and releases inside the render.
fn sidechain(c: usize, n: usize) -> f32 {
    let env = 0.5 + 0.5 * ((n as f32) * 0.02).sin();
    env * (0.9 - 0.1 * c as f32) * ((n as f32) * 0.37).cos()
}

/// A threshold port signal that moves every sample, in dB.
fn threshold_signal(n: usize) -> f32 {
    -30.0 + 12.0 * ((n as f32) * 0.05).sin()
}

struct Case {
    ch: usize,
    port: bool,
}

/// Render through `process` in 64-frame blocks. Returns interleaved-by-channel
/// output `[c][n]`.
fn render_process(node: &mut dyn AudioUnit, case: &Case) -> Vec<Vec<f32>> {
    let ch = case.ch;
    let mut out = vec![vec![0.0f32; FRAMES]; ch];
    let mut inb = BufferVec::new(node.inputs());
    let mut outb = BufferVec::new(ch);
    let mut threshold = [0.0f32; BLOCK];
    for b in 0..BLOCKS {
        for (i, t) in threshold.iter_mut().enumerate() {
            let n = b * BLOCK + i;
            for c in 0..ch {
                inb.set_f32(c, i, audio(c, n));
                inb.set_f32(ch + c, i, sidechain(c, n));
            }
            *t = threshold_signal(n);
        }
        if case.port {
            node.param_feed().expect("fed").feed(0, &threshold);
        }
        node.process(BLOCK, &inb.buffer_ref(), &mut outb.buffer_mut());
        for (c, o) in out.iter_mut().enumerate() {
            for i in 0..BLOCK {
                o[b * BLOCK + i] = outb.at_f32(c, i);
            }
        }
    }
    out
}

/// Render through `tick`, one frame at a time.
fn render_tick(node: &mut dyn AudioUnit, case: &Case) -> Vec<Vec<f32>> {
    let ch = case.ch;
    let mut out = vec![vec![0.0f32; FRAMES]; ch];
    let mut frame = vec![0.0f32; node.inputs()];
    let mut o = vec![0.0f32; ch];
    for n in 0..FRAMES {
        for c in 0..ch {
            frame[c] = audio(c, n);
            frame[ch + c] = sidechain(c, n);
        }
        if case.port {
            node.param_feed()
                .expect("fed")
                .feed(0, &[threshold_signal(n)]);
        }
        node.tick(&frame, &mut o);
        for (lane, &s) in out.iter_mut().zip(&o) {
            lane[n] = s;
        }
    }
    out
}

/// First and last channel, every `STRIDE`th frame.
fn pins(out: &[Vec<f32>]) -> Vec<f32> {
    let last = out.len() - 1;
    let mut v: Vec<f32> = out[0].iter().step_by(STRIDE).copied().collect();
    if last > 0 {
        v.extend(out[last].iter().step_by(STRIDE));
    }
    v
}

fn assert_pinned(name: &str, got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "{name}: pin count");
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            (g - w).abs() <= TOL,
            "{name}: pin {k} is {g}, pinned {w} (diff {})",
            (g - w).abs()
        );
    }
}

// `_port`: whether the case feeds the threshold — a node's feed is always
// there, so building it no longer depends on it (it was `with_param_inputs`'
// port flag when the pins were captured; the fed values are unchanged).
fn compressor(ch: usize, _port: bool) -> CompressorNode {
    let mut n = CompressorNode::with_channels(-24.0, 4.0, 0.002, 0.05, ch as u8)
        .with_soft_knee(6.0)
        .with_makeup(3.0);
    n.set_sample_rate(SampleRate(48_000.0));
    n
}

fn gate(ch: usize, _port: bool) -> GateNode {
    let mut n = GateNode::with_channels(-22.0, 0.001, 0.004, 0.02, ch as u8).with_range(-18.0);
    n.set_sample_rate(SampleRate(48_000.0));
    n
}

type Build = fn(usize, bool) -> Box<dyn AudioUnit>;

fn cases() -> Vec<(&'static str, Build, Case)> {
    let c: Build = |ch, p| Box::new(compressor(ch, p));
    let g: Build = |ch, p| Box::new(gate(ch, p));
    vec![
        ("comp_mono", c, Case { ch: 1, port: false }),
        ("comp_stereo", c, Case { ch: 2, port: false }),
        ("comp_6ch", c, Case { ch: 6, port: false }),
        ("comp_stereo_port", c, Case { ch: 2, port: true }),
        ("gate_mono", g, Case { ch: 1, port: false }),
        ("gate_stereo", g, Case { ch: 2, port: false }),
        ("gate_6ch", g, Case { ch: 6, port: false }),
        ("gate_stereo_port", g, Case { ch: 2, port: true }),
    ]
}

/// Prints the goldens. Ignored: it asserts nothing and exists only to
/// regenerate the table below.
#[test]
#[ignore]
fn print_goldens() {
    for (name, build, case) in cases() {
        let p = pins(&render_process(build(case.ch, case.port).as_mut(), &case));
        println!("(\"{name}\", &{p:?}),");
    }
}

/// Every pinned render, through both `process` and `tick`.
///
/// `tick` is held to the same table: a tick is a block of one, so the per-block
/// read and the per-sample read coincide there, and the table was captured
/// from `process` — agreement is also the old `process_matches_tick` property.
///
/// Mutation (each tried): dropping the `* gain` for every channel but 0 in the
/// planar apply loop fails `comp_stereo`; ignoring the threshold port in favour
/// of the block's atomic fails `comp_stereo_port`, and the same in the gate
/// fails `gate_stereo_port`.
#[test]
fn renders_match_the_per_sample_goldens() {
    let goldens: &[(&str, &[f32])] = GOLDENS;
    for ((name, build, case), (gname, want)) in cases().into_iter().zip(goldens) {
        assert_eq!(name, *gname);
        let p = pins(&render_process(build(case.ch, case.port).as_mut(), &case));
        assert_pinned(&format!("{name} (process)"), &p, want);
        let t = pins(&render_tick(build(case.ch, case.port).as_mut(), &case));
        assert_pinned(&format!("{name} (tick)"), &t, want);
    }
}

/// Steady input for the per-block tests: constant audio and a sidechain loud
/// enough to hold the compressor in steady reduction.
fn steady_block(ch: usize) -> BufferVec {
    let mut b = BufferVec::new(2 * ch);
    for i in 0..BLOCK {
        for c in 0..ch {
            b.set_f32(c, i, 0.5);
            b.set_f32(ch + c, i, 0.5);
        }
    }
    b
}

fn run_block(node: &mut dyn AudioUnit, input: &BufferVec) -> Vec<f32> {
    let mut out = BufferVec::new(node.outputs());
    node.process(BLOCK, &input.buffer_ref(), &mut out.buffer_mut());
    (0..BLOCK).map(|i| out.at_f32(0, i)).collect()
}

/// A makeup change between blocks is picked up by the next block and ramps
/// across it: the first frame is still near the old gain, the last frame is
/// exactly where a node that always had the new makeup sits.
///
/// Mutation: making `ramp_db` return the end value at every frame (no ramp)
/// fails the "first frame near the old gain" assertion.
#[test]
fn a_makeup_change_ramps_across_the_next_block() {
    let input = steady_block(1);
    let mut node = compressor(1, false);
    let mut reference = compressor(1, false);
    reference.set_makeup(9.0);
    // Settle both envelopes.
    for _ in 0..200 {
        run_block(&mut node, &input);
        run_block(&mut reference, &input);
    }
    let before = run_block(&mut node, &input);
    let settled_ref = run_block(&mut reference, &input);

    node.set_makeup(9.0);
    let after = run_block(&mut node, &input);

    let old_level = before[BLOCK - 1];
    let new_level = settled_ref[BLOCK - 1];
    assert!(
        new_level > old_level * 1.5,
        "6 dB more makeup must be audible"
    );
    // Frame 0 moved only 1/64 of the way.
    assert!(
        (after[0] - old_level).abs() < (new_level - old_level) * 0.05,
        "the first frame should start near the old gain: {} vs old {old_level}",
        after[0]
    );
    // Monotone ramp, landing on the new gain.
    for w in after.windows(2) {
        assert!(w[1] >= w[0] - 1e-7, "the ramp must be monotone: {w:?}");
    }
    assert!(
        (after[BLOCK - 1] - new_level).abs() < 1e-6,
        "the block must end on the new makeup: {} vs {new_level}",
        after[BLOCK - 1]
    );
}

/// A threshold written between two blocks is picked up by the next block.
///
/// What a single-threaded test cannot do is land a write *inside* `process`;
/// the per-block contract (a write mid-block is seen from the next block) is
/// the same statement seen from the writer, and its observable half is here
/// and in the ramp tests — identical output up to the write, a changed next
/// block, and (for makeup/range) a first frame still at the old value.
///
/// Mutation: pinning the block's threshold to the constructed -24 dB (a value
/// cached rather than re-read) fails the "next block must see it" assertion.
#[test]
fn a_threshold_change_takes_effect_on_the_next_block() {
    let input = steady_block(2);
    let mut changed = compressor(2, false);
    let mut unchanged = compressor(2, false);
    for _ in 0..50 {
        run_block(&mut changed, &input);
        run_block(&mut unchanged, &input);
    }
    let a = run_block(&mut changed, &input);
    let b = run_block(&mut unchanged, &input);
    assert_eq!(a, b, "identical up to the write");

    // The write lands "during" the block just rendered — i.e. after its read.
    changed.set_threshold(-6.0);
    let a2 = run_block(&mut changed, &input);
    let b2 = run_block(&mut unchanged, &input);
    assert!(
        a2.iter().zip(&b2).any(|(x, y)| (x - y).abs() > 1e-4),
        "the next block must see the new threshold"
    );
}

/// The gate's range is ramped like the compressor's makeup: it multiplies the
/// output directly, so a per-block step would click.
///
/// Mutation: making `ramp_db` return the end value at every frame fails the
/// first-frame assertion.
#[test]
fn a_gate_range_change_ramps_across_the_next_block() {
    // A closed gate: a silent sidechain, so the output sits at the range floor.
    let mut input = BufferVec::new(2);
    for i in 0..BLOCK {
        input.set_f32(0, i, 0.5);
        input.set_f32(1, i, 0.0);
    }
    let mut node = gate(1, false);
    for _ in 0..200 {
        run_block(&mut node, &input);
    }
    let before = run_block(&mut node, &input);
    node.range()
        .store(-6.0, core::sync::atomic::Ordering::Release);
    let after = run_block(&mut node, &input);

    let old_level = before[BLOCK - 1];
    let new_level = 0.5 * tutti_core::Db(-6.0).to_amplitude().get();
    assert!(
        (after[0] - old_level).abs() < (new_level - old_level) * 0.05,
        "first frame near the old floor: {} vs {old_level}",
        after[0]
    );
    assert!(
        (after[BLOCK - 1] - new_level).abs() < 1e-5,
        "last frame at the new floor: {} vs {new_level}",
        after[BLOCK - 1]
    );
}

#[rustfmt::skip]
const GOLDENS: &[(&str, &[f32])] = &[
    ("comp_mono", &[0.0, 0.35863826, 0.42181125, 0.28820556, 0.08227583, -0.10323944, -0.20616908, -0.2108936, -0.12008735, 0.029512826, 0.16756937, 0.22612481, 0.17441511, 0.03559251, -0.118713245, -0.20301493, -0.17867184, -0.0776532, 0.044910748, 0.1343377, 0.15749523, 0.106903374, 0.0035308856]),
    ("comp_stereo", &[0.0, 0.35863826, 0.42181125, 0.28820556, 0.08227583, -0.10323944, -0.20616908, -0.2108936, -0.12008735, 0.029512826, 0.16756937, 0.22612481, 0.17441511, 0.03559251, -0.118713245, -0.20301493, -0.17867184, -0.0776532, 0.044910748, 0.1343377, 0.15749523, 0.106903374, 0.0035308856, 0.0, 0.42048302, 0.41959682, 0.16977933, -0.099987246, -0.2366858, -0.19591077, -0.036069192, 0.14281549, 0.22119386, 0.14147285, -0.042535342, -0.19963044, -0.21417956, -0.07282078, 0.10993738, 0.1927412, 0.13541877, -0.004081658, -0.1264565, -0.15548842, -0.0758094, 0.059402242]),
    ("comp_6ch", &[0.0, 0.35863826, 0.42181125, 0.28820556, 0.08227583, -0.10323944, -0.20616908, -0.2108936, -0.12008735, 0.029512826, 0.16756937, 0.22612481, 0.17441511, 0.03559251, -0.118713245, -0.20301493, -0.17867184, -0.0776532, 0.044910748, 0.1343377, 0.15749523, 0.106903374, 0.0035308856, 0.0, 0.5475238, 0.04492759, -0.3376539, -0.06039524, 0.2425664, 0.07079827, -0.2045418, -0.08993763, 0.19670339, 0.11268031, -0.18906216, -0.13523355, 0.17746224, 0.15122472, -0.14930424, -0.14583911, 0.11239651, 0.13817523, -0.08693369, -0.13811766, 0.07107205, 0.14749658]),
    ("comp_stereo_port", &[0.0, 0.36026707, 0.46140546, 0.2908114, 0.064028054, -0.06463509, -0.12921055, -0.13810794, -0.079384714, 0.01945189, 0.11041676, 0.14959472, 0.11591211, 0.023833973, -0.07835314, -0.113908045, -0.08700306, -0.03962002, 0.024551805, 0.077633716, 0.08515306, 0.05225004, 0.0017118498, 0.0, 0.42239273, 0.45898315, 0.1713144, -0.07781129, -0.14818181, -0.12278143, -0.02362064, 0.09440933, 0.14578877, 0.093220934, -0.028139608, -0.13266961, -0.14342204, -0.048063185, 0.061683897, 0.09385404, 0.069093026, -0.0022313609, -0.073079176, -0.08406804, -0.03705256, 0.028799491]),
    ("gate_mono", &[0.0, 0.14247543, 0.31448328, 0.32433093, 0.12154013, -0.18610953, -0.42658415, -0.46186835, -0.26629534, 0.06530758, 0.3667247, 0.48947558, 0.3745497, 0.07658672, -0.26214913, -0.474941, -0.45647302, -0.21491998, 0.13203396, 0.41504076, 0.49593732, 0.3351485, 0.010977792]),
    ("gate_stereo", &[0.0, 0.14247543, 0.31448328, 0.32433093, 0.12154013, -0.18610953, -0.42658415, -0.46186835, -0.26629534, 0.06530758, 0.3667247, 0.48947558, 0.3745497, 0.07658672, -0.26214913, -0.474941, -0.45647302, -0.21491998, 0.13203396, 0.41504076, 0.49593732, 0.3351485, 0.010977792, 0.0, 0.16704437, 0.31283233, 0.19106045, -0.14770393, -0.42667302, -0.40535867, -0.07899348, 0.3166953, 0.48946974, 0.3096126, -0.0920731, -0.4286986, -0.46086413, -0.16080686, 0.25719175, 0.49241757, 0.3747972, -0.011999744, -0.39069155, -0.4896181, -0.23766701, 0.18468608]),
    ("gate_6ch", &[0.0, 0.14247543, 0.31448328, 0.32433093, 0.12154013, -0.18610953, -0.42658415, -0.46186835, -0.26629534, 0.06530758, 0.3667247, 0.48947558, 0.3745497, 0.07658672, -0.26214913, -0.474941, -0.45647302, -0.21491998, 0.13203396, 0.41504076, 0.49593732, 0.3351485, 0.010977792, 0.0, 0.21751356, 0.03349597, -0.3799774, -0.08921752, 0.43727398, 0.14648859, -0.44795758, -0.19943793, 0.43527588, 0.24660029, -0.40924883, -0.2904088, 0.38185707, 0.33394274, -0.34928814, -0.37259153, 0.3110787, 0.40622398, -0.26858452, -0.4349192, 0.22281516, 0.45857808]),
    ("gate_stereo_port", &[0.0, 0.14628102, 0.31820038, 0.32829383, 0.12323845, -0.1880213, -0.42975432, -0.4640834, -0.26716202, 0.06570449, 0.37029013, 0.49423444, 0.3775695, 0.07694983, -0.2630407, -0.47604603, -0.45716253, -0.2151306, 0.1321152, 0.41520563, 0.49607125, 0.33521253, 0.010979416, 0.0, 0.1715062, 0.3165299, 0.19339496, -0.14976785, -0.4310559, -0.4083711, -0.07937232, 0.31772602, 0.49244452, 0.31262276, -0.09296827, -0.43215498, -0.46304914, -0.16135377, 0.25779018, 0.4931614, 0.37516445, -0.0120071275, -0.39084676, -0.4897503, -0.23771243, 0.18471341]),
];
