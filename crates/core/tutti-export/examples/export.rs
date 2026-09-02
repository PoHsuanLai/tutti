//! What using this crate looks like, end to end.
//!
//! Three cases, in the order a caller meets them:
//!
//! 1. render a graph to a file,
//! 2. render into memory and inspect it,
//! 3. normalize — which the crate does **not** do for you, and this shows why
//!    that is a feature rather than a gap.
//!
//! Run: `cargo run --example export -- <outdir>`

use tutti_analysis::{measure_loudness, LoudnessConfig};
use tutti_core::dsp::{dc, sine_hz, Net};
use tutti_core::{FrozenClock, SampleRate};
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, ChannelLayout, EncodeConfig,
    ExportConfig, RenderConfig,
};
use tutti_types::{Db, Interleaved};

/// A 440 Hz tone at −12 dBFS, in stereo.
fn tone() -> Net {
    let mut net = Net::new(0, 2);
    let id = net.push(Box::new((sine_hz::<f32>(440.0) | sine_hz::<f32>(440.0)) * 0.25));
    net.pipe_output(id);
    net
}

/// A mono graph, to show the channel fold.
fn mono_tone() -> Net {
    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(dc(0.5)));
    net.pipe_output(id);
    net
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
    // cover the rest. There is no builder, so there is no way to set something
    // the path you chose will quietly ignore.
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(48_000.0),
            duration_seconds: 2.0,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Flac(Default::default()),
            bit_depth: BitDepth::Int24,
            ..Default::default()
        },
        ..Default::default()
    };

    // `FrozenClock` says "this graph has no transport" out loud. A graph WITH
    // placed voices takes the `OfflineTimeline` those voices read — the clock is
    // a required argument precisely so a silent render is not something you can
    // get by forgetting one.
    let written = render_to_file(tone(), &config, &FrozenClock, &dir.join("tone.flac"))?;
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);

    // Every format goes through the same call. FLAC streams through flacenc's
    // pull API, Ogg and AIFF through their own incremental writers.
    for (format, ext) in [
        (AudioFormat::Wav, "wav"),
        (AudioFormat::OggVorbis(Default::default()), "ogg"),
        (AudioFormat::Aiff, "aiff"),
    ] {
        let s = ExportConfig {
            encode: EncodeConfig {
                format,
                ..config.encode
            },
            ..config
        };
        let w = render_to_file(tone(), &s, &FrozenClock, &dir.join(format!("tone.{ext}")))?;
        println!("wrote {} ({} bytes)", w.path.display(), w.bytes);
    }

    // ---- 2. a graph into memory -----------------------------------------
    //
    // `Rendered` is one plane per channel, so a surround render survives it. A
    // mono graph asked for as quad folds per the ITU matrix: channel 0 carries
    // the signal, the rest are silent — not four copies of the same thing.
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
    // This crate does not normalize, and that is deliberate. Choosing a gain
    // means measuring the whole signal first, and a stage that hides two passes
    // has to hold the signal to do it. Split apart, the measurement streams
    // (`tutti_analysis` wraps an online R128 meter) and the gain is one value
    // you apply where you like — including on a second render, without ever
    // holding the audio.
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
