//! The native graph backend (`RenderGraph::Graph`, doc 013 Phase 3 PR 7)
//! against the `Net` backend it will replace.
//!
//! # The oracle is the other backend
//!
//! Every case builds one graph twice from the same units — once as the `Net`
//! the existing suites render, once with `tutti_graph::GraphBuilder` — and
//! asserts the two exports are **bit-identical**: the same planes out of
//! `render_to_buffers`, the same bytes out of `render_to_file`. The `Net` path
//! is the one the oracle suites beside this file (`oracle_resample.rs`,
//! `dither_stats.rs`, `surround_export.rs`, `render.rs`'s dBTP cases) already
//! check against first principles, so equality here carries those checks over
//! without restating them.
//!
//! Equality is exact, not a tolerance, and is portable: both sides run the same
//! unit code on the same machine, so a libm `sin` that differs across C
//! runtimes differs on both sides alike.
//!
//! # Blocks
//!
//! The `Net` renders 64-frame blocks and the graph `GRAPH_MAX_BLOCK` (1024).
//! A `Legacy` unit is run in 64-frame chunks from each block's start, so at a
//! multiple of 64 every chunk lands on the frames a `Net` block does, and even
//! a block-oriented unit (the convolver's FFT partitions) agrees. That is why
//! `GRAPH_MAX_BLOCK` is a multiple of 64; the durations below are deliberately
//! *not*, so the last block is short on both sides.
//!
//! # What these do not cover
//!
//! `Net`'s `ping` seeding of noise generators has no graph counterpart (doc
//! 013), so no case here uses a seeded generator.

#![cfg(feature = "wav")]

use std::sync::Arc;

use tutti_core::dsp::Net;
use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Amplitude, AudioUnit, Beat, Bpm, Hz, SampleRate};
use tutti_export::{
    render_normalized_to_file, render_to_buffers, render_to_file, AudioFormat, BitDepth,
    ChannelLayout, Dither, EncodeConfig, Error, ExportConfig, FrozenClock, Normalize,
    RenderConfig, RenderGraph, Resample, GRAPH_MAX_BLOCK,
};
use tutti_graph::{ForkMode, ForkTarget, GraphBuilder, Legacy, Prepare};
use tutti_nodes::testing::{Const, Osc};
use tutti_types::{Db, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// 0.3013 s at 48 kHz is 14 462 frames: 14 × 1024 + 126, and 225 × 64 + 62.
/// Neither backend's last block is full.
const SECS: f64 = 0.3013;

fn config(bit_depth: BitDepth, channels: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: SECS,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth,
            channels,
        },
        dither: Dither::Off,
        ..Default::default()
    }
}

/// The builder's graph, built for the export.
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::Graph { editor, executor }
}

/// The builder's graph as an export gets it from a live one: built at a
/// device's block, then forked offline at the render's.
///
/// A fork **resets** every unit it makes (fundsp's sequence: clone, isolate,
/// rebind, reset), so it is compared against a reset `Net` — which is what
/// today's export does to the `Net` it clones. Against a fresh one it would
/// differ, and not by a bug: a reset `VbapPannerNode` starts on its commanded
/// bearing where a fresh one glides there from front-centre.
fn forked(g: GraphBuilder) -> RenderGraph {
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");
    let timeline: OfflineTransport = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: RATE,
        ..Default::default()
    }));
    RenderGraph::fork(&live, ForkTarget::Master, ForkMode::Offline(&timeline), RATE)
        .expect("every node here is forkable")
}

/// A graph built twice from the same units: `(net, builder)`.
type Pair = (Net, GraphBuilder);

/// A stereo sine at half scale, `Osc` wired to both outputs.
fn sine(freq: f32) -> Pair {
    let unit = || {
        Osc::sine(Hz(freq))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO)
    };
    let mut net = Net::new(0, 2);
    let id = net.push(Box::new(unit()));
    net.pipe_output(id);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let k = g.add_unit(Box::new(unit()));
    g.pipe_output(k);
    (net, g)
}

