//! Does plugin delay compensation actually align a plugin path against a dry
//! one — through a **real out-of-process plugin**, end to end?
//!
//! Every other latency test in this tree stops one step short of the
//! question. `vst2_latency.rs` and the VST3 suites assert that the host *reads*
//! the plugin's declared figure; `tutti-graph`'s own tests assert the
//! compiler's PDC against hand-built nodes; `bevy-tutti`'s assert that the
//! latency poll reaches the editor. None of them puts a real plugin in a real
//! graph beside a real dry path and looks at the samples, so the one thing that
//! could still be wrong — that the figure the host reports, the figure the
//! planner plans against, and the delay the audio actually suffers are the
//! *same number* — is pinned here.
//!
//! It is not one number, which is exactly why. An out-of-process plugin's
//! latency is its own declared figure **plus** the chunk the pipeline holds:
//! the node submits chunk N and collects chunk N−1, so a sample entering
//! leaves one 64-frame chunk later than the plugin alone accounts for. The
//! node's `Shape` declares the sum ([`EXPECTED_TOTAL_LATENCY`]), and getting
//! that addition wrong is invisible to any test that reads the plugin's figure
//! back — the host would report 137, the plugin really would delay 137, and the
//! audio would still arrive 64 samples late against its dry twin.
//!
//! # The rig
//!
//! ```text
//!   impulse ──┬──▶ plugin (137 declared + 64 pipeline) ──▶ ┐
//!             │                                            ├──▶ out
//!             └──────────── dry ───────────────────────────▶ ┘
//! ```
//!
//! A native `tutti-graph` graph, compiled and rendered as the engine renders
//! one: the compiler's PDC pass delays the dry path, so both arrivals land on
//! the same sample and sum to exactly twice the impulse. The plugin path alone
//! arrives [`EXPECTED_TOTAL_LATENCY`] late. And a latency the plugin changes at
//! runtime reaches PDC at the next commit, through `Editor::set_latency`.

#![cfg(feature = "clap")]

// `#[path]` rather than a `support/mod.rs`: this crate's suites each pull in
// only the fixtures they use, which is what keeps a VST2 binary from compiling
// the CLAP harness (and its `tutti-clap-test-plugin` dev-dependency) at all.
#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv};

use tutti_core::{ChannelLayout, SampleRate, Samples};
use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_nodes::testing::Through;
use tutti_nodes::ChannelSumNode;
use tutti_plugin::handles::{Bound, PluginClient, PluginControls};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

const SAMPLE_RATE: f64 = 48_000.0;

/// The block size every rig here renders, and the chunk the out-of-process
/// pipeline holds.
const BLOCK: usize = 64;

/// What the CLAP probe declares through `clap.latency` in `RenderMode::Latency`,
/// and delays by. A prime, so an accidentally-correct result — an off-by-a-block,
/// a doubled value — cannot coincide with it.
///
/// Mirrors `tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES` rather than
/// importing it: this crate does not depend on the probe's rlib, and the value
/// crosses into the subprocess as behaviour rather than as a symbol.
/// `the_probe_declares_the_latency_this_suite_expects` pins the two together.
const PROBE_LATENCY: usize = 137;

/// The total an out-of-process plugin node declares: the plugin's own figure
/// plus the chunk the pipeline holds.
///
/// The sum is the thing under test. `PluginClient::latency()` reports only the
/// first term — it is the plugin's declaration — while the node's `Shape`
/// declares the sum, and only the `Shape` reaches the compiler's PDC pass.
const EXPECTED_TOTAL_LATENCY: usize = PROBE_LATENCY + BLOCK;

/// Blocks to render. Comfortably past `EXPECTED_TOTAL_LATENCY` (201) so the
/// delayed arrival is inside the captured span with room after it, and the
/// "nothing elsewhere" assertion has real silence to check rather than the edge
/// of the buffer.
const BLOCKS: usize = 12;
const TOTAL_FRAMES: usize = BLOCKS * BLOCK;

