//! **A `Legacy::controlled` shadow never moves a live filter.**
//!
//! `SvfFilterNode::clone` shares its param cells (`Param::handle`), which is
//! what keeps a `Net`'s frontend and backend in step. A shadow built from a
//! plain clone would therefore write the *live* cutoff the moment
//! `LegacyControls::set` applied a setting to it — ahead of the settings
//! ring — and the ring's drain would then write older values back. The
//! shadow is `clone()` + `AudioUnit::isolate()`, and `SvfFilterNode::isolate`
//! severs the cells, so the live cutoff follows the ring and nothing else.

use tutti_core::graph::{OutPort, Source};
use tutti_core::unit_param::setting;
use tutti_core::{Hz, NodeKey, SampleRate, Samples, UnitParam, Q};
use tutti_graph::{Delivery, Editor, Legacy, Prepare, Transport, LEGACY_SETTINGS_CAPACITY};
use tutti_nodes::{SvfFilterNode, SvfType};

fn cutoff(hz: f32) -> tutti_core::Setting {
    setting(UnitParam::Cutoff, hz)
}

/// The live cutoff moves only as the ring delivers: not when `set` is called
/// (the shadow's write stays in the shadow), and a held value only after the
/// ring ahead of it has drained — never ahead of ring order.
///
/// Mutation: build the shadow without `isolate()` in `Legacy::controlled` →
/// the first `set` moves the live cutoff at once → fails. Mutation: drop
/// `SvfFilterNode::isolate`'s frequency re-seat → fails the same way.
#[test]
fn a_held_cutoff_never_moves_the_live_filter_ahead_of_the_ring() {
    let filter = SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(500.0), Q(0.707));
    let live = filter.frequency();
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    let (node, mut controls) = Legacy::controlled(&mut ed, filter);
    let key = NodeKey(1);
    ed.insert(key, "svf", node);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut out = vec![0.0f32; 64];
    let mut block = |exec: &mut tutti_graph::Executor| {
        exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    };
    let live_hz = || live.load(std::sync::atomic::Ordering::Acquire);

    // Fill the ring, then hold one more.
    for k in 0..LEGACY_SETTINGS_CAPACITY {
        assert_eq!(controls.set(cutoff(1_000.0 + k as f32)), Delivery::Queued);
        assert_eq!(live_hz(), 500.0, "set #{k} reached the live filter early");
    }
    assert_eq!(controls.set(cutoff(9_000.0)), Delivery::Held);
    assert_eq!(
        live_hz(),
        500.0,
        "the held cutoff stays out of the live filter"
    );
    assert_eq!(
        controls
            .shadow()
            .frequency()
            .load(std::sync::atomic::Ordering::Acquire),
        9_000.0
    );

    block(&mut exec);
    assert_eq!(
        live_hz(),
        1_000.0 + (LEGACY_SETTINGS_CAPACITY - 1) as f32,
        "the ring's last value, not the held one"
    );
    ed.collect();
    block(&mut exec);
    assert_eq!(live_hz(), 9_000.0, "then the held one, in order");
}
