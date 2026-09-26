//! The native graph's exports, pinned to what fundsp's `Net` rendered for
//! the same units.
//!
//! # The oracles
//!
//! Until doc 013 Phase 3 PR 14 this file compared tutti-export's two backends,
//! `RenderGraph::Net` and the native graph, bit for bit; PR 14 kept the
//! comparisons against `net_render`, a test-only `Net` renderer here. PR 15
//! retired that last `Net` oracle with the engine's `Net` backend: each
//! comparison is now pinned to what it stood for, case by case, and two
//! kinds of check sit side by side.
//!
//! - **Portable, on every target:** an analytic figure where the signal has
//!   one (a sine's samples, a DC level, a lookahead's frames, a direct
//!   time-domain convolution, the tone a dry voice reads), and the export's
//!   own invariants where it does not (a file is its planes through
//!   `write_buffers`; a width is the quad render through `fold_frame`; a
//!   trimmed render is the untrimmed one shifted by the trim; a fork of a
//!   graph renders the fresh graph).
//! - **Golden digests, Linux/glibc only:** FNV-1a over the planes' (or the
//!   file's) bits, recorded from the native render on the commit that
//!   retired the `Net` oracle, which rendered exactly what the `Net` did
//!   (that was asserted there, bit for bit). They catch the drift an analytic
//!   tolerance lets through (an output scaled by `1 + f32::EPSILON`), but
//!   they pin `sin`/`cos`/`exp`, which are libm quality-of-implementation and
//!   differ in the last ulp between C runtimes (the reason
//!   `render_is_bit_identical_to_the_audionode_era` is gated off MSVC). So
//!   they are asserted only where they were recorded ([`GOLDEN_HERE`]); CI's
//!   Linux jobs run them. The one libm-free case (a dithered DC level) is
//!   asserted everywhere.
//!
//! Recompute a digest only with a deliberate, argued DSP change: a mismatch
//! prints the new figure.
//!
//! # Blocks
//!
//! The graph renders `GRAPH_MAX_BLOCK` (1024) frames a block. A `Legacy` unit
//! is run in 64-frame chunks from each block's start, so at a multiple of 64
//! every chunk lands on the frames a `Net`'s 64-frame block did, and a unit
//! whose output depends on the call partition (the VBAP panner, which ramps
//! its gains across each call) rendered the same. That is why
//! `GRAPH_MAX_BLOCK` is a multiple of 64; the durations below are
//! deliberately *not*, so the last block is short.
//!
//! # What these do not cover
//!
//! `Net`'s `ping` seeding of noise generators has no graph counterpart (doc
//! 013), so no case here uses a seeded generator.

#![cfg(feature = "wav")]

use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Amplitude, Beat, Bpm, Hz, SampleRate};
use tutti_export::{
    render_normalized_to_file, render_to_buffers, render_to_file, write_buffers, AudioFormat,
    BitDepth, ChannelLayout, Dither, EncodeConfig, Error, ExportConfig, FrozenClock, Normalize,
    RenderConfig, RenderGraph, Rendered, Resample, GRAPH_MAX_BLOCK,
};
use tutti_graph::{ForkMode, ForkTarget, GraphBuilder, Legacy, Prepare, Unforkable};
use tutti_nodes::testing::{Const, Osc};
use tutti_types::{Db, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// 0.3013 s at 48 kHz is 14 462 frames: 14 × 1024 + 126, and 225 × 64 + 62.
/// The last block is short at either size.
const SECS: f64 = 0.3013;
const FRAMES: usize = 14_462;

/// Whether this target is the one the golden digests were recorded on (see
/// the module docs): Linux with glibc's libm.
const GOLDEN_HERE: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// FNV-1a over the little-endian bytes of `bytes`.
fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// [`fnv`] over every plane's `f32` bits, plane after plane.
fn digest(planes: &[Vec<f32>]) -> u64 {
    fnv(planes.iter().flatten().flat_map(|s| s.to_le_bytes()))
}

/// Assert `got` is the digest recorded as `want`, where the goldens hold
/// ([`GOLDEN_HERE`]). The message carries the new figure.
fn assert_golden(what: &str, got: u64, want: u64) {
    if GOLDEN_HERE {
        assert_eq!(
            got, want,
            "{what}: digest {got:#018x}, recorded {want:#018x}"
        );
    }
}

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
    RenderGraph::new(editor, executor).expect("built together")
}