/// Blocks of silence driven before the impulse, to bring the pipeline to steady
/// state. See the note in [`render_impulse`] for why a cold start is lossy.
const WARMUP_BLOCKS: usize = 8;

/// Where the impulse is fed. Not 0: an impulse at the very first sample of the
/// very first block makes "arrived at t + latency" and "arrived at the start of
/// some block" the same statement, so an off-by-a-whole-block error would land
/// on a block boundary and read as correct.
const IMPULSE_AT: usize = 5;

/// One block's worth of wall time — what a real audio callback spends per block.
///
/// See the pacing note in [`render_impulse`] for why sleeping is load-bearing
/// here rather than cosmetic.
const PERIOD: std::time::Duration =
    std::time::Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);

/// How many block periods to actually wait per block.
///
/// A real callback gets exactly one period, and one period is enough *when the
/// subprocess is scheduled promptly*. These tests are not a real callback: they
/// run under a test runner that may have every core busy, the subprocess is not
/// at realtime priority (the server logs when it cannot raise itself), and a
/// `sleep` guarantees only a lower bound on elapsed time — not that the other
/// process was given any CPU inside it.
///
/// So the wait is deliberately generous. This is not a timing assertion —
/// nothing here measures throughput or deadlines, unlike `real_plugin_pressure`
/// — so a longer wait costs a few milliseconds per test and buys a result that
/// does not depend on machine load. With one period, this suite failed roughly
/// one run in three under a parallel runner; the failure always presented as
/// "the plugin contributed nothing", which is also what a genuine routing bug
/// looks like, so a flake here is worse than slow.
const PACE: std::time::Duration = PERIOD.saturating_mul(20);

/// Impulse height. A power of two so every product and sum below is exact in
/// f32 and the assertions can use exact equality — the arithmetic under test is
/// routing and delay, not rounding, so an epsilon would only hide a defect.
const IMPULSE: f32 = 1.0;

/// Render `BLOCKS` blocks of an impulse through `graph`, returning output
/// channel 0.
///
/// One impulse at [`IMPULSE_AT`] and silence everywhere else, so every nonzero
/// sample in the result is an arrival and its index is an arrival time.
fn render_impulse(graph: &mut Renderer) -> Vec<f32> {
    let silence = [0.0f32; BLOCK];
    let mut captured = Vec::with_capacity(TOTAL_FRAMES);

    // Warm the pipeline on silence before the impulse goes in.
    //
    // The out-of-process pipeline submits chunk N and collects chunk N-1, and
    // `Batcher::collectable` substitutes silence for any chunk the server has
    // not answered yet — start-up, or a chunk that missed its budget. Those
    // silences are correct, but they are *lossy*: a starved chunk does not
    // arrive late, it never arrives. Feeding the impulse into a cold pipeline
    // therefore risks dropping the one sample the whole assertion rests on, and
    // the failure reads as "the plugin contributed nothing" — which is also
    // what a real routing bug looks like.
    //
    // Warming on silence costs nothing to assert against (silence in, silence
    // out) and moves the impulse past the start-up transient, so a drop here
    // would have to be a genuine mid-run starvation rather than the cold start
    // every run has.
    for _ in 0..WARMUP_BLOCKS {
        graph.render_input(&[&silence]);
        std::thread::sleep(PACE);
    }

    for block in 0..BLOCKS {
        let mut input = [0.0f32; BLOCK];
        let base = block * BLOCK;
        if (base..base + BLOCK).contains(&IMPULSE_AT) {
            input[IMPULSE_AT - base] = IMPULSE;
        }
        let out = graph.render_input(&[&input]);
        captured.extend_from_slice(&out[0]);
        // Pace to the real block period.
        //
        // Not politeness — correctness. The pipeline never waits for a reply, so
        // the subprocess needs wall-clock time between blocks to have rendered
        // anything. A tight loop runs thousands of blocks inside one block
        // period, the server never publishes, and every block reads back
        // silence — so the plugin path presents as "contributed nothing" when
        // what actually happened is that it was never given a chance to run.
        // `real_plugin_pressure.rs` documents the same constraint.
        std::thread::sleep(PACE);
    }
    captured
}

