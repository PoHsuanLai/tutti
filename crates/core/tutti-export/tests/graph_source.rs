//! The native graph's exports against what fundsp's `Net` rendered for the
//! same units.
//!
//! # The oracle is a `Net`, rendered here
//!
//! Until doc 013 Phase 3 PR 14 this file compared tutti-export's two backends,
//! `RenderGraph::Net` and the native graph. PR 14 removed the `Net` arm, and
//! with it `NetSource`, the frame source that rendered a `Net`. The
//! comparisons stay, as regression pins on the graph: the `Net` side is now
//! [`net_render`], a **test-only oracle** in this file that renders a
//! `tutti_core::dsp::Net` exactly as `NetSource` and `drive` did — re-rated
//! to the render's rate, 64-frame blocks, the clock advanced after each,
//! every frame folded onto the file's width by `tutti_types::fold_frame`, the
//! head trimmed by the latency and the kept span capped. It uses nothing of
//! tutti-export's but its public types, so it compiles without the `Net` arm;
//! it goes with the rest of the `Net` fixtures (doc 013 PR 15).
//!
//! Every case builds one graph twice from the same units — once as a `Net`,
//! once with `tutti_graph::GraphBuilder` — and asserts the two are
//! **bit-identical**: the same planes, and for the file cases the same bytes
//! (the `Net`'s planes written by `write_buffers`, which replays them through
//! the same encoders in 64-frame blocks, as `NetSource` fed them). The oracle
//! suites beside this file (`oracle_resample.rs`, `dither_stats.rs`,
//! `surround_export.rs`, `render.rs`'s dBTP cases) check the graph path
//! against first principles; this one checks that the graph still renders
//! what the `Net` did.
//!
//! Equality is exact, not a tolerance, and is portable: both sides run the same
//! unit code on the same machine, so a libm `sin` that differs across C
//! runtimes differs on both sides alike.
//!
//! # Blocks
//!
//! The `Net` renders 64-frame blocks and the graph `GRAPH_MAX_BLOCK` (1024).
//! A `Legacy` unit is run in 64-frame chunks from each block's start, so at a
//! multiple of 64 every chunk lands on the frames a `Net` block does, and a
//! unit whose output depends on the call partition (the VBAP panner, which
//! ramps its gains across each call) agrees. That is why
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
use tutti_core::{Amplitude, AudioUnit, Beat, Bpm, BufferRef, BufferVec, Hz, SampleRate};
use tutti_export::{
    duration_to_frames, render_normalized_to_file, render_to_buffers, render_to_file,
    write_buffers, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, Error, ExportConfig,
    FrozenClock, Normalize, RenderClock, RenderConfig, RenderGraph, Rendered, Resample,
    GRAPH_MAX_BLOCK,
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
    RenderGraph { editor, executor }
}

/// The builder's graph as an export gets it from a live one: built at a
/// device's block, then forked offline at the render's.
///
/// A fork **resets** every unit it makes (fundsp's sequence: clone, isolate,
/// rebind, reset), so it is compared against a reset `Net` — which is what
/// the `Net` export did to the `Net` it cloned. Against a fresh one it would
/// differ, and not by a bug: a reset `VbapPannerNode` starts on its commanded
/// bearing where a fresh one glides there from front-centre.
fn forked(g: GraphBuilder) -> RenderGraph {
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");
    let timeline: OfflineTransport = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: RATE,
        ..Default::default()
    }));
    RenderGraph::fork(
        &live,
        ForkTarget::Master,
        ForkMode::Offline(&timeline),
        RATE,
    )
    .expect("every node here is forkable")
}

