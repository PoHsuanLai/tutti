//! What an export renders, through the whole adapter: a request spawned in the
//! ECS, rendered on the task pool, reported on its entity.
//!
//! An export forks the live graph (`Editor::fork`, design doc 013 PR 12).
//! Until PR 13 the adapter could also run on fundsp's `Net`, whose export
//! cloned the net; from PR 13 to PR 15 the tests that held the two to the
//! same answer held the fork to the `Net`-era render, built by hand as a
//! test oracle (`chain_net_era`, `synths::poly_node_export_net_era`,
//! rendered by `render_net_era`). PR 15 retired those oracles with the rest
//! of the `Net` fixtures: the exports are now held to the same units wired
//! fresh with `GraphBuilder` (a fork starts reset, which a fresh graph is),
//! to a synth's own reference note, and, for the samples the `Net` rendered,
//! to golden digests on Linux/glibc (see [`GOLDEN_HERE`]). The rest pin
//! what only a fork has: it shares nothing with the live graph, it names a
//! node it cannot copy, it forks a hosted plugin by state transfer.
//! `export_surface.rs` pins the request/response shape; this file pins the
//! audio.

#![cfg(all(feature = "export", feature = "wav"))]

#[macro_use]
mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tutti::export::{
    ExportClock, ExportDone, ExportError, ExportNode, ExportOutput, ExportPlugin, ExportRequest,
    ExportSource, ExportTarget,
};
use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphSource};
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

/// A graph at the export's rate.
fn graph_on() -> AudioGraphRes {
    let mut graph = AudioGraphRes::headless(0, 2);
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
    ForkFailed(ExportNode, tutti_graph::ForkFaultKind, String),
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
                Err(e @ ExportError::ForkFailed { node, kind, .. }) => {
                    Got::ForkFailed(node.clone(), *kind, e.to_string())
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
// What a `Net` export also rendered
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

/// Whether this target is the one the golden digests were recorded on:
/// Linux with glibc's libm. The digests are of the fork's render on the
/// commit that retired the `Net`-era oracles (doc 013, PR 15), which
/// rendered the `Net` era's samples bit for bit (asserted there); the units
/// call `tan` (the filter's coefficients) and `sin` (the synth), libm
/// quality-of-implementation that differs in the last ulp between C
/// runtimes, so they are asserted only where they were recorded.
const GOLDEN_HERE: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// FNV-1a over the planes' little-endian `f32` bits, plane after plane.
fn digest(planes: &[Vec<f32>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in planes.iter().flatten().flat_map(|s| s.to_le_bytes()) {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Assert `got` is the digest recorded as `want`, where the goldens hold.
fn assert_golden(what: &str, got: u64, want: u64) {
    if GOLDEN_HERE {
        assert_eq!(
            got, want,
            "{what}: digest {got:#018x}, recorded {want:#018x}"
        );
    }
}

/// [`chain`]'s units wired fresh, with `GraphBuilder`, and rendered in the
/// export's own blocks for `seconds`: the master (the filter on channel 0,
/// the saw on channel 1) and the filter alone on both channels (a mono node
/// clamped across a stereo root). A fork starts reset, which is what a
/// fresh graph is.
fn chain_fresh(seconds: f64) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    use tutti_export::{render_to_buffers, FrozenClock, RenderGraph};
    use tutti_graph::GraphBuilder;
    let render = |master: bool| {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let osc = g.add_unit(Box::new(Osc::saw(Hz(110.0))));
        let filter = g.add_unit(Box::new(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz(800.0),
            Q(1.0),
        )));
        g.connect(osc, 0, filter, 0).connect_output(filter, 0, 0);
        g.connect_output(if master { osc } else { filter }, 0, 1);
        let (editor, executor) = g
            .build(RenderGraph::prepare(SampleRate(RATE)))
            .expect("builds");
        let graph = RenderGraph::new(editor, executor).expect("built together");
        render_to_buffers(graph, &config(seconds), &FrozenClock)
            .expect("renders")
            .planes
    };
    (render(true), render(false))
}

/// **An export renders its graph as wired fresh**, for the master and for
/// one node, on a graph whose live side has not run: bit for bit the same
/// units in a `GraphBuilder` graph ([`chain_fresh`]), and (Linux/glibc)
/// the `Net`-era export's samples.
///
/// "Where the rules allow" (doc 013, PR 12): a `Net` master export was a
/// plain clone that copied the running state (an oscillator's phase, a
/// filter's memory), and a fork starts reset. With the live side never
/// rendered the two coincided, so this was the like-for-like pair with the
/// `Net`-era export (`chain_net_era`) until doc 013 PR 15 retired it.
///
/// Mutation (run): `AudioGraphRes::export` forking `ForkTarget::Master` for a
/// node export → the node's render is the master's, and the node comparison
/// fails on channel 1 (the saw, not the filter).
#[test]
fn exports_render_their_graph_as_wired_fresh() {
    let mut app = app_over(graph_on());
    let filter = chain(&mut app);
    let native_master = export(&mut app, buffers(ExportSource::Master, 0.25)).planes();
    let native_node = export(&mut app, buffers(ExportSource::Node(filter), 0.25)).planes();
    let (fresh_master, fresh_node) = chain_fresh(0.25);
    for (what, fresh, native) in [
        ("master", &fresh_master, &native_master),
        ("node", &fresh_node, &native_node),
    ] {
        assert_eq!(fresh.len(), native.len(), "{what}: width");
        let peak = native[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "{what}: silent");
        for (c, (a, b)) in fresh.iter().zip(native).enumerate() {
            assert_eq!(a.len(), b.len(), "{what}: length of channel {c}");
            if let Some(i) = a
                .iter()
                .zip(b)
                .position(|(x, y)| x.to_bits() != y.to_bits())
            {
                panic!(
                    "{what}: channel {c} parts at frame {i}: fresh {} export {}",
                    a[i], b[i]
                );
            }
        }
    }
    assert_golden(
        "the master export",
        digest(&native_master),
        0xccbc_b417_89a0_36e4,
    );
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
/// past its duration. Asked of the forked plan.
///
/// The tail is resolved by `GraphTail::resolve`: a finite tail under the cap
/// is rendered whole, and a graph whose only node never said (`Tail::Unknown`,
/// every `AudioUnit`'s default) renders none — not the cap, which would
/// append silence.
///
/// Mutation (run): `start_exports` ignoring `latency_from_graph` → frame 0
/// reads 0; ignoring `tail_from_graph` → the render is `TAIL` frames short;
/// resolving the tail as `samples().unwrap_or(cap)` → the unknown graph
/// renders `cap` extra frames.
#[test]
fn latency_and_tail_come_from_the_graph() {
    let mut app = app_over(graph_on());
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
    assert_eq!(planes[0].len(), frames + TAIL, "the tail");
    assert_eq!(planes[0][0], 1.0, "the latency was not trimmed");
    assert!(planes[0].iter().all(|&s| s == 1.0), "a step from frame 0");

    let mut app = app_over(graph_on());
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
        "an unknown tail renders none, not the cap"
    );
}

/// **The latency trimmed is the latency of the graph rendered — after the
/// `prepare` hook.** The live graph is a latency-free constant; the hook
/// puts a `LATE`-frame source on the output. Trimmed by what the hook left,
/// the source's step lands on frame 0; by the live graph's figure (0), it
/// would land on frame `LATE`.
///
/// Mutation (run): not applying the fork's committed hook edit before the
/// figures are read (`executor.apply_pending()` after the hook's commit, in
/// `start_exports`, a no-op) → the fork's plan is still the live graph's
/// and nothing is trimmed. Mutation (run): reading the latency before the
/// hook runs → fails.
#[test]
fn the_trim_is_read_after_the_prepare_hook() {
    let mut app = app_over(graph_on());
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let dc = graph.insert(tutti_nodes::testing::Const::mono(0.5));
        graph.set_outputs_from(dc);
    }
    let request = buffers(ExportSource::Master, 0.01)
        .trim_reported_latency()
        .with_prepare(|prepared, _world| {
            let key = prepared.fresh_key();
            let editor = prepared.graph.editor_mut();
            editor.insert(key, "test:late", tutti_graph::Legacy::new(Late { pos: 0 }));
            for out in editor.spec_mut().topology.outputs.iter_mut() {
                *out = tutti_types::graph::Source::Node(tutti_types::graph::OutPort {
                    node: key,
                    port: 0,
                });
            }
        });
    let planes = export(&mut app, request).planes();
    assert_eq!(
        planes[0][0], 1.0,
        "trimmed by the live graph's latency, not the rendered one's"
    );
}

/// **An export of a graph with no outputs says so**, for the master and for
/// a node — not that the node has none.
///
/// Mutation (run): dropping the `outputs() == 0` check in
/// `AudioGraphRes::export` → the node export reports its node, the master
/// renders nothing and reports success.
#[test]
fn a_graph_with_no_outputs_says_so() {
    let mut graph = AudioGraphRes::headless(0, 0);
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
                "{source:?}: {why}"
            ),
            other => panic!("{source:?}: expected a refusal, got {other:?}"),
        }
    }
}

