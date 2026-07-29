//! Behavioural tests for the render + encode path.
//!
//! The crate had 45 tests and none of them rendered anything through a real
//! encoder end to end, which is how three of four output formats came to be
//! broken and an upmix export came to panic, both with a green suite. Every test
//! here fails against the code as it was.

use fundsp::prelude32::*;
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, ChannelLayout, EncodeConfig,
    ExportConfig, FrozenClock, RenderClock, RenderConfig, Resample,
};

fn net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new(dc((0.5, 0.5))));
    n.pipe_output(id);
    n
}
fn config(format: AudioFormat, bd: BitDepth, layout: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(44100.0),
            duration_seconds: 0.2,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth: bd,
            channels: layout,
        },
        ..Default::default()
    }
}

#[test]
fn all_four_formats_export() {
    let d = tempfile::tempdir().unwrap();
    for (f, bd, ext) in [
        (AudioFormat::Wav, BitDepth::Int24, "wav"),
        (
            AudioFormat::Flac(Default::default()),
            BitDepth::Int24,
            "flac",
        ),
        (
            AudioFormat::OggVorbis(Default::default()),
            BitDepth::Int24,
            "ogg",
        ),
        (AudioFormat::Aiff, BitDepth::Int24, "aiff"),
    ] {
        let p = d.path().join(format!("a.{ext}"));
        let r = render_to_file(
            net(),
            &config(f, bd, ChannelLayout::Stereo),
            &FrozenClock,
            &p,
        );
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
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Quad),
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
    for (c, &peak) in ch.iter().enumerate().skip(1) {
        assert!(peak < 1e-6, "ch{c} must be silent, got {peak}");
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
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo),
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
    let mut s = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    let untrimmed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    s.render.latency = tutti_types::Samples(512);
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
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Quad),
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
    let mut long = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    long.render.duration_seconds = 2.0;
    let mut out = render_to_buffers(net(), &long, &FrozenClock).unwrap();

    let cfg = LoudnessConfig::new(out.sample_rate, ChannelLayout::Stereo);
    let before = measure_loudness(&cfg, &out.interleaved()).unwrap();
    let gain = before.gain_to(Db(-14.0), Db(-1.0));
    out.apply_gain(gain);
    let after = measure_loudness(&cfg, &out.interleaved()).unwrap();

    // Assert what `gain_to` actually promises, not merely that `apply_gain` is
    // linear. The earlier form checked `after ≈ before + gain`, which is
    // algebraically true for ANY gain — `gain_to` could return `Db(0.0)` and it
    // still passed, so the measure half of measure-then-apply was unverified.
    //
    // The promise is two-sided: reach the target, UNLESS the true-peak ceiling
    // binds first. This signal is a DC-ish 0.5 constant at -29.3 LUFS with a
    // -4.99 dBTP peak, so +15.3 dB would be needed for -14 LUFS — and that
    // would put the peak at +10 dBTP. The ceiling wins, and the peak is what
    // must land exactly.
    assert!(
        after.lufs.get() <= -14.0 + 0.5,
        "must never overshoot the loudness target: {before:?} -> {after:?}"
    );
    assert!(
        (after.true_peak.get() - (-1.0)).abs() < 0.5,
        "the ceiling binds here, so the peak must land on it: {after:?}"
    );

    // And the ceiling must not be an excuse to do nothing: a gain was applied.
    assert!(
        after.lufs.get() > before.lufs.get() + 1.0,
        "the reachable part of the gain must still be applied: \
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
    let mut s = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    s.render.duration_seconds = 1.0;
    s.resample = Some(Resample::to(SampleRate(48_000.0)));

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

/// `write_buffers` completes the measure-then-apply cycle: render, normalize,
/// write. Without it a caller who normalized would have nowhere to put the
/// result — and it shares every encoder with the streaming path rather than
/// growing a second writer per format.
#[test]
fn normalized_audio_can_be_written_to_every_format() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(AudioFormat::Wav, BitDepth::Int24, ChannelLayout::Stereo);
    s.render.duration_seconds = 1.0;

    let mut audio = render_to_buffers(net(), &s, &FrozenClock).unwrap();
    audio.apply_gain(tutti_types::Db(-6.0));

    for (format, ext) in [
        (AudioFormat::Wav, "wav"),
        (AudioFormat::Flac(Default::default()), "flac"),
        (AudioFormat::OggVorbis(Default::default()), "ogg"),
        (AudioFormat::Aiff, "aiff"),
    ] {
        s.encode.format = format;
        let p = d.path().join(format!("n.{ext}"));
        let w = tutti_export::write_buffers(&audio, &s, &p).expect("write");
        assert!(w.bytes > 100, "{ext} wrote {} bytes", w.bytes);
    }

    // The written WAV must carry the gain: 0.5 at -6 dB is ~0.25. (The WAV was
    // written in the loop above; this only re-reads it.)
    let p = d.path().join("n.wav");
    let rd = hound::WavReader::open(&p).unwrap();
    let peak = rd
        .into_samples::<i32>()
        .map(|x| (x.unwrap() as f32 / 8_388_607.0).abs())
        .fold(0.0f32, f32::max);
    assert!(
        (peak - 0.25).abs() < 0.01,
        "expected ~0.25 after -6 dB, got {peak}"
    );
}

