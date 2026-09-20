//! Does plugin delay compensation actually align a plugin path against a dry
//! one — through a **real out-of-process plugin**, end to end?
//!
//! Every existing latency test in this tree stops one step short of the
//! question. `vst2_latency.rs` and the VST3 suites assert that the host *reads*
//! the plugin's declared figure; `tutti_types::latency`'s own tests assert the
//! planner's graph math against a hand-built fixture; `bevy-tutti`'s assert that
//! the compensation system runs and republishes. None of them puts a real plugin
//! in a real `Net` beside a real dry path and looks at the samples, so the one
//! thing that could still be wrong — that the figure the host reports, the
//! figure the planner plans against, and the delay the audio actually suffers
//! are the *same number* — was untested.
//!
//! It is not one number, which is exactly why. An out-of-process plugin's
//! latency is its own declared figure **plus** the block the pipeline holds:
//! `PluginClient` submits block N and collects block N−1, so a sample entering
//! leaves one `BATCH_SIZE` block later than the plugin alone accounts for.
//! `AudioUnit::route` declares the sum ([`EXPECTED_TOTAL_LATENCY`]), and getting
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
//! With compensation, both arrivals land on the same sample and sum to exactly
//! twice the impulse. Without it, they land [`EXPECTED_TOTAL_LATENCY`] apart.
//! The two runs are the same graph and the same driving loop; only
//! `latency::compensate` is called or not.
//!
//! # Why `Net` directly rather than `tutti_core::topology::compile`
//!
//! `compile` builds nodes from a `Catalog` keyed by a **kind string**, so
//! expressing a plugin node means writing a `Catalog` impl whose `build` loads a
//! subprocess — a fixture bigger than the test, and one that would put a second
//! construction path under assertions meant for the first. `Net` is also what
//! `bevy_tutti::graph::latency::compensate_graph` itself drives
//! (`latency::compensate(&mut graph.0)`), so this exercises the production path
//! rather than a parallel one. `Net` implements both `LatencyGraph` and
//! `DelayInsertion` (`fundsp-tutti/src/latency/mod.rs`), which is all the
//! planner needs.
//!
//! Nothing here calls `Net::commit`, and its absence is deliberate rather than
//! an omission: `commit` publishes a graph to a *backend* for the audio thread
//! to render, and asserts one exists. These tests drive `AudioUnit::process` on
//! the `Net` itself, which reads the front graph directly — so a commit would
//! have nothing to publish to and panics on the assertion. A host that renders
//! through a backend commits; a test that renders the graph in place does not.

#![cfg(feature = "clap")]

// `#[path]` rather than a `support/mod.rs`: this crate's suites each pull in
// only the fixtures they use, which is what keeps a VST2 binary from compiling
// the CLAP harness (and its `tutti-clap-test-plugin` dev-dependency) at all.
#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv};

use tutti_core::dsp::Net;
use tutti_core::{latency, AudioUnit, BufferVec, SampleRate, Samples, F32};

const SAMPLE_RATE: f64 = 48_000.0;

/// fundsp's block size, and the one block the out-of-process pipeline holds.
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
/// plus the block the pipeline holds.
///
/// The sum is the thing under test. `PluginClient::latency()` reports only the
/// first term — it is the plugin's declaration — while `AudioUnit::route` adds
/// the second, and only `route` reaches `latency::plan`.
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

