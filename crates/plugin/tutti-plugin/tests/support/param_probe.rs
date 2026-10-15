//! The reference VST2 plugin through the **real out-of-process host**, with
//! as many parameters as a test asks for: what the parameter-delivery suites
//! drive.
//!
//! The VST2 probe takes its parameter count from `TUTTI_VST2_PROBE_PARAMS`,
//! read when its `AEffect` is built, which is inside the `plugin-server` the
//! client spawns: the variable is set for the spawn and cleared once the
//! client is loaded, under `clap_probe::exclusive`.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![allow(dead_code)]

use std::time::{Duration, Instant};

use tutti_mod::ControlProgram;
use tutti_nodes::automation::{ControlLaneNode, LaneControls};
use tutti_plugin::handles::{PluginClient, PluginHandle};
use tutti_plugin::BridgeConfig;

use crate::clap_probe::plugin_server_path;
use crate::probe_path::probe_path;

/// A host's value for plugin parameter `id`: a foreign control lane whose
/// knob is at `value`, in the plugin's normalisation (doc 014 §5). It sends
/// its knob on its first block and whenever the knob moves
/// (`LaneControls::set_knob`), and again on `LaneControls::resync`.
pub fn knob(id: u32, value: f32) -> ControlLaneNode {
    ControlLaneNode::for_foreign(id, ControlProgram::default(), Some(value), (0.0, 1.0))
}

/// The VST2 probe with `params` parameters, loaded through a real
/// `plugin-server`. The caller holds `exclusive()`.
pub fn load_vst2_probe(params: usize, sample_rate: f64) -> (PluginClient, PluginHandle) {
    // SAFETY: the caller holds `exclusive()`, which every test in these
    // suites takes before touching the environment (see `ProbeEnv::set`).
    unsafe {
        std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server_path());
        std::env::set_var("TUTTI_VST2_PROBE_PARAMS", params.to_string());
    }
    let client = PluginClient::new(BridgeConfig::default(), probe_path().clone(), sample_rate);
    // SAFETY: as above. The server has read it; leaving it set would reshape
    // whichever probe loads next.
    unsafe { std::env::remove_var("TUTTI_VST2_PROBE_PARAMS") };
    let client = client.expect("load the reference VST2 plugin through a real plugin-server");
    let handle = PluginHandle::from_client(&client);
    (client, handle)
}

/// One [`knob`] lane per parameter `0..n` (VST2 indices), each at
/// `value(i)`, and their controls.
pub fn knobs(n: usize, value: impl Fn(usize) -> f32) -> (Vec<LaneControls>, Vec<ControlLaneNode>) {
    let lanes: Vec<ControlLaneNode> = (0..n)
        .map(|i| knob(u32::try_from(i).expect("a few params"), value(i)))
        .collect();
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