/// A mono DC level fanned to stereo.
fn dc(level: f32) -> Pair {
    let mut net = Net::new(0, 2);
    let id = net.push(Box::new(Const::mono(level)));
    net.pipe_output(id);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let k = g.add_unit(Box::new(Const::mono(level)));
    g.pipe_output(k);
    (net, g)
}

/// A tone through a lookahead limiter: a latency-bearing chain.
fn limited() -> Pair {
    let limiter = || {
        tutti_nodes::LimiterNode::with_channels(ChannelLayout::STEREO, Db(-6.0), Db(-1.0))
            .with_lookahead(tutti_types::Seconds(0.005))
    };
    let tone = || {
        Osc::sine(Hz(220.0))
            .with_amplitude(Amplitude(0.9))
            .with_layout(ChannelLayout::STEREO)
    };
    let mut net = Net::new(0, 2);
    let src = net.push(Box::new(tone()));
    let lim = net.push(Box::new(limiter()));
    net.pipe_all(src, lim);
    net.pipe_output(lim);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add_unit(Box::new(tone()));
    let lim = g.add_unit(Box::new(limiter()));
    g.pipe(src, lim).pipe_output(lim);
    (net, g)
}

/// A decaying tone through a convolver: a tail, a latency, and a
/// block-oriented unit.
fn convolved() -> Pair {
    let ir: Vec<f32> = (0..3000).map(|i| 0.9f32.powi(i / 40) * 0.05).collect();
    let tone = || Osc::sine(Hz(330.0)).with_amplitude(Amplitude(0.5));
    let mut net = Net::new(0, 1);
    let src = net.push(Box::new(tone()));
    let conv = net.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir)));
    net.connect(src, 0, conv, 0);
    net.pipe_output(conv);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add_unit(Box::new(tone()));
    let conv = g.add_unit(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir)));
    g.connect(src, 0, conv, 0).pipe_output(conv);
    (net, g)
}

/// Quad VBAP: a source at front-left and one at rear-left, each panned and
/// summed into a 4-wide master — `build_vbap_mix`'s quad graph (no LFE send),
/// written out because that helper takes `&mut Net` (doc 013, PR 4 notes).
fn quad_vbap() -> Pair {
    use tutti_nodes::ChannelSumNode;
    use tutti_spatial::VbapPannerNode;
    let panner = |az: f32| {
        let p = VbapPannerNode::for_layout(ChannelLayout::QUAD).expect("quad");
        p.set_position(az, tutti_core::Elevation::LEVEL);
        p
    };
    let tone = |f: f32| {
        Osc::sine(Hz(f))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO)
    };

    let mut net = Net::new(0, 4);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let sum_n = net.push(Box::new(ChannelSumNode::new(2, ChannelLayout::QUAD)));
    let sum_g = g.add_unit(Box::new(ChannelSumNode::new(2, ChannelLayout::QUAD)));
    for (s, (az, f)) in [(45.0, 300.0), (135.0, 500.0)].into_iter().enumerate() {
        let src = net.push(Box::new(tone(f)));
        let pan = net.push(Box::new(panner(az)));
        net.pipe_all(src, pan);
        let gsrc = g.add_unit(Box::new(tone(f)));
        let gpan = g.add_unit(Box::new(panner(az)));
        g.pipe(gsrc, gpan);
        for c in 0..4 {
            net.connect(pan, c, sum_n, s * 4 + c);
            g.connect(gpan, c, sum_g, s * 4 + c);
        }
    }
    net.pipe_output(sum_n);
    g.pipe_output(sum_g);
    (net, g)
}

/// How the graph side is made.
#[derive(Clone, Copy)]
enum Via {
    /// [`built`].
    Built,
    /// [`forked`], against a reset `Net`.
    Forked,
}