/// The test-only `Net` oracle: render `net` as tutti-export's `NetSource`
/// and `drive` did before doc 013 PR 14 removed them (see the module docs).
///
/// Re-rated to the render's rate (a `Net` answers at whatever rate it was
/// last set to); `RenderPlan`'s frame counts (the kept span is the duration
/// plus the tail, and the head the latency is trimmed from is rendered on
/// top); 64-frame blocks, each processed with no input and then the clock
/// advanced by it (emit-then-advance); each frame gathered from the `Net`'s
/// real outputs and folded onto the file's width by `fold_frame`.
///
/// Mutation (run): advancing `clock` before `process` → both clip-reader
/// cases part from the graph, so the oracle keeps `NetSource`'s order.
fn net_render(mut net: Net, config: &ExportConfig, clock: &dyn RenderClock) -> Rendered {
    const BLOCK: usize = tutti_core::MAX_BUFFER_SIZE;
    let rate = config.render.sample_rate;
    let ch = config.encode.channels.count() as usize;
    let latency = config.render.latency.get();
    let output_length =
        duration_to_frames(config.render.duration_seconds, rate).get() + config.render.tail.get();
    let total = output_length + latency;

    net.set_sample_rate(rate);
    let n_out = net.outputs();
    let mut scratch = BufferVec::new(n_out.max(1));
    let mut planes = vec![Vec::with_capacity(output_length); ch];
    let mut frame = vec![0.0f32; ch];
    let mut produced = 0;
    while produced < total {
        let n = (total - produced).min(BLOCK);
        let mut out = scratch.buffer_mut();
        net.process(n, &BufferRef::new(&[]), &mut out);
        clock.advance(tutti_types::Samples(n));
        for i in 0..n {
            let at = produced + i;
            if at < latency || at - latency >= output_length {
                continue;
            }
            let src: Vec<f32> = (0..n_out).map(|c| out.channel_f32(c)[i]).collect();
            tutti_types::fold_frame(&src, &mut frame);
            for (plane, &s) in planes.iter_mut().zip(&frame) {
                plane.push(s);
            }
        }
        produced += n;
    }
    Rendered {
        planes,
        sample_rate: rate,
    }
}

/// The look-ahead latency a `Net` reports, as tutti-export's
/// `reported_latency(&mut Net)` answered it before doc 013 PR 14: asked at
/// the net's current rate, floored.
fn net_latency(net: &mut Net) -> Samples {
    Samples(net.latency().unwrap_or(0.0).floor().max(0.0) as usize)
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
/// written out on both sides, so the two backends share nothing here but the
/// units. (The builder form of the helper, `tutti_spatial::vbap_mix_parts`, is
/// pinned against it in tutti-spatial's `tests/vbap_mix_parts.rs`, and
/// `surround_export.rs` renders through it.)
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

/// The graph and the `Net` oracle into buffers; asserts the planes are
/// bit-identical, and returns them.
fn same_buffers(pair: Pair, config: &ExportConfig, via: Via) -> Vec<Vec<f32>> {
    let (mut net, g) = pair;
    let graph = match via {
        Via::Built => built(g),
        Via::Forked => {
            // As the export that cloned a `Net` did: re-rate, then reset
            // (see `forked`).
            net.set_sample_rate(RATE);
            net.reset();
            forked(g)
        }
    };
    let a = net_render(net, config, &FrozenClock);
    let b = render_to_buffers(graph, config, &FrozenClock).expect("graph renders");
    assert_eq!(a.sample_rate, b.sample_rate);
    assert_eq!(a.channels(), b.channels());
    assert_eq!(a.frames(), b.frames(), "the two backends differ in length");
    for (c, (x, y)) in a.planes.iter().zip(&b.planes).enumerate() {
        if let Some(i) = x
            .iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits())
        {
            panic!(
                "{}-wide: channel {c} differs first at frame {i}: net {} graph {}",
                a.channels(),
                x[i],
                y[i]
            );
        }
    }
    a.planes
}

