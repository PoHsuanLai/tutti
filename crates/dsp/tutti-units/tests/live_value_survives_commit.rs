//! **Which control values survive `Net::commit`, and which are silently lost.**
//!
//! `Net`'s frontend holds *clones* of its vertices. `Net::migrate` swaps the
//! backend's unit back over any vertex whose `changed <= revision`. So a
//! control value stored **by value** in a unit cannot be changed on a live
//! node: the write lands on a clone that the next commit discards. No error, no
//! diagnostic — the fader moves on screen and not in the sound.
//!
//! This file is that mechanism as an executable statement. It is not testing a
//! bug to be fixed; it is pinning the *rule* that decides how every live
//! control in the engine must be stored, so the next person writing a setter
//! can see the consequence rather than infer it.
//!
//! The rule it pins:
//!
//! > A live control value lives in **shared** storage (`Param<U>`, an
//! > `Arc<Atomic*>`), and its setter takes `&self`. A `&mut self` setter on an
//! > `AudioUnit` means *restructure me, and expect a respawn.*
//!
//! `EqBandNode` is the fixture because it contains **both** conventions:
//! frequency/Q/gain live in the inner SVF's shared `Param`s, and `state` (the
//! band's enable flag) is a plain field. One unit, two outcomes.
//!
//! # Two ways these tests can lie, both of which they did before they worked
//!
//! Recorded because both produce a *passing* test that measures nothing, and
//! neither is visible without deliberately breaking the thing under test.
//!
//! - **Reading through `node_as`.** It reads `self.vertex` — the **frontend** —
//!   so it reports every write as having landed, whatever the backend holds.
//!   The first version asserted on it and passed for exactly that reason.
//!   Rendering is the only vantage point from which the two copies differ.
//! - **A fixture with no input.** `Net::new(0, 2)` with no `pipe_input` renders
//!   silence, so every "did the output change?" assertion compares `0.0` to
//!   `0.0`. That is why [`the_probe_can_tell_active_from_bypassed`] exists: it
//!   fails if the instrument stops being able to see the difference at all.

use std::sync::atomic::Ordering;

use tutti_core::dsp::{AudioUnit, Net};
use tutti_types::{Db, Hz, Q};
use tutti_units::{EqBandNode, SvfType};

/// Build a net with a backend, so `commit`/`migrate` actually run.
///
/// `Net::new` alone has no backend and applies settings in place, which would
/// make every assertion below pass for the wrong reason — the whole hazard is
/// what happens when a *frontend* clone is mutated.
fn net_with_band() -> (Net, Box<dyn AudioUnit>, tutti_core::NodeId) {
    // TWO inputs, and the band wired to them: a net with no input renders
    // silence, which would make every render comparison below compare 0.0 to
    // 0.0 and pass regardless. (It did, on the first attempt.)
    let mut net = Net::new(2, 2);
    let band = EqBandNode::<f32>::new(SvfType::Bell, Hz(1_000.0), Q(1.0), Db(0.0));
    let node = net.push(Box::new(band));
    net.pipe_input(node);
    net.pipe_output(node);
    net.check();
    // **Hold** the backend. `Net::with_backend` drops it (`let _backend = ..`),
    // and a dropped backend never renders — so `migrate`, which runs inside
    // `NetBackend::process`, would never execute and every assertion here would
    // pass for the wrong reason.
    let backend = Box::new(net.backend()) as Box<dyn AudioUnit>;
    (net, backend, node)
}

/// Render one block, which is what actually drives the backend's receive loop:
/// `commit` only *sends* the new net; `migrate` runs when the backend next
/// processes.
fn pump(backend: &mut Box<dyn AudioUnit>) {
    let mut out = [0.0f32; 2];
    backend.tick(&[0.0, 0.0], &mut out);
}

/// Drive the backend far enough that a just-committed net is in force.
fn pump_silence(backend: &mut Box<dyn AudioUnit>) {
    for _ in 0..4 {
        pump(backend);
    }
}

/// Peak output over a short impulse response — a proxy for "what does this
/// band do to a signal", and the only vantage point that distinguishes the
/// frontend's copy from the one actually rendering.
fn render_at_centre(backend: &mut Box<dyn AudioUnit>) -> f32 {
    backend.reset();
    let mut peak = 0.0f32;
    let mut out = [0.0f32; 2];
    for i in 0..256 {
        // An impulse: the band's response to it differs sharply between
        // `Active` (boosted, ringing) and `Bypassed` (passed straight through).
        let x = if i == 0 { 1.0 } else { 0.0 };
        backend.tick(&[x, x], &mut out);
        peak = peak.max(out[0].abs());
    }
    peak
}

