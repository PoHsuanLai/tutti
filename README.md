# tutti

A type-driven, real-time audio engine for building DAWs in Rust.

[![CI](https://github.com/PoHsuanLai/tutti/actions/workflows/ci.yml/badge.svg)](https://github.com/PoHsuanLai/tutti/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-blue.svg)](#license)

## Overview

tutti is the audio side of a DAW without the application. A graph of audio
processors is a value; tutti compiles it and runs it block by block, on a sound
card or offline into a file. Quantities carry their units (`Hz`, `Db`, `Beat`,
`Samples`), and a call that names a node or port that does not exist fails at
the call. It provides:

- An audio graph that is edited while it plays, with every processor written
  against one `Node` trait, and built-in filters, delays, dynamics and reverbs.
- A session that ties the graph to a transport, a tempo map, MIDI and a device.
- The edges: a sound card, audio files, MIDI 2.0 and hardware MIDI, VST3, CLAP
  and Audio Unit plugins in or out of process, VST2 plugins in process, and a
  faster-than-real-time bounce.

## Example

tutti is not on crates.io yet. Add it from git:

```toml
[dependencies]
tutti = { git = "https://github.com/PoHsuanLai/tutti", features = ["export", "sampler", "dsp", "modulation"] }
```

A sampled saw through a low-pass filter and a reverb, an LFO on the cutoff, and
eight beats bounced to a 24-bit WAV:

<!-- excerpt: crates/tutti/examples/song_bounce.rs#readme -->
```rust
use tutti::export::Written;
use tutti::{
    Amplitude, Beat, Bounce, Bpm, ChannelLayout, Clip, Depth, Error, Format, FreeverbNode, Hz, Lfo,
    MaxBlock, Pcm, Q, Root, RouteSpec, SampleRate, Samples, Session, SessionEvent, SvfFilterNode,
    SvfType, Wave,
};

const RATE: SampleRate = SampleRate::SR_48K;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}

/// Two seconds of a 110 Hz saw, mono: four beats at 120 BPM, and enough
/// harmonics for the filter to sweep.
fn saw() -> Wave {
    let mut wave = Wave::new(1, RATE);
    for i in 0..96_000u32 {
        let phase = (i as f32 * 110.0 / RATE.get() as f32).fract();
        wave.push_frame(&[0.4 * (2.0 * phase - 1.0)]);
    }
    wave
}

fn main() -> Result<(), Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, Into::into);
    let written = song(&dir)?;
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}

/// The song, bounced into `dir`.
fn song(dir: &std::path::Path) -> Result<Written, Error> {
    std::fs::create_dir_all(dir)?;

    let mut s = Session::builder().build_offline(
        RATE,
        ChannelLayout::STEREO,
        MaxBlock::of(Samples(512)),
    )?;
    s.set_tempo(Bpm::of(120.0)).expect("not following");

    // The sampler: the same mono wave on beats 0 and 4, each a node as it
    // stands, placed at stereo to join the stereo chain.
    let wave = std::sync::Arc::new(saw());
    let (first, _) = s.graph().add(
        Clip::new(wave.clone())?
            .channels(ChannelLayout::STEREO)?
            .at(Beat(0.0)),
    );
    let (second, _) = s.graph().add(
        Clip::new(wave)?
            .channels(ChannelLayout::STEREO)?
            .at(Beat(4.0))
            .gain(Amplitude::new(0.8)),
    );

    // A stereo low-pass, then a reverb whose input is mono: that width change
    // is a bus edge, never a truncation (`connect` would refuse it).
    let (filter, _) = s.graph().add(SvfFilterNode::<f64>::with_channels(
        ChannelLayout::STEREO,
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let (reverb, _) = s.graph().add(FreeverbNode::default());
    s.graph().connect(first, filter)?;
    s.graph().connect(second, filter)?; // adds: both clips sum into the filter
    s.graph().connect(filter.bus_out(0), reverb.bus_in(0))?;
    for c in 0..2 {
        s.graph().connect(filter.out(c), Root::output(c))?;
        s.graph().connect(reverb.out(c), Root::output(c))?;
    }

    // An LFO on the cutoff: 1 Hz across 200 Hz to 3 kHz.
    let cutoff = filter.cutoff();
    let mut m = s.mods_mut();
    m.range(cutoff, Hz(200.0)..Hz(3_000.0))?;
    let wobble = m.lfo(Lfo::sine().rate(Hz(1.0)));
    m.route(RouteSpec::new(wobble, cutoff, Depth::of(0.5)))?;

    // Commit the graph and the lanes, then roll the transport.
    report(s.update());
    report(s.update());
    s.play()?;

    // Eight beats, to a 24-bit WAV.
    let path = dir.join("song.wav");
    let written = s
        .bounce(
            Bounce::project()
                .range(Beat(0.0)..Beat(8.0))
                .master(&path, Format::Wav(Pcm::Int24)),
        )?
        .into_master();
    Ok(written)
}
```

The program, with its test, is
[`song_bounce.rs`](crates/tutti/examples/song_bounce.rs). The sound card is the
`device` feature; on Linux it needs the ALSA headers (`libasound2-dev
pkg-config`).

## Documentation

`cargo doc -p tutti --open` builds the API docs, and every crate's docs carry a
runnable example for its subsystem. The programs in
[`crates/tutti/examples`](crates/tutti/examples) cover whole tasks.

## Crates

- [`tutti`](crates/tutti): everything behind one dependency, chosen by features.
- [`tutti-graph`](crates/tutti-graph): the audio graph, its compiler and its executor.
- [`tutti-session`](crates/tutti-session): the graph, transport, MIDI and device in one `Session`.
- [`tutti-nodes`](crates/tutti-nodes) and [`tutti-dsp`](crates/tutti-dsp): the built-in effects and the algorithms behind them.
- [`tutti-measure`](crates/tutti-measure): loudness, pitch, onsets, tempo and spectra.
- [`tutti-sampler`](crates/tutti-sampler), [`tutti-polysynth`](crates/tutti-polysynth) and [`tutti-soundfont`](crates/tutti-soundfont): sample playback, a synth and SoundFont 2.
- [`tutti-ump`](crates/tutti-ump), [`tutti-midi-protocol`](crates/tutti-midi-protocol), [`tutti-midi-file`](crates/tutti-midi-file) and [`tutti-midi-hardware`](crates/tutti-midi-hardware): MIDI 2.0 messages, translation, files and devices.
- [`tutti-codec`](crates/tutti-codec), [`tutti-export`](crates/tutti-export) and [`tutti-cpal`](crates/tutti-cpal): audio files, offline render and the sound card.
- [`tutti-plugin`](crates/tutti-plugin): plugin hosting.
- [`arc-lend`](crates/arc-lend): a cell that hands shared data to an audio thread, which never frees it.

A Bevy adapter lives at [PoHsuanLai/bevy-tutti](https://github.com/PoHsuanLai/bevy-tutti).

## Getting help

Open an [issue](https://github.com/PoHsuanLai/tutti/issues).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Supported Rust versions

tutti supports the stable Rust pinned in
[`rust-toolchain.toml`](rust-toolchain.toml), and CI tests only that
toolchain.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
