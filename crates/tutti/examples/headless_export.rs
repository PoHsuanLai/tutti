//! Render a graph to a file and measure it — through **one dependency**.
//!
//! This is the same program as `tutti-export`'s own `examples/export.rs`,
//! whose header reads "What using this crate looks like, end to end". That
//! version names five crates:
//!
//! ```text
//! use tutti_analysis::{measure_loudness, LoudnessConfig};
//! use tutti_core::{Amplitude, FrozenClock, Hz, SampleRate};
//! use tutti_export::{render_to_buffers, render_to_file, RenderGraph, ...};
//! use tutti_graph::GraphBuilder;
//! use tutti_types::{Db, Interleaved};
//! ```
//!
//! Here the imports group by *subsystem* rather than by crate boundary, which
//! is the actual readability win — a reader does not have to know which of
//! five packages a given name lives in.
//!
//! Note that `tutti-analysis` is reachable here and **is not reachable
//! through `bevy-tutti` at all** — that umbrella does not depend on it. So
//! this program could not have been written through the Bevy adapter either.
//!
//! The original stays where it is: it proves `tutti-export` is usable
//! standalone, which is the layering claim this crate must not obscure.
//!
//! Run: `cargo run -p tutti --features "export,analysis,wav" \
//!        --example headless_export -- <outdir>`

use tutti::analysis::{measure_loudness, LoudnessConfig};
use tutti::core::FrozenClock;
use tutti::export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, EncodeConfig, ExportConfig,
    RenderConfig, RenderGraph,
};
use tutti::graph::GraphBuilder;
use tutti::prelude::*;
use tutti_core::Hz;
use tutti_nodes::testing::{Const, Osc};

/// The rate every render below runs at.
const RATE: SampleRate = SampleRate(48_000.0);

/// `g`, ready to export: built (every unit prepared) at the render's rate. A
/// graph prepared at another rate is refused rather than re-rated.
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph { editor, executor }
}

/// A 440 Hz tone at −12 dBFS, in stereo.
fn tone() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add_unit(Box::new(
        Osc::sine(Hz(440.0))
            .with_amplitude(Amplitude(0.25))
            .with_layout(ChannelLayout::STEREO),
    ));
    g.pipe_output(id);
    built(g)
}

/// A mono graph, to show the channel fold.
fn mono_tone() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let id = g.add_unit(Box::new(Const::mono(0.5)));
    g.pipe_output(id);
    built(g)
}

fn config(seconds: f64, channels: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: seconds,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Float32,
            channels,
        },
        ..Default::default()
    }
}

fn main() -> tutti::export::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    // 1. A graph to a file.
    let path = dir.join("tone.wav");
    let written = render_to_file(
        tone(),
        &config(1.0, ChannelLayout::STEREO),
        &FrozenClock,
        &path,
    )?;
    println!("wrote {} ({written:?})", path.display());

    // 2. The same graph into memory, and a measurement of it.
    let rendered = render_to_buffers(tone(), &config(1.0, ChannelLayout::STEREO), &FrozenClock)?;
    let flat = rendered.interleaved();
    let cfg = LoudnessConfig::new(rendered.sample_rate, ChannelLayout::STEREO);
    let loudness = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO))
        .expect("stereo is meterable");
    println!(
        "loudness: {:.1} LUFS, true peak {:.1} dBTP",
        loudness.lufs.get(),
        loudness.true_peak.get()
    );

    // 3. A mono graph into a stereo file — the fold, which the export layer
    //    does for you and the graph does not.
    let mono_path = dir.join("mono-to-stereo.wav");
    render_to_file(
        mono_tone(),
        &config(0.5, ChannelLayout::STEREO),
        &FrozenClock,
        &mono_path,
    )?;
    println!("wrote {}", mono_path.display());

    Ok(())
}
