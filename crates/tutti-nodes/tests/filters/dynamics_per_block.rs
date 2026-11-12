//! Gate: the per-block param read. (The compressor is a `FamilyNode`: its controls are on
//! the stream grid and its contract is `tutti-dsp`'s.)
//!
//! Threshold and range (gate) are read once per block. A block of one frame is where a per-block
//! read and a per-sample read coincide, so with the params held the two block
//! lengths must render the same, and a param changed between blocks must land
//! on the next block. The static curve itself is `tutti-dsp`'s (its
//! compressor and gate kernel tests).
//!
//! The tolerance is 1e-6 absolute rather than bit equality: the 64-frame and
//! one-frame renders take different paths through the envelope's `exp`.

use tutti_graph::Node;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::GateNode;
use tutti_types::Db;
use tutti_types::SampleRate;
use tutti_types::Seconds;

const RATE: SampleRate = SampleRate(48_000.0);

const BLOCK: usize = 64;
const BLOCKS: usize = 8;
const FRAMES: usize = BLOCK * BLOCKS;
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

/// Render frames `[start, start + len)` as one block, the threshold on its
/// param port when the case modulates it. Returns one `Vec` per output.
fn render_block(node: &mut dyn Node, case: &Case, start: usize, len: usize) -> Vec<Vec<f32>> {
    let ch = case.ch;
    let mut ins: Vec<Vec<f32>> = Vec::with_capacity(2 * ch);
    for c in 0..ch {
        ins.push((start..start + len).map(|n| audio(c, n)).collect());
    }
    for c in 0..ch {
        ins.push((start..start + len).map(|n| sidechain(c, n)).collect());
    }
    let refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
    let threshold: Vec<f32> = (start..start + len).map(threshold_signal).collect();
    let params: Vec<Option<&[f32]>> = if case.port {
        vec![Some(&threshold[..])]
    } else {
        Vec::new()
    };
    drive(node, RATE, &refs, &params)
}

/// Render in `block`-frame blocks. Returns one `Vec` per output, `[c][n]`.
fn render_blocks(node: &mut dyn Node, case: &Case, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::with_capacity(FRAMES); case.ch];
    for start in (0..FRAMES).step_by(block) {
        for (lane, o) in out.iter_mut().zip(render_block(node, case, start, block)) {
            lane.extend(o);
        }
    }
    out
}

/// Render in 64-frame blocks.
fn render_process(node: &mut dyn Node, case: &Case) -> Vec<Vec<f32>> {
    render_blocks(node, case, BLOCK)
}

/// Render one frame at a time — a block of one.
fn render_tick(node: &mut dyn Node, case: &Case) -> Vec<Vec<f32>> {
    render_blocks(node, case, 1)
}

// `_port`: whether the case feeds the threshold — a node's feed is always
// there, so building it does not depend on it.
fn gate(ch: usize, _port: bool) -> GateNode {
    let n = GateNode::with_channels(
        Db::new(-22.0),
        Seconds::new(0.001),
        Seconds::new(0.004),
        Seconds::new(0.02),
        ch as u8,
    )
    .with_range(Db::new(-18.0));
    prepared(n, RATE, BLOCK)
}

type Build = fn(usize, bool) -> Box<dyn Node>;

fn cases() -> Vec<(&'static str, Build, Case)> {
    let g: Build = |ch, p| Box::new(gate(ch, p));
    vec![
        ("gate_mono", g, Case { ch: 1, port: false }),
        ("gate_stereo", g, Case { ch: 2, port: false }),
        ("gate_6ch", g, Case { ch: 6, port: false }),
        ("gate_stereo_port", g, Case { ch: 2, port: true }),
    ]
}

/// **With its params held, a node renders the same in 64-frame blocks as one
/// frame at a time**, on every channel and every frame: the per-block read
/// changes cost, not sound.
#[test]
fn held_params_render_the_same_in_blocks_and_one_frame_at_a_time() {
    for (name, build, case) in cases() {
        let blocks = render_process(build(case.ch, case.port).as_mut(), &case);
        let frames = render_tick(build(case.ch, case.port).as_mut(), &case);
        assert!(
            blocks.iter().flatten().any(|x| x.abs() > 0.05),
            "{name}: silent"
        );
        for (c, (b, f)) in blocks.iter().zip(&frames).enumerate() {
            for (n, (x, y)) in b.iter().zip(f).enumerate() {
                assert!(
                    (x - y).abs() <= TOL,
                    "{name}: channel {c} frame {n}: {x} in blocks, {y} a frame at a time"
                );
            }
        }
    }
}

/// One block over `input`; output channel 0.
fn run_block(node: &mut dyn Node, input: &[Vec<f32>]) -> Vec<f32> {
    let refs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
    drive(node, RATE, &refs, &[]).remove(0)
}

/// The gate's range is ramped across the block: it multiplies the output directly, so a
/// per-block step would click.
#[test]
fn a_gate_range_change_ramps_across_the_next_block() {
    // A closed gate: a silent sidechain, so the output sits at the range floor.
    let input = vec![vec![0.5f32; BLOCK], vec![0.0f32; BLOCK]];
    let mut node = gate(1, false);
    for _ in 0..200 {
        run_block(&mut node, &input);
    }
    let before = run_block(&mut node, &input);
    node.range()
        .store(-6.0, core::sync::atomic::Ordering::Release);
    let after = run_block(&mut node, &input);

    let old_level = before[BLOCK - 1];
    let new_level = 0.5 * tutti_types::Db(-6.0).to_amplitude().get();
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
