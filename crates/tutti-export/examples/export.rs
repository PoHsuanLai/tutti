//! What using this crate looks like, end to end.
//!
//! Four cases, in the order a caller meets them:
//!
//! 1. render a graph to a file,
//! 2. render once into several files of different formats,
//! 3. render into memory and inspect it,
//! 4. normalize, which keeps the render and measures it rather than rendering
//!    twice.
//!
//! Run: `cargo run --example export -- <outdir>`

use tutti_export::{
    BufferTarget, ChannelLayout, Control, FileTarget, FlacLevel, Format, IntDepth, Job, Normalize,
    NormalizeSpec, Output, Pcm, Produced, Quality, RenderGraph, Setup, render, render_one,
};
use tutti_graph::GraphBuilder;
use tutti_measure::{LoudnessConfig, measure_loudness};
use tutti_nodes::testing::{Const, Osc};
use tutti_transport::FrozenClock;
use tutti_types::{Amplitude, Hz, SampleRate, Samples, Seconds};
use tutti_types::{Db, Interleaved};

/// The rate every render below runs at.
const RATE: SampleRate = SampleRate(48_000.0);

/// `g`, ready to export: built (every unit prepared) at the render's rate. A
/// graph prepared at another rate is refused rather than re-rated, so a graph
/// is built for the render that takes it.
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
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
    // What is rendered is a `Job`: a rate and frame counts (the audible
    // length, the latency trimmed from the head, the tail kept past the end),
    // and nothing else. Where it goes is a target: `FileTarget` names the
    // path, the format and the width, and its setters add a resample, a dither
    // or a normalize.
    let job = Job::new(RATE, Seconds(2.0).to_samples(RATE), Samples(0), Samples(0))
        .expect("a two-second render");
    let flac = Format::Flac {
        depth: IntDepth::Int24,
        level: FlacLevel::default(),
    };

    // `FrozenClock` says "this graph has no transport" out loud. A graph WITH
    // placed voices takes the `OfflineTimeline` those voices read — the clock is
    // a required argument precisely so a silent render is not something you can
    // get by forgetting one.
    let target = FileTarget::new(dir.join("tone.flac"), flac, ChannelLayout::STEREO);
    let written = render_one(
        &mut tone(),
        &job,
        &FrozenClock,
        Output::master(target),
        &mut Control::none(),
    )?
    .finished()
    .and_then(Produced::file)
    .expect("nothing cancels a control with no token");
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);

    // ---- 2. one render, several files ------------------------------------
    //
    // The graph is stepped once per block however many outputs there are; each
    // output has its own format, resample, dither and normalize. Every format
    // is written block by block through `tutti-codec`'s encoder.
    let outputs: Vec<Output> = [
        (Format::Wav(Pcm::Int24), "wav"),
        (
            Format::Vorbis {
                quality: Quality::default(),
            },
            "ogg",
        ),
        (Format::Aiff(Pcm::Int24), "aiff"),
    ]
    .into_iter()
    .map(|(format, ext)| {
        Output::master(FileTarget::new(
            dir.join(format!("tone.{ext}")),
            format,
            ChannelLayout::STEREO,
        ))
    })
    .collect();
    let produced = render(
        &mut tone(),
        &job,
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )?
    .finished()
    .expect("nothing cancels a control with no token");
    for p in produced {
        let w = p.file().expect("a file output");
        println!("wrote {} ({} bytes)", w.path.display(), w.bytes);
    }

    // ---- 3. a graph into memory -----------------------------------------
    //
    // `Rendered` is one plane per channel, so a surround render survives it. A
    // mono graph asked for as quad is placed by the engine's order: the front
    // pair carries the signal and the rears are silent — not four copies of the
    // same thing.
    let quad = render_one(
        &mut mono_tone(),
        &job,
        &FrozenClock,
        Output::master(BufferTarget::new(ChannelLayout::QUAD)),
        &mut Control::none(),
    )?
    .finished()
    .and_then(Produced::buffers)
    .expect("nothing cancels a control with no token");
    let peaks: Vec<f32> = quad
        .planes()
        .iter()
        .map(|p| p.iter().fold(0.0f32, |a, &b| a.max(b.abs())))
        .collect();
    println!(
        "mono -> quad: {} frames, per-channel peaks {peaks:?}",
        quad.frames().get()
    );

    // ---- 4. normalization ------------------------------------------------
    //
    // Choosing a gain means measuring the whole signal first, so an output that
    // asks for it keeps what the graph rendered (in memory, or in a temporary
    // file past 512 MiB), measures it at the file's rate and writes it with the
    // gain. The graph is not rendered twice; `Written::gain` is what was applied.
    //
    // −14 LUFS with a −1 dBTP ceiling — the usual streaming target.
    let normalized = FileTarget::new(
        dir.join("tone-normalized.flac"),
        flac,
        ChannelLayout::STEREO,
    )
    .with_normalize(Some(NormalizeSpec::own(Normalize::lufs(Db(-14.0)))));
    let written = render_one(
        &mut tone(),
        &job,
        &FrozenClock,
        Output::master(normalized),
        &mut Control::none(),
    )?
    .finished()
    .and_then(Produced::file)
    .expect("nothing cancels a control with no token");
    println!(
        "normalize: gain {:.2} dB, measured {:.2} LUFS before",
        written
            .gain
            .expect("a normalized output reports its gain")
            .get(),
        written.measured.expect("and the reading").lufs.get()
    );

    // The measurement is also a plain function over planes, when a caller
    // wants to hold the audio and decide for itself.
    let audio = render_one(
        &mut tone(),
        &job,
        &FrozenClock,
        Output::master(BufferTarget::new(ChannelLayout::STEREO)),
        &mut Control::none(),
    )?
    .finished()
    .and_then(Produced::buffers)
    .expect("nothing cancels a control with no token");
    let cfg = LoudnessConfig::new(audio.sample_rate(), ChannelLayout::STEREO);
    let flat = audio.interleaved();
    let before = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO))
        .expect("stereo is meterable");
    println!("by hand: {:.2} LUFS", before.lufs.get());

    Ok(())
}
