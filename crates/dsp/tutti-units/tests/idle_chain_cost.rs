//! Is "two nodes per modulatable param" actually a cost worth caring about?
//!
//! Ignored by default — this is a measurement, not an assertion. Run with:
//!
//! ```text
//! cargo test -p tutti-units --test idle_chain_cost -- --ignored --nocapture
//! ```
//!
//! The question is whether an always-on base chain (`AtomicSourceUnit →
//! ParamSumUnit → port`) per modulatable param is a real RT burden or just a
//! bigger number in `Net::size()`. A node in `Net` is not free — every one is a
//! virtual `process` call plus an input gather per block — but "not free" and
//! "matters" are different claims, and only one of them should drive a design
//! decision.

use std::time::Instant;

use tutti_core::dsp::{AudioUnit as _, Net};
use tutti_types::UnitParam;
use tutti_units::{AtomicSourceUnit, DistortionNode, ParamPorts, ParamSumUnit, ShapeKind};

const BLOCK: usize = 128;
const BLOCKS: usize = 20_000;

/// A chain of `n` distortions, each either plain or ported-with-base-chain.
fn build(n: usize, with_chains: bool) -> Net {
    let mut net = Net::new(2, 2);
    let mut prev: Option<tutti_core::NodeId> = None;

    for _ in 0..n {
        let id = if with_chains {
            let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
            let port = dist.param_port(UnitParam::Drive).unwrap();
            let target = net.push(Box::new(dist));
            // The always-on idle chain: two extra nodes, two extra edges.
            let base = net.push(Box::new(AtomicSourceUnit::new(5.0)));
            let sum = net.push(Box::new(ParamSumUnit::new(0, 0.0, 10.0)));
            net.connect(base, 0, sum, 0);
            net.connect(sum, 0, target, port);
            target
        } else {
            net.push(Box::new(DistortionNode::new(ShapeKind::Tanh, 5.0)))
        };

        match prev {
            None => {
                net.connect_input(0, id, 0);
                net.connect_input(1, id, 1);
            }
            Some(p) => {
                net.connect(p, 0, id, 0);
                net.connect(p, 1, id, 1);
            }
        }
        prev = Some(id);
    }

    net.pipe_output(prev.unwrap());
    net.check();
    net
}

fn run(net: &mut Net) -> f32 {
    let mut sink = 0.0f32;
    let mut out = [0.0f32; 2];
    for i in 0..BLOCKS * BLOCK {
        let x = 0.5 * ((i % 128) as f32 * 0.05).sin();
        net.tick(&[x, x], &mut out);
        sink += out[0];
    }
    sink
}

/// Wall-clock cost of the idle chains, as a fraction of the audio work they sit
/// beside.
///
/// The number that matters is not "how many nodes" but "what share of a block's
/// budget". A distortion chain is a fair reference: the shaper is a `tanh` per
/// sample per channel, which is real but not heavy — so this is closer to a
/// worst case for the chains than a best one. Against a convolution reverb or a
/// ladder filter the same chains would disappear entirely.
#[test]
#[ignore = "measurement, not an assertion"]
fn idle_chain_overhead_against_real_dsp() {
    for n in [4usize, 16, 64] {
        let mut plain = build(n, false);
        let mut ported = build(n, true);

        // Warm both, so neither pays first-touch page faults in the timed run.
        let _ = run(&mut plain);
        let _ = run(&mut ported);

        let t0 = Instant::now();
        let a = run(&mut plain);
        let plain_ns = t0.elapsed().as_nanos() as f64;

        let t1 = Instant::now();
        let b = run(&mut ported);
        let ported_ns = t1.elapsed().as_nanos() as f64;

        let overhead = (ported_ns - plain_ns) / plain_ns * 100.0;
        let per_sample_ns = (ported_ns - plain_ns) / (BLOCKS * BLOCK) as f64;

        println!(
            "{n:3} effects | plain {:>3} nodes {:>7.1}ms | ported {:>3} nodes {:>7.1}ms | \
             +{overhead:5.1}%  {per_sample_ns:.2}ns/sample  (sink {a:.1}/{b:.1})",
            plain.size(),
            plain_ns / 1e6,
            ported.size(),
            ported_ns / 1e6,
        );
    }
}

/// The same chains against the real budget: one block at 48 kHz is 2.67 ms.
///
/// A cost is only a cost relative to the deadline it eats into. This reports the
/// idle chains' share of one block, which is the number a decision should
/// actually turn on.
#[test]
#[ignore = "measurement, not an assertion"]
fn idle_chain_share_of_a_block_budget() {
    const SAMPLE_RATE: f64 = 48_000.0;
    let block_budget_ns = BLOCK as f64 / SAMPLE_RATE * 1e9;

    for n in [4usize, 16, 64] {
        let mut plain = build(n, false);
        let mut ported = build(n, true);
        let _ = run(&mut plain);
        let _ = run(&mut ported);

        let t0 = Instant::now();
        let _ = run(&mut plain);
        let plain_ns = t0.elapsed().as_nanos() as f64;
        let t1 = Instant::now();
        let _ = run(&mut ported);
        let ported_ns = t1.elapsed().as_nanos() as f64;

        // `run` ticks per SAMPLE, so normalise by total samples and scale to a
        // block — not by BLOCKS, which would over-report by a factor of BLOCK.
        let total_samples = (BLOCKS * BLOCK) as f64;
        let delta_per_block = (ported_ns - plain_ns) / total_samples * BLOCK as f64;
        let share = delta_per_block / block_budget_ns * 100.0;

        println!(
            "{n:3} effects ({n:2} idle chains) | +{delta_per_block:7.0}ns/block | \
             {share:5.2}% of the {block_budget_ns:.0}ns budget @48k/{BLOCK}"
        );
    }
}