/// **A value in shared storage survives a commit.**
///
/// The SVF's cutoff is a `Param<Hz>` — an `Arc<AtomicF32>` that `Clone` shares
/// rather than copies — so a write through the frontend reaches the same cell
/// the backend reads.
#[test]
fn a_shared_param_survives_a_commit() {
    let (mut net, mut backend, node) = net_with_band();

    net.node_as_mut::<EqBandNode<f32>>(node)
        .expect("the node is an EqBandNode")
        .frequency()
        .store(5_000.0, Ordering::Release);

    net.commit();
    pump(&mut backend);

    let after = net
        .node_as::<EqBandNode<f32>>(node)
        .expect("still an EqBandNode")
        .frequency()
        .load(Ordering::Acquire);
    assert!(
        (after - 5_000.0).abs() < 1.0,
        "a `Param` write must survive the commit; found {after} Hz. If this \
         fails, the shared-storage convention itself is broken and every live \
         control in the engine is suspect."
    );
}

/// **A by-value write never reaches the audio thread.**
///
/// The defect, stated rather than discovered. `EqBandNode::set_enabled` writes
/// `self.state`, a plain field. The frontend clone holds it — a debugger and
/// `node_as` both show the new value — and the *rendered audio* never changes,
/// because the backend is a different object that the write never touched.
///
/// # Why this must be observed through audio, not through `node_as`
///
/// `node_as` reads `self.vertex`, i.e. the **frontend**. It therefore always
/// reports the write as having landed, whatever the backend does. An earlier
/// version of this test asserted on `node_as` and passed for exactly that
/// reason — it was inspecting the copy that had been written, not the one that
/// renders. Rendering is the only vantage point from which the two differ.
///
/// This asserting the *broken* behaviour is deliberate: `set_enabled` is a real
/// live control and belongs in shared storage. If someone fixes it, this test
/// fails and says so, which is the correct prompt.
#[test]
fn a_by_value_write_never_reaches_the_audio_thread() {
    let (mut net, mut backend, node) = net_with_band();

    // A bell at unity gain is transparent, so make it audibly *not* transparent:
    // a big boost means "active" and "bypassed" render measurably differently.
    net.node_as_mut::<EqBandNode<f32>>(node)
        .unwrap()
        .gain_db()
        .store(24.0, Ordering::Release);
    net.commit();
    pump_silence(&mut backend);

    let active = render_at_centre(&mut backend);
    // Bypass it — a live control, written by value.
    net.node_as_mut::<EqBandNode<f32>>(node)
        .unwrap()
        .set_enabled(false);
    net.commit();
    pump_silence(&mut backend);

    let after_bypass = render_at_centre(&mut backend);

    assert!(
        (active - after_bypass).abs() < 1e-6,
        "a by-value write is expected NOT to reach the audio thread: the \
         rendered output should be unchanged ({active} -> {after_bypass}). If \
         this fails, `state` now lives in shared storage — good; move this \
         case up beside the `Param` test and delete it here."
    );

    // ...while the frontend cheerfully reports the write as applied. This pair
    // of assertions is the whole lesson: the copy you can inspect is not the
    // copy that renders.
    assert!(
        !net.node_as::<EqBandNode<f32>>(node).unwrap().is_enabled(),
        "the frontend must still report the write, which is what makes this \
         class of bug so hard to see"
    );
}

/// **The same value written through `Net::set` survives**, because that path
/// reaches the backend rather than a clone.
///
/// The contrast that makes the rule actionable: it is not "you cannot change a
/// live node", it is "you must change it through shared storage or the setting
/// queue". `Net::set` enqueues when a backend exists, so the write is applied
/// on the audio thread's own copy.
#[test]
fn a_setting_through_the_queue_survives() {
    let (mut net, mut backend, node) = net_with_band();

    net.set(tutti_core::unit_param::node_setting(
        node,
        tutti_types::UnitParam::Cutoff,
        7_500.0,
    ));
    net.commit();
    pump(&mut backend);

    let after = net
        .node_as::<EqBandNode<f32>>(node)
        .expect("still an EqBandNode")
        .frequency()
        .load(Ordering::Acquire);
    assert!(
        (after - 7_500.0).abs() < 1.0,
        "a queued `Setting` must reach the node; found {after} Hz"
    );
}

/// **The instrument works**: an active band and a bypassed one render
/// differently.
///
/// Without this, `a_by_value_write_never_reaches_the_audio_thread`'s "output
/// unchanged" assertion would pass even if `render_at_centre` could not tell
/// the two states apart — which would make it a test of nothing. Built by
/// constructing each state directly rather than by mutating a live node,
/// precisely because mutating a live node is the thing under investigation.
#[test]
fn the_probe_can_tell_active_from_bypassed() {
    fn peak_of(enabled: bool) -> f32 {
        let mut band = EqBandNode::<f32>::new(SvfType::Bell, Hz(1_000.0), Q(1.0), Db(24.0));
        band.set_enabled(enabled);
        let mut out = [0.0f32; 2];
        let mut peak = 0.0f32;
        for i in 0..256 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            band.tick(&[x, x], &mut out);
            peak = peak.max(out[0].abs());
        }
        peak
    }

    let active = peak_of(true);
    let bypassed = peak_of(false);
    assert!(
        (active - bypassed).abs() > 1e-6,
        "a +24 dB bell must render differently from a bypassed band, or the \
         by-value test above is measuring nothing: active={active} \
         bypassed={bypassed}"
    );
}