/// The builder's graph as an export gets it from a live one: built at a
/// device's block, then forked offline at the render's.
///
/// A fork **resets** every unit it makes (fundsp's sequence: clone, isolate,
/// rebind, reset), which is what the `Net` export did to the `Net` it
/// cloned. For most units a reset one renders what a fresh one does; not
/// for all: a reset `VbapPannerNode` starts on its commanded bearing where a
/// fresh one glides there from front-centre.
fn forked(g: GraphBuilder) -> RenderGraph {
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");
    let timeline: OfflineTransport =
        OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: RATE,
            ..Default::default()
        })));
    RenderGraph::fork(
        &live,
        ForkTarget::Master,
        ForkMode::Offline(&timeline),
        RATE,
    )
    .expect("every node here is forkable")
}

/// `graph` into buffers under `config`.
fn buffers(graph: RenderGraph, config: &ExportConfig) -> Rendered {
    render_to_buffers(graph, config, &FrozenClock).expect("graph renders")
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

/// Panic with where `a` and `b` part, if they do.
fn assert_same(what: &str, a: &[Vec<f32>], b: &[Vec<f32>]) {
    if let Some((c, i)) = first_difference(a, b) {
        panic!(
            "{what}: channel {c} differs first at frame {i}: {} vs {}",
            a[c][i], b[c][i]
        );
    }
}

/// A stereo sine at half scale, `Osc` wired to both outputs.
fn sine(freq: f32) -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let k = g.add_unit(Box::new(
        Osc::sine(Hz(freq))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO),
    ));
    g.pipe_output(k);
    g
}

/// Frame `i` of [`sine`]'s channels, in closed form: `Osc` evaluates
/// `sin(2π · phase)` of an `f64` phase stepped by `freq / rate`, so the
/// closed form agrees to the phase's accumulated rounding, far inside 1e-5
/// over these renders.
fn sine_at(freq: f64, amplitude: f64, i: usize) -> f64 {
    amplitude * (std::f64::consts::TAU * freq * i as f64 / RATE.get()).sin()
}

/// A mono DC level fanned to stereo.
fn dc(level: f32) -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let k = g.add_unit(Box::new(Const::mono(level)));
    g.pipe_output(k);
    g
}

/// A tone through a lookahead limiter: a latency-bearing chain.
fn limited() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add_unit(Box::new(
        Osc::sine(Hz(220.0))
            .with_amplitude(Amplitude(0.9))
            .with_layout(ChannelLayout::STEREO),
    ));
    let lim = g.add_unit(Box::new(
        tutti_nodes::LimiterNode::with_channels(ChannelLayout::STEREO, Db(-6.0), Db(-1.0))
            .with_lookahead(tutti_types::Seconds(0.005)),
    ));
    g.pipe(src, lim).pipe_output(lim);
    g
}

/// The convolver's impulse response: 3 000 taps, decaying in steps of 40.
fn ir() -> Vec<f32> {
    (0..3000).map(|i| 0.9f32.powi(i / 40) * 0.05).collect()
}

/// A tone through a convolver: a tail, a latency, and a block-oriented unit.
fn convolved() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add_unit(Box::new(
        Osc::sine(Hz(330.0)).with_amplitude(Amplitude(0.5)),
    ));
    let conv = g.add_unit(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir())));
    g.connect(src, 0, conv, 0).pipe_output(conv);
    g
}

/// [`convolved`]'s output in the time domain, computed directly, `n` frames
/// after its latency: the node's default blend, half the dry input (delayed
/// by the latency, so it leaves with the wet) and half the wet, and the wet
/// frame `n` is `Σ ir[k] · x[n − k]` over the taps, with `x` the tone in
/// closed form and the IR as the node holds it (`f32`), summed in `f64`. No
/// FFT, and so no libm beyond `sin`.
fn convolved_at(ir: &[f32], n: usize) -> f64 {
    let wet: f64 = (0..ir.len().min(n + 1))
        .map(|k| f64::from(ir[k]) * sine_at(330.0, 0.5, n - k))
        .sum();
    0.5 * sine_at(330.0, 0.5, n) + 0.5 * wet
}