/// Both backends into buffers; asserts the planes are bit-identical, and
/// returns them.
fn same_buffers(pair: Pair, config: &ExportConfig, via: Via) -> Vec<Vec<f32>> {
    let (mut net, g) = pair;
    let graph = match via {
        Via::Built => built(g),
        Via::Forked => {
            // As the export that clones a `Net` does: re-rate, then reset
            // (see `forked`).
            net.set_sample_rate(RATE);
            net.reset();
            forked(g)
        }
    };
    let a = render_to_buffers(net, config, &FrozenClock).expect("net renders");
    let b = render_to_buffers(graph, config, &FrozenClock).expect("graph renders");
    assert_eq!(a.sample_rate, b.sample_rate);
    assert_eq!(a.channels(), b.channels());
    assert_eq!(a.frames(), b.frames(), "the two backends differ in length");
    for (c, (x, y)) in a.planes.iter().zip(&b.planes).enumerate() {
        if let Some(i) = x.iter().zip(y).position(|(p, q)| p.to_bits() != q.to_bits()) {
            panic!(
                "{}-wide: channel {c} differs first at frame {i}: net {} graph {}",
                a.channels(),
                x[i], y[i]
            );
        }
    }
    a.planes
}

/// Both backends to a file each; asserts the files are byte-identical.
fn same_file(
    pair: Pair,
    write: impl Fn(RenderGraph, &std::path::Path) -> tutti_export::Result<tutti_export::Written>,
) -> Vec<u8> {
    let (net, g) = pair;
    let d = tempfile::tempdir().unwrap();
    let (pn, pg) = (d.path().join("net.wav"), d.path().join("graph.wav"));
    write(net.into(), &pn).expect("net writes");
    write(built(g), &pg).expect("graph writes");
    let (a, b) = (std::fs::read(&pn).unwrap(), std::fs::read(&pg).unwrap());
    assert_eq!(a.len(), b.len(), "the two files differ in length");
    assert!(a == b, "the two files differ");
    a
}

/// Not vacuous: a render that is all zeros would agree with anything.
fn assert_audible(planes: &[Vec<f32>]) {
    let peak = planes
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "the render is (nearly) silent: peak {peak}");
}

/// The sine oracle's graph, through both backends, built for the export and
/// forked from a live graph.
///
/// Mutation (run): render the graph path's frames folded from plane 0 only
/// (`fold_graph_frame` reading `planes[0]` for every channel) → still equal
/// here (both channels carry the same sine), so the surround case below is
/// what catches it; skip the graph's last short block (`block_size` rounded
/// down to a multiple of 64 in `GraphSource::fill`) → the lengths differ.
#[test]
fn a_sine_renders_bit_identically_through_both_backends() {
    let cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    let planes = same_buffers(sine(1000.0), &cfg, Via::Built);
    assert_audible(&planes);
    assert_eq!(planes[0].len(), 14_462);
    same_buffers(sine(1000.0), &cfg, Via::Forked);
}

/// The resample oracle's case (48 k → 44.1 k), to a file: the conversion,
/// the gate and the encoder behind both backends are one path, so equal
/// renders make equal bytes. The graph pulls 1024-frame blocks and the `Net`
/// 64: the resampler's carry makes its output independent of the partition,
/// and this pins that too.
///
/// Mutation (run): scale the graph path's output by `1.0 + f32::EPSILON` in
/// `GraphSource::fill` → the files differ.
#[test]
fn a_resampled_export_is_byte_identical_through_both_backends() {
    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.resample = Some(Resample::to(SampleRate(44_100.0)));
    let bytes = same_file(sine(1000.0), |g, p| {
        render_to_file(g, &cfg, &FrozenClock, p)
    });
    let r = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(r.spec().sample_rate, 44_100, "not vacuous: it resampled");
}

/// The dBTP case: a peak-normalized export (render, measure, gain, write)
/// through both backends lands on the same bytes.
///
/// Mutation (run): as above; also a `GraphSource` that renders the first
/// block twice (not advancing `produced`) → the files differ.
#[test]
fn a_peak_normalized_export_is_byte_identical_through_both_backends() {
    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = 1.0;
    same_file(sine(440.0), |g, p| {
        render_normalized_to_file(g, &cfg, &FrozenClock, Normalize::peak(Db(-1.0)), p)
    });
}

