//! What using this crate looks like, end to end.
//!
//! Three cases, in the order a caller meets them:
//!
//! 1. render a graph to a file,
//! 2. render into memory and inspect it,
//! 3. normalize — which the crate does not do for you, and this shows why
//!    that is a feature rather than a gap.
//!
//! Run: `cargo run --example export -- <outdir>`

use tutti_export::{
    ChannelLayout, EncodeConfig, ExportConfig, FlacLevel, Format, IntDepth, Pcm, Quality,
    RenderConfig, RenderGraph, render_to_buffers, render_to_file,
};
use tutti_graph::GraphBuilder;
use tutti_measure::{LoudnessConfig, measure_loudness};
use tutti_nodes::testing::{Const, Osc};
use tutti_transport::FrozenClock;
use tutti_types::{Amplitude, Hz, SampleRate};
use tutti_types::{Db, Interleaved};

/// The rate every render below runs at.
const RATE: SampleRate = SampleRate(48_000.0);

/// `g`, ready to export: built (every unit prepared) at the render's rate. A
/// graph prepared at another rate is refused rather than re-rated, so a graph
/// is built for the render that takes it.
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

/// A 440 Hz tone at −12 dBFS, in stereo.
fn tone() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(
        Osc::sine(Hz(440.0))
            .with_amplitude(Amplitude(0.25))
            .with_layout(ChannelLayout::STEREO),
    );
    g.pipe_output(id);
    built(g)
}

/// A mono graph, to show the channel fold.
fn mono_tone() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let id = g.add(Const::mono(0.5));
    g.pipe_output(id);
    built(g)
}

fn main() -> tutti_export::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    // ---- 1. a graph to a file -------------------------------------------
    //
    // Configuration is a struct literal: fill in what you mean, let `Default`
    // cover the rest. Every field is read by `render_to_file`; the other entry
    // points ignore some (see `ExportConfig`'s "What each entry point reads").
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: 2.0,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Flac {
                depth: IntDepth::Int24,
                level: FlacLevel::default(),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    // `FrozenClock` says "this graph has no transport" out loud. A graph WITH
    // placed voices takes the `OfflineTimeline` those voices read — the clock is
    // a required argument precisely so a silent render is not something you can
    // get by forgetting one.
    let written = render_to_file(tone(), &config, &FrozenClock, dir.join("tone.flac"))?;
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);

    // Every format goes through the same call. Each is written block by
    // block through `tutti-codec`'s encoder.
    for (format, ext) in [
        (Format::Wav(Pcm::Int24), "wav"),
        (
            Format::Vorbis {
                quality: Quality::default(),
            },
            "ogg",
        ),
        (Format::Aiff(Pcm::Int24), "aiff"),
    ] {
        let s = ExportConfig {
            encode: EncodeConfig {
                format,
                ..config.encode.clone()
            },
            ..config.clone()
        };
        let w = render_to_file(tone(), &s, &FrozenClock, dir.join(format!("tone.{ext}")))?;
        println!("wrote {} ({} bytes)", w.path.display(), w.bytes);
    }

    // ---- 2. a graph into memory -----------------------------------------
    //
    // `Rendered` is one plane per channel, so a surround render survives it. A
    // mono graph asked for as quad is placed by the engine's order: the front
    // pair carries the signal and the rears are silent — not four copies of the
    // same thing.
    let quad = render_to_buffers(
        mono_tone(),
        &ExportConfig {
            encode: EncodeConfig {
                channels: ChannelLayout::QUAD,
                ..Default::default()
            },
            ..config
        },
        &FrozenClock,
    )?;
    let peaks: Vec<f32> = quad
        .planes
        .iter()
        .map(|p| p.iter().fold(0.0f32, |a, &b| a.max(b.abs())))
        .collect();
    println!(
        "mono -> quad: {} frames, per-channel peaks {peaks:?}",
        quad.frames().get()
    );

    // ---- 3. normalization, composed by the caller ------------------------
    //
    // `render_to_file` never normalizes: choosing a gain means measuring the
    // whole signal first, and a stage that hides two passes has to hold the
    // signal to do it. `render_normalized_to_file` is the named two-pass entry
    // point. Done by hand, as here, the measurement streams (`tutti_measure` wraps
    // an online R128 meter) and the gain is one value you apply where you
    // like — including on a second render, without ever holding the audio.
    let mut audio = render_to_buffers(tone(), &config, &FrozenClock)?;

    let cfg = LoudnessConfig::new(audio.sample_rate, ChannelLayout::STEREO);
    let flat = audio.interleaved();
    let before = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO))
        .expect("stereo is meterable");

    // −14 LUFS with a −1 dBTP ceiling — the usual streaming target.
    let gain = before.gain_to(Db(-14.0), Db(-1.0));
    audio.apply_gain(gain);

    let flat = audio.interleaved();
    let after = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO))
        .expect("stereo is meterable");
    println!(
        "normalize: {:.2} LUFS + {:.2} dB -> {:.2} LUFS (peak {:.2} dBTP)",
        before.lufs.get(),
        gain.get(),
        after.lufs.get(),
        after.true_peak.get()
    );

    Ok(())
}
