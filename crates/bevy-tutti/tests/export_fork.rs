//! What an export renders, through the whole adapter: a request spawned in the
//! ECS, rendered on the task pool, reported on its entity.
//!
//! On `GraphBackend::Native` an export forks the live graph (`Editor::fork`,
//! design doc 013 PR 12); on `Net` it clones the net. Where both apply, a
//! test runs on both and they are held to the same answer; where only the
//! fork has the property (it shares nothing with the live graph, it names a
//! node it cannot copy, it forks a hosted plugin by state transfer), the test
//! is native-only and says why. `export_surface.rs` pins the request/response
//! shape; this file pins the audio.

#![cfg(all(feature = "export", feature = "wav"))]

#[macro_use]
mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tutti::export::RenderGraph;
use bevy_tutti::export::{
    ExportClock, ExportDone, ExportError, ExportNode, ExportOutput, ExportPlugin, ExportRequest,
    ExportSource, ExportTarget,
};
use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphBackend, GraphSource};
use tutti_core::{AudioUnit, BufferMut, BufferRef, Hz, SignalFrame, Tail, Q};
use tutti_export::{
    AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, RenderConfig,
};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_types::{SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn config(seconds: f64) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(RATE),
            duration_seconds: seconds,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Float32,
            channels: ChannelLayout::STEREO,
        },
        ..Default::default()
    }
}

/// An app with the export plugin and a ready engine over `graph`.
fn app_over(graph: AudioGraphRes) -> App {
    let mut app = App::new();
    app.add_plugins((bevy_app::TaskPoolPlugin::default(), ExportPlugin));
    app.insert_resource(graph);
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(RATE),
        channels: ChannelLayout::STEREO,
    });
    app.insert_resource(bevy_tutti::AudioEngineState::Running);
    app
}

/// A graph on `backend` at the export's rate.
fn graph_on(backend: GraphBackend) -> AudioGraphRes {
    let mut graph = AudioGraphRes::headless_with(backend, 0, 2);
    graph.set_sample_rate(SampleRate(RATE));
    graph
}

/// What an export came back with, owned so an observer can hand it out.
///
/// Some fields are only ever read through `Debug`, in a failing test's panic
/// message, and `ForkFailed` only by the plugin tests.
#[allow(dead_code)]
#[derive(Debug)]
enum Got {
    Planes(Vec<Vec<f32>>),
    NotForkable(ExportNode),
    ForkSource(ExportNode, bevy_tutti::export::ForkCause),
    ForkFailed(ExportNode, tutti_graph::ForkFaultKind),
    Other(String),
}

impl Got {
    fn planes(self) -> Vec<Vec<f32>> {
        match self {
            Got::Planes(p) => p,
            other => panic!("expected rendered buffers, got {other:?}"),
        }
    }
}

/// Spawn `request`, tick until it reports, and return what it reported.
fn export(app: &mut App, request: ExportRequest) -> Got {
    let slot: Arc<Mutex<Option<Got>>> = Arc::default();
    let seen = Arc::clone(&slot);
    app.world_mut()
        .spawn(request)
        .observe(move |done: On<ExportDone>| {
            *seen.lock().unwrap() = Some(match &done.result {
                Ok(ExportOutput::Buffers(r)) => Got::Planes(r.planes.clone()),
                Ok(other) => Got::Other(format!("{other:?}")),
                Err(ExportError::NotForkable { node }) => Got::NotForkable(node.clone()),
                Err(ExportError::ForkSource { node, cause }) => {
                    Got::ForkSource(node.clone(), cause.clone())
                }
                Err(ExportError::ForkFailed { node, kind, .. }) => {
                    Got::ForkFailed(node.clone(), *kind)
                }
                Err(e) => Got::Other(e.to_string()),
            });
        });
    for _ in 0..6000 {
        app.update();
        if let Some(got) = slot.lock().unwrap().take() {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the export never reported");
}

fn buffers(source: ExportSource, seconds: f64) -> ExportRequest {
    ExportRequest::new(
        source,
        ExportTarget::Buffers,
        config(seconds),
        ExportClock::frozen(),
    )
}

// ---------------------------------------------------------------------------
// Both backends
// ---------------------------------------------------------------------------

/// A saw through a low-pass on channel 0, the raw saw on channel 1: a master
/// that differs from a node export of the filter on every channel, so the
/// two cannot pass for each other.
fn chain(app: &mut App) -> Entity {
    let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
    let osc = graph.insert(Osc::saw(Hz(110.0)));
    let filter = graph.insert(SvfFilterNode::<f64>::new(
        SvfType::LowPass,
        Hz(800.0),
        Q(1.0),
    ));
    graph.set_source(filter, 0, GraphSource::Node(osc, 0));
    graph.set_output_source(0, GraphSource::Node(filter, 0));
    graph.set_output_source(1, GraphSource::Node(osc, 0));
    app.world_mut().spawn(osc);
    app.world_mut().spawn(filter).id()
}

/// **A native export renders what a `Net` export renders, bit for bit**, for
/// the master and for one node, on a graph whose live side has not run.
///
/// "Where the rules allow" (doc 013, PR 12): a `Net` master export is a plain
/// clone that copies the running state (an oscillator's phase, a filter's
/// memory), and a fork starts reset. With the live side never rendered the
/// two coincide, so this is the like-for-like pair; on a graph that has been
/// playing, a `Net` master export continues from the live state and a fork
/// does not.
///
/// Mutation (run): the native arm of `AudioGraphRes::export` forking
/// `ForkTarget::Master` for a node export → the node's render is the
/// master's, and the node comparison fails on channel 1 (the saw, not the
/// filter).
#[test]
fn native_and_net_exports_are_bit_identical() {
    let render = |backend: GraphBackend| {
        let mut app = app_over(graph_on(backend));
        let filter = chain(&mut app);
        let master = export(&mut app, buffers(ExportSource::Master, 0.25)).planes();
        let node = export(&mut app, buffers(ExportSource::Node(filter), 0.25)).planes();
        (master, node)
    };
    let (net_master, net_node) = render(GraphBackend::Net);
    let (native_master, native_node) = render(GraphBackend::Native);
    for (what, net, native) in [
        ("master", &net_master, &native_master),
        ("node", &net_node, &native_node),
    ] {
        assert_eq!(net.len(), native.len(), "{what}: width");
        let peak = net[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "{what}: silent");
        for (c, (a, b)) in net.iter().zip(native).enumerate() {
            assert_eq!(a.len(), b.len(), "{what}: length of channel {c}");
            if let Some(i) = a
                .iter()
                .zip(b)
                .position(|(x, y)| x.to_bits() != y.to_bits())
            {
                panic!(
                    "{what}: channel {c} parts at frame {i}: net {} native {}",
                    a[i], b[i]
                );
            }
        }
    }
    // And the node export is the node: both channels are the filter (a mono
    // node clamps across a stereo root), which is the master's channel 0.
    assert_eq!(native_node[0], native_master[0]);
    assert_eq!(native_node[1], native_master[0]);
    assert_ne!(native_master[1], native_master[0]);
}

/// A source that steps from 0 to 1 at frame `LATE` and declares `LATE`
/// frames of latency, with a finite tail of `TAIL` frames: a look-ahead
/// processor reduced to its bookkeeping.
#[derive(Clone)]
struct Late {
    pos: usize,
}

const LATE: usize = 100;
const TAIL: usize = 300;

impl AudioUnit for Late {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn reset(&mut self) {
        self.pos = 0;
    }
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = if self.pos >= LATE { 1.0 } else { 0.0 };
        self.pos += 1;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, if self.pos >= LATE { 1.0 } else { 0.0 });
            self.pos += 1;
        }
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, tutti_core::Signal::Latency(LATE as f64));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::Finite(Samples(TAIL))
    }
    fn get_id(&self) -> u64 {
        0x1a7e
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// **`trim_reported_latency` and `with_reported_tail` take the figures from
/// the graph that is rendered**: the step a `LATE`-frame source makes at its
/// declared latency lands on frame 0, and the render runs `TAIL` frames
/// past its duration. Asked of the native plan on `Native`, of the net on
/// `Net`.
///
/// The tail is resolved by `GraphTail::resolve`: a finite tail under the cap
/// is rendered whole, and a graph whose only node never said (`Tail::Unknown`,
/// every `AudioUnit`'s default) renders none — not the cap, which would
/// append silence.
///
/// Mutation (run): `start_exports` ignoring `latency_from_graph` → frame 0
/// reads 0 on both; ignoring `tail_from_graph` → the render is `TAIL`
/// frames short on both; resolving the tail as `samples().unwrap_or(cap)`
/// → the unknown graph renders `cap` extra frames on both.
fn latency_and_tail_come_from_the_graph(backend: GraphBackend) {
    let mut app = app_over(graph_on(backend));
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let late = graph.insert(Late { pos: 0 });
        graph.set_outputs_from(late);
    }
    let seconds = 0.01;
    let frames = (seconds * RATE) as usize;
    let planes = export(
        &mut app,
        buffers(ExportSource::Master, seconds)
            .trim_reported_latency()
            .with_reported_tail(Samples(48_000)),
    )
    .planes();
    assert_eq!(planes[0].len(), frames + TAIL, "{backend:?}: the tail");
    assert_eq!(
        planes[0][0], 1.0,
        "{backend:?}: the latency was not trimmed"
    );
    assert!(planes[0].iter().all(|&s| s == 1.0), "{backend:?}");

    let mut app = app_over(graph_on(backend));
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let unknown = graph.insert(Counter {
            n: Arc::new(AtomicU64::new(0)),
        });
        graph.set_outputs_from(unknown);
    }
    let planes = export(
        &mut app,
        buffers(ExportSource::Master, seconds).with_reported_tail(Samples(4_800)),
    )
    .planes();
    assert_eq!(
        planes[0].len(),
        frames,
        "{backend:?}: an unknown tail renders none, not the cap"
    );
}
both_backends!(latency_and_tail_come_from_the_graph);