/// **A node export on a 90 BPM timeline plays where 90 BPM puts it.** A
/// sampler voice placed at beat 3 on the live transport, exported on its own
/// against an offline timeline at 90 BPM from beat 0: at 48 kHz beat 3 is
/// frame 96 000 (at the default 120 BPM it would be 72 000). Silent before,
/// the clip's own samples from there.
///
/// Mutation (run): `ExportClock::offline` answering the stopped timeline
/// for a named one → the voice never sounds. Mutation
/// (run): `Stopped::is_rolling` answering `true` → the frozen export of the
/// voice at beat 0 sounds.
#[cfg(feature = "sampler")]
#[test]
fn a_node_export_follows_a_90_bpm_timeline() {
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_sampler::MemorySource;
    let tone = |i: usize| (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin();

    let mut app = app_over(graph_on());
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
        "the voice sounded before beat 3"
    );
    // From there the clip's frame `k` is the render's frame `AT + k`, from
    // the first: the voice reads the clock once per 64-frame chunk
    // (`Legacy`), and the chunk starting on beat 3 reads exactly beat 3,
    // since the timeline counts frames and derives the beat (doc 013 §6).
    // (It accumulated once, read a hair under 3 there, and the voice entered
    // a chunk late, at 96 064.) Doc 013's chunk-major mode moves the graph's
    // clock per chunk, as `Net`'s moved.
    for k in 0..4_000 {
        let got = planes[0][AT + k];
        assert!(
            (got - tone(k)).abs() < 1e-3,
            "frame {k} of the clip read {got}, want {}",
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
        "a frozen export played the voice"
    );
}

// ---------------------------------------------------------------------------
// What a fork has and a `Net` clone did not
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
/// A `Net` master export (before PR 13) was a plain clone that shared the
/// counter with the live net (doc 013, PR 12's behaviour change), and this
/// test failed there by design.
///
/// Mutation (run): `Boxed::isolate` not forwarding to the unit (so the shadow
/// a fork is cloned from shares the live cell) → the fork's reset and render
/// move the live count, which jumps.
#[test]
fn live_playback_continues_unaffected_while_an_export_renders() {
    let mut graph = graph_on();
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
/// declares: `forkable() == false`.
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
    let mut app = app_over(graph_on());
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
    let mut app = app_over(graph_on());
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
// A hosted plugin, forked by state transfer
// ---------------------------------------------------------------------------

/// The reference CLAP plugin through a real `plugin-server`, exported through
/// a fork: a fresh instance in a server of its own, loaded with the live
/// one's state (tutti-plugin's `PluginClient::fork_instance`). (A `Net`
/// master export, before PR 13, drove the **live** plugin from the render
/// thread.)
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
        let mut app = app_over(graph_on());
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
    /// Mutation: hand the editor no fork source from `IntoNode for
    /// PluginClient<Bound>` (`fork: None`; before PR 12 the same came of
    /// inserting the plugin boxed) → the export is refused as not forkable,
    /// naming "Probe". Mutation (run):
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
            _ctx: &tutti_core::transport::OfflineTransport,
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

    /// **An exported plugin instrument plays its notes.** The plugin has a
    /// MIDI event input, so its `MidiSourceInstall` plays through a clip node
    /// wired to it (doc 013 item 5), which the export forks with it and which
    /// reads the render's `Env`. A note from beat 1 to beat 2 holds the probe's gate open
    /// from beat 1 to beat 2 of the render's timeline, heard one pipeline
    /// chunk (the export's 1024-frame block) later, and nowhere else.
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
    /// Mutation (run): the event wiring never writing the clip node's edge
    /// (`graph::events::reconcile`) → the fork renders silence, at both
    /// rates. (Before the clip node, dropping the `rebind_offline_into` call
    /// in `PluginFork::instance` did the same; that call now serves a clip a
    /// host installs on the port itself.) The plugin polling its MIDI port at a fixed 48 kHz instead
    /// of its own rate (`build_block_payload`) → at 96 kHz the notes land
    /// where 48 kHz puts them. Gathering a chunk's MIDI at its submission
    /// rather than its start (tutti-plugin's `PluginChunks::begin`) → the
    /// clip's window is read from the chunk's last 64-frame pass and the
    /// notes land 960 frames early, 64 frames after their beat (this test
    /// asserted exactly that while the chunk was 64 frames, so the early
    /// placement hid behind the pipeline's delay).
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
        // The plugin's batcher holds one chunk: what it is sent in chunk `k`
        // it returns in chunk `k + 1`. A fork has no device, so its chunk is
        // the export's block.
        const PIPELINE: usize = tutti_export::GRAPH_MAX_BLOCK.get();
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
            Got::ForkFailed(node, kind, _) => {
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
/// A `Net` export had no fork, and its synth played no clip unless the host
/// refilled one (the `Net`-era oracle that did, to compare, went with doc
/// 013 PR 15).
#[cfg(all(feature = "midi", feature = "synth", feature = "soundfont"))]
mod synths {
    use super::*;

    use bevy_tutti::graph::{GraphReconcilePlugin, MasterSources, SpawnAudioNode, TransportRes};
    use bevy_tutti::midi::{MidiSequencePlugin, MidiSourceInstall, MidiTarget, MidiTargetRegistry};
    use tutti_core::transport::{MotionEvent, OfflineTimeline, OfflineTimelineConfig, Transport};
    use tutti_core::{Beat, Bpm, BufferVec, Seconds, MAX_BUFFER_SIZE};
    use tutti_midi_runtime::{OfflineRebind, TimedMidiEvent};
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

    /// An app with the sequencer, both synth types registered
    /// for MIDI, and `unit` spawned (`spawn_audio_node`, the path a host
    /// takes) as "Synth", routed to the master.
    fn app_with(unit: impl AudioUnit + 'static) -> (App, Entity) {
        let mut app = app_over(graph_on());
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
        let ctx: tutti_core::transport::OfflineTransport =
            tutti_core::transport::OfflineTransport::new(timeline(RATE));
        assert_eq!(
            port.rebind_offline_into(&tutti_midi_runtime::MidiInPort::new(), &ctx),
            OfflineRebind::Rebound,
            "the live synth still holds a clip"
        );
        let transport = app.world().resource::<TransportRes>().clone();
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(Beat(1.0));
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
    /// Mutation (run): `controlled` dropping a unit's own fork source (the
    /// synth forked from its `Legacy::controlled` shadow alone, the path
    /// before this) → the export is silent at both rates. (Its
    /// `MidiNode::fork_source` answering `None` instead — the generic fork —
    /// still plays the clip here; what the synth's own source adds, control
    /// cells read at the fork, is pinned in tutti-polysynth's `fork.rs`.)
    /// Mutation (run): dropping the
    /// `rebind_offline_into` call in `PolySynth::fork_instance` → silent.
    /// Mutation (run): `fork_instance` not isolating the fork (so it shares
    /// the live port, and its offline clip replaces the live one) → the live
    /// synth is silent on beat 1 after the export.
    #[test]
    fn an_exported_polysynth_plays_its_clip() {
        for rate in [RATE, 2.0 * RATE] {
            let (mut app, synth) = app_with(poly());
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

    /// **A synth crossfaded to a new unit still exports its notes.** A
    /// crossfade lands the incoming unit with the fork its captured port asks
    /// for (`AudioGraphRes::replace_with`), and the graph records that the
    /// unit now at the key carries its clip, so an export reaching it forks
    /// it (here through `PolySynth`'s own fork source) rather than refusing
    /// it as a unit whose fork would drop the clip. The clip the entity
    /// installed follows it onto the incoming unit's port (the sequencer
    /// re-installs on the re-captured `MidiTarget`), and is heard once.
    ///
    /// Mutations (run): `NativeGraph::replace` recording `carries_midi =
    /// false` for the incoming unit → the export is refused as
    /// `NotForkable`; `replace` dropping the fork it took (landing the unit
    /// with `None`) → the fork drops the clip, and the export is silent.
    #[test]
    fn a_crossfaded_synth_still_exports_its_notes() {
        let (mut app, synth) = app_with(poly());
        install_clip(&mut app, synth);
        bevy_tutti::graph::crossfade_audio_node(
            &mut app.world_mut().commands(),
            synth,
            Box::new(poly()),
        );
        app.world_mut().flush();
        app.update();
        let planes = export(&mut app, request(ExportSource::Master, RATE)).planes();
        let reference = reference(poly(), |s| {
            s.midi_sender().queue(&[note_on()]);
        });
        assert_note_at_beat_1("crossfaded PolySynth", RATE, &planes, &reference);
    }

    /// **An exported `SoundFontUnit` plays its clip**, on the live unit's
    /// preset, over the same decoded SoundFont, at the render's rate — a
    /// 48 kHz unit exported at 96 kHz renders what a unit built at 96 kHz
    /// does, not the 48 kHz note an octave down at half its frame. The live
    /// unit keeps its clip and its inbox.
    ///
    /// Mutation (run): `controlled` dropping a unit's own fork source →
    /// silent at both rates. Mutation (run): `RateFollowing::set_sample_rate`
    /// doing nothing, or the unit's `MidiNode::fork_source` answering `None`
    /// (the generic fork, at the live rate) → at 96 kHz the 48 kHz unit places
    /// the note by its own rate, and it enters at frame 64 009 for 64 089.
    #[test]
    fn an_exported_soundfont_plays_its_clip() {
        let font = font();
        for rate in [RATE, 2.0 * RATE] {
            let (mut app, synth) = app_with(sf2(&font, RATE));
            install_clip(&mut app, synth);
            let planes = export(&mut app, request(ExportSource::Master, rate)).planes();
            let reference = reference(sf2(&font, rate), |s| {
                s.midi_sender().queue(&[note_on()]);
            });
            assert_note_at_beat_1("SoundFontUnit", rate, &planes, &reference);
            assert_live_untouched(&mut app, synth);
        }
    }

    /// **A node export of a synth plays its clip**: silent until beat 1,
    /// then the synth's own note ([`reference`]) sample for sample, and
    /// (Linux/glibc) the `Net`-era export's samples. The fork carries the
    /// clip itself.
    ///
    /// Until doc 013 PR 15 the oracle was `poly_node_export_net_era`: the
    /// synth `clone_isolated` out of a `Net`, the clip reinstalled by hand
    /// (a `Net`-era host's `prepare` hook), rendered as tutti-export's `Net`
    /// arm rendered it, bit for bit.
    ///
    /// Mutation (run): `controlled` dropping a unit's own fork source → the
    /// render is silent ("the note enters on beat 1" fails).
    #[test]
    fn a_synth_node_export_plays_its_clip() {
        let (mut app, synth) = app_with(poly());
        install_clip(&mut app, synth);
        let native = export(&mut app, request(ExportSource::Node(synth), RATE)).planes();
        let reference = reference(poly(), |s| {
            s.midi_sender().queue(&[note_on()]);
        });
        assert_note_at_beat_1("PolySynth node export", RATE, &native, &reference);
        assert_golden(
            "the synth's node export",
            digest(&native),
            0x2f47_791e_730f_81b5,
        );
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
            _ctx: &tutti_core::transport::OfflineTransport,
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

        let (mut app, synth) = app_with(poly());
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

        let (mut app, synth) = app_with(sf2(&font(), RATE));
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

/// A **host-defined** MIDI-receiving unit — a type bevy-tutti has never heard
/// of, registered with `MidiTargetRegistry` like any other — exported
/// through a fork. Its export plays the clip on its live port (the generic
/// fork: the shadow, plus the clip re-installed, rebound, on the fork's own
/// port), or refuses by name; it is never silent.
///
/// (A `Net` export, before PR 13, had no fork: its node export severed the
/// port, and nothing rebound the clip — the `Net`-era host refilled it by
/// hand.)
#[cfg(feature = "midi")]
mod host_midi {
    use super::*;

    use bevy_tutti::graph::{
        CapturedControls, GraphReconcilePlugin, MasterSources, SpawnAudioNode, TransportRes,
    };
    use bevy_tutti::midi::{
        MidiForkError, MidiNode, MidiSequencePlugin, MidiSourceInstall, MidiTarget,
        MidiTargetRegistry,
    };
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, Transport};
    use tutti_core::{Beat, Bpm};
    use tutti_midi_runtime::{MidiInPort, TimedMidiEvent};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// A beat at 90 BPM and 48 kHz, in frames.
    const BEAT: usize = 32_000;

    /// A note gate: 1.0 while any note is held, from the note-on's own frame
    /// to the note-off's, 0.0 otherwise. Owns a MIDI port, as a host's
    /// instrument would; nothing in bevy-tutti names it.
    #[derive(Clone)]
    struct Gate {
        midi: MidiInPort,
        rate: SampleRate,
        held: u32,
        events: Vec<MidiEvent>,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                midi: MidiInPort::new(),
                rate: SampleRate(RATE),
                held: 0,
                events: vec![MidiEvent::noop(); 64],
            }
        }
    }

    impl MidiNode for Gate {
        fn midi_port(&self) -> &MidiInPort {
            &self.midi
        }
    }

    impl AudioUnit for Gate {
        fn inputs(&self) -> usize {
            0
        }
        fn outputs(&self) -> usize {
            1
        }
        fn reset(&mut self) {
            self.held = 0;
        }
        /// Severs the live port, as every MIDI unit's must.
        fn isolate(&mut self) {
            self.midi.isolate();
        }
        fn set_sample_rate(&mut self, rate: SampleRate) {
            self.rate = rate;
        }
        fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
            let n = self.midi.poll(1, self.rate, &mut self.events);
            for i in 0..n {
                let e = self.events[i];
                self.apply(&e);
            }
            output[0] = if self.held > 0 { 1.0 } else { 0.0 };
        }
        fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
            let n = self.midi.poll(size, self.rate, &mut self.events);
            let mut events = self.events[..n].to_vec();
            events.sort_by_key(|e| e.frame_offset);
            let mut next = 0;
            for i in 0..size {
                while next < events.len() && events[next].frame_offset as usize <= i {
                    self.apply(&events[next]);
                    next += 1;
                }
                output.set_f32(0, i, if self.held > 0 { 1.0 } else { 0.0 });
            }
        }
        fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
            SignalFrame::new(1)
        }
        fn get_id(&self) -> u64 {
            0x6a7e
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    impl Gate {
        fn apply(&mut self, e: &MidiEvent) {
            if e.is_note_on() {
                self.held += 1;
            } else if e.is_note_off() {
                self.held = self.held.saturating_sub(1);
            }
        }
    }

    /// A native app with the sequencer and `Gate` registered for MIDI.
    fn app() -> App {
        let mut app = app_over(graph_on());
        app.insert_resource(TransportRes(Transport::new(RATE)));
        app.add_plugins((GraphReconcilePlugin, MidiSequencePlugin));
        app.init_resource::<MidiTargetRegistry>();
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<Gate>();
        app
    }

    /// A gate spawned the way a host spawns a node, routed to the master,
    /// named "Gate".
    fn spawned(app: &mut App) -> Entity {
        let world = app.world_mut();
        let gate = world
            .commands()
            .spawn_audio_node(Gate::new())
            .insert(Name::new("Gate"))
            .id();
        world.commands().insert_resource(MasterSources::from(gate));
        world.flush();
        app.update();
        gate
    }

    /// Middle C from beat 1 to beat 2, installed as a host does.
    fn install_clip(app: &mut App, gate: Entity) {
        let note = |beat: f64, on: bool| {
            let event = if on {
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
            } else {
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)
            };
            TimedMidiEvent::new(Beat(beat), event)
        };
        app.world_mut().spawn(MidiSourceInstall::new(
            gate,
            vec![note(1.0, true), note(2.0, false)],
        ));
        app.update();
    }

    /// A 1.5 s master export against a 90 BPM timeline from beat 0.
    fn request() -> ExportRequest {
        ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            config(1.5),
            ExportClock::timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: Beat(0.0),
                tempo: Bpm(90.0),
                sample_rate: SampleRate(RATE),
                loop_range: None,
            }))),
        )
    }

    /// **A host's own MIDI unit exports its clip**: registered and spawned,
    /// with a clip installed on its live port, its native export holds the
    /// gate open from beat 1 to beat 2 — frames 32 000 to 64 000 — and
    /// nowhere else. No fork source of its own: the generic fork carries the
    /// clip.
    ///
    /// Mutation (run): the native graph dropping the carry hook
    /// (`controlled` building `UnitFork::Carry` without `with_fork_hook`) →
    /// the export is silent. Mutation (run): the hook's `rebind_offline_into`
    /// installing on a fresh port rather than the fork's (`MidiInPort::new()`
    /// for `fork`) → silent.
    #[test]
    fn a_host_midi_unit_exports_its_clip() {
        let mut app = app();
        let gate = spawned(&mut app);
        install_clip(&mut app, gate);
        let planes = export(&mut app, request()).planes();
        let open: Vec<usize> = planes[0]
            .iter()
            .enumerate()
            .filter(|(_, &s)| s != 0.0)
            .map(|(i, _)| i)
            .collect();
        assert!(!open.is_empty(), "the host's unit rendered no notes");
        assert_eq!(
            (open[0], *open.last().unwrap() + 1),
            (BEAT, 2 * BEAT),
            "the gate is open from beat 1 to beat 2"
        );
        assert_eq!(open.len(), BEAT, "one unbroken note");
    }

    /// **A MIDI unit pushed without its fork refuses the export by name.** A
    /// host that captures the unit's controls, then inserts it with the
    /// plain `AudioGraphRes::insert` and binds them, gets a node whose fork
    /// would clone the shadow and drop the clip; the export says so
    /// (`ExportError::NotForkable`, naming "Gate") rather than render silence.
    ///
    /// Mutation (run): `NativeGraph::fork_for_export` not checking the
    /// forked keys against the captured MIDI ports → the export renders, and
    /// the gate never opens.
    #[test]
    fn a_midi_unit_inserted_without_its_fork_refuses_by_name() {
        let mut app = app();
        let unit = Gate::new();
        let controls = CapturedControls::capture(app.world(), &unit);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(unit);
        let mut gate = app.world_mut().spawn(Name::new("Gate"));
        controls.bind(&mut gate, node);
        let gate = gate.id();
        app.insert_resource(MasterSources::from(gate));
        app.update();
        install_clip(&mut app, gate);
        match export(&mut app, request()) {
            Got::NotForkable(node) => {
                assert_eq!(node.entity, Some(gate));
                assert_eq!(node.name.as_deref(), Some("Gate"));
            }
            other => panic!("expected a named refusal, got {other:?}"),
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
            _ctx: &tutti_core::transport::OfflineTransport,
        ) -> Option<Arc<dyn tutti_midi_types::MidiUnitIn>> {
            None
        }
    }

    /// **A host's MIDI unit playing a source that cannot be rebound refuses
    /// the export by name** (`ExportError::ForkSource`,
    /// `MidiForkError::NotRebindable`), rather than render its notes as
    /// silence.
    ///
    /// Mutation (run): the carry hook answering `Ok` for `NotRebindable` →
    /// the export renders.
    #[test]
    fn a_host_midi_unit_with_an_unrebindable_source_refuses_by_name() {
        let mut app = app();
        let gate = spawned(&mut app);
        app.world()
            .get::<MidiTarget>(gate)
            .expect("captured")
            .port()
            .install(Arc::new(Unrebindable));
        match export(&mut app, request()) {
            Got::ForkSource(node, cause) => {
                assert_eq!(node.name.as_deref(), Some("Gate"));
                assert_eq!(
                    cause.downcast_ref::<MidiForkError>(),
                    Some(&MidiForkError::NotRebindable)
                );
            }
            other => panic!("expected a named fork-source failure, got {other:?}"),
        }
    }
}
// ---------------------------------------------------------------------------
// Native only: a disk-streamed sampler voice, which a fork reads from its file
// ---------------------------------------------------------------------------