/// The graph to a file through `write`, and the `Net` oracle's planes to
/// another through `write_planes` (the same export, from planes already
/// rendered); asserts the files are byte-identical.
fn same_file(
    pair: Pair,
    config: &ExportConfig,
    write: impl Fn(RenderGraph, &std::path::Path) -> tutti_export::Result<tutti_export::Written>,
    write_planes: impl Fn(&Rendered, &std::path::Path) -> tutti_export::Result<tutti_export::Written>,
) -> Vec<u8> {
    let (net, g) = pair;
    let d = tempfile::tempdir().unwrap();
    let (pn, pg) = (d.path().join("net.wav"), d.path().join("graph.wav"));
    write_planes(&net_render(net, config, &FrozenClock), &pn).expect("net writes");
    write(built(g), &pg).expect("graph writes");
    let (a, b) = (std::fs::read(&pn).unwrap(), std::fs::read(&pg).unwrap());
    assert_eq!(a.len(), b.len(), "the two files differ in length");
    assert!(a == b, "the two files differ");
    a
}

/// Not vacuous: a render that is all zeros would agree with anything.
fn assert_audible(planes: &[Vec<f32>]) {
    let peak = planes.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "the render is (nearly) silent: peak {peak}");
}

/// The sine oracle's graph, through both backends, built for the export and
/// forked from a live graph.
///
/// Mutations (run): `block_size` rounded down to a multiple of 64 in
/// `GraphSource::fill` (the last, short block is never rendered) → the
/// lengths differ; the graph's output scaled by `1.0 + f32::EPSILON` → the
/// planes differ. (Folding every channel from plane 0 passes here — both
/// channels carry the same sine — and fails the surround case.)
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
    let bytes = same_file(
        sine(1000.0),
        &cfg,
        |g, p| render_to_file(g, &cfg, &FrozenClock, p),
        |r, p| write_buffers(r, &cfg, p),
    );
    let r = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(r.spec().sample_rate, 44_100, "not vacuous: it resampled");
}

/// The dBTP case: a peak-normalized export (render, measure, gain, write)
/// through both backends lands on the same bytes.
///
/// Mutation (run): as above → the files differ.
#[test]
fn a_peak_normalized_export_is_byte_identical_through_both_backends() {
    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = 1.0;
    let normalize = Normalize::peak(Db(-1.0));
    same_file(
        sine(440.0),
        &cfg,
        |g, p| render_normalized_to_file(g, &cfg, &FrozenClock, normalize, p),
        // `render_normalized_to_file`'s own steps on planes already rendered:
        // measure, apply, write (no resample here to convert first).
        |r, p| {
            let mut r = r.clone();
            r.apply_gain(normalize.gain_for_rendered(&r)?);
            write_buffers(&r, &cfg, p)
        },
    );
}