/// **The latency trimmed is the latency of the graph rendered — after the
/// `prepare` hook.** The live graph is a latency-free constant; the hook
/// puts a `LATE`-frame source on the output. Trimmed by what the hook left,
/// the source's step lands on frame 0; by the live graph's figure (0), it
/// would land on frame `LATE`.
///
/// Mutation (run): not applying the fork's committed hook edit before the
/// figures are read (`executor.apply_pending()` after the hook's commit, in
/// `start_exports`, a no-op) → the fork's plan is still the live graph's
/// and `native` trims nothing. Mutation (run): reading the latency before
/// the hook runs → both fail.
fn the_trim_is_read_after_the_prepare_hook(backend: GraphBackend) {
    let mut app = app_over(graph_on(backend));
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let dc = graph.insert(tutti_nodes::testing::Const::mono(0.5));
        graph.set_outputs_from(dc);
    }
    let request = buffers(ExportSource::Master, 0.01)
        .trim_reported_latency()
        .with_prepare(|prepared, _world| {
            let key = prepared.fresh_key();
            match prepared.graph {
                RenderGraph::Net(net) => {
                    net.master(Late { pos: 0 });
                }
                RenderGraph::Graph { editor, .. } => {
                    editor.insert(key, "test:late", tutti_graph::Legacy::new(Late { pos: 0 }));
                    for out in editor.spec_mut().topology.outputs.iter_mut() {
                        *out = tutti_types::graph::Source::Node(tutti_types::graph::OutPort {
                            node: key,
                            port: 0,
                        });
                    }
                }
            }
        });
    let planes = export(&mut app, request).planes();
    assert_eq!(
        planes[0][0], 1.0,
        "{backend:?}: trimmed by the live graph's latency, not the rendered one's"
    );
}
both_backends!(the_trim_is_read_after_the_prepare_hook);

/// **An export of a graph with no outputs says so**, for the master and for
/// a node — not that the node has none.
///
/// Mutation (run): dropping the `outputs() == 0` check in
/// `AudioGraphRes::export` → the node export reports its node, the master
/// renders nothing and reports success.
fn a_graph_with_no_outputs_says_so(backend: GraphBackend) {
    let mut graph = AudioGraphRes::headless_with(backend, 0, 0);
    graph.set_sample_rate(SampleRate(RATE));
    let mut app = app_over(graph);
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.insert(Osc::sine(Hz(220.0)))
    };
    let entity = app.world_mut().spawn(node).id();
    for source in [ExportSource::Master, ExportSource::Node(entity)] {
        match export(&mut app, buffers(source, 0.01)) {
            Got::Other(why) => assert!(
                why.contains("the graph has no outputs"),
                "{backend:?}, {source:?}: {why}"
            ),
            other => panic!("{backend:?}, {source:?}: expected a refusal, got {other:?}"),
        }
    }
}
both_backends!(a_graph_with_no_outputs_says_so);