/// The dither case: triangular dither to 16 bits, on a DC level between two
/// codes so every sample is perturbed. Dither is seeded per export and runs
/// sample by sample, so equal renders dither identically.
#[test]
fn a_dithered_export_is_byte_identical_through_both_backends() {
    let mut cfg = config(BitDepth::Int16, ChannelLayout::STEREO);
    cfg.dither = Dither::Triangular;
    let bytes = same_file(dc(0.25 + 0.3 / 32_768.0), |g, p| {
        render_to_file(g, &cfg, &FrozenClock, p)
    });
    // Not vacuous: the dither moved samples off the one code.
    let s: Vec<i16> = hound::WavReader::new(std::io::Cursor::new(bytes))
        .unwrap()
        .into_samples::<i16>()
        .map(|s| s.unwrap())
        .collect();
    assert!(s.iter().any(|&x| x != s[0]), "dither left the level flat");
}

/// The surround case: a quad VBAP mix, exported at its own width and folded
/// down to stereo and to mono by the ITU matrix, through both backends.
///
/// Mutation (run): `fold_graph_frame` reading `planes[0]` for every source
/// channel → the quad render differs on channel 1.
#[test]
fn a_surround_mix_renders_bit_identically_at_every_width() {
    for width in [ChannelLayout::QUAD, ChannelLayout::STEREO, ChannelLayout::MONO] {
        let planes = same_buffers(quad_vbap(), &config(BitDepth::Float32, width), Via::Built);
        assert_audible(&planes);
    }
    let quad = same_buffers(quad_vbap(), &config(BitDepth::Float32, ChannelLayout::QUAD), Via::Forked);
    // Not vacuous: the rear-left source put energy in channel 2.
    assert!(quad[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
}

/// A convolver — block-oriented, latency- and tail-bearing — renders
/// bit-identically, which is the 64-frame chunking claim in the module docs.
///
/// Mutation (run): `GRAPH_MAX_BLOCK = 1000` (not a multiple of 64) → the
/// convolver's partitions fall on other frames and the planes differ.
#[test]
fn a_convolver_renders_bit_identically_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get() % 64, 0);
    let planes = same_buffers(convolved(), &config(BitDepth::Float32, ChannelLayout::MONO), Via::Built);
    assert_audible(&planes);
}

/// The graph reports a lookahead limiter's latency as the `Net` does, and a
/// render trimmed by it is the `Net`'s trimmed render, sample for sample.
///
/// Mutation (run): `RenderGraph::reported_latency` returning
/// `Samples::ZERO` for the graph → the figures differ (the limiter reports
/// 240 frames at 48 kHz).
#[test]
fn the_latency_trim_equals_the_net_paths() {
    let (mut net, g) = limited();
    let mut graph = built(g);
    // A `Net` answers at whatever rate it was last set to — 44.1 kHz for
    // one never rendered, where this limiter reports 221 frames, not 240 —
    // so it is re-rated to the render's first, as a caller must. The graph
    // answers at the rate it was prepared at and cannot be asked early.
    net.set_sample_rate(RATE);
    let net_latency = RenderGraph::Net(net.clone()).reported_latency();
    let graph_latency = graph.reported_latency();
    assert_eq!(net_latency, tutti_export::reported_latency(&mut net));
    assert!(net_latency.get() > 0, "not vacuous: the limiter looks ahead");
    assert_eq!(graph_latency, net_latency);

    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.latency = graph_latency;
    let (net, g) = limited();
    let planes = same_buffers((net, g), &cfg, Via::Built);
    assert_eq!(planes[0].len(), 14_462, "the trim does not shorten the output");
    assert_audible(&planes);
}

