//! A small song, end to end, through **one import**: a sampler into a filter
//! into a reverb, an LFO sweeping the cutoff, the transport played, and eight
//! beats bounced to a WAV file (the reverb's tail rings on past the eighth).
//!
//! ```text
//! cargo run -p tutti --features "export,sampler,dsp,modulation" --example song_bounce -- <outdir>
//! ```
//!
//! `use tutti::prelude::*;` carries the session, the units, the sampler's
//! `Clip`, the nodes, `Lfo`, `Bounce` and `Format`; the two bus names a fold
//! needs are `tutti::session::` paths. Every fallible call returns the one
//! `tutti::Error`, so `?` runs the whole program.

use tutti::export::Written;
use tutti::prelude::*;

const RATE: SampleRate = SampleRate(48_000.0);

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

fn main() -> Result<(), tutti::Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, Into::into);
    let written = song(&dir)?;
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}

/// The song, bounced into `dir`.
fn song(dir: &std::path::Path) -> Result<Written, tutti::Error> {
    std::fs::create_dir_all(dir)?;

    let mut s = Session::builder().build_offline(
        RATE,
        ChannelLayout::STEREO,
        MaxBlock::of(Samples(512)),
    )?;
    s.set_tempo(Bpm::of(120.0));

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
    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 200.0, 3_000.0);
    let wobble = m.lfo(Lfo::sine().hz(Hz(1.0)));
    m.route(wobble, cutoff, Depth(0.5))?;

    // Commit the graph and the lanes, then roll the transport.
    report(s.update());
    report(s.update());
    s.play()?;

    // Eight beats, to a 24-bit WAV.
    let path = dir.join("song.wav");
    let written = s
        .bounce(
            Bounce::project()
                .format(Format::Wav(Pcm::Int24))
                .range(Beat(0.0)..Beat(8.0))
                .master(&path),
        )?
        .into_master();
    Ok(written)
}

#[cfg(test)]
mod tests {
    /// **The song bounces eight beats and more, and is not silence.** At 120
    /// BPM eight beats are 192 000 frames; the file (24-bit stereo, 6 bytes a
    /// frame) holds at least that, plus the reverb's tail, and its samples are
    /// not all zero.
    #[test]
    fn the_song_bounces_and_is_heard() {
        let dir = tempfile::tempdir().unwrap();
        let written = super::song(dir.path()).expect("the song renders");
        assert!(written.bytes >= 192_000 * 6, "{} bytes", written.bytes);
        let file = std::fs::read(&written.path).unwrap();
        assert!(file[44..].iter().any(|&b| b != 0), "silence");
    }
}
