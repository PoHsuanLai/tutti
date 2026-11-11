//! The graph's exports, pinned by analytic figures and golden digests.
//!
//! # The oracles
//!
//! Two kinds of check sit side by side.
//!
//! - Portable, on every target: an analytic figure where the signal has
//!   one (a sine's samples, a DC level, a lookahead's frames, a direct
//!   time-domain convolution, the tone a dry voice reads), and the export's
//!   own invariants where it does not (a file is its planes through
//!   `write_buffers`; a width is the quad render folded by the ITU matrix; a
//!   trimmed render is the untrimmed one shifted by the trim; a fork of a
//!   graph renders the fresh graph).
//! - Golden digests, on the recorded target only: FNV-1a over the planes'
//!   (or the file's) bits. All but three were recorded from the graph's render
//!   against an independent reference renderer it matched bit for bit. Three
//!   are self-recorded: the pitched sampler voice's and the convolver's (the
//!   scalar FFT has no reference renderer; their cross-checks are the
//!   fifth-up pitch figure and the direct convolution), and the quad VBAP
//!   digest (re-recorded when the panner's smoother moved to one advance per
//!   block; its folded stereo and mono renders are checked against it bit for
//!   bit).
//!   They catch the drift an analytic tolerance lets through (an output
//!   scaled by `1 + f32::EPSILON`), but they pin `sin`/`cos`/`exp`, which are
//!   libm quality-of-implementation and differ in the last ulp between C
//!   runtimes. So they are asserted only where they were recorded
//!   (`tutti_fixtures::assert_golden`); CI's Linux jobs run them. The
//!   one libm-free case (a dithered DC level) is asserted everywhere.
//!
//! Recompute a digest only with a deliberate, argued DSP change: a mismatch
//! prints the new figure.
//!
//! # Blocks
//!
//! The graph renders `GRAPH_MAX_BLOCK` (1024) frames a block and every node
//! renders whole blocks. A node whose output depends on the call partition
//! (the VBAP panner, which ramps its gains across each call) is pinned at that
//! partition (see `a_surround_mix_folds_to_every_width`). The durations below
//! are deliberately *not* multiples, so the last block is short.
//!
//! # What these do not cover
//!
//! The graph has no seeding of noise generators, so no case here uses a
//! seeded generator.

#![cfg(feature = "wav")]

mod support;
use crate::support::{EncodeConfig, ExportConfig, RenderConfig, write_buffers};
use tutti_types::MaxBlock;

use std::sync::Arc;
use tutti_types::Seconds;

use crate::support::{render_normalized_to_file, render_to_buffers, render_to_file};
use tutti_codec::{Format, Pcm};
use tutti_dsp::DitherShape;
use tutti_export::{Error, GRAPH_MAX_BLOCK, Normalize, RenderGraph, Rendered, Resample, Setup};
use tutti_fixtures::assert_golden;
use tutti_graph::{Cx, GraphBuilder, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_nodes::testing::{Const, Osc, vbap_mix};
use tutti_tempo::TempoMap;
use tutti_transport::FrozenClock;
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::ChannelLayout;
use tutti_types::{Amplitude, Azimuth, Beat, Bpm, Hz, SampleRate};
use tutti_types::{Db, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// 0.3013 s at 48 kHz is 14 462 frames: 14 × 1024 + 126, and 225 × 64 + 62.
/// The last block is short at either size.
const SECS: f64 = 0.3013;
const FRAMES: usize = 14_462;

/// FNV-1a over the little-endian bytes of `bytes`.
fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut h = tutti_fixtures::Fnv::new();
    h.bytes(bytes);
    h.finish()
}

/// [`fnv`] over every plane's `f32` bits, plane after plane.
fn digest(planes: &[Vec<f32>]) -> u64 {
    fnv(planes.iter().flatten().flat_map(|s| s.to_le_bytes()))
}

fn config(bit_depth: Pcm, channels: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(SECS),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(bit_depth),
            channels,
            topology: None,
        },
        dither: None,
        ..Default::default()
    }
}

