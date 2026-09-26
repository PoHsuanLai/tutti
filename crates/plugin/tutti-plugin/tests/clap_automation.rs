//! Parameter automation reaches a hosted plugin through the graph: a
//! `PluginAutomation` node (doc 013 item 5) on the plugin node's event input,
//! against the reference CLAP plugin's gain parameter.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv};

use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Beat, SampleRate, Samples};
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, ForkMode, ForkTarget, Prepare};
use tutti_nodes::automation::Curve;
use tutti_plugin::handles::{ParamAddress, ParamId, TimedParam};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 256;

/// The probe's gain parameter (`GAIN_PARAM_ID`), and its plain range in dB.
const GAIN: u32 = 77;
const GAIN_DB: (f64, f64) = (-60.0, 12.0);

/// A curve holding one value at every beat.
struct Constant(f32);

impl Curve for Constant {
    fn value_at(&self, _beat: Beat) -> Option<f32> {
        Some(self.0)
    }
}

/// The probe's output (its tag, 1.0 on channel 0) scaled by the gain a
/// normalized `v` sets.
fn gained(v: f64) -> f32 {
    let db = GAIN_DB.0 + v * (GAIN_DB.1 - GAIN_DB.0);
    10f64.powf(db / 20.0) as f32
}

/// **Automation reaches the plugin.** The probe renders its tag (1.0 on
/// channel 0) scaled by its gain parameter; an automation node holding the
/// gain at normalized 0.75 (-6 dB) feeds the plugin's event input. Rendered
/// in an export (an offline fork of both, rolling), so the pipeline answers
/// every chunk: once the first chunk has passed, the output is 10^(-6/20).
///
/// Mutation: the plugin ignoring ramp events in `take` → the gain stays at
/// its default (0 dB) → fails. Mutation: decode the ramp's number in the
/// other address model (`address(id, !indexed)`) → the loader finds no
/// parameter `Index(77)` → the gain stays → fails. Mutation: no fork source
/// on the automation node → the fork is refused → fails.
#[test]
fn automation_reaches_the_plugin_through_its_event_input() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_ONLY)
        .gain_enabled(true);
    let probe = load_probe(SAMPLE_RATE);
    let automation = probe.client.automation([TimedParam {
        param_id: ParamAddress::Opaque(ParamId::new(GAIN)),
        curve: Arc::new(Constant(0.75)),
    }]);
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut live, _exec) = Editor::new(prepare);
    let (auto_key, key) = (NodeKey(1), NodeKey(9));
    let _automation = live.insert(auto_key, "automation", automation);
    let _controls = live.insert(key, "plugin", probe.client.bind());
    live.spec_mut().connect_events(
        EventIn { node: key, port: 0 },
        EventEdge::Direct(EventOut {
            node: auto_key,
            port: 0,
        }),
    );
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let offline = OfflineTransport::new(timeline.clone());
    let (_fork_ed, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("the automation node and the plugin fork");

    let mut block = vec![0.0f32; BLOCK];
    for _ in 0..8 {
        timeline.render_graph(&mut fork_exec, BLOCK, &[], &mut [&mut block[..]]);
    }
    let last = *block.last().expect("rendered");
    assert!(
        (last - gained(0.75)).abs() < 1e-4,
        "{last} ≠ {} (-6 dB)",
        gained(0.75)
    );
}