/// The graph reports a convolver's tail as the `Net` does, and a render
/// extended by it is the `Net`'s, sample for sample.
///
/// Mutation (run): `RenderGraph::reported_tail` folding an empty topology
/// for the graph → `Some(0)` against the convolver's 2999.
#[test]
fn the_tail_length_equals_the_net_paths() {
    let (net, g) = convolved();
    let graph = built(g);
    let net_tail = tutti_export::reported_tail(&net);
    assert_eq!(net_tail.samples(), Some(Samples(2999)), "the IR's ring-out");
    assert_eq!(graph.reported_tail(), net_tail);
    assert_eq!(RenderGraph::Net(net).reported_tail(), net_tail);

    let mut cfg = config(BitDepth::Float32, ChannelLayout::MONO);
    cfg.render.tail = graph.reported_tail().samples().unwrap();
    // The latency trim too, so both halves of the gate run together.
    let mut probe = built(convolved().1);
    cfg.render.latency = probe.reported_latency();
    let planes = same_buffers(convolved(), &cfg, Via::Built);
    assert_eq!(planes[0].len(), 14_462 + 2999);
}

/// A graph holding a node that cannot be forked (a plugin, a mic monitor —
/// here a `Legacy` built unforkable) is refused with the node's key, and
/// nothing renders.
///
/// Mutation (run): map every `ForkError` to `Error::Fork` in
/// `RenderGraph::fork` → the match below fails.
#[test]
fn an_unforkable_node_is_an_export_error_naming_it() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let fine = g.add_unit(Box::new(Const::mono(0.1)));
    let plugin = g.add(Legacy::from_box(Box::new(Const::mono(0.2))).unforkable());
    g.connect_output(fine, 0, 0).connect_output(plugin, 0, 1);
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");

    let err = match RenderGraph::fork(&live, ForkTarget::Master, ForkMode::Live, RATE) {
        Ok(_) => panic!("forked a graph holding an unforkable node"),
        Err(e) => e,
    };
    let text = err.to_string();
    match err {
        Error::NotForkable { key } => assert_eq!(key, plugin),
        other => panic!("expected NotForkable, got {other:?}"),
    }
    assert!(text.contains(&format!("{plugin:?}")), "{text}");

    // Only what the target needs must fork: the other branch does.
    assert!(RenderGraph::fork(&live, ForkTarget::Node(fine), ForkMode::Live, RATE).is_ok());
}

/// A pair prepared at another rate is refused, not rendered at the wrong one.
///
/// Mutation (run): drop the rate check in `GraphSource::new` → it renders.
#[test]
fn a_graph_prepared_at_another_rate_is_refused() {
    let (_, g) = sine(1000.0);
    let (editor, executor) = g
        .build(RenderGraph::prepare(SampleRate(44_100.0)))
        .expect("builds");
    let r = render_to_buffers(
        RenderGraph::Graph { editor, executor },
        &config(BitDepth::Float32, ChannelLayout::STEREO),
        &FrozenClock,
    );
    assert!(matches!(r, Err(Error::InvalidConfig(_))), "{r:?}");
}

/// The graph is handed the render clock's transport, block by block, read
/// before each block and advanced after: an `EnvClock` in the graph emits the
/// offline timeline's beat on every frame, starting at its start beat, and the
/// timeline ends exactly the render's frames on.
///
/// Mutations (run):
/// - `GraphSource::fill` passing `Transport::default()` instead of the
///   clock's → every beat reads 0;
/// - `RenderClock::render_graph` advancing before processing → the first
///   frame reads a block past the start beat.
#[test]
fn the_graph_reads_the_render_clocks_transport() {
    let start = 4.0;
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(start),
        tempo: Bpm(120.0),
        sample_rate: RATE,
        loop_range: None,
    });
    let bps = timeline.beats_per_sample().get();

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let clock = g.add(tutti_core::EnvClock::new());
    g.connect_output(clock, 0, 0).connect_output(clock, 1, 1);
    let out = render_to_buffers(
        built(g),
        &config(BitDepth::Float32, ChannelLayout::STEREO),
        &timeline,
    )
    .expect("renders");

    let frames = out.frames().get();
    for i in 0..frames {
        let beat = f64::from(out.planes[0][i]) + f64::from(out.planes[1][i]);
        let want = start + bps * i as f64;
        assert!(
            (beat - want).abs() < 1e-5,
            "frame {i}: the graph read beat {beat}, the timeline was at {want}"
        );
    }
    assert!(
        (timeline.beat().get() - (start + bps * frames as f64)).abs() < 1e-9,
        "the timeline must advance by exactly the frames rendered"
    );
}