/// Indices of every sample that is not exactly zero, with its value.
fn arrivals(samples: &[f32]) -> Vec<(usize, f32)> {
    samples
        .iter()
        .enumerate()
        .filter(|(_, &s)| s != 0.0)
        .map(|(i, &s)| (i, s))
        .collect()
}

/// A native graph with one global input and one output, holding `plugin`
/// fed by the input on every port; `sum` decides what reaches the output.
struct Rig {
    graph: Renderer,
    plugin: NodeKey,
    controls: PluginControls,
}

/// Build the two-path graph: one input fanned to a plugin path and a dry path,
/// summed into output channel 0. The compiler's PDC pass aligns the two, as it
/// does for every graph (doc 013 §3, "Latency solve").
///
/// **`set_source` per edge, never `connect`/`pipe`.** The latter walk *every*
/// port of a node, so a later wiring call silently clobbers an earlier one.
fn two_paths(plugin: PluginClient<Bound>) -> Rig {
    // The probe is a stereo effect (2 in, 2 out). Both of its ports are fed
    // from the same mono impulse and only channel 0 is read back: an unwired
    // port renders silence, which out of the plugin path is indistinguishable
    // from "the plugin contributed nothing", exactly what this suite measures.
    let plugin_inputs = plugin.inputs();
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let (key, controls) = g.add_with_controls(plugin);
    // The dry twin. A pass-through rather than wiring the global input straight
    // to the sum, so each path is a node the compiler aligns.
    let dry = g.add_unit(Box::new(Through::mono()));
    // Summing is a node's job — a port holds one source, so a fan-in has to be
    // an explicit adder. `ChannelSumNode`, which sums rather than averages: two
    // aligned arrivals of `IMPULSE` must come out as `2 · IMPULSE`, which one
    // arrival alone cannot produce.
    let sum = g.add_unit(Box::new(ChannelSumNode::new(2, ChannelLayout::MONO)));

    for port in 0..plugin_inputs {
        g.set_source(key, port, Source::Global(0));
    }
    g.set_source(dry, 0, Source::Global(0));
    g.set_source(sum, 0, Source::Node(OutPort { node: key, port: 0 }));
    g.set_source(sum, 1, Source::Node(OutPort { node: dry, port: 0 }));
    g.set_output(0, Source::Node(OutPort { node: sum, port: 0 }));
    finish(g, key, controls)
}

/// The plugin alone: input to every port, its port 0 to the output. No other
/// path, so the compiler inserts no delay and the output is exactly what the
/// plugin path suffers.
fn plugin_alone(plugin: PluginClient<Bound>) -> Rig {
    let plugin_inputs = plugin.inputs();
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let (key, controls) = g.add_with_controls(plugin);
    for port in 0..plugin_inputs {
        g.set_source(key, port, Source::Global(0));
    }
    g.set_output(0, Source::Node(OutPort { node: key, port: 0 }));
    finish(g, key, controls)
}

fn finish(g: GraphBuilder, plugin: NodeKey, controls: PluginControls) -> Rig {
    let graph = g
        .renderer(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)))
        .expect("the rig compiles");
    Rig {
        graph,
        plugin,
        controls,
    }
}

impl Rig {
    /// The compiled plan's total latency: the worst path's.
    fn total_latency(&self) -> Samples {
        self.graph
            .editor()
            .base()
            .expect("committed")
            .total_latency()
            .samples()
    }
}

// ---------------------------------------------------------------------------
// The fixture agrees with the plugin.
// ---------------------------------------------------------------------------