/// The builder's graph, built for the export.
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
}

/// The builder's graph as an export gets it from a live one: built at a
/// device's block, then forked offline at the render's.
///
/// A fork resets every unit it makes (each node's fork source hands a
/// reset copy). For most units a reset one renders what a fresh one does; not for all: a reset
/// `VbapPannerNode` starts on its commanded bearing where a fresh one glides
/// there from front-centre.
fn forked(g: GraphBuilder) -> RenderGraph {
    let (live, _exec) = g
        .build(Prepare::new(RATE, MaxBlock::of(Samples(256))))
        .expect("builds");
    support::fork_offline(&live, RATE)
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
    let k = g.add(
        Osc::sine(Hz(freq))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO),
    );
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
    let k = g.add(Const::mono(level));
    g.pipe_output(k);
    g
}

/// A tone through a lookahead limiter: a latency-bearing chain.
fn limited() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add(
        Osc::sine(Hz(220.0))
            .with_amplitude(Amplitude(0.9))
            .with_layout(ChannelLayout::STEREO),
    );
    let (lim, _) = g.add_with_controls(
        tutti_nodes::LimiterNode::with_channels(ChannelLayout::STEREO, Db(-6.0), Db(-1.0))
            .with_lookahead(tutti_types::Seconds(0.005)),
    );
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
    let src = g.add(Osc::sine(Hz(330.0)).with_amplitude(Amplitude(0.5)));
    let conv = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&ir()))
        .0;
    // Half the dry input, aligned by the latency, and half the wet: the graph's mix.
    g.spec_mut().set_mix(conv, tutti_types::Mix::new(0.5));
    g.connect(src, 0, conv, 0).pipe_output(conv);
    g
}

/// [`convolved`]'s output in the time domain, computed directly, `n` frames
/// after its latency: the graph's blend at a mix of 0.5, half the dry input (delayed
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
/// summed (by fan-in) into a 4-wide master.
fn quad_vbap() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let sources: Vec<_> = [(45.0, 300.0), (135.0, 500.0)]
        .into_iter()
        .map(|(az, f)| {
            let src = g.add(
                Osc::sine(Hz(f))
                    .with_amplitude(Amplitude(0.5))
                    .with_layout(ChannelLayout::STEREO),
            );
            (src, Azimuth::new(az))
        })
        .collect();
    vbap_mix(&mut g, ChannelLayout::QUAD, &sources).expect("quad");
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
/// bit, and its golden digest.
#[test]
fn a_sine_renders_its_closed_form() {
    let cfg = config(Pcm::Float32, ChannelLayout::STEREO);
    let planes = buffers(built(sine(1000.0)), &cfg).into_planes();
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
    let forked = buffers(forked(sine(1000.0)), &cfg).into_planes();
    assert_same("forked against built", &planes, &forked);
}

/// The resample case (48 k → 44.1 k), to a file: the file is the render's
/// planes through the same conversion and encoder (`write_buffers`), byte
/// for byte, at 44.1 kHz, and its golden digest. The
/// resampler's carry makes its output independent of the block partition;
/// `oracle_resample.rs` holds the conversion to first principles.
///
/// The digest is of the scalar-FFT resampler; the analytic figures it must
/// satisfy (frequency, level, DC, rejection, length, latency) are pinned in
/// `tutti-dsp`'s `resample/streaming` tests and `oracle_resample.rs`.
#[test]
fn a_resampled_export_writes_its_render() {
    let mut cfg = config(Pcm::Float32, ChannelLayout::STEREO);
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
        0x964e_0424_498c_a85a,
    );
}