/// A resample must reach **every** format, not just the ones that happened to
/// route through the shared pump.
///
/// FLAC and Ogg used to pull from `drive` directly while still taking their
/// header rate from `encoder_rate`, so each wrote un-resampled audio under a
/// header claiming the target: a 1 s render played back 8.8% fast. WAV and AIFF
/// were correct, which is exactly why a WAV-only test could not see it.
#[test]
fn a_resample_reaches_every_format_not_just_wav() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(AudioFormat::Wav, BitDepth::Int24, ChannelLayout::Stereo);
    s.render.duration_seconds = 1.0; // 44100 frames in, 48000 expected out
    s.resample = Some(Resample::to(tutti_core::SampleRate(48_000.0)));

    // WAV is the control: it was already correct.
    let p = d.path().join("r.wav");
    render_to_file(net(), &s, &FrozenClock, &p).unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let wav_frames = rd.into_samples::<i32>().count() / 2;
    assert!(
        (wav_frames as i64 - 48_000).abs() < 512,
        "wav: expected ~48000 frames, got {wav_frames}"
    );

    // FLAC: the header claims 48k, so the payload must actually BE 48k. If the
    // resample were skipped the file would hold 44100 frames.
    s.encode.format = AudioFormat::Flac(Default::default());
    let pf = d.path().join("r.flac");
    render_to_file(net(), &s, &FrozenClock, &pf).unwrap();
    let flac_frames = flac_frame_count(&pf);
    assert!(
        (flac_frames as i64 - 48_000).abs() < 512,
        "flac: header says 48000 but payload holds {flac_frames} frames \
         (44100 means the resample was skipped)"
    );

    // Ogg: same property, read from the final page's granule position.
    s.encode.format = AudioFormat::OggVorbis(Default::default());
    let po = d.path().join("r.ogg");
    render_to_file(net(), &s, &FrozenClock, &po).unwrap();
    let ogg_frames = ogg_granule(&po);
    assert!(
        (ogg_frames as i64 - 48_000).abs() < 2048,
        "ogg: header says 48000 but granule reports {ogg_frames} frames"
    );
}

/// `write_buffers` must resample from the rate the FRAMES are at, not from
/// whatever `config.render.sample_rate` happens to hold.
///
/// The frames handed to `write_buffers` already exist; `config.render` describes
/// a render that already happened, and may be a different rate entirely. Reading
/// it made the output 2.18x too long and pitched down, under a header that said
/// otherwise.
#[test]
fn write_buffers_resamples_from_the_frames_own_rate() {
    let d = tempfile::tempdir().unwrap();

    // Render at 96k...
    let mut render_cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    render_cfg.render.sample_rate = tutti_core::SampleRate(96_000.0);
    render_cfg.render.duration_seconds = 1.0;
    let audio = render_to_buffers(net(), &render_cfg, &FrozenClock).unwrap();
    assert_eq!(audio.sample_rate.get(), 96_000.0);

    // ...then write with a config whose `render` half is left at its DEFAULT
    // 44100 — the case the doc tells a caller is fine to ignore.
    let mut write_cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::Stereo);
    write_cfg.resample = Some(Resample::to(tutti_core::SampleRate(48_000.0)));
    assert_eq!(
        write_cfg.render.sample_rate.get(),
        44_100.0,
        "this test is only meaningful while the config's render rate differs"
    );

    let p = d.path().join("w.wav");
    tutti_export::write_buffers(&audio, &write_cfg, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let frames = rd.into_samples::<f32>().count() / 2;
    // 96k -> 48k halves the length. Reading 44100 as the source instead would
    // give ~104_490 frames.
    assert!(
        (frames as i64 - 48_000).abs() < 512,
        "expected ~48000 frames (96k halved), got {frames} \
         (~104490 means it resampled from the config's rate, not the audio's)"
    );
}

/// Frame count from a FLAC STREAMINFO block (bytes 21..27, 36 bits).
fn flac_frame_count(path: &std::path::Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[0..4], b"fLaC", "not a FLAC file");
    // 4 magic + 4 block header, then STREAMINFO; total-samples is 36 bits
    // starting 13 bytes into it.
    let si = &b[8..];
    ((si[13] as u64 & 0x0F) << 32)
        | (si[14] as u64) << 24
        | (si[15] as u64) << 16
        | (si[16] as u64) << 8
        | (si[17] as u64)
}

/// Granule position of the last Ogg page — the total frames encoded.
///
/// Walks pages by seeking the `OggS` capture pattern rather than stepping
/// blindly: a page body can contain those four bytes, and a naive walk that
/// mis-steps once reads a random eight bytes as a granule (it reported 3830784
/// for a 96000-frame file while I was writing this).
fn ogg_granule(path: &std::path::Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    let mut last = 0i64;
    let mut i = 0usize;
    while let Some(off) = find_from(&b, b"OggS", i) {
        if off + 27 > b.len() {
            break;
        }
        let gran = i64::from_le_bytes(b[off + 6..off + 14].try_into().unwrap());
        let segs = b[off + 26] as usize;
        if off + 27 + segs > b.len() {
            break;
        }
        // -1 marks a page that completes no packet; it is not a frame count.
        if gran >= 0 {
            last = gran;
        }
        let body: usize = b[off + 27..off + 27 + segs]
            .iter()
            .map(|&s| s as usize)
            .sum();
        i = off + 27 + segs + body;
    }
    last as u64
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}
