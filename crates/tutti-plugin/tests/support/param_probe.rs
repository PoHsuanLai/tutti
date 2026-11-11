//! The reference CLAP plugin with as many parameters as a test asks for,
//! through the **real out-of-process host**: what the parameter-delivery suites
//! drive.
//!
//! The probe advertises `TUTTI_CLAP_PROBE_EXTRA_PARAMS` parameters after its
//! table, read inside the `plugin-server` the client spawns: the variable is
//! set for the spawn and cleared once the client is loaded (`ProbeEnv`, under
//! `exclusive()`). Extra `i` has id `EXTRA_PARAM_BASE + i`, a `0..1` range and
//! a default of `0`.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![allow(dead_code)]

use std::time::{Duration, Instant};

use tutti_clap_test_plugin::EXTRA_PARAM_BASE;
use tutti_modulation::ControlProgram;
use tutti_nodes::automation::{ControlLaneNode, LaneControls};
use tutti_plugin::handles::PluginClient;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::ParamId;

use crate::clap_probe::{ProbeEnv, load_probe};

/// The plugin's id for extra parameter `i`.
pub fn extra(i: usize) -> ParamId {
    ParamId::new(EXTRA_PARAM_BASE + u32::try_from(i).expect("a few params"))
}

/// A host's value for plugin parameter `id`: a foreign control lane whose
/// knob is at `value`, in the plugin's normalisation (doc 014 §5). It sends
/// its knob on its first block and whenever the knob moves
/// (`LaneControls::set_knob`), and again on `LaneControls::resync`.
pub fn knob(id: u32, value: f32) -> ControlLaneNode {
    ControlLaneNode::for_foreign(id, ControlProgram::default(), Some(value), (0.0, 1.0))
}

/// The CLAP probe with `params` extra parameters, loaded through a real
/// `plugin-server`. The caller holds `exclusive()`.
pub fn load_param_probe(params: usize, sample_rate: f64) -> (PluginClient, PluginHandle) {
    let _env = ProbeEnv::new().extra_params(params);
    let probe = load_probe(sample_rate);
    (probe.client, probe.handle)
}

/// One [`knob`] lane per extra parameter `0..n`, each at `value(i)`, and their
/// controls.
pub fn knobs(n: usize, value: impl Fn(usize) -> f32) -> (Vec<LaneControls>, Vec<ControlLaneNode>) {
    let lanes: Vec<ControlLaneNode> = (0..n).map(|i| knob(extra(i).get(), value(i))).collect();
    (lanes.iter().map(ControlLaneNode::controls).collect(), lanes)
}

/// Polls `check` while `step` renders, until it holds or `timeout` passes.
/// Returns whether it held.
pub fn until(timeout: Duration, mut step: impl FnMut(), mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        step();
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
