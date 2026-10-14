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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_nodes::automation::Curve;
use tutti_plugin::handles::{ParamAddress, PluginClient, PluginHandle, TimedParam};
use tutti_plugin::BridgeConfig;
use tutti_types::Beat;

use crate::clap_probe::plugin_server_path;
use crate::probe_path::probe_path;

/// A curve holding one value, changeable while the graph runs: the host's
/// value for one parameter.
#[derive(Default)]
pub struct Knob(AtomicU32);

impl Knob {
    pub fn new(v: f32) -> Arc<Self> {
        let k = Arc::new(Self::default());
        k.set(v);
        k
    }

    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

impl Curve for Knob {
    fn value_at(&self, _beat: Beat) -> Option<f32> {
        Some(f32::from_bits(self.0.load(Ordering::Relaxed)))
    }
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

/// One knob per parameter `0..n`, each at `value(i)`, and the timed params
/// that automate the plugin's parameters from them.
pub fn knobs(n: usize, value: impl Fn(usize) -> f32) -> (Vec<Arc<Knob>>, Vec<TimedParam>) {
    let knobs: Vec<Arc<Knob>> = (0..n).map(|i| Knob::new(value(i))).collect();
    let params = knobs
        .iter()
        .enumerate()
        .map(|(i, k)| TimedParam {
            param_id: ParamAddress::Index(i32::try_from(i).expect("a few params")),
            curve: Arc::clone(k) as Arc<dyn Curve>,
        })
        .collect();
    (knobs, params)
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