/// A disk-streamed clip through a fork. The live voice plays what the butler
/// streams into its ring; the fork cannot (the ring's one consumer is the live
/// audio thread, and a seek moves the live stream), so it reads the file the
/// butler's record names itself, on the render's thread (tutti-sampler's
/// `offline_read`). (A `Net` master export, before PR 13, was a plain clone
/// that read the live ring.)
///
/// The butler is hand-stepped (`DiskStreamer::manual`), so a refill is a
/// step, not a race with a render that runs faster than real time.
#[cfg(feature = "sampler")]
mod disk {
    use super::*;

    use std::path::Path;

    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_core::{Beat, Bpm, SamplePosition, Timeline};
    use tutti_sampler::{
        Command, DiskStreamer, DiskVoice, MemorySource, Playback, Voice, VoiceNode, VoiceSource,
    };

    /// Frame `i` of every test file: distinct per frame, and exactly what an
    /// f32 WAV hands back.
    fn value(i: usize) -> f32 {
        (i as f32 + 1.0) * 1e-5
    }

    /// The file frame a sample of [`value`] came from.
    fn frame_of(s: f32) -> i64 {
        (s / 1e-5).round() as i64 - 1
    }

    /// A stereo f32 WAV of `frames` at `rate`: `value(i)` left, `-value(i)`
    /// right.
    fn write_ramp(path: &Path, rate: u32, frames: usize) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).expect("writes");
        for i in 0..frames {
            w.write_sample(value(i)).expect("writes");
            w.write_sample(-value(i)).expect("writes");
        }
        w.finalize().expect("writes");
    }

    /// A hand-stepped butler at the export's rate streaming `path` on
    /// channel 0, primed.
    fn streamer_on(path: &Path) -> DiskStreamer {
        let mut streamer =
            DiskStreamer::manual(SampleRate(RATE), Default::default()).expect("builds");
        streamer
            .commands()
            .send(Command::Stream {
                channel_index: 0,
                file_path: path.to_path_buf(),
                offset: SamplePosition(0.0),
            })
            .expect("the butler is alive");
        assert!(
            streamer.step_until_settled(1_000) < 1_000,
            "the ring primes"
        );
        streamer
    }

    /// The live voice on channel 0, placed at `beat` on `clock`.
    fn disk_voice(streamer: &DiskStreamer, clock: Arc<dyn Timeline>, beat: f64) -> DiskVoice {
        streamer
            .status()
            .take_disk_voice(0, clock, Beat(beat), None)
            .expect("the link is installed")
    }

    /// A timeline at `tempo` BPM from beat 0, at the export's rate.
    fn timeline(tempo: f64) -> Arc<OfflineTimeline> {
        Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(tempo),
            sample_rate: SampleRate(RATE),
            loop_range: None,
        }))
    }

    fn node_of(source: VoiceSource) -> VoiceNode {
        VoiceNode::with_channels(
            Voice {
                source,
                play: Playback::default(),
                channel_index: None,
            },
            2usize,
        )
    }

    fn on(seconds: f64, source: ExportSource, clock: &Arc<OfflineTimeline>) -> ExportRequest {
        ExportRequest::new(
            source,
            ExportTarget::Buffers,
            config(seconds),
            ExportClock::timeline(clock.clone()),
        )
    }

    /// **A disk-streamed clip exports its file, from the frame its beat falls
    /// on**, as the master and as a node: placed at beat 3 of a 90 BPM render
    /// (frame 96 000), silent before, then the file's own frame `k` on render
    /// frame `96 000 + k`, exactly, on both channels, and silent after its
    /// last. The master is a bare `DiskVoice`; the node a `VoiceNode` holding
    /// one, which reads it a frame at a time. **And it is the same clip as a
    /// memory voice**: a node export of the file's frames in memory, placed
    /// the same, renders the same planes bit for bit.
    ///
    /// Mutation (run): `DiskVoice::rebind_offline` not handing the copy its
    /// file (`offline.read` left `None`) → both exports silent → fails.
    /// Mutation (run): both whole-frame rules removed (`snap_to_whole_frame`
    /// in the gate, and tutti-sampler `tap_indices`'s carry of a fraction
    /// that rounds to 1.0) → a frame of the clip reads an ulp off → fails. Mutation (run): `DiskVoice::forkable` answering
    /// `false` again → refused as not forkable → fails.
    #[test]
    fn a_disk_voice_exports_its_file_from_its_beat_and_matches_memory() {
        const AT: usize = 96_000;
        const LEN: usize = 48_000;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        write_ramp(&path, RATE as u32, LEN);
        // A stream serves one live voice, so each of the two has its own.
        let (streamer, other) = (streamer_on(&path), streamer_on(&path));
        let live = timeline(90.0) as Arc<dyn Timeline>;

        let mut app = app_over(graph_on());
        let (disk_node, memory_node) = {
            let bare = disk_voice(&streamer, live.clone(), 3.0);
            let wrapped = node_of(VoiceSource::Disk(disk_voice(&other, live.clone(), 3.0)));
            let mut wave = tutti_io::Wave::new(2, RATE);
            for i in 0..LEN {
                wave.push_frame(&[value(i), -value(i)]);
            }
            let memory = node_of(VoiceSource::Memory(MemorySource::with_transport(
                Arc::new(wave),
                live.clone(),
                Beat(3.0),
                None,
            )));
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let bare = graph.insert(bare);
            graph.set_outputs_from(bare);
            let (wrapped, memory) = (graph.insert(wrapped), graph.insert(memory));
            app.world_mut().spawn(bare);
            (
                app.world_mut().spawn(wrapped).id(),
                app.world_mut().spawn(memory).id(),
            )
        };

        let render = |app: &mut App, source| export(app, on(3.0, source, &timeline(90.0))).planes();
        let master = render(&mut app, ExportSource::Master);
        let disk = render(&mut app, ExportSource::Node(disk_node));
        for (what, planes) in [("master", &master), ("node", &disk)] {
            assert!(
                planes[0][..AT].iter().all(|&s| s == 0.0),
                "{what}: sounded before beat 3"
            );
            for k in 0..LEN {
                assert_eq!(planes[0][AT + k], value(k), "{what}: left, file frame {k}");
                assert_eq!(
                    planes[1][AT + k],
                    -value(k),
                    "{what}: right, file frame {k}"
                );
            }
            assert!(
                planes[0][AT + LEN..].iter().all(|&s| s == 0.0),
                "{what}: sounded past the file's end"
            );
        }

        let memory = render(&mut app, ExportSource::Node(memory_node));
        for c in 0..2 {
            if let Some(i) =
                (0..disk[c].len()).find(|&i| disk[c][i].to_bits() != memory[c][i].to_bits())
            {
                panic!(
                    "channel {c} parts from memory at frame {i}: disk {} memory {}",
                    disk[c][i], memory[c][i]
                );
            }
        }
    }

    /// **A node export of a disk voice plays its file.** (Until PR 13 this
    /// also ran on `Net`, whose node export isolated and rebound a clone of
    /// the node, the same calls a fork makes.)
    ///
    /// Mutation (run): `DiskVoice::rebind_offline` not handing the copy its
    /// file → silent → fails.
    #[test]
    fn a_node_export_of_a_disk_voice_plays_its_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        write_ramp(&path, RATE as u32, 4_800);
        let streamer = streamer_on(&path);

        let mut app = app_over(graph_on());
        let voice = {
            let voice = node_of(VoiceSource::Disk(disk_voice(
                &streamer,
                timeline(120.0),
                1.0,
            )));
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(voice);
            app.world_mut().spawn(node).id()
        };
        let planes = export(
            &mut app,
            on(1.0, ExportSource::Node(voice), &timeline(120.0)),
        )
        .planes();
        assert!(
            planes[0][..24_000].iter().all(|&s| s == 0.0),
            "sounded before beat 1"
        );
        for k in 0..4_800 {
            assert_eq!(planes[0][24_000 + k], value(k), "file frame {k}");
        }
    }

    /// **A clip whose file is at another rate is resampled to the export's**:
    /// a 24 kHz file exported at 48 kHz reads file frame `n / 2` on render
    /// frame `n` of the clip (a ramp, which the cubic kernel reproduces
    /// between frames up to rounding), and its one second of file lasts
    /// 48 000 render frames.
    ///
    /// Mutation (run): the fork's read rate without the conversion
    /// (`SrcRatio::for_rates` → `UNITY` in `DiskVoice::offline_read_rate`)
    /// → a file frame per render frame → fails.
    #[test]
    fn a_disk_voice_at_another_rate_exports_resampled() {
        const AT: usize = 24_000;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp_24k.wav");
        write_ramp(&path, 24_000, 24_000);
        let streamer = streamer_on(&path);

        let mut app = app_over(graph_on());
        {
            let voice = disk_voice(&streamer, timeline(120.0), 1.0);
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(voice);
            graph.set_outputs_from(node);
        }
        let planes = export(&mut app, on(2.0, ExportSource::Master, &timeline(120.0))).planes();
        assert!(
            planes[0][..AT].iter().all(|&s| s == 0.0),
            "sounded before beat 1"
        );
        assert_eq!(planes[0][AT], value(0), "the clip's first frame on beat 1");
        // From frame 4: the first frames' taps clamp at the file's start.
        for n in 4..47_990 {
            let want = (n as f32 / 2.0 + 1.0) * 1e-5;
            let got = planes[0][AT + n];
            assert!(
                (got - want).abs() < 1e-6,
                "render frame {n} of the clip read {got}, want {want}"
            );
        }
        assert!(
            planes[0][AT + 48_000..].iter().all(|&s| s == 0.0),
            "sounded past the file's end"
        );
    }

    /// **Live disk playback is untouched while its voice exports.** The live
    /// graph renders on its own thread (the butler stepped between its
    /// blocks, its clock moved after each), from before the export starts
    /// until after it reports, while a master export forks the same voice.
    /// The live voice plays the file's frames one after another the whole
    /// time: a render that popped its ring, or asked its butler to seek,
    /// would show as a jump. The export plays the clip from its first frame.
    ///
    /// Mutation (run): the fork taking the live path (the offline branch
    /// removed from `DiskVoice::process`) with `isolate` keeping the live
    /// control cell → the fork's gate asks the live butler to seek to the
    /// render's position, and the live frames jump → fails.
    #[test]
    fn live_disk_playback_is_untouched_while_its_voice_exports() {
        const BLOCK: usize = 256;
        // Past the entry seek the live voice raises on its first block.
        const SETTLED: usize = 16 * BLOCK;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("long.wav");
        // Long enough that the live side, paced below, cannot run out of
        // file while the export renders.
        let len = 30 * RATE as usize;
        write_ramp(&path, RATE as u32, len);
        let mut streamer = streamer_on(&path);
        let live_clock = timeline(120.0);

        let mut graph = graph_on();
        let node = graph.insert(disk_voice(&streamer, live_clock.clone(), 0.0));
        graph.set_outputs_from(node);
        graph.render_frame(&mut [0.0, 0.0]);
        let mut live = graph.take_audio_side();
        let mut app = app_over(graph);

        let (settled, done) = (AtomicBool::new(false), AtomicBool::new(false));
        let (heard, got) = std::thread::scope(|s| {
            let device = s.spawn(|| {
                let mut heard = Vec::new();
                let mut block = vec![Vec::new(), Vec::new()];
                let mut after = 0;
                while after < 8 {
                    let _ = streamer.step_until_settled(64);
                    live.render(BLOCK, BLOCK, &mut block);
                    live_clock.advance(BLOCK);
                    heard.extend_from_slice(&block[0]);
                    assert!(heard.len() < len, "the live side ran out of file first");
                    if heard.len() > SETTLED {
                        settled.store(true, Ordering::Release);
                    }
                    if done.load(Ordering::Acquire) {
                        after += 1;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                heard
            });
            // The export starts once the live voice has settled, so every
            // frame it could disturb is one the assertions below check.
            while !settled.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let got = export(&mut app, on(1.0, ExportSource::Master, &timeline(120.0)));
            done.store(true, Ordering::Release);
            (device.join().expect("the live side"), got)
        });

        let first = frame_of(heard[SETTLED]);
        assert!(first >= 0, "the live voice is playing by frame {SETTLED}");
        for (i, &s) in heard[SETTLED..].iter().enumerate() {
            assert_eq!(
                frame_of(s),
                first + i as i64,
                "the live voice jumped at frame {} while the export ran",
                SETTLED + i
            );
        }
        let rendered = got.planes();
        for (k, &s) in rendered[0].iter().enumerate() {
            assert_eq!(s, value(k), "the export's frame {k}");
        }
    }

    /// Open file descriptors of this process that name `path` (Linux:
    /// `/proc/self/fd`).
    #[cfg(target_os = "linux")]
    fn open_handles(path: &Path) -> usize {
        let path = path.canonicalize().expect("the file exists");
        std::fs::read_dir("/proc/self/fd")
            .expect("procfs")
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .filter(|target| *target == path)
            .count()
    }

    /// **An export leaves nothing open behind it.** The fork's only resource
    /// is the file it reads (it registers no butler stream: it holds no
    /// command handle to register one with), and it is closed once the
    /// export has reported: the process holds the same handles on the file
    /// as before, the live butler's. That the fork opened it at all is the
    /// export playing the clip.
    ///
    /// Linux only: it counts handles through `/proc/self/fd`, which the other
    /// platforms do not have. What it pins is platform-independent.
    ///
    /// Mutation (run): the offline reader leaking its decoder
    /// (`Box::leak` of the `FileIn` in `Open::open_path`) → one handle more
    /// after the export → fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_export_leaves_nothing_open_behind_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        write_ramp(&path, RATE as u32, 4_800);
        let streamer = streamer_on(&path);

        let mut app = app_over(graph_on());
        {
            let voice = disk_voice(&streamer, timeline(120.0), 0.0);
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(voice);
            graph.set_outputs_from(node);
        }
        let before = open_handles(&path);
        let planes = export(&mut app, on(0.2, ExportSource::Master, &timeline(120.0))).planes();
        assert_eq!(planes[0][100], value(100), "the export played the clip");
        assert_eq!(
            open_handles(&path),
            before,
            "the export left a handle on the file open"
        );
    }

    /// **A disk voice whose file cannot be read fails the export by name**,
    /// with the file, rather than write its silence as a success. The file is
    /// removed after the stream started (the live butler keeps its own
    /// handle); the fork re-opens it by its path, cannot, and its render is
    /// `ExportError::ForkFailed` naming the voice's entity and `Name`.
    ///
    /// Mutation (run): `LegacyFork::fork` not asking the copy for its
    /// `render_fault` → the export succeeds, silent → fails.
    #[test]
    fn a_disk_voice_whose_file_cannot_be_read_fails_the_export_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("vanished.wav");
        write_ramp(&path, RATE as u32, 4_800);
        let streamer = streamer_on(&path);

        let mut app = app_over(graph_on());
        let clip = {
            let voice = disk_voice(&streamer, timeline(120.0), 0.0);
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(voice);
            graph.set_outputs_from(node);
            app.world_mut().spawn((node, Name::new("Clip"))).id()
        };
        std::fs::remove_file(&path).expect("removes");
        match export(&mut app, on(0.2, ExportSource::Master, &timeline(120.0))) {
            Got::ForkFailed(node, kind, message) => {
                assert_eq!(node.entity, Some(clip));
                assert_eq!(node.name.as_deref(), Some("Clip"));
                assert_eq!(kind, tutti_graph::ForkFaultKind::Failed);
                assert!(
                    message.contains("vanished.wav"),
                    "names the file: {message}"
                );
            }
            other => panic!("expected a named failure, got {other:?}"),
        }
    }
}
