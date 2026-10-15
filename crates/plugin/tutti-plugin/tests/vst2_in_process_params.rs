//! **Parameter events reach an in-process VST2 plugin**, against the
//! reference probe (`tutti-vst2-test-plugin`), read back through the plugin's
//! own `getParameter`.
//!
//! The node delivers through the out-of-process node's delivery
//! (`host::node`'s `param_delivery`): a slot per parameter, **changes only**
//! against the value last delivered to the instance, and, VST2 having no
//! sample-accurate parameter events, each parameter's last point in a block
//! set before the plugin processes it (as the out-of-process VST2 loader sets
//! a chunk's). What it pins: a moving parameter reaches the plugin; a value
//! held still is sent once, so an edit made on the plugin survives it while a
//! moving one replaces it; and an export (a fork) renders that edit as live
//! playback does.
//!
//! The probe takes its parameter count from `TUTTI_VST2_PROBE_PARAMS`, and,
//! with `TUTTI_VST2_PROBE_STATE_DC` and `TUTTI_VST2_PROBE_NO_CHUNKS`, adds
//! `1000 · param0` to every output sample (its saved state as a DC, see
//! `vst2_in_process_fork.rs`). It reads its environment in its constructor,
//! so the variables are set for the load (and a fork's second instance) and
//! cleared after.

#![cfg(feature = "vst2")]

