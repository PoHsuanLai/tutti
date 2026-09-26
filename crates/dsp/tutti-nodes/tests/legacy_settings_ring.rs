//! **A `Legacy::controlled` shadow never moves a live filter.**
//!
//! Pinned on [`CellFilter`], a test-local `AudioUnit` shaped like the filters
//! that used to carry this test: a cutoff in a `Param` cell its `Clone`
//! shares, severed by `isolate`. (It was the SVF's, then the ladder's, until
//! each became a native node, which has no settings ring: its `ParamSet`
//! writes the live cell directly. A unit of the test's own keeps the subject
//! on `Legacy` whichever nodes are ported next. This file goes with `Legacy`.)
//!
//! `CellFilter::clone` shares its param cell (`Param::handle`), which is
//! what keeps a `Net`'s frontend and backend in step. A shadow built from a
//! plain clone would therefore write the *live* cutoff the moment
//! `LegacyControls::set` applied a setting to it — ahead of the settings
//! ring — and the ring's drain would then write older values back. The
//! shadow is `clone()` + `AudioUnit::isolate()`, and `CellFilter::isolate`
//! severs the cell, so the live cutoff follows the ring and nothing else.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::unit_param::setting;
use tutti_core::{
    AtomicF32, AudioUnit, BufferMut, BufferRef, Hz, NodeKey, Param, SampleRate, Samples, Setting,
    SignalFrame, UnitParam,
};
use tutti_graph::{Delivery, Editor, Legacy, Prepare, Transport, LEGACY_SETTINGS_CAPACITY};

/// A one-output unit with a cutoff cell, as a filter holds one: `Clone`
/// shares the cell, `isolate` severs it, `set` writes it. It renders
/// silence — the test reads the cell, not the audio.
struct CellFilter {
    cutoff: Param<Hz>,
}

impl CellFilter {
    fn new(cutoff: Hz) -> Self {
        Self {
            cutoff: Param::new(cutoff),
        }
    }

    fn frequency(&self) -> Arc<AtomicF32> {
        self.cutoff.as_atomic()
    }
}

impl Clone for CellFilter {
    fn clone(&self) -> Self {
        Self {
            cutoff: self.cutoff.handle(),
        }
    }
}

impl AudioUnit for CellFilter {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = 0.0;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, 0.0);
        }
    }

    fn set(&mut self, setting: Setting) {
        if let Some((UnitParam::Cutoff, hz)) = tutti_core::unit_param::from_setting(&setting) {
            self.cutoff.store(Hz(hz));
        }
    }

    fn isolate(&mut self) {
        self.cutoff.detach();
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }

    fn get_id(&self) -> u64 {
        0x_4345_4C4C_4649_4C54 // "CELLFILT"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

fn cutoff(hz: f32) -> tutti_core::Setting {
    setting(UnitParam::Cutoff, hz)
}

/// The live cutoff moves only as the ring delivers: not when `set` is called
/// (the shadow's write stays in the shadow), and a held value only after the
/// ring ahead of it has drained — never ahead of ring order.
///
/// Mutation (run): build the shadow without `isolate()` in
/// `Legacy::controlled` → the first `set` moves the live cutoff at once →
/// fails. Mutation (run): drop `CellFilter::isolate`'s `detach` → fails the
/// same way.
#[test]
fn a_held_cutoff_never_moves_the_live_filter_ahead_of_the_ring() {
    let filter = CellFilter::new(Hz(500.0));
    let live = filter.frequency();
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    let (node, mut controls) = Legacy::controlled(&mut ed, filter);
    let key = NodeKey(1);
    ed.insert(key, "cell-filter", node);
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