/// **A node export on a 90 BPM timeline plays where 90 BPM puts it.** A
/// sampler voice placed at beat 3 on the live transport, exported on its own
/// against an offline timeline at 90 BPM from beat 0: at 48 kHz beat 3 is
/// frame 96 000 (at the default 120 BPM it would be 72 000). Silent before,
/// the clip's own samples from there.
///
/// Mutation (run): `ExportClock::offline` answering the stopped timeline
/// for a named one → the voice never sounds, on both backends. Mutation
/// (run): `Stopped::is_rolling` answering `true` → the frozen export of the
/// voice at beat 0 sounds.
#[cfg(feature = "sampler")]
fn a_node_export_follows_a_90_bpm_timeline(backend: GraphBackend) {
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_sampler::MemorySource;
    let tone = |i: usize| (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin();

    let mut app = app_over(graph_on(backend));
    let live = tutti_core::transport::Transport::new(SampleRate(RATE));
    let mut wave = tutti_io::Wave::new(1, RATE);
    for i in 0..RATE as usize {
        wave.push_frame(&[tone(i)]);
    }
    let voice = {
        let source = MemorySource::with_transport(
            Arc::new(wave),
            Arc::new(live) as Arc<dyn tutti_core::Timeline>,
            tutti_core::Beat(3.0),
            None,
        );
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let node = graph.insert(source);
        graph.set_outputs_from(node);
        app.world_mut().spawn(node).id()
    };
    let zero = {
        let mut wave = tutti_io::Wave::new(1, RATE);
        for i in 0..RATE as usize {
            wave.push_frame(&[tone(i)]);
        }
        let source = MemorySource::with_transport(
            Arc::new(wave),
            Arc::new(tutti_core::transport::Transport::new(SampleRate(RATE)))
                as Arc<dyn tutti_core::Timeline>,
            tutti_core::Beat(0.0),
            None,
        );
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let node = graph.insert(source);
        app.world_mut().spawn(node).id()
    };

    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_core::Beat(0.0),
        tempo: tutti_core::Bpm(90.0),
        sample_rate: SampleRate(RATE),
        loop_range: None,
    }));
    let planes = export(
        &mut app,
        ExportRequest::new(
            ExportSource::Node(voice),
            ExportTarget::Buffers,
            config(2.5),
            ExportClock::timeline(timeline.clone()),
        ),
    )
    .planes();
    // Beat 3 at 90 BPM is frame 96 000.
    const AT: usize = 96_000;
    assert!(
        planes[0][..AT].iter().all(|&s| s == 0.0),
        "{backend:?}: the voice sounded before beat 3"
    );
    // From there the clip's frame `k` is the render's frame `AT + k`, from
    // the first: the voice reads the clock once per 64-frame chunk
    // (`Legacy`), and the chunk starting on beat 3 reads exactly beat 3,
    // since the timeline counts frames and derives the beat (doc 013 §6).
    // (It accumulated once, read a hair under 3 there, and the voice entered
    // a chunk late, at 96 064.) Both backends, since doc 013's chunk-major
    // mode moves the native clock as `Net`'s moves.
    for k in 0..4_000 {
        let got = planes[0][AT + k];
        assert!(
            (got - tone(k)).abs() < 1e-3,
            "{backend:?}: frame {k} of the clip read {got}, want {}",
            tone(k)
        );
    }

    // A frozen clock rebinds the voice onto a timeline stopped at beat 0: it
    // plays nothing, rather than loop its first block against a rolling
    // playhead nothing advances (what a default timeline did before
    // `ExportClock`). Placed at beat 0 here, where a rolling one would sound.
    let planes = export(
        &mut app,
        ExportRequest::new(
            ExportSource::Node(zero),
            ExportTarget::Buffers,
            config(0.1),
            ExportClock::frozen(),
        ),
    )
    .planes();
    assert!(
        planes[0].iter().all(|&s| s == 0.0),
        "{backend:?}: a frozen export played the voice"
    );
}
#[cfg(feature = "sampler")]
both_backends!(a_node_export_follows_a_90_bpm_timeline);

// ---------------------------------------------------------------------------
// Native only: what a fork has and a clone does not
// ---------------------------------------------------------------------------

/// A source that counts frames in a cell its clones **share**, and that its
/// `isolate` gives a fresh copy of: a unit whose live state a copy could
/// move. Its output is the count, so a render that touched the live cell
/// shows as a jump in the live signal.
#[derive(Clone)]
struct Counter {
    n: Arc<AtomicU64>,
}

