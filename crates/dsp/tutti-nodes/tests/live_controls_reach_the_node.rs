//! **Which control writes reach a node running in the graph, and which
//! cannot.**
//!
//! The rule this pins decides how every live control in the engine is
//! stored:
//!
//! > A live control value lives in **shared** storage (`Param<U>`, an
//! > `Arc<Atomic*>`), reached through the controls the node was inserted with
//! > (its `ParamSet`, or typed handles). A `&mut self` setter means
//! > *restructure me*: it can only reach a node that is not running, and a
//! > running one is changed by replacing it.
//!
//! A graph node leaves no clone to write: once inserted it belongs to the
//! executor, so a `&mut self` setter on it does not compile. What is pinned
//! here is the other half — that the shared path reaches the node that
//! renders, across a commit — and the way a by-value change is made instead
//! (a replace).
//!
//! `EqBandNode` is the fixture because it has both conventions: its
//! frequency/Q/gain are the inner SVF's shared `Param`s, and its enable flag
//! is a plain field.
//!
//! Every assertion is on **rendered audio**: the copy one can inspect is not
//! the one that renders, and a test that reads a control back proves only
//! that the write happened somewhere. Mutations (run): make `ParamSet::set`
//! store only the authored value → `a_param_set_through_the_controls_*`
//! fails; build the replacement band `Active` → `a_by_value_change_*` fails.

use tutti_core::UnitParam;
use tutti_graph::{CrossfadeCurve, Fade, IntoNode, Prepare, Solo};
use tutti_nodes::{EqBandNode, SvfType};
use tutti_types::{Db, Hz, SampleRate, Samples, Q};

const RATE: SampleRate = SampleRate(48_000.0);

fn band(gain: f32) -> EqBandNode<f32> {
    EqBandNode::<f32>::new(SvfType::Bell, Hz(1_000.0), Q(1.0), Db(gain))
}

/// Energy of the band's response to a 1 kHz tone at its centre — the
/// band's effect on a signal (a +24 dB bell multiplies it by ~250; bypassed
/// or flat, it is the tone's own).
fn energy<C>(solo: &mut Solo<C>) -> f32 {
    let x: Vec<f32> = (0..2048)
        .map(|i| (std::f32::consts::TAU * 1_000.0 * i as f32 / 48_000.0).sin())
        .collect();
    solo.render_input(&[&x])[0][1024..]
        .iter()
        .map(|s| s * s)
        .sum()
}

fn solo<N: IntoNode>(node: N) -> Solo<N::Controls> {
    Solo::new(node, Prepare::new(RATE, Samples(64)))
}

/// **The instrument works**: an active +24 dB bell and a bypassed band
/// render differently. Without it the assertions below could compare two
/// identical renders and pass.
#[test]
fn the_probe_can_tell_active_from_bypassed() {
    let active = energy(&mut solo(band(24.0)));
    let mut off = band(24.0);
    off.set_enabled(false);
    let bypassed = energy(&mut solo(off));
    assert!(
        active > 10.0 * bypassed,
        "active={active} bypassed={bypassed}: the probe cannot see the difference"
    );
}

/// **A param set through the node's controls reaches the node that renders,
/// and survives a commit.** A unity-gain bell is transparent; set to +24 dB
/// through its `ParamSet` it must ring, and still after an unrelated commit.
#[test]
fn a_param_set_through_the_controls_reaches_the_running_node() {
    let mut s = solo(band(0.0));
    let flat = energy(&mut s);
    assert!(s.controls().set(UnitParam::GainDb, 24.0));
    let boosted = energy(&mut s);
    assert!(
        boosted > 10.0 * flat,
        "a +24 dB write through the controls did not reach the audio: {flat} -> {boosted}"
    );
    s.renderer_mut()
        .editor_mut()
        .commit()
        .expect("an unrelated commit");
    let after = energy(&mut s);
    assert!(
        (after - boosted).abs() < 1e-3 * boosted,
        "the value did not survive the commit: {boosted} -> {after}"
    );
}

/// **A by-value change is made by replacing the node.** The enable flag is
/// a plain field, so a running band cannot be bypassed by a setter; a
/// bypassed band faded in over it (`Editor::replace`) is how the change
/// reaches the audio.
#[test]
fn a_by_value_change_reaches_the_audio_by_replacing_the_node() {
    let mut s = solo(band(24.0));
    let active = energy(&mut s);
    let mut off = band(24.0);
    off.set_enabled(false);
    let key = s.key();
    let editor = s.renderer_mut().editor_mut();
    editor
        .replace(key, off, Fade::new(Samples(1), CrossfadeCurve::default()))
        .expect("same shape");
    editor.commit().expect("commits");
    let bypassed = energy(&mut s);
    assert!(
        active > 10.0 * bypassed,
        "the replacement did not reach the audio: {active} -> {bypassed}"
    );
}
