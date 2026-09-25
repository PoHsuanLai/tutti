//! Offline render throughput — how much faster than realtime a bounce runs.
//!
//! The number to read is **× realtime**: a case rendering 1 second of audio
//! in 10 ms runs at 100× realtime, so an hour-long project bounces in about
//! 36 seconds. Criterion's `Throughput::Elements(frames)` prints elem/s;
//! divide by the sample rate.
//!
//! # What is measured, and what is deliberately not
//!
//! `render/` drives `render_to_buffers` — no file, no encoder, no
//! filesystem. That is the part this crate owns and the part a change here
//! can regress.
//!
//! `encode/` writes real files into a `tempfile::TempDir`, and is
//! **track-only: never gate it.** Its cost is dominated by the page cache and
//! by third-party encoders (`hound`, `flacenc`, `vorbis_rs`), so a regression
//! in it usually says something about the machine. It is here because the
//! render/encode split is the useful diagnostic: when an export is slow,
//! `encode` minus `render` at matching settings says which half to look at.
//!
//! Durations are kept short on purpose. Once the rendered planes get large
//! the working set leaves criterion's domain — the same boundary
//! `tutti-sampler`'s `profile_stretch_clone` documents at 81× wall-clock
//! spread — and the honest instrument becomes `--profile-time` plus samply.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{Amplitude, ChannelLayout, FrozenClock, Hz, SampleRate};
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, Dither, EncodeConfig, ExportConfig,
    RenderConfig, RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};

const SR: f64 = 48_000.0;

/// `g`, built for an export at `rate` — the config's render rate, which the
/// graph must be prepared at.
///
/// Built per iteration, like the `Net` it replaces was: an export consumes its
/// graph, so the build (preparing every unit, compiling the plan) is part of
/// what a bounce costs.
fn built(g: GraphBuilder, rate: f64) -> RenderGraph {
    let (editor, executor) = g
        .build(RenderGraph::prepare(SampleRate(rate)))
        .expect("builds");
    RenderGraph::Graph { editor, executor }
}

fn tone_graph(rate: f64) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add_unit(Box::new(
        Osc::sine(Hz(440.0))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO),
    ));
    g.pipe_output(id);
    built(g, rate)
}

fn dc_graph(rate: f64) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add_unit(Box::new(Const::frame(&[0.25, 0.25])));
    g.pipe_output(id);
    built(g, rate)
}

fn config(seconds: f64, rate: f64, channels: ChannelLayout, format: AudioFormat) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(rate),
            duration_seconds: seconds,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth: BitDepth::Float32,
            channels,
        },
        dither: Dither::Off,
        ..Default::default()
    }
}

/// Render throughput by duration, width and rate. No file touched.
fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");

    for seconds in [0.25f64, 1.0] {
        let cfg = config(seconds, SR, ChannelLayout::STEREO, AudioFormat::Wav);
        let frames = (seconds * SR) as u64;
        group.throughput(Throughput::Elements(frames));
        group.bench_with_input(
            BenchmarkId::new("stereo-48k", format!("{seconds}s")),
            &cfg,
            |b, cfg| {
                b.iter(|| {
                    black_box(
                        render_to_buffers(tone_graph(SR), cfg, &FrozenClock).expect("renders"),
                    )
                })
            },
        );
    }

    // Width: the graph work is constant, the planes are not.
    for (name, layout) in [
        ("mono", ChannelLayout::MONO),
        ("stereo", ChannelLayout::STEREO),
        ("quad", ChannelLayout::from(4usize)),
        ("5.1", ChannelLayout::from(6usize)),
    ] {
        let cfg = config(0.25, SR, layout, AudioFormat::Wav);
        group.throughput(Throughput::Elements((0.25 * SR) as u64));
        group.bench_with_input(BenchmarkId::new("width", name), &cfg, |b, cfg| {
            b.iter(|| {
                black_box(render_to_buffers(dc_graph(SR), cfg, &FrozenClock).expect("renders"))
            })
        });
    }

    // Rate: twice the samples for the same wall-clock duration.
    for rate in [48_000.0f64, 96_000.0] {
        let cfg = config(0.25, rate, ChannelLayout::STEREO, AudioFormat::Wav);
        group.throughput(Throughput::Elements((0.25 * rate) as u64));
        group.bench_with_input(BenchmarkId::new("rate", rate as u64), &cfg, |b, cfg| {
            b.iter(|| {
                black_box(render_to_buffers(tone_graph(rate), cfg, &FrozenClock).expect("renders"))
            })
        });
    }

    group.finish();
}

/// Encoder cost, by format. **Track-only — never gate this.**
fn bench_encode(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut group = c.benchmark_group("encode");
    group.throughput(Throughput::Elements((0.25 * SR) as u64));

    // A `Vec`, not an array: a `#[cfg]` on an array element changes its
    // length, which the type cannot express. The `mut` is only used when a
    // pushing arm is compiled in.
    #[cfg_attr(not(any(feature = "flac", feature = "ogg")), allow(unused_mut))]
    let mut formats: Vec<(&str, AudioFormat)> = vec![("wav", AudioFormat::Wav)];
    #[cfg(feature = "flac")]
    formats.push(("flac", AudioFormat::Flac(Default::default())));
    #[cfg(feature = "ogg")]
    formats.push(("ogg", AudioFormat::OggVorbis(Default::default())));

    for (name, format) in formats {
        // Int24, not Float32: FLAC is an integer codec and errors on a float
        // depth rather than downgrading (see `tests/roundtrip.rs`). One depth
        // across the group also keeps the formats comparable.
        let mut cfg = config(0.25, SR, ChannelLayout::STEREO, format);
        cfg.encode.bit_depth = BitDepth::Int24;
        let path = dir.path().join(format!("bench.{name}"));
        group.bench_with_input(BenchmarkId::from_parameter(name), &cfg, |b, cfg| {
            b.iter(|| {
                black_box(render_to_file(tone_graph(SR), cfg, &FrozenClock, &path).expect("writes"))
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_render, bench_encode);
criterion_main!(benches);