/// The host reports the plugin's own declared latency, and the node declares
/// that plus the pipeline chunk — at insert, to the compiler, and through the
/// controls a host keeps.
///
/// Three readings, because they are different claims and the second is the
/// one the planner uses. `PluginClient::latency()` is the plugin's figure;
/// the node's `Shape` adds the chunk the pipeline holds; the controls'
/// `declared_latency` is what a host hands `Editor::set_latency` when the
/// figure moves, and must be the same sum. A test that checked only the first
/// would pass while the planner compensated by the wrong amount — which is
/// precisely the bug this suite exists to make visible.
///
/// Mutation: declare `Latency::new(self.latency())` in the node's `Shape`
/// (drop the pipeline) → the graph's figure is 137 → fails. Mutation: the
/// same in `PluginControls::declared_latency` → fails the third.
#[test]
fn the_probe_declares_the_latency_this_suite_expects() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);

    assert_eq!(
        probe.client.latency(),
        Samples(PROBE_LATENCY),
        "the host must report the plugin's own declared figure"
    );

    let rig = plugin_alone(probe.client.bind());
    assert_eq!(
        rig.graph.editor().spec().topology.nodes[&rig.plugin].latency,
        Samples(EXPECTED_TOTAL_LATENCY),
        "the NODE must declare the plugin's figure PLUS the pipeline chunk \
         ({PROBE_LATENCY} + {BLOCK}); only this reaches the compiler's PDC"
    );
    assert_eq!(
        rig.controls.declared_latency().samples(),
        Samples(EXPECTED_TOTAL_LATENCY),
        "the controls a host keeps declare the same sum"
    );
}

// ---------------------------------------------------------------------------
// What the plugin path suffers.
// ---------------------------------------------------------------------------

/// The plugin path, alone, arrives exactly [`EXPECTED_TOTAL_LATENCY`] late.
///
/// The baseline the compensated test is measured against, and a real assertion
/// in its own right: it pins the *actual* delay the audio suffers, which is the
/// third of the three numbers that must agree. The two tests together say the
/// declared figure and the suffered delay are the same; either alone says only
/// that one of them has some value. (Under `Net` this ran the two-path graph
/// uncompensated; the native graph always compensates, so the plugin path is
/// measured on its own, where there is nothing to compensate.)
#[test]
fn the_plugin_path_alone_arrives_a_full_latency_late() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle;

    let mut rig = plugin_alone(probe.client.bind());
    let out = render_impulse(&mut rig.graph);
    assert!(
        !handle.status().is_dead(),
        "the plugin must survive the run"
    );

    let hits = arrivals(&out);
    assert_eq!(hits.len(), 1, "exactly one arrival, got {hits:?}");
    let (wet_at, wet_v) = hits[0];
    assert_eq!(
        wet_at - IMPULSE_AT,
        EXPECTED_TOTAL_LATENCY,
        "the plugin path must arrive exactly {EXPECTED_TOTAL_LATENCY} samples \
         late ({PROBE_LATENCY} declared + {BLOCK} pipeline). A gap of \
         {PROBE_LATENCY} means the pipeline chunk is not really there; a gap of \
         {BLOCK} means the plugin is not delaying."
    );
    assert_eq!(
        wet_v, IMPULSE,
        "the plugin path passes the impulse unchanged"
    );
}

// ---------------------------------------------------------------------------
// Compensated: they land together.
// ---------------------------------------------------------------------------

