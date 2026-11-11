//! **A band's bypass and a strip's mute are the graph's `NodeMode`**, and
//! once its declick is over they are exact: the band's output is its input
//! bit for bit, the strip's is `+0.0`, and un-muted the strip is its fader
//! again. The nodes hold no flag of their own (doc 013, "Bypass and mute are
//! graph state", M3), so what is pinned here is the graph's mode through the
//! nodes this change took the flags out of.
//!
//! The figures are analytic (the input itself, zero, the fader's gain), never
//! another render. The declick is 256 frames (`MODE_DECLICK`), counted from
//! the block the toggle lands on; each render below is longer, and only the
//! frames after it are compared.

use tutti_graph::{IntoNode, MODE_DECLICK, NodeMode, Prepare, Solo};
use tutti_nodes::{BusStripNode, EqBandNode, SvfType};
use tutti_types::MaxBlock;
use tutti_types::{Amplitude, Db, Hz, Q, SampleRate, Samples, UnitParam};

const RATE: SampleRate = SampleRate(48_000.0);
const FRAMES: usize = 1024;

fn solo<N: IntoNode>(node: N) -> Solo<N::Controls> {
    Solo::new(node, Prepare::new(RATE, MaxBlock::of(Samples(64))))
}

fn set_mode<C>(solo: &mut Solo<C>, mode: NodeMode) {
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mode(key, mode)
        .expect("the node is in the graph");
}

/// A 1 kHz tone with the awkward samples in it: a negative zero and a
/// denormal beyond the declick, which a copy keeps and arithmetic would not.
fn input() -> Vec<f32> {
    let mut x: Vec<f32> = (0..FRAMES)
        .map(|i| (std::f32::consts::TAU * 1_000.0 * i as f32 / 48_000.0).sin())
        .collect();
    x[MODE_DECLICK.get() + 10] = -0.0;
    x[MODE_DECLICK.get() + 11] = f32::from_bits(1);
    x
}

fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|v| v.to_bits()).collect()
}

/// A bypassed EQ band passes its input bit for bit (`-0.0` and the denormal
/// included) once the declick is over, though its +24 dB bell would have
/// multiplied the tone by ~250; the band is not called to do it.
///
/// Mutations (run): `NodeMode::Active` in place of `Bypassed` → the +24 dB
/// bell is heard → fails; `Muted` → the output is silence → fails.
#[test]
fn a_bypassed_band_is_its_input_bit_for_bit() {
    let mut s = solo(EqBandNode::<f32>::new(
        SvfType::Bell,
        Hz(1_000.0),
        Q(1.0),
        Db(24.0),
    ));
    set_mode(&mut s, NodeMode::Bypassed);
    let x = input();
    let y = s.render_input(&[&x]).remove(0);
    let from = MODE_DECLICK.get();
    assert_eq!(bits(&y[from..]), bits(&x[from..]));
}

/// A muted strip is `+0.0` once the declick is over, and un-muted it is its
/// fader again, exactly: the mute never touched the fader's cell.
///
/// Mutations (run): `Bypassed` in place of `Muted` → the input passes → the
/// silence fails; the un-mute left out → the strip stays silent → the
/// fader's figure fails.
#[test]
fn a_muted_strip_is_silence_and_un_muted_is_its_fader() {
    let mut s = solo(BusStripNode::new());
    assert!(s.controls().set(UnitParam::Volume, 0.75));
    // No denormal: the node multiplies under flush-to-zero, this test does not.
    let mut x = input();
    x[MODE_DECLICK.get() + 11] = 0.5;
    let from = MODE_DECLICK.get();

    set_mode(&mut s, NodeMode::Muted);
    let muted = s.render_input(&[&x, &x]);
    for (c, y) in muted.iter().enumerate() {
        assert!(
            y[from..].iter().all(|v| v.to_bits() == 0),
            "channel {c} is +0.0 after the declick"
        );
    }

    set_mode(&mut s, NodeMode::Active);
    let back = s.render_input(&[&x, &x]);
    let fader = Amplitude(0.75).get();
    for (c, y) in back.iter().enumerate() {
        let want: Vec<u32> = x[from..].iter().map(|v| (v * fader).to_bits()).collect();
        assert_eq!(bits(&y[from..]), want, "channel {c} is the fader again");
    }
}