/// Quad VBAP: a source at front-left and one at rear-left, each panned and
/// summed into a 4-wide master — `build_vbap_mix`'s quad graph (no LFE send),
/// written out. (The builder form of the helper, `tutti_spatial::vbap_mix_parts`,
/// is pinned against it in tutti-spatial's `tests/vbap_mix_parts.rs`, and
/// `surround_export.rs` renders through it.)
fn quad_vbap() -> GraphBuilder {
    use tutti_nodes::ChannelSumNode;
    use tutti_spatial::VbapPannerNode;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let sum = g.add_unit(Box::new(ChannelSumNode::new(2, ChannelLayout::QUAD)));
    for (s, (az, f)) in [(45.0, 300.0), (135.0, 500.0)].into_iter().enumerate() {
        let src = g.add_unit(Box::new(
            Osc::sine(Hz(f))
                .with_amplitude(Amplitude(0.5))
                .with_layout(ChannelLayout::STEREO),
        ));
        let pan = VbapPannerNode::for_layout(ChannelLayout::QUAD).expect("quad");
        pan.set_position(az, tutti_core::Elevation::LEVEL);
        let pan = g.add_unit(Box::new(pan));
        g.pipe(src, pan);
        for c in 0..4 {
            g.connect(pan, c, sum, s * 4 + c);
        }
    }
    g.pipe_output(sum);
    g
}

/// Not vacuous: a render that is all zeros would agree with anything.
fn assert_audible(planes: &[Vec<f32>]) {
    let peak = planes.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "the render is (nearly) silent: peak {peak}");
}

/// A file written through `write` from one graph, and the planes of another
/// built the same way written through `write_buffers` after `prepare`: the
/// export writes what it renders, so the two are byte-identical. Returns the
/// file.
fn same_file(
    graph: impl Fn() -> GraphBuilder,
    config: &ExportConfig,
    write: impl Fn(RenderGraph, &std::path::Path) -> tutti_export::Result<tutti_export::Written>,
    prepare: impl Fn(Rendered) -> tutti_export::Result<Rendered>,
) -> Vec<u8> {
    let d = tempfile::tempdir().unwrap();
    let (pf, pb) = (d.path().join("file.wav"), d.path().join("buffers.wav"));
    write(built(graph()), &pf).expect("the graph writes");
    let planes = prepare(buffers(built(graph()), config)).expect("prepares");
    write_buffers(&planes, config, &pb).expect("its planes write");
    let (a, b) = (std::fs::read(&pf).unwrap(), std::fs::read(&pb).unwrap());
    assert_eq!(a.len(), b.len(), "the two files differ in length");
    assert!(a == b, "the file is not its planes");
    a
}

/// A WAV file's float samples, interleaved.
fn float_samples(bytes: &[u8]) -> Vec<f32> {
    hound::WavReader::new(std::io::Cursor::new(bytes))
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect()
}

/// The sine, built for the export and forked from a live graph: the closed
/// form on every frame of both channels, the fork the fresh graph to the
/// bit, and (Linux/glibc) the digest the `Net` rendered.
///
/// Until doc 013 PR 15 both were compared bit for bit with a `Net`
/// rendering the same unit (`net_render`).
///
/// Mutations (run): `block_size` rounded down to a multiple of 64 in
/// `GraphSource::fill` (the last, short block is never rendered) → the
/// length is short; the graph's output scaled by `1.0 + f32::EPSILON` →
/// the digest moves (the closed form's tolerance cannot see it).
#[test]
fn a_sine_renders_its_closed_form() {
    let cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    let planes = buffers(built(sine(1000.0)), &cfg).planes;
    assert_eq!(planes.len(), 2);
    assert_eq!(planes[0].len(), FRAMES);
    for (c, plane) in planes.iter().enumerate() {
        for (i, &s) in plane.iter().enumerate() {
            let want = sine_at(1000.0, 0.5, i);
            assert!(
                (f64::from(s) - want).abs() < 1e-5,
                "channel {c}, frame {i}: {s}, the sine is {want}"
            );
        }
    }
    assert_golden("sine", digest(&planes), 0x4b59_8dd3_79f5_90cd);
    let forked = buffers(forked(sine(1000.0)), &cfg).planes;
    assert_same("forked against built", &planes, &forked);
}