/// Render `BLOCKS` blocks of an impulse through `net`, returning output
/// channel 0.
///
/// One impulse at [`IMPULSE_AT`] and silence everywhere else, so every nonzero
/// sample in the result is an arrival and its index is an arrival time.
fn render_impulse(net: &mut Net) -> Vec<f32> {
    let mut input = BufferVec::<F32>::new(1);
    let mut output = BufferVec::<F32>::new(1);
    let mut captured = Vec::with_capacity(TOTAL_FRAMES);

    // Warm the pipeline on silence before the impulse goes in.
    //
    // The out-of-process pipeline submits block N and collects block N-1, and
    // `Batcher::collectable` substitutes silence for any block the server has
    // not answered yet — start-up, or a block that missed its budget. Those
    // silences are correct, but they are *lossy*: a starved block does not
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
        input.clear();
        output.clear();
        net.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        std::thread::sleep(PACE);
    }

    for block in 0..BLOCKS {
        input.clear();
        let base = block * BLOCK;
        if (base..base + BLOCK).contains(&IMPULSE_AT) {
            input.set_scalar(0, IMPULSE_AT - base, IMPULSE);
        }
        output.clear();
        net.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        for i in 0..BLOCK {
            captured.push(output.at_f32(0, i));
        }
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

/// Build the two-path graph: one input fanned to a plugin path and a dry path,
/// summed into output channel 0.
///
/// Returns the `Net` and the plugin's `NodeId`. Nothing is committed and no
/// compensation is run — the caller decides, which is the whole point.
///
/// **`set_source` per edge, never `connect`/`pipe`.** The latter walk *every*
/// port of a node, so a later wiring call silently clobbers an earlier one; this
/// is the same discipline `tutti_core::topology::compile` and
/// `bevy_tutti::graph::wire::rebuild` follow.
fn build_graph(plugin: Box<dyn AudioUnit>) -> Net {
    use tutti_core::dsp::{pass, sum, Source};

    // The probe is a stereo effect (2 in, 2 out), and `Net` requires every input
    // port of every node to have a source — an unwired port renders silence, and
    // silence out of the plugin path is indistinguishable from "the plugin
    // contributed nothing", which is exactly what this suite is trying to
    // measure. So both of its ports are fed from the same mono impulse and only
    // channel 0 is read back.
    let plugin_inputs = AudioUnit::inputs(&*plugin);
    let mut net = Net::new(1, 1);
    net.set_sample_rate(SampleRate(SAMPLE_RATE));

    let plugin_id = net.push(plugin);
    // The dry twin. A `pass` rather than wiring the global input straight to the
    // sum: the planner delays an *edge into a node*, and a path that is only a
    // global-to-output link has no node on it to delay.
    let dry = net.push(Box::new(pass()));
    // Summing is a node's job — `Net` holds one source per input port, so a
    // fan-in has to be an explicit adder.
    //
    // `sum(pass(), pass())` and NOT `join::<U2>()`: join *averages* its inputs,
    // so two aligned arrivals of `IMPULSE` would come out as `IMPULSE` — exactly
    // what one arrival alone produces. The assertion that both paths contributed
    // would then be satisfied by either of them arriving alone, which is the
    // thing it exists to rule out.
    let sum = net.push(Box::new(sum(pass(), pass())));

    for port in 0..plugin_inputs {
        net.set_source(plugin_id, port, Source::Global(0));
    }
    net.set_source(dry, 0, Source::Global(0));
    net.set_source(sum, 0, Source::Local(plugin_id, 0));
    net.set_source(sum, 1, Source::Local(dry, 0));
    net.set_output_source(0, Source::Local(sum, 0));

    net
}

// ---------------------------------------------------------------------------
// The fixture agrees with the plugin.
// ---------------------------------------------------------------------------

/// The host reports the plugin's own declared latency, and the node declares
/// that plus the pipeline block.
///
/// Both halves, because they are different claims and the second is the one the
/// planner uses. `PluginClient::latency()` is the plugin's figure;
/// `AudioUnit::latency()` derives from `route()`, which adds the block the
/// pipeline holds. A test that checked only the first would pass while the
/// planner compensated by the wrong amount — which is precisely the bug this
/// suite exists to make visible.
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

    let mut unit: Box<dyn AudioUnit> = Box::new(probe.client);
    assert_eq!(
        unit.latency().map(|l| l as usize),
        Some(EXPECTED_TOTAL_LATENCY),
        "the NODE must declare the plugin's figure PLUS the pipeline block \
         ({PROBE_LATENCY} + {BLOCK}); only this reaches `latency::plan`"
    );
}

