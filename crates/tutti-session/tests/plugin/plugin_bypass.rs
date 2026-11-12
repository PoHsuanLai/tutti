//! A hosted plugin's bypass is the graph's (doc 013, "Bypass and mute are
//! graph state", M2's host half): `Graph::set_mode` on a plugin node, over
//! the reference CLAP plugin in a real `plugin-server`.
//!
//! The plugin's own bypass parameter is not used (doc 013, "Plugins"): the
//! graph's aligned-input path is delayed by the node's **declared** latency,
//! which for a served plugin is the plugin's own latency plus the pipeline's
//! chunk. So a bypassed plugin node passes a click exactly that many frames
//! late, and its server is not called while it is parked.
//!
//! Every figure is analytic: a single `1.0` at frame [`FIRE`], so the bypassed
//! output is `1.0` at `FIRE + L` and `+0.0` at every other frame. The plugin
//! when active adds a constant tag (`TagPassthrough`), so it is never that.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! it is not a dev-dependency (a cycle through `tutti-plugin`), and without it
//! every test here fails, naming that command. They must fail rather than
//! skip.

use crate::support;

use std::time::{Duration, Instant};
use tutti_types::MaxBlock;

use support::{clap_probe, plugin_server};
use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_plugin::catalog::PluginRef;
use tutti_session::{InPort, NodeMode, OutPort, PluginLoadRequest, Root, Session};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 1_024;
/// The click's frame: inside the first block, not on its edge.
const FIRE: usize = 100;

/// The server's path, and the plugin in `TagPassthrough` (`1`): it adds its
/// tag to what it is given, so an active plugin is never silent.
///
/// SAFETY: nextest runs each test in its own process, and both are set before
/// the first server is spawned, from the test's own thread.
fn set_server() {
    unsafe {
        std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server());
        std::env::set_var("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
    }
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// `1.0` at absolute frame [`FIRE`], `+0.0` everywhere else.
#[derive(Clone, Default)]
struct Click {
    at: usize,
}

impl Node for Click {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_cost(tutti_graph::Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (_, mut outs) = io.split();
        let out = outs.get(0);
        for (i, o) in out.iter_mut().enumerate() {
            *o = if self.at + i == FIRE { 1.0 } else { 0.0 };
        }
        self.at += out.len();
        Status::Modified
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

struct Rig {
    s: Session,
    plugin: NodeKey,
}

/// `Click -> plugin (input 0) -> output 0`, in an offline stereo session whose
/// block is the plugin's pipeline chunk. The plugin is put in `mode` before
/// the first commit, so its unit starts steady in it (no fade).
fn rig(mode: NodeMode) -> Rig {
    set_server();
    let mut s = Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(BLOCK)))
        .expect("an offline session builds");
    let plugin = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest::new(PluginRef::file(clap_probe()).expect("a plugin file")),
        )
        .expect("the reference CLAP plugin loads");
    let g = s.graph();
    let (click, _) = g.insert(ForkByClone(Click::default()));
    g.set_sources(InPort::new(plugin, 0), [OutPort::new(click, 0)])
        .unwrap();
    g.set_sources(Root::output(0), [OutPort::new(plugin, 0)])
        .unwrap();
    g.set_mode(plugin, mode).expect("the plugin node is there");
    let _ = s.update();
    Rig { s, plugin }
}

impl Rig {
    /// Renders `blocks` blocks and returns output channel 0, concatenated.
    fn render(&mut self, blocks: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(blocks * BLOCK);
        let mut block = vec![f32::NAN; BLOCK * 2];
        for _ in 0..blocks {
            self.s
                .render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
            out.extend(block.iter().step_by(2));
        }
        out
    }

    /// The blocks the plugin's server has answered.
    fn replies(&self) -> u64 {
        self.s
            .plugin(self.plugin)
            .expect("hosted")
            .handle()
            .block_counts()
            .replies
    }
}

/// **A bypassed plugin passes its input delayed by its declared latency, bit
/// for bit.** `L` is the graph's figure for the node (the plugin's latency and
/// the pipeline's chunk, so at least [`BLOCK`]); the click comes out at
/// `FIRE + L` as `1.0` and every other frame is `+0.0`, the first `L` being
/// what the bypass line held (silence at start).
#[test]
fn a_bypassed_plugin_passes_a_click_its_declared_latency_late() {
    let mut r = rig(NodeMode::Bypassed);
    let latency = r.s.graph_ref().node_latency(r.plugin).0;
    assert!(
        latency >= BLOCK,
        "a served plugin's node declares at least its pipeline chunk, got {latency}"
    );

    let out = r.render((FIRE + latency) / BLOCK + 2);
    for (frame, &x) in out.iter().enumerate() {
        let want: f32 = if frame == FIRE + latency { 1.0 } else { 0.0 };
        assert_eq!(
            x.to_bits(),
            want.to_bits(),
            "frame {frame}: {x} (the click is due at {}, L = {latency})",
            FIRE + latency
        );
    }
}

/// **A bypassed plugin is parked: its server is not called.** Bypassed from
/// the start (so no fade), its server answers no block; made active again, it
/// is reset, called and heard (its tag is not silence).
#[test]
fn a_bypassed_plugin_is_not_called_and_comes_back() {
    let mut r = rig(NodeMode::Bypassed);
    // Paced at the device's rate, so a server that was called has had time to
    // answer: the count not moving is the node not calling it, not a late reply.
    let block_time = Duration::from_secs_f64(BLOCK as f64 / RATE.0);
    let before = r.replies();
    for _ in 0..8 {
        std::thread::sleep(block_time);
        let _ = r.render(1);
    }
    assert_eq!(
        r.replies(),
        before,
        "the server answered blocks while its node was bypassed"
    );

    r.s.graph()
        .set_mode(r.plugin, NodeMode::Active)
        .expect("the plugin node is there");
    let _ = r.s.update();
    // The served pipeline is realtime here: a block whose reply is late plays
    // silence and is not counted. So the blocks stay paced and the plugin is
    // waited for (reset, the warm-up of `L` frames, the 256-frame fade, then
    // the plugin alone), up to a deadline that only a plugin that never comes
    // back reaches.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(block_time);
        let out = r.render(1);
        if r.replies() > before && out.iter().any(|&x| x != 0.0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the plugin was not heard (replies {} from {before}) 10 s after it \
             was made active",
            r.replies()
        );
    }
}
