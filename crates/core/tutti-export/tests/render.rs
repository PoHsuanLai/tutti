//! Behavioural tests for the render + encode path.
//!
//! The crate had 45 tests and none of them rendered anything through a real
//! encoder end to end, which is how three of four output formats came to be
//! broken and an upmix export came to panic, both with a green suite. Every test
//! here fails against the code as it was.

use fundsp::prelude32::*;
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, ChannelLayout, EncodeSpec,
    ExportSpec, FrozenClock, LatencyTrim, RenderClock, RenderDuration, RenderSpec, Resample,
};

fn net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new(dc((0.5, 0.5))));
    n.pipe_output(id);
    n
}
fn spec(format: AudioFormat, bd: BitDepth, layout: ChannelLayout) -> ExportSpec {
    ExportSpec {
        render: RenderSpec {
            sample_rate: tutti_core::SampleRate(44100.0),
            duration: RenderDuration::Seconds(0.2),
            ..Default::default()
        },
        encode: EncodeSpec {
            format,
            bit_depth: bd,
            channels: layout,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn all_four_formats_export() {
    let d = tempfile::tempdir().unwrap();
    for (f, bd, ext) in [
        (AudioFormat::Wav, BitDepth::Int24, "wav"),
        (AudioFormat::Flac, BitDepth::Int24, "flac"),
        (AudioFormat::OggVorbis, BitDepth::Int24, "ogg"),
        (AudioFormat::Aiff, BitDepth::Int24, "aiff"),
    ] {
        let p = d.path().join(format!("a.{ext}"));
        let r = render_to_file(net(), &spec(f, bd, ChannelLayout::Stereo), &FrozenClock, &p);
        match &r {
            Ok(w) => println!("{ext}: OK {} bytes", w.bytes),
            Err(e) => println!("{ext}: ERR {e}"),
        }
        assert!(r.is_ok(), "{ext} failed: {r:?}");
        assert!(
            r.unwrap().bytes > 100,
            "{ext} wrote a suspiciously small file"
        );
    }
}

#[test]
fn upmix_does_not_panic_and_leaves_extras_silent() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("q.wav");
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let id = n.push(Box::new(dc(0.5)));
    n.pipe_output(id);
    render_to_file(
        n,
        &spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Quad),
        &FrozenClock,
        &p,
    )
    .unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().channels, 4);
    let s: Vec<f32> = rd.into_samples::<f32>().map(|x| x.unwrap()).collect();
    let mut ch = [0.0f32; 4];
    for f in s.chunks_exact(4) {
        for (a, &v) in ch.iter_mut().zip(f) {
            *a = a.max(v.abs());
        }
    }
    println!("mono->quad channel peaks: {ch:?}");
    assert!(ch[0] > 0.4, "ch0 carries the signal");
    for c in 1..4 {
        assert!(ch[c] < 1e-6, "ch{c} must be silent, got {}", ch[c]);
    }
}

/// The clock is advanced once per block, by exactly the frames produced.
///
/// This is the test the old `transport()` setter never had: sabotaging that
/// setter to discard its argument passed all 45 tests, even though its own doc
/// warned the failure mode was total silence. A clock that is never advanced
/// leaves every placed voice at beat 0.
#[test]
fn the_clock_advances_by_exactly_the_frames_rendered() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingClock(AtomicUsize);
    impl RenderClock for CountingClock {
        fn advance(&self, frames: tutti_types::Samples) {
            self.0.fetch_add(frames.get(), Ordering::Relaxed);
        }
    }

    let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
    let out = render_to_buffers(
        net(),
        &spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo),
        clock.as_ref(),
    )
    .unwrap();

    let frames = out.frames().get();
    assert_eq!(frames, 8820, "0.2 s at 44.1 kHz");
    assert_eq!(
        clock.0.load(Ordering::Relaxed),
        frames,
        "the clock must advance exactly once per rendered frame"
    );
}

/// A latency trim renders extra frames and drops them from the head, so the
/// output is still the requested length — not short by the trim.
#[test]
fn a_latency_trim_preserves_the_output_length() {
    let mut s = spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    let untrimmed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    s.render.latency = LatencyTrim::Exact(tutti_types::Samples(512));
    let trimmed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    assert_eq!(
        untrimmed.frames(),
        trimmed.frames(),
        "trimming must not shorten the output"
    );
}

/// `render_to_buffers` reports the rate its samples are actually at, and gives
/// back one plane per channel rather than a stereo pair.
#[test]
fn buffers_report_their_own_shape() {
    let out = render_to_buffers(
        net(),
        &spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Quad),
        &FrozenClock,
    )
    .unwrap();
    assert_eq!(out.channels(), 4, "a quad request yields four planes");
    assert_eq!(out.sample_rate.get(), 44100.0);
    assert!(out.planes.iter().all(|p| p.len() == out.frames().get()));
}

/// Normalization composes: measure with tutti-analysis, apply the gain it
/// reports. The crate does not hide this, which is why it can stream.
#[test]
fn a_caller_can_compose_normalization() {
    use tutti_analysis::{measure_loudness, LoudnessConfig};
    use tutti_types::Db;

    // Longer than R128's 400 ms gating block, or the meter reports nothing
    // passed the gate and there is no loudness to normalize toward.
    let mut long = spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    long.render.duration = RenderDuration::Seconds(2.0);
    let mut out = render_to_buffers(net(), &long, &FrozenClock).unwrap();

    let cfg = LoudnessConfig::new(out.sample_rate, ChannelLayout::Stereo);
    let before = measure_loudness(&cfg, &out.interleaved()).unwrap();
    let gain = before.gain_to(Db(-14.0), Db(-1.0));
    out.apply_gain(gain);
    let after = measure_loudness(&cfg, &out.interleaved()).unwrap();

    assert!(
        (after.lufs.get() - (before.lufs.get() + gain.get())).abs() < 0.5,
        "applying the reported gain must move loudness by that gain: \
         {before:?} + {gain:?} -> {after:?}"
    );
}

/// A requested resample reaches the file: the header carries the target rate,
/// and the frame count matches the converted duration.
///
/// `render_to_file` used to accept `sample_rate` and silently ignore it on the
/// in-memory path while honouring it on the file path — two terminals with the
/// same settings and different behaviour.
#[test]
fn a_resample_request_reaches_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("r.wav");
    let mut s = spec(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    s.render.duration = RenderDuration::Seconds(1.0);
    s.resample = Some(Resample::to(48_000));

    render_to_file(net(), &s, &FrozenClock, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(
        rd.spec().sample_rate,
        48_000,
        "header must carry the target"
    );
    let frames = rd.into_samples::<f32>().count() / 2;
    let err = (frames as i64 - 48_000).abs();
    assert!(
        err < 512,
        "expected ~48000 frames at the new rate, got {frames}"
    );
}