/// The dither case: triangular dither to 16 bits, on a DC level between two
/// codes so every sample is perturbed. Dither is seeded per export and runs
/// sample by sample, so equal renders dither identically.
///
/// Mutation (run): as above → the files differ.
#[test]
fn a_dithered_export_is_byte_identical_through_both_backends() {
    let mut cfg = config(BitDepth::Int16, ChannelLayout::STEREO);
    cfg.dither = Dither::Triangular;
    let bytes = same_file(
        dc(0.25 + 0.3 / 32_768.0),
        &cfg,
        |g, p| render_to_file(g, &cfg, &FrozenClock, p),
        |r, p| write_buffers(r, &cfg, p),
    );
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
/// This is also the case that pins the block rule in the module docs: the
/// VBAP panner ramps its gains across each call, so it is the unit here whose
/// output depends on where the 64-frame chunks fall.
///
/// Mutations (run): `fold_graph_frame` reading `planes[0]` for every source
/// channel → the quad render differs on channel 1; `GRAPH_MAX_BLOCK = 1000`
/// (not a multiple of 64) → the panners' ramps restart on other frames and
/// the renders differ.
#[test]
fn a_surround_mix_renders_bit_identically_at_every_width() {
    for width in [
        ChannelLayout::QUAD,
        ChannelLayout::STEREO,
        ChannelLayout::MONO,
    ] {
        let planes = same_buffers(quad_vbap(), &config(BitDepth::Float32, width), Via::Built);
        assert_audible(&planes);
    }
    let quad = same_buffers(
        quad_vbap(),
        &config(BitDepth::Float32, ChannelLayout::QUAD),
        Via::Forked,
    );
    // Not vacuous: the rear-left source put energy in channel 2.
    assert!(quad[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
}

/// A convolver — FFT-partitioned, latency- and tail-bearing — renders
/// bit-identically at the graph's block.
///
/// It turns out not to be the block-sensitive one: it buffers its partitions
/// internally, so `GRAPH_MAX_BLOCK = 1000` still passes here (run) and the
/// surround case is what catches that. The assertion on the constant below
/// is the cheap guard; this test is here for the latency/tail-bearing unit.
///
/// Mutations (run): `block_size` rounded down to a multiple of 64, or the
/// output scaled by `1.0 + f32::EPSILON` → the planes differ.
#[test]
fn a_convolver_renders_bit_identically_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get() % 64, 0);
    let planes = same_buffers(
        convolved(),
        &config(BitDepth::Float32, ChannelLayout::MONO),
        Via::Built,
    );
    assert_audible(&planes);
}

/// The graph reports a lookahead limiter's latency as the `Net` did, and a
/// render trimmed by it is the `Net`'s trimmed render, sample for sample.
///
/// Mutation (run): `RenderGraph::reported_latency` returning
/// `Samples::ZERO` → the figures differ (the limiter reports 240 frames at
/// 48 kHz).
///
/// Doc 013 PR 14 dropped one assertion here with the `Net` arm: that
/// `RenderGraph::Net`'s answer equalled the free `reported_latency(&mut Net)`
/// — two spellings of the one removed call. The figure itself is now also
/// pinned analytically: 5 ms of lookahead at 48 kHz is 240 frames.
#[test]
fn the_latency_trim_equals_the_net_paths() {
    let (mut net, g) = limited();
    let graph = built(g);
    // A `Net` answers at whatever rate it was last set to — 44.1 kHz for
    // one never rendered, where this limiter reports 221 frames, not 240 —
    // so it is re-rated to the render's first, as a caller had to. The graph
    // answers at the rate it was prepared at and cannot be asked early.
    net.set_sample_rate(RATE);
    let net_latency = net_latency(&mut net);
    let graph_latency = graph.reported_latency();
    assert!(
        net_latency.get() > 0,
        "not vacuous: the limiter looks ahead"
    );
    assert_eq!(graph_latency, net_latency);
    assert_eq!(graph_latency, Samples(240), "5 ms at 48 kHz");

    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.latency = graph_latency;
    let (net, g) = limited();
    let planes = same_buffers((net, g), &cfg, Via::Built);
    assert_eq!(
        planes[0].len(),
        14_462,
        "the trim does not shorten the output"
    );
    assert_audible(&planes);
}

/// The graph reports a convolver's tail as the `Net` did, and a render
/// extended by it is the `Net`'s, sample for sample.
///
/// Mutation (run): `RenderGraph::reported_tail` folding an empty topology
/// → `Some(0)` against the convolver's 2999.
///
/// Doc 013 PR 14 dropped one assertion here with the `Net` arm:
/// `RenderGraph::Net`'s tail equalled the free `reported_tail(&Net)`, both
/// the one fold this test now calls directly (`graph_tail` over the `Net`).
#[test]
fn the_tail_length_equals_the_net_paths() {
    let (net, g) = convolved();
    let graph = built(g);
    let net_tail = tutti_types::graph_tail(&net);
    assert_eq!(net_tail.samples(), Some(Samples(2999)), "the IR's ring-out");
    assert_eq!(graph.reported_tail(), net_tail);

    let mut cfg = config(BitDepth::Float32, ChannelLayout::MONO);
    cfg.render.tail = graph.reported_tail().samples().unwrap();
    // The latency trim too, so both halves of the gate run together.
    let probe = built(convolved().1);
    cfg.render.latency = probe.reported_latency();
    let planes = same_buffers(convolved(), &cfg, Via::Built);
    assert_eq!(planes[0].len(), 14_462 + 2999);
}

/// A graph holding a node that cannot be forked (a plugin, a mic monitor —
/// here a `Legacy` built unforkable) is refused with the node's key, and
/// nothing renders.
///
/// Mutation (run): `ForkError::NotForkable` wrapped in `Error::Fork` in
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
        RenderGraph { editor, executor },
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
/// - `GraphSource::fill` rendering through `FrozenClock` (a stopped
///   transport at beat 0) and only advancing the clock → every beat reads 0;
/// - `RenderClock::render_graph` advancing before `graph_block` → the
///   first frame reads a block past the start beat.
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

// ---- a clip reader: a `Legacy` unit that polls the render clock ----------

/// A 440 Hz sine at the render's rate, one second long: a plain wave table,
/// so a voice reading it at unit rate reproduces it sample for sample.
fn tone() -> Arc<tutti_io::Wave> {
    let mut w = tutti_io::Wave::new(1, RATE.get());
    for i in 0..RATE.get() as usize {
        w.push_frame(&[tone_at(i)]);
    }
    Arc::new(w)
}

/// The tone's frame `i`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / RATE.get() as f32).sin()
}