use std::sync::{Arc, Mutex, MutexGuard};

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Beat, Bpm, ChannelLayout, SampleRate, Samples};
use tutti_graph::contract::Direct;
use tutti_graph::{
    Event, ForkMode, ForkTarget, GraphBuilder, Offset, ParamRamp, Prepare, Transport,
};
use tutti_mod::ControlProgram;
use tutti_nodes::automation::ControlLaneNode;
use tutti_plugin::handles::{Normalized, ParamAddress, PluginHandle};
use tutti_plugin::{in_process_vst2_client, InProcessVst2Client};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Serializes against the probe's process-global environment.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The probe's environment for one test, cleared when it drops. The caller
/// holds `PROBE_LOCK`.
struct ProbeEnv(Vec<&'static str>);

impl ProbeEnv {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn set(mut self, key: &'static str, value: &str) -> Self {
        // SAFETY: callers hold `PROBE_LOCK`; no other thread here touches the
        // environment.
        unsafe { std::env::set_var(key, value) };
        self.0.push(key);
        self
    }

    fn params(self, n: usize) -> Self {
        self.set("TUTTI_VST2_PROBE_PARAMS", &n.to_string())
    }

    /// `1000 · param0` on every output sample.
    fn state_dc(self) -> Self {
        self.set("TUTTI_VST2_PROBE_STATE_DC", "1")
            .set("TUTTI_VST2_PROBE_NO_CHUNKS", "1")
    }
}

impl Drop for ProbeEnv {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: as in `set`.
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn load() -> (InProcessVst2Client, PluginHandle) {
    let path = probe_path::probe_path();
    in_process_vst2_client(path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process load of the probe at {path:?}: {e:?}"))
}

/// The plugin's own value for parameter `i`.
fn read(handle: &PluginHandle, i: u32) -> Option<f32> {
    handle
        .params()
        .parameter_value(ParamAddress::Index(i32::try_from(i).expect("small")))
}

/// A ramp to `value` on parameter `id` (a VST2 index) at block frame `at`.
fn ramp(at: usize, id: u32, value: f32) -> Event {
    Event::ramp(
        Offset::new(at, Samples(BLOCK)).expect("inside the block"),
        ParamRamp::foreign(id, value, Samples(0)),
    )
}

/// **A moving parameter reaches the plugin**: each block the host moves
/// parameters 1 and 3 (twice inside the block, at frames 0 and 40), and
/// after the block the plugin holds each one's last value in it. Every point
/// is counted as sent, none dropped or merged; a block with no event leaves
/// the plugin's values where they are.
///
/// Mutations (run), each fails: no `apply_params` in `drive_f32` → the probe
/// keeps its defaults (`i / 16`); the first point of a queue applied instead
/// of the last (`points.first()`) → each block's frame-0 value; the walk
/// handing ramps to `other` (`EventKind::Ramp(ramp) if false` in
/// `param_input::take`) → defaults, and nothing counted.
#[test]
fn a_moving_parameter_reaches_the_plugin() {
    let _lock = lock_probe();
    let _env = ProbeEnv::new().params(8);
    let (client, handle) = load();
    let controls = client.controls();
    let mut unit = Direct::new(client, SampleRate(SAMPLE_RATE), BLOCK);
    assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "the probe's default");

    // Exact eighths, distinct within and across blocks.
    let at = |b: usize, k: usize| ((b * 2 + k) % 8) as f32 / 8.0 + 0.0625;
    for b in 0..6 {
        unit.events(
            0,
            &[
                ramp(0, 1, at(b, 0)),
                ramp(0, 3, 1.0 - at(b, 0)),
                ramp(40, 1, at(b, 1)),
                ramp(40, 3, 1.0 - at(b, 1)),
            ],
        );
        unit.block();
        assert_eq!(read(&handle, 1), Some(at(b, 1)), "block {b}, parameter 1");
        assert_eq!(
            read(&handle, 3),
            Some(1.0 - at(b, 1)),
            "block {b}, parameter 3"
        );
        assert_eq!(read(&handle, 2), Some(2.0 / 16.0), "an unmoved parameter");
    }
    unit.block();
    assert_eq!(read(&handle, 1), Some(at(5, 1)), "held without events");

    let stats = controls.param_delivery();
    assert_eq!(stats.points_sent, 6 * 4, "{stats:?}");
    assert_eq!(stats.points_dropped, 0, "{stats:?}");
    assert_eq!(stats.points_merged, 0, "{stats:?}");
    drop(handle);
}

/// **A held value is sent once, so an edit made on the plugin survives it;
/// a moving value replaces it.** The host sends parameter 2 at 0.25 every
/// block; the plugin's value is then set to 0.75 behind the node's back
/// (through the plugin handle, as an edit in its editor would be); the host
/// keeps sending 0.25 and the plugin keeps 0.75, with nothing more sent;
/// then the host moves to 0.5 and the plugin takes it.
///
/// Mutations (run), each fails: the node forgetting what it delivered
/// (`slot.sent = None` for `Some(slot.staged)` in `ParamDelivery::settle`)
/// → every block resends 0.25 over the edit; the node never sending a
/// parameter again once it has sent one (`place` returning when
/// `slot.sent.is_some()`) → 0.5 never arrives.
#[test]
fn a_held_value_is_sent_once_and_an_edit_on_the_plugin_survives_it() {
    let _lock = lock_probe();
    let _env = ProbeEnv::new().params(8);
    let (client, handle) = load();
    let controls = client.controls();
    let mut unit = Direct::new(client, SampleRate(SAMPLE_RATE), BLOCK);
    let p = 2;

    let held = |unit: &mut Direct<InProcessVst2Client>, v: f32, blocks: usize| {
        for _ in 0..blocks {
            unit.events(0, &[ramp(0, p, v), ramp(32, p, v)]);
            unit.block();
        }
    };
    held(&mut unit, 0.25, 4);
    assert_eq!(read(&handle, p), Some(0.25));
    assert_eq!(controls.param_delivery().points_sent, 1, "sent once");

    handle
        .params()
        .set_parameter_value(ParamAddress::Index(p as i32), Normalized::new(0.75));
    assert_eq!(read(&handle, p), Some(0.75), "the edit reached the plugin");
    held(&mut unit, 0.25, 32);
    assert_eq!(
        read(&handle, p),
        Some(0.75),
        "a held host value overwrote the plugin-side edit"
    );
    assert_eq!(controls.param_delivery().points_sent, 1, "nothing resent");

    held(&mut unit, 0.5, 2);
    assert_eq!(
        read(&handle, p),
        Some(0.5),
        "a moving host value did not replace the edit"
    );
    assert_eq!(controls.param_delivery().points_sent, 2);
    drop(handle);
}

/// **An export renders an edit made in the plugin's own editor, as live
/// playback does.** A foreign control lane holds parameter 0 at 0.25 and
/// feeds the plugin node's event input; the probe plays `1000 · param0` as a
/// DC on top of its channel tag (1 on channel 0), so live renders 251. The
/// plugin's parameter 0 is then set to 0.5 behind the node's back, through
/// the plugin handle: live, the held 0.25 is never re-sent, so it renders
/// 501. The graph is then forked offline (an export): the fork is loaded
/// with the live state (param0 = 0.5) and its delivery is seeded with what
/// the live node delivered (0.25), so the fork's fresh lane, which sends its
/// 0.25 on its first block (doc 014 §10's fork requirement), is not passed
/// on, and the export renders 501 too.
///
/// Mutation (run): the fork's delivery not seeded (no
/// `fork.params.seed(&delivered)` in `vst2_in_process/fork.rs`) → the fork
/// sends the lane's 0.25 over the loaded 0.5 → the export renders 251 →
/// fails.
#[test]
fn an_export_renders_an_edit_made_in_the_plugin_as_live_does() {
    let _lock = lock_probe();
    let _env = ProbeEnv::new().params(4).state_dc();
    let (client, handle) = load();
    // A host's knob for VST2 parameter 0, held still.
    let held = ControlLaneNode::for_foreign(0, ControlProgram::default(), Some(0.25), (0.0, 1.0));

    let stereo = ChannelLayout::from_count(2);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, stereo);
    let (lane, _) = g.add_with_controls(held);
    let (plugin, controls) = g.add_with_controls(client);
    g.event_connect(lane, 0, plugin, 0);
    g.pipe_output(plugin);
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let mut live = g.renderer(prepare).expect("the graph compiles");
    live.set_transport(Transport::new(true, Bpm(120.0), Beat(0.0), None));
    let channel_0 = |out: &[Vec<f32>]| out[0].last().copied();

    assert_eq!(
        channel_0(&live.render(BLOCK * 4)),
        Some(251.0),
        "live, held"
    );
    handle
        .params()
        .set_parameter_value(ParamAddress::Index(0), Normalized::new(0.5));
    assert_eq!(
        channel_0(&live.render(BLOCK * 8)),
        Some(501.0),
        "live keeps the edit"
    );
    assert_eq!(controls.param_delivery().points_sent, 1);

    // The export.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let offline = OfflineTransport::new(Arc::clone(&timeline));
    let (_fork_ed, mut fork_exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("the lane and the plugin fork");
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    for _ in 0..8 {
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        timeline.render_graph(&mut fork_exec, BLOCK, &[], &mut outs);
    }
    assert_eq!(
        channel_0(&out),
        Some(501.0),
        "the export plays the held lane over the edit live playback keeps"
    );
    drop(handle);
}