/// Cloning the node carries its latency.
///
/// Not incidental: `LatencyGraph::latency for Net` probes a node by
/// `dyn_clone::clone_box`-ing it, because `AudioUnit::latency` takes `&mut
/// self`. A clone that reported zero would make the planner see an
/// all-zero-latency graph and insert no delays at all — while
/// `PluginClient::latency()` kept answering 137 to anyone who asked directly.
/// The alignment test below would fail, but with a symptom (nothing was
/// compensated) far from the cause, so it is pinned here.
#[test]
fn a_cloned_node_still_declares_its_latency() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);

    let unit: Box<dyn AudioUnit> = Box::new(probe.client);
    let mut cloned = dyn_clone::clone_box(&*unit);
    assert_eq!(
        cloned.latency().map(|l| l as usize),
        Some(EXPECTED_TOTAL_LATENCY),
        "a clone must carry the latency: the planner probes every node by \
         cloning it, so a clone that forgets reports a zero-latency graph"
    );
}

// ---------------------------------------------------------------------------
// Uncompensated: the two paths arrive apart.
// ---------------------------------------------------------------------------

/// Without compensation the dry and plugin arrivals are exactly
/// [`EXPECTED_TOTAL_LATENCY`] apart.
///
/// The baseline the compensated test is measured against, and a real assertion
/// in its own right: it pins the *actual* delay the audio suffers, which is the
/// third of the three numbers that must agree. The two tests together say the
/// declared figure and the suffered delay are the same; either alone says only
/// that one of them has some value.
#[test]
fn without_compensation_the_two_paths_arrive_a_full_latency_apart() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle;

    let mut net = build_graph(Box::new(probe.client));

    let out = render_impulse(&mut net);
    assert!(
        !handle.status().is_dead(),
        "the plugin must survive the run"
    );

    let hits = arrivals(&out);
    assert_eq!(
        hits.len(),
        2,
        "expected exactly two arrivals (dry, then plugin), got {hits:?}"
    );

    let (dry_at, dry_v) = hits[0];
    let (wet_at, wet_v) = hits[1];
    assert_eq!(dry_at, IMPULSE_AT, "the dry path is undelayed");
    assert_eq!(dry_v, IMPULSE, "the dry path passes the impulse unchanged");
    assert_eq!(
        wet_at - dry_at,
        EXPECTED_TOTAL_LATENCY,
        "the plugin path must arrive exactly {EXPECTED_TOTAL_LATENCY} samples \
         late ({PROBE_LATENCY} declared + {BLOCK} pipeline). A gap of \
         {PROBE_LATENCY} means the pipeline block is not really there; a gap of \
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

    let mut net = build_graph(Box::new(probe.client));
    let plan = latency::compensate(&mut net);

    assert_eq!(
        plan.total(),
        Samples(EXPECTED_TOTAL_LATENCY),
        "the plan's total is the worst path's latency"
    );

    let out = render_impulse(&mut net);
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

/// Re-planning after the plugin's latency changes at runtime realigns the graph.
///
/// A plugin may raise its latency mid-session — a CLAP host is told so through
/// `clap.latency`'s host extension, and `PluginClient::set_latency` is the seam
/// that lands it. What that does *not* do on its own is re-run PDC: the node
/// reports a new figure and the delays already spliced into the graph still
/// carry the old one, so the graph is misaligned by exactly the difference until
/// something re-plans.
///
/// This is the case a static rig cannot reach, and the one where a stale
/// compensation is most damaging — the graph was aligned, so nothing looks
/// broken until you measure it.
#[test]
fn re_planning_realigns_after_a_runtime_latency_change() {
    const EXTRA: usize = 64;
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle;
    let client = probe.client.clone();

    let mut net = build_graph(Box::new(probe.client));
    let _ = latency::compensate(&mut net);

    // The plugin now claims more latency than it did at load. Its *rendering*
    // is unchanged — the probe still delays by 137 — which is deliberate: the
    // question here is whether the planner re-reads the declaration and
    // re-splices, not whether the audio moved.
    client.set_latency(Samples(PROBE_LATENCY + EXTRA));
    let replan = latency::compensate(&mut net);

    assert_eq!(
        replan.total(),
        Samples(EXPECTED_TOTAL_LATENCY + EXTRA),
        "the re-plan must read the plugin's NEW figure. The old total means \
         `clear_delays` did not remove the previous compensation, or the node's \
         `route` cached its latency instead of reading the live cell."
    );

    let out = render_impulse(&mut net);
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