impl AudioUnit for Counter {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn reset(&mut self) {
        self.n.store(0, Ordering::Relaxed);
    }
    fn isolate(&mut self) {
        self.n = Arc::new(AtomicU64::new(self.n.load(Ordering::Relaxed)));
    }
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.n.fetch_add(1, Ordering::Relaxed) as f32;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, self.n.fetch_add(1, Ordering::Relaxed) as f32);
        }
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }
    fn get_id(&self) -> u64 {
        0xc0c0
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// **Live playback is untouched by an export rendering beside it.** The live
/// graph's audio side renders on its own thread, block after block, from
/// before the export starts until after it reports; the export renders a
/// fork of the same graph on the task pool meanwhile. The live signal counts
/// on without a jump, and the export counts from 0 (a fork is reset) — the
/// two never shared the counter.
///
/// Native only: a `Net` master export is a plain clone that shares the
/// counter with the live net (doc 013, PR 12's behaviour change), and this
/// test would fail there by design.
///
/// Mutation (run): `Boxed::isolate` not forwarding to the unit (so the shadow
/// a fork is cloned from shares the live cell) → the fork's reset and render
/// move the live count, which jumps.
#[test]
fn live_playback_continues_unaffected_while_an_export_renders() {
    let mut graph = graph_on(GraphBackend::Native);
    let node = graph.insert(Counter {
        n: Arc::new(AtomicU64::new(0)),
    });
    graph.set_outputs_from(node);
    // Commit on this side, then hand the audio side to the "device".
    graph.render_frame(&mut [0.0, 0.0]);
    let mut live = graph.take_audio_side();
    let mut app = app_over(graph);

    let done = AtomicBool::new(false);
    let (heard, got) = std::thread::scope(|s| {
        let device = s.spawn(|| {
            let mut heard = Vec::new();
            let mut block = vec![Vec::new(), Vec::new()];
            // At least a few blocks after the export reports, so the two
            // overlap from end to end.
            let mut after = 0;
            while after < 8 {
                live.render(256, 256, &mut block);
                heard.extend_from_slice(&block[0]);
                if done.load(Ordering::Acquire) {
                    after += 1;
                }
            }
            heard
        });
        let got = export(&mut app, buffers(ExportSource::Master, 1.0));
        done.store(true, Ordering::Release);
        (device.join().expect("the live side"), got)
    });

    let rendered = got.planes();
    for (i, &s) in rendered[0].iter().enumerate() {
        assert_eq!(s, i as f32, "the export's count at frame {i}");
    }
    let first = heard[0];
    for (i, &s) in heard.iter().enumerate() {
        assert_eq!(
            s,
            first + i as f32,
            "the live count jumped at frame {i} while the export ran"
        );
    }
}

/// A source whose `isolate` it will not vouch for, as a microphone monitor
/// or a disk voice declares: `forkable() == false`.
#[derive(Clone)]
struct Unforkable;

impl AudioUnit for Unforkable {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn forkable(&self) -> bool {
        false
    }
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = 0.5;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        output.channel_f32_mut(0)[..size].fill(0.5);
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }
    fn get_id(&self) -> u64 {
        0x0f0f
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// **A master export forks what the outputs hear**: an unforkable node no
/// output reaches (an unrouted mic) does not refuse it, and the render is
/// the routed node's.
///
/// Mutation (run): `ForkTarget::Master` forking every node in the spec →
/// refused as not forkable, naming the mic.
#[test]
fn an_unrouted_unforkable_node_does_not_refuse_a_master_export() {
    let mut app = app_over(graph_on(GraphBackend::Native));
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let _mic = graph.insert(Unforkable);
        let dc = graph.insert(tutti_nodes::testing::Const::mono(0.25));
        graph.set_outputs_from(dc);
    }
    let planes = export(&mut app, buffers(ExportSource::Master, 0.01)).planes();
    assert!(planes[0].iter().all(|&s| s == 0.25));
}

/// **A node that cannot be forked refuses the export by its entity and
/// name**, not by a graph key a host cannot map back; and a node export of
/// a branch it does not feed is not refused (a fork copies only the
/// sub-graph feeding the node).
///
/// Mutation (run): `NodeNames::name` passing `NotForkable` through as
/// `ExportError::Render` → the refusal names no entity.
#[test]
fn an_unforkable_node_refuses_the_export_by_name() {
    let mut app = app_over(graph_on(GraphBackend::Native));
    let (mic, osc) = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mic = graph.insert(Unforkable);
        let osc = graph.insert(Osc::sine(Hz(220.0)));
        graph.set_output_source(0, GraphSource::Node(mic, 0));
        graph.set_output_source(1, GraphSource::Node(osc, 0));
        (mic, osc)
    };
    let mic_entity = app.world_mut().spawn((mic, Name::new("Mic"))).id();
    let osc_entity = app.world_mut().spawn(osc).id();

    match export(&mut app, buffers(ExportSource::Master, 0.05)) {
        Got::NotForkable(node) => {
            assert_eq!(node.entity, Some(mic_entity));
            assert_eq!(node.name.as_deref(), Some("Mic"));
            assert!(
                ExportError::NotForkable { node }
                    .to_string()
                    .contains("\"Mic\""),
                "the message names it"
            );
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
    let planes = export(&mut app, buffers(ExportSource::Node(osc_entity), 0.05)).planes();
    assert!(planes[0].iter().any(|&s| s != 0.0), "the sibling renders");
}

// ---------------------------------------------------------------------------
// Native only: a hosted plugin, forked by state transfer
// ---------------------------------------------------------------------------

/// The reference CLAP plugin through a real `plugin-server`, exported through
/// a fork: a fresh instance in a server of its own, loaded with the live
/// one's state (tutti-plugin's `PluginClient::fork_instance`). Native only —
/// `Net` has no fork, and its master export drives the **live** plugin from
/// the render thread.
///
/// Build the server first: `cargo build -p tutti-plugin-server`.
#[cfg(feature = "plugin")]
mod plugin {
    use super::*;
    use crate::common::plugin::{clap_probe, plugin_server};

    use std::time::{Duration, Instant};

    use bevy_tutti::graph::{
        GraphReconcilePlugin, MasterSources, MetronomeRes, PortSource, PortSources, TransportRes,
    };
    use bevy_tutti::midi::{MidiSequencePlugin, MidiSourceInstall};
    use bevy_tutti::plugin_host::{PluginLoadTerminated, PluginRequest, TuttiHostingPlugin};
    use tutti_core::transport::{ClickState, OfflineTimeline, OfflineTimelineConfig, Transport};
    use tutti_core::AudioNode;
    use tutti_midi_runtime::TimedMidiEvent;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_nodes::testing::Const;
    use tutti_plugin::catalog::PluginId;

    /// The probe's render modes this file drives (`tutti-clap-test-plugin`'s
    /// `RenderMode`): `out = in + tag(port, ch)`; `out = in` delayed by the
    /// 137 frames it reports; and a note gate.
    const TAG_PASSTHROUGH: u32 = 1;
    const LATENCY: u32 = 3;
    const NOTES: u32 = 4;

    /// Set a switch the probe reads when its server starts. A fork's server
    /// starts from this process too, so a switch set after the live load
    /// reaches the fork only.
    fn probe_env(key: &str, value: impl ToString) {
        // SAFETY: nextest runs each test in its own process, and nothing in it
        // reads the environment concurrently; the probe's switches and the
        // server path are only reachable through it.
        unsafe { std::env::set_var(key, value.to_string()) };
    }

    /// An engine-less native app with the hosting and sequencing plugins, and
    /// the reference plugin loaded on an entity named "Probe" in `mode`.
    fn app_with_probe(mode: u32) -> (App, Entity) {
        app_with_probe_at(mode, clap_probe())
    }

    /// [`app_with_probe`], the plugin loaded from `path`.
    fn app_with_probe_at(mode: u32, path: std::path::PathBuf) -> (App, Entity) {
        probe_env("TUTTI_PLUGIN_SERVER", plugin_server().display());
        probe_env("TUTTI_CLAP_PROBE_RENDER_MODE", mode);
        let mut app = app_over(graph_on(GraphBackend::Native));
        app.insert_resource(TransportRes(Transport::new(RATE)));
        app.insert_resource(MetronomeRes(Arc::new(ClickState::new())));
        app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin, MidiSequencePlugin));

        let probe = app
            .world_mut()
            .spawn((
                PluginRequest {
                    id: PluginId::from_path(path),
                    sample_rate: SampleRate(RATE),
                    ..Default::default()
                },
                Name::new("Probe"),
            ))
            .id();
        app.insert_resource(MasterSources::from(probe));
        let deadline = Instant::now() + Duration::from_secs(30);
        while app.world().get::<PluginLoadTerminated>(probe).is_none() {
            assert!(Instant::now() < deadline, "the plugin never loaded");
            app.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.update();
        assert!(
            app.world().get::<AudioNode>(probe).is_some(),
            "the plugin loaded into the graph"
        );
        (app, probe)
    }

    /// Feed the probe's two main inputs a constant 0.25.
    fn feed(app: &mut App, probe: Entity) {
        feed_with(app, probe, Const::mono(0.25));
    }

    /// Feed the probe's two main inputs `unit`'s output.
    fn feed_with(app: &mut App, probe: Entity, unit: impl tutti_core::AudioUnit + 'static) {
        let dc = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(unit)
        };
        let dc = app.world_mut().spawn(dc).id();
        app.world_mut().entity_mut(probe).insert(
            PortSources::silent()
                .with(
                    0,
                    PortSource::Node {
                        entity: dc,
                        port: 0,
                    },
                )
                .with(
                    1,
                    PortSource::Node {
                        entity: dc,
                        port: 0,
                    },
                ),
        );
        app.update();
    }

    /// A ramp: frame `n` is `n / 1024`, exact in `f32` over a render, so
    /// a render one frame off reads a different value at every frame.
    #[derive(Clone)]
    struct Ramp {
        n: u32,
    }

    impl tutti_core::AudioUnit for Ramp {
        fn inputs(&self) -> usize {
            0
        }
        fn outputs(&self) -> usize {
            1
        }
        fn reset(&mut self) {
            self.n = 0;
        }
        fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
            output[0] = self.n as f32 / 1024.0;
            self.n += 1;
        }
        fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
            for i in 0..size {
                output.set_f32(0, i, self.n as f32 / 1024.0);
                self.n += 1;
            }
        }
        fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
            SignalFrame::new(1)
        }
        fn tail(&mut self) -> Tail {
            Tail::None
        }
        fn get_id(&self) -> u64 {
            0x4a3b
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    /// **A graph holding a hosted plugin as an effect exports**, rendered by
    /// a fork of the plugin, and the trim is the delay it applies. In its
    /// latency mode the probe delays its input by the 137 frames it reports;
    /// the node adds its 64-frame pipeline block to that, and the export
    /// trims the plan's 201. Fed a ramp, the render is the ramp from frame 0,
    /// sample for sample: a trim one frame off in either direction reads the
    /// neighbouring frame's value everywhere. (A constant input could not
    /// tell a trim of 64 from one of 201.)
    ///
    /// Mutation (run): `plugin_load_promote` inserting the plugin boxed
    /// (`insert_boxed(plugin.into_unit())`, the path before PR 12) → the
    /// export is refused as not forkable, naming "Probe". Mutation (run):
    /// trimming one frame less than the plan's latency
    /// (`reported_latency() - 1` in `start_exports`) → every frame reads its
    /// predecessor.
    #[test]
    fn a_plugin_effect_exports_through_its_fork() {
        let (mut app, probe) = app_with_probe(LATENCY);
        feed_with(&mut app, probe, Ramp { n: 0 });
        let planes = match export(
            &mut app,
            buffers(ExportSource::Master, 0.1).trim_reported_latency(),
        ) {
            Got::Planes(p) => p,
            other => panic!("the export failed: {other:?}"),
        };
        for (c, plane) in planes.iter().enumerate().take(2) {
            for (i, &got) in plane.iter().enumerate() {
                let want = i as f32 / 1024.0;
                assert_eq!(got, want, "channel {c}: frame {i}");
            }
        }
    }

    /// A MIDI source that is not a function of a timeline: it cannot be
    /// carried into an offline render (as a live sequencer, or a snapshot
    /// reader bound to another render, cannot).
    struct Unrebindable;

    impl tutti_midi_types::MidiUnitIn for Unrebindable {
        fn poll_unit(
            &self,
            _unit: tutti_midi_types::MidiUnitId,
            _block: usize,
            _rate: SampleRate,
            _buffer: &mut [MidiEvent],
        ) -> usize {
            0
        }
        fn rebind_offline(
            &self,
            _unit: tutti_midi_types::MidiUnitId,
            _ctx: &dyn std::any::Any,
        ) -> Option<Arc<dyn tutti_midi_types::MidiUnitIn>> {
            None
        }
    }

    /// **A plugin fork that cannot be built fails the export by the
    /// plugin's entity and name**, before anything renders
    /// (`ExportError::ForkSource`), with the plugin's own reason:
    ///
    /// - the plugin plays a MIDI source that cannot be rebound for an
    ///   offline render — its notes would render as silence
    ///   (`PluginForkError::MidiSource`);
    /// - the plugin's file is gone since it loaded, so no fresh instance
    ///   loads (`PluginForkError::Load`).
    ///
    /// Mutation (run): `PluginFork::instance` ignoring a `NotRebindable`
    /// answer → the first export renders. Mutation (run): `NodeNames::name`
    /// passing `ForkSource` through as `ExportError::Render` → no entity
    /// is named.
    #[test]
    fn a_plugin_fork_that_cannot_be_built_is_a_named_failure() {
        use tutti_plugin::PluginForkError;
        // The cause, once the failure is checked to name the plugin.
        let named = |got: Got, what: &str| match got {
            Got::ForkSource(node, cause) => {
                assert_eq!(node.name.as_deref(), Some("Probe"), "{what}");
                cause
            }
            other => panic!("{what}: expected a named fork-source failure, got {other:?}"),
        };

        let (mut app, probe) = app_with_probe(TAG_PASSTHROUGH);
        app.world()
            .get::<bevy_tutti::midi::MidiTarget>(probe)
            .expect("the plugin's MIDI port was captured")
            .port()
            .install(Arc::new(Unrebindable));
        let cause = named(
            export(&mut app, buffers(ExportSource::Master, 0.05)),
            "an unrebindable MIDI source",
        );
        assert!(
            matches!(
                cause.downcast_ref::<PluginForkError>(),
                Some(PluginForkError::MidiSource)
            ),
            "{cause:?}"
        );

        // A link of our own to the plugin, so removing it touches no other
        // test's (the suites publish one shared `.clap` link).
        let shared = clap_probe();
        let own = shared.with_extension(format!("{}.clap", std::process::id()));
        let _ = std::fs::remove_file(&own);
        let real = std::fs::canonicalize(&shared).expect("the reference plugin");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &own).expect("link the plugin");
        #[cfg(windows)]
        std::fs::copy(&real, &own).expect("copy the plugin");
        let (mut app, _probe) = app_with_probe_at(TAG_PASSTHROUGH, own.clone());
        std::fs::remove_file(&own).expect("remove the plugin's file");
        let cause = named(
            export(&mut app, buffers(ExportSource::Master, 0.05)),
            "a plugin file gone since it loaded",
        );
        assert!(
            matches!(
                cause.downcast_ref::<PluginForkError>(),
                Some(PluginForkError::Load { path, .. }) if *path == own
            ),
            "{cause:?}"
        );
    }

    /// **An exported plugin instrument plays its notes.** A fork has a fresh
    /// MIDI port, so the clip the live instance plays (`MidiSourceInstall`,
    /// installed on its port) is rebound onto the fork's port and the render's
    /// timeline. A note from beat 1 to beat 2 holds the probe's gate open
    /// from beat 1 to beat 2 of the render's timeline, heard one pipeline
    /// block (64 frames) later, and nowhere else.
    ///
    /// At 90 BPM and 48 kHz a beat is 32 000 frames, a per-frame step binary
    /// cannot hold, and the edges are asserted to the frame: 32 000 and
    /// 64 000 (at the default 120 BPM, 24 000 and 48 000). The timeline
    /// counts frames and derives the beat, and the clip places by frame
    /// (doc 013 §6); when the timeline accumulated, the note landed a frame
    /// early (measured: 32 063 for 32 064), and this test ran at 87.890625
    /// BPM, where a beat is exactly 2^15 frames, to dodge it.
    ///
    /// Not trimmed: the probe reports 137 frames of latency in every mode
    /// but delays only in its latency mode, so the graph's figure is not
    /// this render's delay.
    ///
    /// Rendered at the device's 48 kHz and at 96 kHz: the fork is launched at
    /// the live rate and prepared at the render's, and the clip places its
    /// notes at the rate the fork polls it at (a beat is 64 000 frames
    /// there).
    ///
    /// Mutation (run): dropping the `rebind_offline_into` call in
    /// tutti-plugin's `PluginFork::instance` → the fork renders silence, at
    /// both rates. The plugin polling its MIDI port at a fixed 48 kHz instead
    /// of its own rate (`build_block_payload`) → at 96 kHz the notes land
    /// where 48 kHz puts them.
    #[test]
    fn an_exported_plugin_instrument_plays_its_clip() {
        for rate in [RATE, 2.0 * RATE] {
            instrument_at(rate);
        }
    }

    fn instrument_at(rate: f64) {
        let (mut app, probe) = app_with_probe(NOTES);
        let note = |beat: f64, on: bool| {
            let event = if on {
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
            } else {
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)
            };
            TimedMidiEvent::new(tutti_core::Beat(beat), event)
        };
        app.world_mut().spawn(MidiSourceInstall::new(
            probe,
            vec![note(1.0, true), note(2.0, false)],
        ));
        app.update();

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: tutti_core::Beat(0.0),
            tempo: tutti_core::Bpm(90.0),
            sample_rate: SampleRate(rate),
            loop_range: None,
        }));
        let planes = match export(
            &mut app,
            ExportRequest::new(
                ExportSource::Node(probe),
                ExportTarget::Buffers,
                ExportConfig {
                    render: RenderConfig {
                        sample_rate: SampleRate(rate),
                        ..config(1.5).render
                    },
                    ..config(1.5)
                },
                ExportClock::timeline(timeline.clone()),
            ),
        ) {
            Got::Planes(p) => p,
            other => panic!("the export failed: {other:?}"),
        };
        let open: Vec<usize> = planes[0]
            .iter()
            .enumerate()
            .filter(|(_, &s)| s != 0.0)
            .map(|(i, _)| i)
            .collect();
        assert!(
            !open.is_empty(),
            "{rate} Hz: the instrument rendered no notes"
        );
        // The plugin's batcher holds one block: what it is sent in block `k`
        // it returns in block `k + 1`.
        const PIPELINE: usize = 64;
        let beat = 32_000 * (rate / RATE) as usize;
        let (on, off) = (open[0], *open.last().unwrap() + 1);
        assert_eq!(
            (on, off),
            (beat + PIPELINE, 2 * beat + PIPELINE),
            "{rate} Hz: the gate is open from beat 1 to beat 2, {PIPELINE} frames late"
        );
        assert_eq!(open.len(), off - on, "one unbroken note");
        assert!(planes[0][on] > 0.9, "the note's velocity");
    }

    /// **A plugin fork that dies mid-export fails the export by the plugin's
    /// entity and name**, promptly, instead of writing the silence it
    /// rendered after the crash. The crash switch is set after the live
    /// instance loaded, so only the fork's server reads it; it aborts on its
    /// 8th block.
    ///
    /// Mutation (run): `NodeNames::name` passing `ForkFailed` through as
    /// `ExportError::Render` → the failure names no entity.
    #[test]
    fn a_plugin_fork_that_crashes_mid_export_is_a_named_failure() {
        let (mut app, probe) = app_with_probe(TAG_PASSTHROUGH);
        feed(&mut app, probe);
        probe_env("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", 8);
        let started = Instant::now();
        match export(&mut app, buffers(ExportSource::Master, 2.0)) {
            Got::ForkFailed(node, kind) => {
                assert_eq!(node.entity, Some(probe));
                assert_eq!(node.name.as_deref(), Some("Probe"));
                assert_eq!(kind, tutti_graph::ForkFaultKind::Crashed);
            }
            other => panic!("expected a named fork failure, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the failure took {:?}",
            started.elapsed()
        );
    }
}