/// An offline timeline at beat 0, 120 BPM, at the render's rate.
fn timeline() -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm(120.0),
        sample_rate: RATE,
        loop_range: None,
    }))
}

/// A voice placed at beat 0 on `clock`, pitched by `cents`, in a
/// `VoicePool` (the unit a clip track renders).
fn placed_pool(clock: &Arc<OfflineTimeline>, cents: f32) -> tutti_sampler::VoicePool {
    use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
    let source = MemorySource::with_transport(
        tone(),
        Arc::clone(clock) as Arc<dyn tutti_core::Timeline>,
        Beat(0.0),
        None,
    );
    let (mut pool, _handle) = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                pitch: tutti_core::Cents::new(cents),
                ..Default::default()
            },
            channel_index: None,
        },
    );
    pool
}

/// The planes of `graph`, rendered as float stereo (or the graph's width)
/// under `clock`.
fn render_under(
    graph: RenderGraph,
    clock: &OfflineTimeline,
    width: ChannelLayout,
) -> Vec<Vec<f32>> {
    render_to_buffers(graph, &config(BitDepth::Float32, width), clock)
        .expect("renders")
        .planes
}

/// [`render_under`] for the `Net` oracle.
fn net_render_under(net: Net, clock: &OfflineTimeline, width: ChannelLayout) -> Vec<Vec<f32>> {
    net_render(net, &config(BitDepth::Float32, width), clock).planes
}

/// The first `(channel, frame)` at which two renders differ, if any.
fn first_difference(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<(usize, usize)> {
    assert_eq!(a.len(), b.len(), "widths differ");
    a.iter().zip(b).enumerate().find_map(|(c, (x, y))| {
        assert_eq!(x.len(), y.len(), "lengths differ");
        x.iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits())
            .map(|i| (c, i))
    })
}

