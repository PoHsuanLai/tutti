//! Bounce a project to a file and measure it: a session and `Bounce`, through
//! **one dependency**.
//!
//! This is the program `tutti-export`'s own `examples/export.rs` is, one layer
//! up (its header reads "What using this crate looks like, end to end"). That
//! one renders a graph it built and sealed itself, with the pipeline's `Job`
//! and targets: it proves `tutti-export` is usable standalone, which is the
//! layering claim this crate must not obscure. This one has a `Session`, which
//! knows the project: the rate, the clock, the graph's latency and tail and the
//! root's width are derived from it (doc 019 section 3), and a request names
//! only what it wants.
//!
//! The imports group by *subsystem* rather than by crate boundary, which is
//! the actual readability win. `tutti-dsp` and `tutti-measure` are reachable
//! here and **are not reachable through `bevy-tutti` at all**, which does not
//! depend on them.
//!
//! Run: `cargo run -p tutti --features "export,dsp,wav" \
//!        --example headless_export -- <outdir>`

use tutti::measure::{LoudnessConfig, measure_loudness};
use tutti::nodes::testing::{Const, Osc};
use tutti::prelude::Root;
use tutti::prelude::*;

/// The rate every render below runs at.
const RATE: SampleRate = SampleRate(48_000.0);

/// A detached session whose outputs hear `node`, one second of a project.
fn session_with(node: impl tutti::graph::Node + Clone + 'static) -> Session {
    let mut session = Session::builder()
        .build_detached(RATE)
        .expect("a detached session builds");
    let (node, _) = session.graph().add(tutti::graph::ForkByClone(node));
    session.graph().connect(node, Root).unwrap();
    session
}

/// A 440 Hz tone at -12 dBFS, in stereo.
fn tone() -> Session {
    session_with(
        Osc::sine(Hz(440.0))
            .with_amplitude(Amplitude(0.25))
            .with_layout(ChannelLayout::STEREO),
    )
}

/// A mono project, to show the channel fold.
fn mono_tone() -> Session {
    session_with(Const::mono(0.5))
}

/// The first `seconds` of the project, 24-bit.
fn first(seconds: f64) -> Bounce {
    Bounce::project()
        .format(Format::Wav(Pcm::Int24))
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    // 1. A project to a file.
    let path = dir.join("tone.wav");
    let written = tone().bounce(first(1.0).master(&path))?.into_master();
    println!("wrote {} ({written:?})", path.display());

    // 2. The same project into memory, and a measurement of it.
    let rendered = tone().bounce(first(1.0).master_buffers())?.into_master();
    let flat = rendered.interleaved();
    let cfg = LoudnessConfig::new(rendered.sample_rate(), ChannelLayout::STEREO);
    let loudness = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO))
        .expect("stereo is meterable");
    println!(
        "loudness: {:.1} LUFS, true peak {:.1} dBTP",
        loudness.lufs.get(),
        loudness.true_peak.get()
    );

    // 3. A mono project into a stereo file: the fold, which the export layer
    //    does for you and the graph does not.
    let mono_path = dir.join("mono-to-stereo.wav");
    mono_tone().bounce(
        first(0.5)
            .channels(ChannelLayout::STEREO)
            .master(&mono_path),
    )?;
    println!("wrote {}", mono_path.display());

    Ok(())
}