/// Built-in synths — `PolySynth` and `SoundFontUnit` — exported through a
/// fork with the clip a `MidiSourceInstall` put on the live synth's port.
///
/// A fork's synth used to be cloned from its `Legacy::controlled` shadow,
/// whose MIDI port was severed when the node was inserted — before any clip
/// was installed — so the export rendered silence. Each synth now brings its
/// own fork source (`PolySynth::fork_source`, `SoundFontUnit::fork_source`),
/// which `graph::native` hands the editor at insert, and a fork rebinds the
/// live port's clip onto the render's timeline.
///
/// Every tempo here is 90 BPM: a beat is 32 000 frames at 48 kHz (64 000 at
/// 96 kHz), placed to the frame since clips place on integer frames (#40).
///
/// Native only but for the A/B: a `Net` export has no fork, and its synth
/// plays no clip unless the host refills one (the A/B does, to compare).
#[cfg(all(feature = "midi", feature = "synth", feature = "soundfont"))]
mod synths {
    use super::*;

    use bevy_tutti::graph::{GraphReconcilePlugin, MasterSources, SpawnAudioNode, TransportRes};
    use bevy_tutti::midi::{MidiSequencePlugin, MidiSourceInstall, MidiTarget, MidiTargetRegistry};
    use tutti_core::transport::{MotionEvent, OfflineTimeline, OfflineTimelineConfig, Transport};
    use tutti_core::{AudioNode, Beat, Bpm, BufferVec, Seconds, MAX_BUFFER_SIZE};
    use tutti_midi_runtime::{MidiClipSource, OfflineRebind, TimedMidiEvent};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_polysynth::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig};
    use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

    /// A beat at 90 BPM and 48 kHz, in frames.
    const BEAT_48K: usize = 32_000;
    /// Frames of the note compared against a reference render.
    const NOTE: usize = 4_096;
    /// A preset of the test soundfont that is not the default piano, so a
    /// fork that lost the live unit's preset is heard.
    const PRESET: i32 = 24;

    /// A saw with an instant attack, built at the live graph's rate.
    fn poly() -> PolySynth {
        PolySynth::new(SynthConfig {
            sample_rate: SampleRate(RATE),
            oscillator: OscillatorType::Saw,
            envelope: EnvelopeConfig {
                attack: Seconds(0.0),
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("the synth builds")
    }

    /// The repo's committed test soundfont (as `midi_soundfont.rs` finds it).
    fn font() -> Arc<SoundFont> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("bevy-tutti lives two levels below the repo root")
            .join("assets/soundfonts/TimGM6mb.sf2");
        let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
            panic!(
                "committed test soundfont missing at {}: {e}",
                path.display()
            )
        });
        Arc::new(SoundFont::new(&mut file).expect("the test soundfont parses"))
    }

    /// A SoundFont unit at `rate`, on [`PRESET`].
    fn sf2(font: &Arc<SoundFont>, rate: f64) -> SoundFontUnit {
        let mut settings = SynthesizerSettings::new(rate as i32);
        settings.enable_reverb_and_chorus = false;
        let mut unit = SoundFontUnit::new(Arc::clone(font), &settings).expect("the unit builds");
        unit.program_change(0, PRESET);
        unit
    }

    /// An app on `backend` with the sequencer, both synth types registered
    /// for MIDI, and `unit` spawned (`spawn_audio_node`, the path a host
    /// takes) as "Synth", routed to the master.
    fn app_with(backend: GraphBackend, unit: impl AudioUnit + 'static) -> (App, Entity) {
        let mut app = app_over(graph_on(backend));
        app.insert_resource(TransportRes(Transport::new(RATE)));
        app.add_plugins((GraphReconcilePlugin, MidiSequencePlugin));
        app.init_resource::<MidiTargetRegistry>();
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>()
            .register::<SoundFontUnit>();
        let world = app.world_mut();
        let synth = world
            .commands()
            .spawn_audio_node(unit)
            .insert(Name::new("Synth"))
            .id();
        world.commands().insert_resource(MasterSources::from(synth));
        world.flush();
        app.update();
        assert!(
            app.world().get::<MidiTarget>(synth).is_some(),
            "the synth's MIDI port was captured"
        );
        (app, synth)
    }

    /// Middle C from beat 1 to beat 2.
    fn clip() -> Vec<TimedMidiEvent> {
        vec![
            TimedMidiEvent::new(Beat(1.0), note_on()),
            TimedMidiEvent::new(
                Beat(2.0),
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
            ),
        ]
    }

    fn note_on() -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
    }

    fn install_clip(app: &mut App, synth: Entity) {
        app.world_mut().spawn(MidiSourceInstall::new(synth, clip()));
        app.update();
    }

    /// A 90 BPM timeline from beat 0 at `rate`.
    fn timeline(rate: f64) -> Arc<OfflineTimeline> {
        Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(90.0),
            sample_rate: SampleRate(rate),
            loop_range: None,
        }))
    }

    /// Export `source` for 0.8 s at `rate` against [`timeline`].
    fn request(source: ExportSource, rate: f64) -> ExportRequest {
        ExportRequest::new(
            source,
            ExportTarget::Buffers,
            ExportConfig {
                render: RenderConfig {
                    sample_rate: SampleRate(rate),
                    ..config(0.8).render
                },
                ..config(0.8)
            },
            ExportClock::timeline(timeline(rate)),
        )
    }

    /// `unit` fed a note-on at frame 0 through `queue`, rendered in 64-frame
    /// blocks: what the note sounds like from its first frame, at the unit's
    /// rate.
    fn reference<U: AudioUnit>(mut unit: U, queue: impl FnOnce(&U)) -> Vec<f32> {
        queue(&unit);
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut out = Vec::with_capacity(NOTE);
        while out.len() < NOTE {
            unit.process(
                MAX_BUFFER_SIZE,
                &input.buffer_ref(),
                &mut output.buffer_mut(),
            );
            out.extend_from_slice(&output.buffer_ref().channel_f32(0)[..MAX_BUFFER_SIZE]);
        }
        out
    }

    /// Assert the render is silent until beat 1 and then, sample for sample,
    /// `reference` — the note at the render's rate, from its first frame.
    fn assert_note_at_beat_1(what: &str, rate: f64, planes: &[Vec<f32>], reference: &[f32]) {
        let beat = BEAT_48K * (rate / RATE) as usize;
        let lead = reference
            .iter()
            .position(|&s| s != 0.0)
            .unwrap_or_else(|| panic!("{what}: the reference note is silent"));
        let left = &planes[0];
        assert_eq!(
            left.iter().position(|&s| s != 0.0),
            Some(beat + lead),
            "{what} at {rate} Hz: the note enters on beat 1"
        );
        assert_eq!(
            &left[beat..beat + NOTE],
            reference,
            "{what} at {rate} Hz: from beat 1, the note at the render's rate"
        );
    }

    /// The live synth after an export plays its own clip on the **live**
    /// transport: rolled and seated on beat 1, the live graph sounds the
    /// note. A fork that shared the live port (not isolated) would have left
    /// its offline copy installed there — a cursor already past beat 1 on a
    /// timeline nothing advances — and the live graph would stay silent.
    fn assert_live_untouched(app: &mut App, synth: Entity) {
        let port = app
            .world()
            .get::<MidiTarget>(synth)
            .expect("still captured")
            .port()
            .clone();
        let ctx: tutti_core::transport::OfflineTransport = timeline(RATE);
        assert_eq!(
            port.rebind_offline_into(&tutti_midi_runtime::MidiInPort::new(), &ctx),
            OfflineRebind::Rebound,
            "the live synth still holds a clip"
        );
        let transport = app.world().resource::<TransportRes>().clone();
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        transport.settings.set_beat(Beat(1.0));
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mut heard = false;
        for _ in 0..4_096 {
            let mut frame = [0.0f32; 2];
            graph.render_frame(&mut frame);
            heard |= frame[0] != 0.0;
        }
        assert!(heard, "the live synth plays its clip on the live transport");
    }

    /// **An exported `PolySynth` plays its clip**, on the render's timeline
    /// and at the render's rate: a master export at 48 and 96 kHz is silent
    /// until beat 1 and then, sample for sample, what the synth renders for
    /// the note re-rated to the render's rate. The live synth keeps its clip
    /// and its inbox.
    ///
    /// Mutation (run): `own_fork_source` answering `None` (the synth forked
    /// from its `Legacy::controlled` shadow, the path before this) → the
    /// export is silent at both rates. Mutation (run): dropping the
    /// `rebind_offline_into` call in `PolySynth::fork_instance` → silent.
    /// Mutation (run): `fork_instance` not isolating the fork (so it shares
    /// the live port, and its offline clip replaces the live one) → the live
    /// synth is silent on beat 1 after the export.
    #[test]
    fn an_exported_polysynth_plays_its_clip() {
        for rate in [RATE, 2.0 * RATE] {
            let (mut app, synth) = app_with(GraphBackend::Native, poly());
            install_clip(&mut app, synth);
            let planes = export(&mut app, request(ExportSource::Master, rate)).planes();
            let mut rerated = poly();
            rerated.set_sample_rate(SampleRate(rate));
            let reference = reference(rerated, |s| {
                s.midi_sender().queue(&[note_on()]);
            });
            assert_note_at_beat_1("PolySynth", rate, &planes, &reference);
            assert_live_untouched(&mut app, synth);
        }
    }

    /// **An exported `SoundFontUnit` plays its clip**, on the live unit's
    /// preset, over the same decoded SoundFont, at the render's rate — a
    /// 48 kHz unit exported at 96 kHz renders what a unit built at 96 kHz
    /// does, not the 48 kHz note an octave down at half its frame. The live
    /// unit keeps its clip and its inbox.
    ///
    /// Mutation (run): `own_fork_source` answering `None` → silent at both
    /// rates. Mutation (run): `RateFollowing::set_sample_rate` doing nothing
    /// → at 96 kHz the 48 kHz unit places the note by its own rate, and it
    /// enters at frame 64 009 for 64 089.
    #[test]
    fn an_exported_soundfont_plays_its_clip() {
        let font = font();
        for rate in [RATE, 2.0 * RATE] {
            let (mut app, synth) = app_with(GraphBackend::Native, sf2(&font, RATE));
            install_clip(&mut app, synth);
            let planes = export(&mut app, request(ExportSource::Master, rate)).planes();
            let reference = reference(sf2(&font, rate), |s| {
                s.midi_sender().queue(&[note_on()]);
            });
            assert_note_at_beat_1("SoundFontUnit", rate, &planes, &reference);
            assert_live_untouched(&mut app, synth);
        }
    }

    /// **A native export of a synth renders what a `Net` export renders,
    /// bit for bit**, once the `Net` export is handed the clip. A `Net` node
    /// export isolates the synth, which severs its port, and nothing rebinds
    /// the clip: the `Net`-era host refilled it in the `prepare` hook, which
    /// this does (a `MidiClipSource` of the same events on the render's
    /// timeline). The native fork carries it itself.
    ///
    /// Mutation (run): `own_fork_source` answering `None` → the native render
    /// is silent ("the note sounds" fails). Mutation (run): the hook
    /// installing the clip 0.002 beat (one 64-frame chunk) late → the two
    /// part at the native onset, frame 32 001.
    #[test]
    fn native_and_net_synth_exports_are_bit_identical() {
        let render = |backend: GraphBackend| {
            let (mut app, synth) = app_with(backend, poly());
            install_clip(&mut app, synth);
            let node = app.world().get::<AudioNode>(synth).unwrap().0;
            let request =
                request(ExportSource::Node(synth), RATE).with_prepare(move |prepared, _world| {
                    let RenderGraph::Net(net) = prepared.graph else {
                        return;
                    };
                    let ctx = prepared.ctx.expect("a Net node export is rebound").clone();
                    let unit = net.node_mut(node);
                    let synth = unit
                        .as_any_mut()
                        .downcast_mut::<PolySynth>()
                        .expect("the synth");
                    let id = synth.midi_unit_id();
                    synth.set_midi_source(Arc::new(MidiClipSource::new(id, clip(), ctx)));
                });
            export(&mut app, request).planes()
        };
        let (native, net) = (render(GraphBackend::Native), render(GraphBackend::Net));
        assert!(
            native[0][..BEAT_48K].iter().all(|&s| s == 0.0),
            "nothing before beat 1"
        );
        assert!(
            native[0][BEAT_48K..].iter().any(|&s| s != 0.0),
            "the note sounds"
        );
        for c in 0..2 {
            assert_eq!(
                native[c].iter().zip(&net[c]).position(|(a, b)| a != b),
                None,
                "channel {c}: the first frame the backends differ"
            );
        }
    }

    /// A MIDI source that is not a function of a timeline.
    struct Unrebindable;

    impl tutti_midi_types::MidiUnitIn for Unrebindable {
        fn poll_unit(
            &self,
            _unit: tutti_midi_types::MidiUnitId,
            _block: usize,
            _rate: SampleRate,
            _buffer: &mut [MidiEvent],
        ) -> usize {
            0
        }
        fn rebind_offline(
            &self,
            _unit: tutti_midi_types::MidiUnitId,
            _ctx: &dyn std::any::Any,
        ) -> Option<Arc<dyn tutti_midi_types::MidiUnitIn>> {
            None
        }
    }

    /// **A synth playing a MIDI source that cannot be rebound refuses the
    /// export by its entity and name** (`ExportError::ForkSource`), with the
    /// synth's own reason, rather than render its notes as silence.
    ///
    /// Mutation (run): `PolySynth::fork_instance` and
    /// `SoundFontUnit::fork_instance` ignoring `NotRebindable` → both
    /// exports render.
    #[test]
    fn an_unrebindable_synth_source_is_a_named_failure() {
        let named = |got: Got, what: &str| match got {
            Got::ForkSource(node, cause) => {
                assert_eq!(node.name.as_deref(), Some("Synth"), "{what}");
                cause
            }
            other => panic!("{what}: expected a named fork-source failure, got {other:?}"),
        };
        let unrebindable = |app: &mut App, synth: Entity| {
            app.world()
                .get::<MidiTarget>(synth)
                .unwrap()
                .port()
                .install(Arc::new(Unrebindable));
        };

        let (mut app, synth) = app_with(GraphBackend::Native, poly());
        unrebindable(&mut app, synth);
        let cause = named(
            export(&mut app, request(ExportSource::Master, RATE)),
            "PolySynth",
        );
        assert!(
            matches!(
                cause.downcast_ref::<tutti_polysynth::Error>(),
                Some(tutti_polysynth::Error::MidiSource)
            ),
            "{cause:?}"
        );

        let (mut app, synth) = app_with(GraphBackend::Native, sf2(&font(), RATE));
        unrebindable(&mut app, synth);
        let cause = named(
            export(&mut app, request(ExportSource::Master, RATE)),
            "SoundFontUnit",
        );
        assert!(
            matches!(
                cause.downcast_ref::<tutti_soundfont::Error>(),
                Some(tutti_soundfont::Error::MidiSource)
            ),
            "{cause:?}"
        );
    }
}