/// **A sampler voice renders bit-identically through both backends at
/// `GRAPH_MAX_BLOCK`**, dry and a fifth up, and the dry one is the tone it
/// plays, frame for frame.
///
/// The voice polls the render clock (its `Arc<dyn Timeline>`) on every
/// `AudioUnit::process` call, which `Legacy` makes per 64-frame chunk. The
/// export asks for 1024-frame blocks, so unless the render moves the clock
/// between chunks every chunk of a block reads the block's first beat, and
/// the voice replays its first 64 frames sixteen times (a dry 440 Hz voice
/// measured 768 Hz). `RenderClock::render_graph` therefore renders a graph
/// holding a `Legacy` unit chunk-major, 64 frames across every node with
/// the clock advanced between (doc 013's `Legacy` compatibility mode), as
/// a `Net` was rendered, so the two agree to the bit.
///
/// Why to the bit and not to rounding: the pitched voice runs through the
/// vocoder, which turns an ulp of beat into far more. Measured with the
/// clock advanced a block at a time and the chunk positions computed in one
/// multiply each: the fifth-up voice left the `Net`'s render by 1e-3 at
/// frame 3076.
///
/// Mutation (run): `render_graph` rendering whole blocks with a `Legacy`
/// unit present (`has_legacy` ignored) → the backends part at frame 64, and
/// the dry render leaves the tone there.
#[test]
fn a_sampler_voice_renders_bit_identically_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get(), 1024, "the block this pins");
    for cents in [0.0f32, 700.0] {
        let net_clock = timeline();
        let mut net = Net::new(0, 2);
        let id = net.push(Box::new(placed_pool(&net_clock, cents)));
        net.pipe_output(id);
        let a = net_render_under(net, &net_clock, ChannelLayout::STEREO);

        let graph_clock = timeline();
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let k = g.add_unit(Box::new(placed_pool(&graph_clock, cents)));
        g.pipe_output(k);
        let b = render_under(built(g), &graph_clock, ChannelLayout::STEREO);

        if let Some((c, i)) = first_difference(&a, &b) {
            panic!(
                "{cents} cents: channel {c} differs first at frame {i}: net {} graph {}",
                a[c][i], b[c][i]
            );
        }
        assert_audible(&b);
        // Both clocks stand where the render ends, to the bit.
        assert_eq!(
            net_clock.beat().get().to_bits(),
            graph_clock.beat().get().to_bits(),
            "{cents} cents: the clocks end apart"
        );
        if cents == 0.0 {
            // Not merely the `Net`'s render: the right one. At unit rate the
            // voice reads the table at (nearly) integer positions.
            for (i, &s) in b[0].iter().enumerate() {
                let want = tone_at(i);
                assert!(
                    (s - want).abs() < 1e-3,
                    "frame {i}: the voice read {s}, the tone is {want} there"
                );
            }
        }
    }
}

/// The same through a **fork**: a placed `MemorySource` in a live graph,
/// forked offline onto the render's timeline (its `rebind_offline`
/// re-points it), renders what the `Net` path renders with the voice on
/// that timeline from the start.
///
/// Mutation (run): `render_graph` rendering whole blocks with a `Legacy`
/// unit present → the backends part at frame 64.
#[test]
fn a_forked_clip_reader_renders_bit_identically_at_the_graph_block() {
    use tutti_sampler::MemorySource;
    let placed = |clock: Arc<OfflineTimeline>| {
        MemorySource::with_transport(
            tone(),
            clock as Arc<dyn tutti_core::Timeline>,
            Beat(0.0),
            None,
        )
    };

    let net_clock = timeline();
    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(placed(Arc::clone(&net_clock))));
    net.pipe_output(id);
    net.set_sample_rate(RATE);
    net.reset();
    let a = net_render_under(net, &net_clock, ChannelLayout::MONO);

    // Live, the voice follows another clock, somewhere else; the fork
    // re-points it at the render's.
    let live_clock = timeline();
    live_clock.seek_to(Beat(3.0));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let k = g.add_unit(Box::new(placed(live_clock)));
    g.pipe_output(k);
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");
    let graph_clock = timeline();
    let rebind: OfflineTransport = graph_clock.clone();
    let forked = RenderGraph::fork(&live, ForkTarget::Master, ForkMode::Offline(&rebind), RATE)
        .expect("a memory source is forkable");
    let b = render_under(forked, &graph_clock, ChannelLayout::MONO);

    if let Some((c, i)) = first_difference(&a, &b) {
        panic!(
            "channel {c} differs first at frame {i}: net {} graph {}",
            a[c][i], b[c][i]
        );
    }
    assert_audible(&b);
}