/// With compensation both paths arrive on the **same** sample, summing to
/// exactly twice the impulse, and there is nothing anywhere else.
///
/// The three assertions are one property split so a failure names itself:
/// *one* arrival (not two) says the paths were aligned; the value `2 * IMPULSE`
/// says both really arrived and neither was dropped; and the total nonzero count
/// says nothing was smeared — a compensation that inserted a filter rather than
/// a delay would still produce a peak at the right place with energy either side
/// of it.
#[test]
fn with_compensation_both_paths_land_on_the_same_sample() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle;

    let mut rig = two_paths(probe.client.bind());
    assert_eq!(
        rig.total_latency(),
        Samples(EXPECTED_TOTAL_LATENCY),
        "the plan's total is the worst path's latency"
    );

    let out = render_impulse(&mut rig.graph);
    assert!(
        !handle.status().is_dead(),
        "the plugin must survive the run"
    );

    let hits = arrivals(&out);
    assert_eq!(
        hits.len(),
        1,
        "compensation must collapse the two arrivals into one, got {hits:?} — \
         two arrivals means the dry path was not delayed to meet the plugin"
    );

    let (at, value) = hits[0];
    assert_eq!(
        at,
        IMPULSE_AT + EXPECTED_TOTAL_LATENCY,
        "the aligned arrival sits at the impulse plus the graph's latency"
    );
    assert_eq!(
        value,
        2.0 * IMPULSE,
        "both paths must contribute: {IMPULSE} means one of them was lost, and \
         anything else means the impulse was scaled on the way through"
    );
}

/// **A plugin latency change reaches PDC at the next commit.** The plugin's
/// figure moves (what its `latency.changed` delivers into its cell), the host
/// hands the controls' declared latency to `Editor::set_latency` — the node's
/// `Shape` changes — and the next commit re-plans the graph around it.
///
/// A plugin may raise its latency mid-session — a CLAP host is told so through
/// `clap.latency`'s host extension, and `PluginClient::set_latency` is the seam
/// that lands it. What that does *not* do on its own is re-run PDC: the node's
/// figure is the editor's until told, so the graph is misaligned by exactly
/// the difference until something re-plans. This is the case a static rig
/// cannot reach, and the one where a stale compensation is most damaging — the
/// graph was aligned, so nothing looks broken until you measure it.
///
/// Mutation: skip the `set_latency` call → the total stays 201 and the paths
/// stay aligned → fails. Mutation: `declared_latency` without the pipeline
/// chunk → the total is 201 + 64 − 64 → fails.
#[test]
fn a_latency_change_reaches_pdc_at_the_next_commit() {
    const EXTRA: usize = 64;
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle;

    let mut rig = two_paths(probe.client.bind());
    assert_eq!(rig.total_latency(), Samples(EXPECTED_TOTAL_LATENCY));

    // The plugin now claims more latency than it did at load. Its *rendering*
    // is unchanged — the probe still delays by 137 — which is deliberate: the
    // question here is whether the planner re-reads the declaration and
    // re-plans, not whether the audio moved.
    rig.controls.set_latency(Samples(PROBE_LATENCY + EXTRA));
    let declared = rig.controls.declared_latency();
    rig.graph
        .editor_mut()
        .set_latency(rig.plugin, declared)
        .expect("a latency inside what PDC compensates");
    rig.graph
        .editor_mut()
        .commit()
        .expect("the re-plan commits");

    assert_eq!(
        rig.total_latency(),
        Samples(EXPECTED_TOTAL_LATENCY + EXTRA),
        "the re-plan must carry the plugin's NEW figure plus the pipeline chunk"
    );

    let out = render_impulse(&mut rig.graph);
    assert!(
        !handle.status().is_dead(),
        "the plugin must survive the run"
    );

    let hits = arrivals(&out);
    // The dry path is now delayed by the *claimed* total while the plugin still
    // renders its real 137, so the two separate again — by exactly the amount
    // the plugin over-claimed. That is the correct behaviour for a host that
    // believes its plugin, and asserting it pins that the re-plan used the new
    // figure rather than silently keeping the old one.
    assert_eq!(
        hits.len(),
        2,
        "the plugin over-claims by {EXTRA}, so the paths must separate by that \
         much: {hits:?}"
    );
    assert_eq!(
        hits[1].0 - hits[0].0,
        EXTRA,
        "the separation must be exactly the over-claim. Zero would mean the \
         re-plan never happened and the graph is still carrying the old delays."
    );
}