/// The resample case (48 k → 44.1 k), to a file: the file is the render's
/// planes through the same conversion and encoder (`write_buffers`), byte
/// for byte, at 44.1 kHz, and (Linux/glibc) the bytes the `Net` export
/// wrote.
///
/// Until doc 013 PR 15 the planes written through `write_buffers` were a
/// `Net`'s, rendered in 64-frame blocks against the graph's 1024 (the
/// resampler's carry makes its output independent of the partition, which
/// that pinned too; `oracle_resample.rs` holds the conversion to first
/// principles).
///
/// Mutation (run): scale the graph path's output by `1.0 + f32::EPSILON` in
/// `GraphSource::fill` → the digest moves.
#[test]
fn a_resampled_export_writes_its_render() {
    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.resample = Some(Resample::to(SampleRate(44_100.0)));
    let bytes = same_file(
        || sine(1000.0),
        &cfg,
        |g, p| render_to_file(g, &cfg, &FrozenClock, p),
        Ok,
    );
    let r = hound::WavReader::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(r.spec().sample_rate, 44_100, "not vacuous: it resampled");
    // A 1 kHz sine at half scale is in the passband: it keeps its level.
    let peak = float_samples(&bytes)
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    assert_golden(
        "resampled file",
        fnv(bytes.iter().copied()),
        0x742b_d54e_8409_650e,
    );
}

/// The dBTP case: a peak-normalized export (render, measure, gain, write)
/// is its render normalized and written (`Normalize::gain_for_rendered`,
/// then `write_buffers`), byte for byte; its sample peak sits at the
/// -1 dBTP target or just under it (the true peak, between samples, is
/// what reaches -1 dB); and (Linux/glibc) the bytes are the `Net` export's.
///
/// Mutation (run): as above → the digest moves.
#[test]
fn a_peak_normalized_export_writes_its_normalized_render() {
    let mut cfg = config(BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = 1.0;
    let normalize = Normalize::peak(Db(-1.0));
    let bytes = same_file(
        || sine(440.0),
        &cfg,
        |g, p| render_normalized_to_file(g, &cfg, &FrozenClock, normalize, p),
        // `render_normalized_to_file`'s own steps on planes already rendered:
        // measure, apply (no resample here to convert first).
        |mut r| {
            r.apply_gain(normalize.gain_for_rendered(&r)?);
            Ok(r)
        },
    );
    let target = Db(-1.0).to_amplitude().get();
    let peak = float_samples(&bytes)
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        peak <= target + 1e-6 && peak > target * 0.99,
        "sample peak {peak}, target {target}"
    );
    assert_golden(
        "peak-normalized file",
        fnv(bytes.iter().copied()),
        0x5c90_1de8_221d_8251,
    );
}

/// The dither case: triangular dither to 16 bits, on a DC level between two
/// codes, so every sample is perturbed. The file is its planes dithered and
/// written (`write_buffers`), byte for byte; its samples scatter around the
/// level's code (8 192.05) within the dither's two codes and average onto it;
/// and it is the `Net` export's file to the byte, on **every** target: a DC
/// level and a seeded integer dither use no libm.
///
/// Mutation (run): as above → the digest moves.
#[test]
fn a_dithered_export_writes_its_dithered_render() {
    let mut cfg = config(BitDepth::Int16, ChannelLayout::STEREO);
    cfg.dither = Dither::Triangular;
    let level = 0.25 + 0.3 / 32_768.0;
    let bytes = same_file(
        || dc(level),
        &cfg,
        |g, p| render_to_file(g, &cfg, &FrozenClock, p),
        Ok,
    );
    let s: Vec<i16> = hound::WavReader::new(std::io::Cursor::new(&bytes))
        .unwrap()
        .into_samples::<i16>()
        .map(|s| s.unwrap())
        .collect();
    // Not vacuous: the dither moved samples off the one code.
    assert!(s.iter().any(|&x| x != s[0]), "dither left the level flat");
    // In the encoder's scale (`f32_to_i16`: full scale is `i16::MAX`).
    let code = f64::from(level) * f64::from(i16::MAX);
    assert!(s.iter().all(|&x| (f64::from(x) - code).abs() <= 2.0));
    let mean = s.iter().map(|&x| f64::from(x)).sum::<f64>() / s.len() as f64;
    assert!(
        (mean - code).abs() < 0.05,
        "mean {mean}, the level is {code}"
    );
    assert_eq!(
        fnv(bytes.iter().copied()),
        0xcc78_4f0b_ee7c_d5f9,
        "the dithered file's digest"
    );
}