/// The dBTP case: a peak-normalized export (render, measure, gain, write)
/// is its render normalized and written (`Normalize::gain_for_rendered`,
/// then `write_buffers`), byte for byte; its sample peak sits at the
/// -1 dBTP target or just under it (the true peak, between samples, is
/// what reaches -1 dB); and the bytes match their golden
/// digest.
#[test]
fn a_peak_normalized_export_writes_its_normalized_render() {
    let mut cfg = config(Pcm::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = Seconds(1.0);
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
/// and its golden digest holds on every target: a DC level and a seeded
/// integer dither use no libm.
#[test]
fn a_dithered_export_writes_its_dithered_render() {
    let mut cfg = config(Pcm::Int16, ChannelLayout::STEREO);
    cfg.dither = Some(DitherShape::Triangular);
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
/// frame by frame by the ITU matrix, written out, to the bit; the quad
/// render puts the front-left source in channel 0 and the rear-left one in
/// channel 2; and the built and forked quad renders match their
/// golden digests.
///
/// This is also the case that pins the block rule in the module docs: the
/// VBAP panner ramps its gains across each call, so it is the unit here whose
/// output depends on where the blocks fall.
///
/// The panners ramp across the render's 1024-frame blocks. A fresh panner
/// glides in from front-centre, so the built render's digest depends on the
/// block partition; the forked render's panners start on their bearing.
#[test]
fn a_surround_mix_folds_to_every_width() {
    let quad = buffers(
        built(quad_vbap()),
        &config(Pcm::Float32, ChannelLayout::QUAD),
    )
    .into_planes();
    assert_audible(&quad);
    // Not vacuous: the rear-left source put energy in channel 2, and the
    // front-left one in channel 0.
    assert!(quad[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
    assert!(quad[0].iter().any(|s| s.abs() > 0.05), "no front energy");
    assert_golden("quad", digest(&quad), 0xe087_842d_4937_bca6);
    // No digest for these: each is the quad render (whose digest is pinned)
    // folded, to the bit.
    for width in [ChannelLayout::STEREO, ChannelLayout::MONO] {
        let got = buffers(built(quad_vbap()), &config(Pcm::Float32, width)).into_planes();
        let n = width.count() as usize;
        // The ITU quad fold, written out: `Lo = FL + M3DB·BL`, `Ro = FR + M3DB·BR`,
        // and the mono file is `(Lo + Ro)·M3DB`.
        let m = tutti_types::M3DB;
        let mut folded = vec![Vec::with_capacity(FRAMES); n];
        let [fl, fr, bl, br] = [0, 1, 2, 3].map(|c| &quad[c]);
        for (((&fl, &fr), &bl), &br) in fl.iter().zip(fr).zip(bl).zip(br) {
            let lo = fl + m * bl;
            let ro = fr + m * br;
            if n == 2 {
                folded[0].push(lo);
                folded[1].push(ro);
            } else {
                folded[0].push((lo + ro) * m);
            }
        }
        assert_same(&format!("{n}-wide against the quad folded"), &folded, &got);
    }
    let fork = buffers(
        forked(quad_vbap()),
        &config(Pcm::Float32, ChannelLayout::QUAD),
    )
    .into_planes();
    assert!(fork[2].iter().any(|s| s.abs() > 0.05), "no rear energy");
    assert_golden("quad, forked", digest(&fork), 0x800b_0ec2_8842_1715);
}

/// A convolver — FFT-partitioned, latency- and tail-bearing — renders the
/// direct time-domain convolution of its input, delayed by the latency it
/// reports, at the graph's block; and its golden digest.
///
/// It turns out not to be the block-sensitive unit: it buffers its
/// partitions internally, so `GRAPH_MAX_BLOCK = 1000` still passes here
/// and the surround case is what catches that. The assertion on the
/// constant below is the cheap guard; this test is here for the
/// latency/tail-bearing unit.
///
/// The tolerance is the FFT's `f32` rounding over a 3 000-tap sum whose
/// output reaches ~10: 1e-3, against an error measured below 1e-4.
///
/// The digest moved when the convolver's FFT went scalar (`RealFft`): the
/// inner transform no longer rounds as the AVX one did. Swapping
/// `RealFft::new` back to rustfft's auto planner on an AVX machine restores the
/// previous digest (0x1163_cc03_0b49_99c6) exactly, so the real-FFT passes
/// are unchanged and the scalar planner is the whole difference. The new
/// digest is self-recorded.
#[test]
fn a_convolver_renders_the_direct_convolution_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get() % 64, 0);
    let graph = built(convolved());
    let latency = graph.reported_latency().get();
    let planes = buffers(graph, &config(Pcm::Float32, ChannelLayout::MONO)).into_planes();
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
    assert_golden("convolver", digest(&planes), 0x72ca_224c_c4cd_9a4e);
}

/// The graph reports a lookahead limiter's latency analytically (5 ms at
/// 48 kHz, 240 frames), and a render trimmed by it is the untrimmed render
/// from frame 240 on, sample for sample.
#[test]
fn the_latency_trim_drops_the_lookahead() {
    let latency = built(limited()).reported_latency();
    assert_eq!(latency, Samples(240), "5 ms at 48 kHz");

    let mut trimmed = config(Pcm::Float32, ChannelLayout::STEREO);
    trimmed.render.latency = latency;
    let a = buffers(built(limited()), &trimmed).into_planes();
    assert_eq!(a[0].len(), FRAMES, "the trim does not shorten the output");
    assert_audible(&a);

    // Untrimmed, and 240 frames longer (as a tail), so it covers the same
    // span shifted.
    let mut whole = config(Pcm::Float32, ChannelLayout::STEREO);
    whole.render.tail = latency;
    let b = buffers(built(limited()), &whole).into_planes();
    let shifted: Vec<Vec<f32>> = b.iter().map(|p| p[240..].to_vec()).collect();
    assert_same("trimmed against untrimmed from 240", &a, &shifted);
}

/// A lookahead limiter reports no tail: a tail excludes latency, and its
/// lookahead is a pure delay. So a render trimmed by its latency and
/// extended by its tail is exactly the requested duration, `FRAMES`, and not
/// the 239 frames longer the lookahead less one used to add.
///
/// The declared latency must also be the whole of the delay, or a tail of
/// none would cut what is still in the ring. Rendered untrimmed over the
/// latency plus the tail (what the session's `with_reported_tail` renders
/// when nothing is trimmed), the sine (exactly 0 at its frame 0) leaves the
/// limiter exactly the latency late: frames `0..=240` are exactly zero and
/// frame 241 is not.
#[test]
fn a_limiter_adds_no_tail_to_the_render() {
    let graph = built(limited());
    assert_eq!(graph.reported_tail().samples(), Some(Samples::ZERO));
    let latency = graph.reported_latency();
    let mut cfg = config(Pcm::Float32, ChannelLayout::STEREO);
    cfg.render.latency = latency;
    cfg.render.tail = graph.reported_tail().samples().unwrap();
    let planes = buffers(built(limited()), &cfg).into_planes();
    assert_eq!(
        planes[0].len(),
        FRAMES,
        "the duration, and nothing after it"
    );

    let mut whole = config(Pcm::Float32, ChannelLayout::STEREO);
    whole.render.tail = latency + graph.reported_tail().samples().unwrap();
    let planes = buffers(built(limited()), &whole).into_planes();
    let n = latency.get();
    assert_eq!(planes[0].len(), FRAMES + n, "the duration plus the latency");
    for p in &planes {
        assert!(
            p[..=n].iter().all(|&s| s == 0.0),
            "nothing leaves before the latency"
        );
        assert_ne!(p[n + 1], 0.0, "the programme leaves at the latency");
    }
}

/// The graph reports a convolver's tail analytically (a 3 000-tap IR rings
/// 2 999 frames), and a render trimmed by its latency and extended by its
/// tail is the direct convolution, frame for frame, through the tail.
#[test]
fn the_tail_extends_the_render_by_the_ring_out() {
    let graph = built(convolved());
    assert_eq!(
        graph.reported_tail().samples(),
        Some(Samples(2999)),
        "the IR's ring-out"
    );
    let mut cfg = config(Pcm::Float32, ChannelLayout::MONO);
    cfg.render.tail = graph.reported_tail().samples().unwrap();
    // The latency trim too, so both halves of the gate run together.
    cfg.render.latency = graph.reported_latency();
    let planes = buffers(built(convolved()), &cfg).into_planes();
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

/// An editor that does not feed the executor beside it is refused when the
/// `RenderGraph` is assembled, not first at render.
#[test]
fn a_crossed_pair_is_refused_at_construction() {
    let (editor_a, executor_a) = sine(1000.0).build(RenderGraph::prepare(RATE)).unwrap();
    let (editor_b, executor_b) = sine(500.0).build(RenderGraph::prepare(RATE)).unwrap();
    for (editor, executor) in [(editor_a, executor_b), (editor_b, executor_a)] {
        let r = RenderGraph::assemble(editor, executor, Setup::new());
        assert!(
            matches!(r, Err(Error::InvalidConfig(_))),
            "a crossed pair was wrapped"
        );
    }
}

/// A pair prepared at a rate that is not a sample rate (zero, negative, NaN,
/// infinite) is refused when the `RenderGraph` is assembled, naming the rate.
#[test]
fn a_pair_prepared_at_no_rate_is_refused_at_assembly() {
    for bad in [0.0, -48_000.0, f64::INFINITY] {
        let (editor, executor) = sine(1000.0)
            .build(RenderGraph::prepare(SampleRate(bad)))
            .expect("the builder does not judge the rate");
        match RenderGraph::assemble(editor, executor, Setup::new()) {
            Err(Error::InvalidConfig(tutti_export::ConfigError::InvalidRate { prepared })) => {
                assert!(prepared.to_bits() == bad.to_bits(), "{prepared} for {bad}");
            }
            other => panic!("{bad}: expected InvalidRate, got {other:?}"),
        }
    }
}

/// A pair prepared at another rate is refused, not rendered at the wrong one.
#[test]
fn a_graph_prepared_at_another_rate_is_refused() {
    let g = sine(1000.0);
    let (editor, executor) = g
        .build(RenderGraph::prepare(SampleRate(44_100.0)))
        .expect("builds");
    let r = render_to_buffers(
        RenderGraph::assemble(editor, executor, Setup::new()).expect("built together"),
        &config(Pcm::Float32, ChannelLayout::STEREO),
        &FrozenClock,
    );
    assert!(matches!(r, Err(Error::InvalidConfig(_))), "{r:?}");
}

/// The graph is handed the render clock's transport, block by block, read
/// before each block and advanced after: a node walking each block's `Env`
/// ([`BeatPorts`], what `EnvClock` did) emits the offline timeline's beat on
/// every frame, starting at its start beat, and the
/// timeline ends exactly the render's frames on.
#[test]
fn the_graph_reads_the_render_clocks_transport() {
    let start = 4.0;
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(start),
        tempo_map: Arc::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo")),
        sample_rate: RATE,
        loop_range: None,
    });
    let bps = tutti_types::beats_per_sample(Bpm::of(120.0), RATE).get();

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let clock = g.add(Unforkable(BeatPorts));
    g.connect_output(clock, 0, 0).connect_output(clock, 1, 1);
    let out = render_to_buffers(
        built(g),
        &config(Pcm::Float32, ChannelLayout::STEREO),
        &timeline,
    )
    .expect("renders");

    let frames = out.frames().get();
    for i in 0..frames {
        let beat = f64::from(out.planes()[0][i]) + f64::from(out.planes()[1][i]);
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

/// Each frame's beat from its block's `Env` (`Env::for_each_beat`), whole
/// beats then the fraction.
struct BeatPorts;

impl Node for BeatPorts {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO).with_tail(tutti_types::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        cx.env.for_each_beat(|i, beat| {
            io.output(0)[i] = beat.floor().get() as f32;
            io.output(1)[i] = beat.fract().get() as f32;
        });
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- a clip reader: a node that reads the render's transport -------------

/// A 440 Hz sine at the render's rate, one second long: a plain wave table,
/// so a voice reading it at unit rate reproduces it sample for sample.
fn tone() -> Arc<tutti_codec::Wave> {
    let mut w = tutti_codec::Wave::new(1, SampleRate::new(RATE.get()));
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
        tempo_map: Arc::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo")),
        sample_rate: RATE,
        loop_range: None,
    }))
}

/// A voice placed at beat 0, pitched by `cents`, in a `VoicePool` (the node a
/// clip track renders). It reads whatever transport its blocks' `Env` carry:
/// here, the render clock's.
fn placed_pool(cents: f32) -> tutti_sampler::VoicePool {
    use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
    let source = MemorySource::placed(tone(), Beat(0.0), None).expect("a placeable wave");
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                pitch: tutti_types::Cents::new(cents),
                ..Default::default()
            },
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
    render_to_buffers(graph, &config(Pcm::Float32, width), clock)
        .expect("renders")
        .into_planes()
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

/// A sampler voice renders in time at `GRAPH_MAX_BLOCK`, dry and a fifth
/// up: the dry one is the tone it plays, to the bit, on both channels; the
/// render clock ends the render's frames on; and the pitched
/// one matches its golden digest.
///
/// The voice reads the render clock's transport from each block's `Env`,
/// per frame: it seats its read at the playhead on the first frame of each
/// 1024-frame block (rendered in one piece, its scratch sized from the
/// graph's `max_block`) and steps from there. The digest was recorded when
/// voices rendered 64-frame pieces, each re-seated; the one-piece render
/// keeps it, because a seat lands where the step would have put it.
///
/// The pitched digest is self-recorded: it carries the rounding of the
/// vocoder's scalar FFT, over the portable fifth-up figure below; it moves only
/// when that does. The dry digest was cross-checked against a reference render.
///
/// Why a digest and not a tolerance for the pitched voice: the vocoder turns
/// an ulp of beat into far more — computing the chunk positions a different
/// way moves the fifth-up voice by 1e-3 at frame 3076.
#[test]
fn a_sampler_voice_renders_in_time_at_the_graph_block() {
    assert_eq!(GRAPH_MAX_BLOCK.get(), 1024, "the block this pins");
    for (cents, want) in [
        (0.0f32, 0xbd52_29c7_8f1d_ca41u64),
        (700.0, 0xc1f4_ffd7_8658_2ff1),
    ] {
        let clock = timeline();
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let (k, _handle) = g.add_with_controls(placed_pool(cents));
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

/// The same through a fork: a placed `MemorySource` in a live graph that
/// has played on another transport (at beat 0.25), forked offline, plays the
/// tone from the render's start, to the bit, as a voice on the render's
/// transport from the start does. The fork shares nothing with the live node
/// and holds no clock: it reads the render's transport from its `Env`.
#[test]
fn a_forked_clip_reader_renders_the_tone_at_the_graph_block() {
    use tutti_sampler::MemorySource;

    // Live, the voice plays on another transport, somewhere else.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (k, _params) = g.add_with_controls(
        MemorySource::placed(tone(), Beat(0.0), None).expect("a placeable wave"),
    );
    g.pipe_output(k);
    let (live, mut exec) = g
        .build(Prepare::new(RATE, MaxBlock::of(Samples(256))))
        .expect("builds");
    exec.apply_pending();
    let elsewhere = tutti_graph::Transport::new(true, Bpm::of(120.0), Beat(0.25), None);
    let mut sink = vec![0.0f32; 256];
    for _ in 0..8 {
        exec.process(256, &elsewhere, &[], &mut [&mut sink[..]]);
    }
    assert!(
        sink.iter().any(|&s| s != 0.0),
        "sanity: the live reader played before the fork"
    );
    let graph_clock = timeline();
    let forked = support::fork_offline(&live, RATE);
    let b = render_under(forked, &graph_clock, ChannelLayout::MONO);
    assert_audible(&b);
    assert_the_tone("forked", &b[0]);
}