/// The surround case: a quad VBAP mix, exported at its own width and folded
/// down to stereo and to mono. The narrower files are the quad render folded
/// frame by frame with `fold_frame` (the ITU matrix), to the bit; the quad
/// render puts the front-left source in channel 0 and the rear-left one in
/// channel 2; and (Linux/glibc) the quad render, built and forked, is the
/// `Net`'s.
///
/// This is also the case that pins the block rule in the module docs: the
/// VBAP panner ramps its gains across each call, so it is the unit here whose
/// output depends on where the 64-frame chunks fall.
///
/// Mutations (run): `fold_graph_frame` reading `planes[0]` for every source
/// channel → the stereo file is not the quad render folded; `GRAPH_MAX_BLOCK
/// = 1000` (not a multiple of 64) → the panners' ramps restart on other
/// frames and the quad digest moves.
#[test]
fn a_surround_mix_folds_to_every_width() {
    let quad = buffers(
        built(quad_vbap()),
        &config(BitDepth::Float32, ChannelLayout::QUAD),
    )
    .planes;
    assert_audible(&quad);
    // Not vacuous: the rear-left source put energy in channel 2, and the
    // front-left one in channel 0.
    assert!(quad[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
    assert!(quad[0].iter().any(|s| s.abs() > 0.05), "no front energy");
    assert_golden("quad", digest(&quad), 0x2df3_9481_9b23_375a);
    // No digest for these: each is the quad render (whose digest is pinned)
    // folded, to the bit.
    for width in [ChannelLayout::STEREO, ChannelLayout::MONO] {
        let got = buffers(built(quad_vbap()), &config(BitDepth::Float32, width)).planes;
        let n = width.count() as usize;
        let mut folded = vec![Vec::with_capacity(FRAMES); n];
        let mut frame = vec![0.0f32; n];
        for i in 0..FRAMES {
            let src: Vec<f32> = quad.iter().map(|p| p[i]).collect();
            tutti_types::fold_frame(&src, &mut frame);
            for (plane, &s) in folded.iter_mut().zip(&frame) {
                plane.push(s);
            }
        }
        assert_same(&format!("{n}-wide against the quad folded"), &folded, &got);
    }
    let fork = buffers(
        forked(quad_vbap()),
        &config(BitDepth::Float32, ChannelLayout::QUAD),
    )
    .planes;
    assert!(fork[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
    assert_golden("quad, forked", digest(&fork), 0x800b_0ec2_8842_1715);
}

/// A convolver — FFT-partitioned, latency- and tail-bearing — renders the
/// direct time-domain convolution of its input, delayed by the latency it
/// reports, at the graph's block; and (Linux/glibc) the `Net`'s render.
///
/// It turns out not to be the block-sensitive unit: it buffers its
/// partitions internally, so `GRAPH_MAX_BLOCK = 1000` still passes here
/// (run) and the surround case is what catches that. The assertion on the
/// constant below is the cheap guard; this test is here for the
/// latency/tail-bearing unit.
///
/// The tolerance is the FFT's `f32` rounding over a 3 000-tap sum whose
/// output reaches ~10: 1e-3, against an error measured below 1e-4.
///
/// Mutations (run): `block_size` rounded down to a multiple of 64 → the
/// length is short; the output scaled by `1.0 + f32::EPSILON` → the digest
/// moves.
#[test]
fn a_convolver_renders_the_direct_convolution_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get() % 64, 0);
    let graph = built(convolved());
    let latency = graph.reported_latency().get();
    let planes = buffers(graph, &config(BitDepth::Float32, ChannelLayout::MONO)).planes;
    assert_audible(&planes);
    assert_eq!(planes[0].len(), FRAMES);
    let ir = ir();
    for (n, &s) in planes[0].iter().enumerate() {
        let want = if n < latency {
            0.0
        } else {
            convolved_at(&ir, n - latency)
        };
        assert!(
            (f64::from(s) - want).abs() < 1e-3,
            "frame {n}: {s}, the convolution is {want} (latency {latency})"
        );
    }
    assert_golden("convolver", digest(&planes), 0x1163_cc03_0b49_99c6);
}

/// The graph reports a lookahead limiter's latency analytically (5 ms at
/// 48 kHz, 240 frames), and a render trimmed by it is the untrimmed render
/// from frame 240 on, sample for sample.
///
/// Until doc 013 PR 15 the figure was also compared with a `Net`'s
/// (`net_latency`, re-rated to the render's rate) and the trimmed render
/// with the `Net`'s trimmed render (`net_render`).
///
/// Mutations (run): `RenderGraph::reported_latency` returning
/// `Samples::ZERO` → the figure is 0; `drive` trimming one frame more than
/// the latency → the trimmed render is a frame short.
#[test]
fn the_latency_trim_drops_the_lookahead() {
    let latency = built(limited()).reported_latency();
    assert_eq!(latency, Samples(240), "5 ms at 48 kHz");

    let mut trimmed = config(BitDepth::Float32, ChannelLayout::STEREO);
    trimmed.render.latency = latency;
    let a = buffers(built(limited()), &trimmed).planes;
    assert_eq!(a[0].len(), FRAMES, "the trim does not shorten the output");
    assert_audible(&a);

    // Untrimmed, and 240 frames longer (as a tail), so it covers the same
    // span shifted.
    let mut whole = config(BitDepth::Float32, ChannelLayout::STEREO);
    whole.render.tail = latency;
    let b = buffers(built(limited()), &whole).planes;
    let shifted: Vec<Vec<f32>> = b.iter().map(|p| p[240..].to_vec()).collect();
    assert_same("trimmed against untrimmed from 240", &a, &shifted);
}

/// The graph reports a convolver's tail analytically (a 3 000-tap IR rings
/// 2 999 frames), and a render trimmed by its latency and extended by its
/// tail is the direct convolution, frame for frame, through the tail.
///
/// Until doc 013 PR 15 the figure was also compared with the tail fold over
/// a `Net` (`graph_tail`), and the render with the `Net`'s (`net_render`).
///
/// Mutation (run): `RenderGraph::reported_tail` folding an empty topology
/// → `Some(0)` against the convolver's 2999.
#[test]
fn the_tail_extends_the_render_by_the_ring_out() {
    let graph = built(convolved());
    assert_eq!(
        graph.reported_tail().samples(),
        Some(Samples(2999)),
        "the IR's ring-out"
    );
    let mut cfg = config(BitDepth::Float32, ChannelLayout::MONO);
    cfg.render.tail = graph.reported_tail().samples().unwrap();
    // The latency trim too, so both halves of the gate run together.
    cfg.render.latency = graph.reported_latency();
    let planes = buffers(built(convolved()), &cfg).planes;
    assert_eq!(planes[0].len(), FRAMES + 2999);
    let ir = ir();
    for (n, &s) in planes[0].iter().enumerate() {
        let want = convolved_at(&ir, n);
        assert!(
            (f64::from(s) - want).abs() < 1e-3,
            "frame {n}: {s}, the convolution is {want}"
        );
    }
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

/// An editor that does not feed the executor beside it is refused when the
/// `RenderGraph` is made, not first at render.
///
/// Mutation (run): `RenderGraph::new` skipping `check_paired` → it wraps
/// the crossed pair.
#[test]
fn a_crossed_pair_is_refused_at_construction() {
    let (editor_a, executor_a) = sine(1000.0).build(RenderGraph::prepare(RATE)).unwrap();
    let (editor_b, executor_b) = sine(500.0).build(RenderGraph::prepare(RATE)).unwrap();
    for (editor, executor) in [(editor_a, executor_b), (editor_b, executor_a)] {
        let r = RenderGraph::new(editor, executor);
        assert!(
            matches!(r, Err(Error::InvalidConfig(_))),
            "a crossed pair was wrapped"
        );
    }
}

/// An editor swapped in through `editor_mut` after construction is refused
/// at render: the render keeps the pairing check as a real error.
///
/// Mutation (run): drop `check_paired` from `render::with_source` → it
/// renders the crossed pair.
#[test]
fn an_editor_swapped_after_construction_is_refused_at_render() {
    let mut graph = built(sine(1000.0));
    let (other, _exec) = sine(500.0).build(RenderGraph::prepare(RATE)).unwrap();
    let _live = std::mem::replace(graph.editor_mut(), other);
    let r = render_to_buffers(
        graph,
        &config(BitDepth::Float32, ChannelLayout::STEREO),
        &FrozenClock,
    );
    assert!(matches!(r, Err(Error::InvalidConfig(_))), "{r:?}");
}

/// A pair prepared at another rate is refused, not rendered at the wrong one.
///
/// Mutation (run): drop the rate check in `GraphSource::new` → it renders.
#[test]
fn a_graph_prepared_at_another_rate_is_refused() {
    let g = sine(1000.0);
    let (editor, executor) = g
        .build(RenderGraph::prepare(SampleRate(44_100.0)))
        .expect("builds");
    let r = render_to_buffers(
        RenderGraph::new(editor, executor).expect("built together"),
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
    let clock = g.add(Unforkable(tutti_core::EnvClock::new()));
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

/// The dominant frequency of `x[from..]` between 500 and 900 Hz: the peak of
/// its Hann-windowed spectrum, scanned in quarter-hertz steps. A tolerance
/// check on it is portable (a last-ulp libm difference moves no peak).
///
/// Not zero crossings: the vocoder's output carries low-level phase
/// artefacts that add crossings, and a crossing count read the fifth-up
/// voice 2% sharp (672.7 Hz) where its spectrum peaks at 658.75 Hz.
fn dominant_frequency(x: &[f32], from: usize, rate: f64) -> f64 {
    use std::f64::consts::TAU;
    let w = &x[from..];
    let n = w.len() as f64;
    let mut best = (0.0, 0.0);
    let mut f = 500.0;
    while f < 900.0 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in w.iter().enumerate() {
            let hann = 0.5 - 0.5 * (TAU * i as f64 / n).cos();
            let p = TAU * f * i as f64 / rate;
            re += f64::from(s) * hann * p.cos();
            im -= f64::from(s) * hann * p.sin();
        }
        let m = re * re + im * im;
        if m > best.1 {
            best = (f, m);
        }
        f += 0.25;
    }
    best.0
}

/// Every frame of `plane` is the tone's, to the bit: a dry voice read at a
/// whole source frame copies the sample the wave holds.
fn assert_the_tone(what: &str, plane: &[f32]) {
    for (i, &s) in plane.iter().enumerate() {
        assert_eq!(
            s.to_bits(),
            tone_at(i).to_bits(),
            "{what}: frame {i} read {s}, the tone is {} there",
            tone_at(i)
        );
    }
}

/// **A sampler voice renders in time at `GRAPH_MAX_BLOCK`**, dry and a fifth
/// up: the dry one is the tone it plays, to the bit, on both channels; the
/// render clock ends the render's frames on; and (Linux/glibc) the pitched
/// one is the `Net`'s render.
///
/// The voice polls the render clock (its `Arc<dyn Timeline>`) on every
/// `AudioUnit::process` call, which `Legacy` makes per 64-frame chunk. The
/// export asks for 1024-frame blocks, so unless the render moves the clock
/// between chunks every chunk of a block reads the block's first beat, and
/// the voice replays its first 64 frames sixteen times (a dry 440 Hz voice
/// measured 768 Hz). `RenderClock::render_graph` therefore renders a graph
/// holding a `Legacy` unit chunk-major, 64 frames across every node with
/// the clock advanced between (doc 013's `Legacy` compatibility mode), as
/// a `Net` was rendered.
///
/// Why a digest and not a tolerance for the pitched voice: the vocoder turns
/// an ulp of beat into far more. Measured with the clock advanced a block at
/// a time and the chunk positions computed in one multiply each, the
/// fifth-up voice left the `Net`'s render by 1e-3 at frame 3076. Until doc
/// 013 PR 15 both voices were compared with a `Net` rendering them
/// (`net_render`), bit for bit.
///
/// Mutations (run): the graph path's output scaled by `1.0 + f32::EPSILON`
/// → the dry render is not the tone, and the pitched digest moves;
/// `Cents::to_pitch_ratio` dividing by 1 100 cents to the octave → the
/// pitched voice is not a fifth up, on every target.
///
/// Not caught here: `render_graph` rendering whole blocks with a `Legacy`
/// unit present (`has_legacy` ignored). A voice placed at beat 0 enters on
/// frame 0 and then reads its own cursor, so with these fixtures a
/// whole-block render is the same bits (measured: both voices, both
/// digests). Where the polled beat matters, at a clip's entry mid-render,
/// tutti-sampler's `frame_exact_entry.rs` catches it
/// (`a_clip_enters_on_its_frame_offline_through_the_graph`). This test's
/// note used to claim the backends parted at frame 64; that was measured
/// before the sampler's own cursor took over, and no longer holds.
#[test]
fn a_sampler_voice_renders_in_time_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get(), 1024, "the block this pins");
    for (cents, want) in [
        (0.0f32, 0xbd52_29c7_8f1d_ca41u64),
        (700.0, 0xd11e_26e8_e6d8_b81d),
    ] {
        let clock = timeline();
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let k = g.add_unit(Box::new(placed_pool(&clock, cents)));
        g.pipe_output(k);
        let b = render_under(built(g), &clock, ChannelLayout::STEREO);
        assert_audible(&b);
        assert_eq!(b[0].len(), FRAMES);
        // The clock stands where the render ends: the offline timeline's
        // closed form, frames × tempo / (60 × rate), to rounding.
        let end = (FRAMES as f64 * 120.0) / (60.0 * RATE.get());
        assert!(
            (clock.beat().get() - end).abs() < 1e-9,
            "{cents} cents: the clock ends at {}, not {end}",
            clock.beat().get()
        );
        if cents == 0.0 {
            assert_the_tone("dry, left", &b[0]);
            assert_the_tone("dry, right", &b[1]);
        } else {
            // Portable, where the digest is not: a fifth up from 440 Hz is
            // 440 · 2^(7/12) ≈ 659.26 Hz. Past the vocoder's first few
            // thousand frames, within 1% (a semitone is 6%).
            let want = 440.0 * 2f64.powf(7.0 / 12.0);
            let got = dominant_frequency(&b[0], 4_096, RATE.get());
            assert!(
                (got - want).abs() < want * 1e-2,
                "{cents} cents: {got} Hz, a fifth up is {want} Hz"
            );
        }
        assert_golden(&format!("voice, {cents} cents"), digest(&b), want);
    }
}

/// The same through a **fork**: a placed `MemorySource` in a live graph,
/// forked offline onto the render's timeline (its `rebind_offline`
/// re-points it), plays the tone from the render's start, to the bit, as a
/// voice on that timeline from the start does. (Until doc 013 PR 15 it was
/// compared with a `Net` rendering the source on that timeline.)
///
/// Mutation (run): the graph path's output scaled by `1.0 + f32::EPSILON`
/// → the render is not the tone. (Whole-block rendering is not caught
/// here, for the reason the test above gives.)
#[test]
fn a_forked_clip_reader_renders_the_tone_at_the_graph_block() {
    use tutti_sampler::MemorySource;
    let placed = |clock: Arc<OfflineTimeline>| {
        MemorySource::with_transport(
            tone(),
            clock as Arc<dyn tutti_core::Timeline>,
            Beat(0.0),
            None,
        )
    };

    // Live, the voice follows another clock, somewhere else; the fork
    // re-points it at the render's.
    let live_clock = timeline();
    live_clock.seek_to(Beat(3.0));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let k = g.add_unit(Box::new(placed(live_clock)));
    g.pipe_output(k);
    let (live, _exec) = g.build(Prepare::new(RATE, Samples(256))).expect("builds");
    let graph_clock = timeline();
    let rebind: OfflineTransport = OfflineTransport::new(graph_clock.clone());
    let forked = RenderGraph::fork(&live, ForkTarget::Master, ForkMode::Offline(&rebind), RATE)
        .expect("a memory source is forkable");
    let b = render_under(forked, &graph_clock, ChannelLayout::MONO);
    assert_audible(&b);
    assert_the_tone("forked", &b[0]);
}
