use super::*;
use tutti_fixtures::signal::{impulse_at, sine};
use tutti_types::{Amplitude, Hz, Samples};

/// Push `planes` through a [`StreamingResampler`] in blocks of `block` frames
/// (`None` is the whole signal at once, the two-pass path), then finish.
fn resample_blocked(
    planes: &[Vec<f32>],
    from: u32,
    to: u32,
    chunk: ChunkSize,
    block: Option<usize>,
) -> Vec<Vec<f32>> {
    let n = planes[0].len();
    let mut r = StreamingResampler::new(planes.len(), from.into(), to.into(), chunk).unwrap();
    let mut out = vec![Vec::new(); planes.len()];
    match block {
        None => r.push(planes, &mut out).unwrap(),
        Some(len) => {
            for start in (0..n).step_by(len) {
                let end = (start + len).min(n);
                let part: Vec<Vec<f32>> = planes.iter().map(|p| p[start..end].to_vec()).collect();
                r.push(&part, &mut out).unwrap();
            }
        }
    }
    r.finish(&mut out).unwrap();
    out
}

const RATE_PAIRS: [(u32, u32); 3] = [(44_100, 48_000), (48_000, 44_100), (48_000, 96_000)];
const BLOCKINGS: [Option<usize>; 3] = [None, Some(64), Some(333)];

/// The output length is `round(input * ratio)` however the input is
/// blocked, at every preset chunk size, in both directions, for lengths
/// that end mid-chunk or before the first one.
#[test]
fn the_length_is_the_rounded_ratio_however_the_input_is_blocked() {
    for (from, to) in RATE_PAIRS {
        for chunk in ChunkSize::PRESETS {
            for n in [0usize, 1, 1000, 4096, 4410, 10_007] {
                let planes: Vec<Vec<f32>> = (0..2).map(|_| vec![0.1f32; n]).collect();
                let want = (n as f64 * f64::from(to) / f64::from(from)).round() as usize;
                assert_eq!(
                    StreamingResampler::output_frames(from.into(), to.into(), Samples(n)),
                    Samples(want),
                    "{from}->{to} n={n}: the stated length is the stream's"
                );
                for block in BLOCKINGS {
                    let out = resample_blocked(&planes, from, to, chunk, block);
                    for (c, plane) in out.iter().enumerate() {
                        assert_eq!(
                            plane.len(),
                            want,
                            "{from}->{to} {chunk:?} n={n} block={block:?} ch{c}"
                        );
                    }
                }
            }
        }
    }
}

/// The signal itself survives every blocking: a 1 kHz sine of amplitude
/// 0.5 on one channel keeps its level (within 0.001 dB, the bound
/// `a_sine_keeps_its_frequency_and_amplitude` holds the default chunk to)
/// and matches the analytic waveform `0.5 sin(2 pi f j / to)` to 1e-4 (the
/// latency is compensated, so frame `j` is time `j / to`), and a 0.25
/// constant on the other stays within 1e-4 of it, measured in the middle
/// half, so a chunk read from the wrong place, a phase error or a seam
/// shows. The bounds sit above the worst error measured at every preset
/// (under 1e-5).
#[test]
fn the_signal_survives_every_blocking() {
    for (from, to) in RATE_PAIRS {
        let n = from as usize;
        let planes = vec![tone(1000.0, f64::from(from), n, 0.5), vec![0.25f32; n]];
        for chunk in ChunkSize::PRESETS {
            for block in BLOCKINGS {
                let out = resample_blocked(&planes, from, to, chunk, block);
                let len = out[0].len();
                let (a, b) = (len / 4, 3 * len / 4);
                let db = 20.0 * (level_at(&out[0], f64::from(to), 1000.0, a, b) / 0.5).log10();
                assert!(
                    db.abs() < 0.001,
                    "{from}->{to} {chunk:?} block={block:?}: sine {db:+.4} dB"
                );
                // Latency is compensated, so output frame j is the input
                // tone at time j / to: compare the waveform, not only its
                // level, so a phase or chunk-offset error shows.
                let waveform = (a..b)
                    .map(|j| {
                        let want = 0.5
                            * (2.0 * std::f64::consts::PI * 1000.0 * j as f64 / f64::from(to))
                                .sin();
                        (f64::from(out[0][j]) - want).abs()
                    })
                    .fold(0.0f64, f64::max);
                assert!(
                    waveform < 1e-4,
                    "{from}->{to} {chunk:?} block={block:?}: waveform off by {waveform}"
                );
                let worst = out[1][a..b]
                    .iter()
                    .map(|s| (s - 0.25).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    worst < 1e-4,
                    "{from}->{to} {chunk:?} block={block:?}: DC off by {worst}"
                );
            }
        }
    }
}

/// The carry holds less than one chunk between pushes, so a streaming
/// encode keeps O(chunk) of input, not the whole render: every `push`
/// leaves the consumed prefix dropped (`head == 0`) and fewer frames
/// than the next chunk needs.
#[test]
fn the_carry_stays_under_one_chunk_between_pushes() {
    for chunk in ChunkSize::PRESETS {
        let mut r = StreamingResampler::new(2, 44_100.into(), 48_000.into(), chunk).unwrap();
        let mut out = vec![Vec::new(); 2];
        let block = vec![vec![0.25f32; 37]; 2];
        for i in 0..2_000 {
            r.push(&block, &mut out).unwrap();
            let (backlog, head, need) = r.backlog();
            assert_eq!(
                head, 0,
                "{chunk:?}: push {i} left the consumed prefix in place"
            );
            assert!(
                backlog < need,
                "{chunk:?}: push {i} left {backlog} frames, a whole chunk is {need}"
            );
        }
        r.finish(&mut out).unwrap();
    }
}

fn resample(planes: &[Vec<f32>], from: u32, to: u32, chunk: ChunkSize) -> Vec<Vec<f32>> {
    // The `u32`s keep the call sites below reading as plain rates; the
    // conversion is here, once.
    let mut r = StreamingResampler::new(planes.len(), from.into(), to.into(), chunk).unwrap();
    let mut out = vec![Vec::new(); planes.len()];
    r.push(planes, &mut out).unwrap();
    r.finish(&mut out).unwrap();
    out
}

/// `new` refuses anything it cannot honour, rather than rounding to
/// something adjacent and reporting success.
#[test]
fn a_chunk_size_must_be_a_power_of_two_that_its_subdivision_divides() {
    assert!(
        ChunkSize::new(Samples(1000), 2).is_none(),
        "not a power of 2"
    );
    assert!(
        ChunkSize::new(Samples(1024), 0).is_none(),
        "zero sub-chunks"
    );
    assert!(ChunkSize::new(Samples(32), 1).is_none(), "below MIN");
    assert!(ChunkSize::new(Samples(65536), 1).is_none(), "above MAX");
    assert!(
        ChunkSize::new(Samples(1024), 3).is_none(),
        "3 does not divide 1024"
    );
    assert_eq!(ChunkSize::new(Samples(1024), 2), Some(ChunkSize::N1024));
}

/// Every preset must be constructible through `new` — if one is not, the
/// constants and the validation disagree about what is legal.
#[test]
fn every_preset_satisfies_its_own_constructor() {
    for p in ChunkSize::PRESETS {
        assert_eq!(
            ChunkSize::new(p.chunk(), p.sub_chunks()),
            Some(p),
            "{p:?} is a preset its own constructor rejects"
        );
    }
}

/// An impulse must come out where the rate change puts it, not
/// `output_delay()` frames later (320 frames late at 44.1→48 k if the
/// delay is not compensated).
#[test]
fn an_impulse_keeps_its_position() {
    let n = 8192;
    let at = 1000usize;
    let plane = impulse_at(Samples(n), Samples(at));

    for q in [
        ChunkSize::N512,
        ChunkSize::N1024,
        ChunkSize::N2048,
        ChunkSize::N4096,
    ] {
        let out = resample(std::slice::from_ref(&plane), 44100, 48000, q);
        let peak = out[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let expected = (at as f64 * 48000.0 / 44100.0).round() as usize;
        let err = peak.abs_diff(expected);
        assert!(
            err <= 2,
            "{q:?}: impulse landed at {peak}, expected ~{expected} (off by {err})"
        );
    }
}

/// …and the tail must survive. A marker on the final input frame vanishes
/// entirely if the output is truncated where the delayed tail would be.
#[test]
fn the_tail_is_flushed_not_truncated() {
    let n = 4096;
    let plane = impulse_at(Samples(n), Samples(n - 1));
    let out = resample(&[plane], 44100, 48000, ChunkSize::N1024);
    let tail_peak = out[0]
        .iter()
        .rev()
        .take(256)
        .fold(0.0f32, |a, &b| a.max(b.abs()));
    assert!(
        tail_peak > 0.1,
        "the last input frame must appear in the output; tail peak {tail_peak}"
    );
}

#[test]
fn output_length_tracks_the_rate_ratio() {
    let n = 44100;
    let out = resample(&[vec![0.5f32; n]], 44100, 48000, ChunkSize::N1024);
    let expected = 48000usize;
    let err = out[0].len().abs_diff(expected);
    assert!(
        err < 256,
        "expected ~{expected} frames, got {} (off by {err})",
        out[0].len()
    );
}

/// Amplitude and phase-free level of `f_hz` in `x[from..to]` (sampled at
/// `rate`): the magnitude of the single-bin DFT, `2/N * |sum x e^-iwt|`,
/// so a sine of amplitude `a` at `f_hz` reads `a`.
fn level_at(x: &[f32], rate: f64, f_hz: f64, from: usize, to: usize) -> f64 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (n, &v) in x.iter().enumerate().take(to).skip(from) {
        let w = 2.0 * std::f64::consts::PI * f_hz * n as f64 / rate;
        re += v as f64 * w.cos();
        im -= v as f64 * w.sin();
    }
    2.0 * re.hypot(im) / (to - from) as f64
}

/// `n` samples of a sine of amplitude `amp` at `hz`.
fn tone(hz: f64, rate: f64, n: usize, amp: f32) -> Vec<f32> {
    // `hz` narrows to the unit's `f32`; the probes sit within 1e-3 Hz of it.
    sine(
        Hz::new(hz as f32),
        Amplitude::new(amp),
        SampleRate::new(rate),
        Samples(n),
    )
}

/// A sine keeps its frequency and amplitude: a 1 kHz, 0.5-amplitude sine
/// at 44.1 kHz read at 48 kHz has level 0.5 within 0.001 dB at 1 kHz
/// (so the output's frequency is 1 kHz) and the strongest of a +-10 Hz scan
/// is the 1 kHz bin. Measured in the middle half, clear of the edges.
#[test]
fn a_sine_keeps_its_frequency_and_amplitude() {
    for (from, to) in [
        (44_100.0, 48_000.0),
        (48_000.0, 44_100.0),
        (48_000.0, 96_000.0),
    ] {
        let n = from as usize; // one second
        let out = resample(
            &[tone(1000.0, from, n, 0.5)],
            from as u32,
            to as u32,
            ChunkSize::N1024,
        );
        let x = &out[0];
        let (a, b) = (x.len() / 4, 3 * x.len() / 4);
        let level = level_at(x, to, 1000.0, a, b);
        let db = 20.0 * (level / 0.5).log10();
        assert!(
            db.abs() < 0.001,
            "{from}->{to}: level {level} ({db:+.4} dB)"
        );
        for off in [-10.0, 10.0] {
            let other = level_at(x, to, 1000.0 + off, a, b);
            assert!(
                other < level,
                "{from}->{to}: {off:+} Hz reads {other} >= {level}"
            );
        }
    }
}

/// DC gain is 1: a constant comes out as that constant (within 1e-5 once
/// the filter has settled), and the output never sits at the start-up value.
#[test]
fn dc_passes_with_unit_gain() {
    for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100)] {
        let out = resample(
            &[vec![0.5f32; from as usize / 2]],
            from,
            to,
            ChunkSize::N1024,
        );
        let x = &out[0];
        let mid = &x[x.len() / 4..3 * x.len() / 4];
        let worst = mid.iter().map(|s| (s - 0.5).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "{from}->{to}: DC off by {worst}");
    }
}

/// Content above the new Nyquist is attenuated, not folded down. Tones of
/// 0.5 amplitude at 48 kHz, converted to 44.1 kHz (Nyquist 22.05 kHz),
/// would alias to `44.1 kHz - f`; the output holds under -120 dB re the
/// input (`AMOUNT` below) at that frequency for every `f` from 22.3 kHz to
/// 23.9 kHz, at every preset chunk size. A 15 kHz tone, well inside the
/// band, passes within 0.01 dB.
#[test]
fn content_above_the_new_nyquist_is_rejected_not_folded() {
    const AMOUNT: f64 = -120.0;
    let (from, to) = (48_000.0, 44_100.0);
    let n = 48_000;
    for chunk in ChunkSize::PRESETS {
        for f in [22_300.0, 22_600.0, 23_000.0, 23_500.0, 23_900.0] {
            let out = resample(&[tone(f, from, n, 0.5)], 48_000, 44_100, chunk);
            let x = &out[0];
            let (a, b) = (x.len() / 4, 3 * x.len() / 4);
            let alias = level_at(x, to, to - f, a, b);
            let db = 20.0 * (alias / 0.5).log10();
            assert!(
                db < AMOUNT,
                "{chunk:?} {f} Hz: alias at {} Hz is {db:.1} dB",
                to - f
            );
        }

        let inband = resample(&[tone(15_000.0, from, n, 0.5)], 48_000, 44_100, chunk);
        let x = &inband[0];
        let (a, b) = (x.len() / 4, 3 * x.len() / 4);
        let kept = 20.0 * (level_at(x, to, 15_000.0, a, b) / 0.5).log10();
        assert!(
            kept.abs() < 0.01,
            "{chunk:?}: 15 kHz moved by {kept:+.4} dB"
        );
    }
}

/// The length is exactly `round(input * ratio)` for every input length, and
/// the latency is compensated to the frame: an impulse at input frame 1000
/// lands on output frame 1088 (1000 x 48/44.1 = 1088.4) at every chunk
/// size, and the filter's `output_delay` (320 frames at the default chunk,
/// 44.1 -> 48 kHz) is what is dropped from the head.
#[test]
fn length_and_latency_match_the_documented_values() {
    for n in [1usize, 441, 1000, 4410, 10_007, 44_100] {
        let out = resample(&[vec![0.1f32; n]], 44_100, 48_000, ChunkSize::N1024);
        assert_eq!(
            out[0].len(),
            (n as f64 * 48_000.0 / 44_100.0).round() as usize,
            "n = {n}"
        );
    }
    let r = FftResampler::new(44_100, 48_000, 1024, 2, 1).unwrap();
    assert_eq!(r.output_delay(), 320);
    for q in ChunkSize::PRESETS {
        let plane = impulse_at(Samples(8192), Samples(1000));
        let out = resample(&[plane], 44_100, 48_000, q);
        let peak = out[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, 1088, "{q:?}");
    }
}

/// Upsampling filters at the input's band edge, not the output's. The
/// low-pass is designed to be 6 dB down (gain 0.5) at its relative cutoff
/// `cutoff(fft_size_in)`: 0.97679 of the input Nyquist for the 588-frame
/// blocks of 44.1 -> 48 kHz (21 538 Hz), 0.97337 for the 512-frame blocks
/// of 48 -> 96 kHz (23 361 Hz). A tone at that edge reads 0.5 x its
/// amplitude within 0.5 dB, and one at 0.995 Nyquist, in the stopband above
/// it, is at least 60 dB down, so nothing near the input's Nyquist reaches
/// the output to image against it (the output spectrum above the input
/// Nyquist is zero by construction).
#[test]
fn upsampling_cuts_at_the_input_band_edge() {
    for (from, to, edge) in [
        (44_100.0, 48_000.0, 21_538.0),
        (48_000.0, 96_000.0, 23_361.0),
    ] {
        let at = |f: f64| {
            let out = resample(
                &[tone(f, from, from as usize, 0.5)],
                from as u32,
                to as u32,
                ChunkSize::N1024,
            );
            let x = &out[0];
            let (a, b) = (x.len() / 4, 3 * x.len() / 4);
            20.0 * (level_at(x, to, f, a, b) / 0.5).log10()
        };
        let edge_db = at(edge);
        assert!(
            (edge_db + 6.02).abs() < 0.5,
            "{from}->{to}: {edge} Hz reads {edge_db:+.2} dB, expected -6.02"
        );
        let stop_db = at(0.995 * from / 2.0);
        assert!(
            stop_db < -60.0,
            "{from}->{to}: 0.995 Nyquist reads {stop_db:.1} dB"
        );
    }
}

/// A rate the converter cannot take is an error that names the rate, not a
/// saturated or wrapped integer: NaN, infinities, zero, negatives and
/// anything under 0.5 Hz (which rounds to 0) or at 2^32 Hz and over. A
/// pair whose blocks would be enormous (1 Hz -> 192 kHz plans a ~98 M-point
/// FFT) is refused too.
#[test]
fn an_unusable_rate_is_an_error() {
    let new = |from: f64, to: f64| {
        StreamingResampler::new(1, SampleRate(from), SampleRate(to), ChunkSize::N1024)
    };
    for bad in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -48_000.0,
        0.4,
        1e300,
        4_294_967_296.0,
    ] {
        for (from, to) in [(bad, 48_000.0), (44_100.0, bad)] {
            let err = new(from, to).err().unwrap_or_else(|| {
                panic!("{from} -> {to} Hz was accepted");
            });
            assert!(
                err.to_string().contains("invalid sample rate"),
                "{from} -> {to}: {err}"
            );
        }
    }
    assert!(new(44_100.0, 48_000.0).is_ok());
    assert!(new(0.5, 1.0).is_ok());
    let err = new(1.0, 192_000.0).err().unwrap();
    assert!(err.to_string().contains("block"), "{err}");
}

#[test]
fn channels_stay_aligned() {
    let n = 4410;
    let planes: Vec<Vec<f32>> = (0..4).map(|c| vec![c as f32 * 0.1; n]).collect();
    let out = resample(&planes, 44100, 48000, ChunkSize::N1024);
    assert_eq!(out.len(), 4);
    let len = out[0].len();
    assert!(out.iter().all(|p| p.len() == len));
}

/// The output length is exactly `round(n * ratio)` for rate pairs in both
/// directions (including the steep ones, where the converter's delay is many
/// input frames), every preset chunk size and the shortest, and input lengths
/// that are short, odd, and straddle chunk edges. Whole-signal and blocked
/// pushes agree.
#[test]
fn the_length_is_exact_over_rate_pairs_chunks_and_odd_lengths() {
    const PAIRS: [(u32, u32); 12] = [
        (44_100, 48_000),
        (48_000, 44_100),
        (96_000, 44_100),
        (192_000, 48_000),
        (48_000, 8_000),
        (44_100, 22_050),
        (88_200, 44_100),
        (22_050, 48_000),
        (8_000, 48_000),
        (44_100, 96_000),
        (48_000, 32_000),
        (11_025, 44_100),
    ];
    const LENGTHS: [usize; 15] = [
        0, 1, 2, 3, 7, 63, 64, 65, 511, 513, 999, 1_023, 1_025, 4_097, 20_001,
    ];
    let chunks = [
        ChunkSize::MIN,
        ChunkSize::N512,
        ChunkSize::N1024,
        ChunkSize::N2048,
        ChunkSize::N4096,
    ];
    for (from, to) in PAIRS {
        for chunk in chunks {
            for n in LENGTHS {
                let planes = vec![vec![0.3f32; n]];
                let want = (n as f64 * f64::from(to) / f64::from(from)).round() as usize;
                for block in [None, Some(97)] {
                    let out = resample_blocked(&planes, from, to, chunk, block);
                    assert_eq!(
                        out[0].len(),
                        want,
                        "{from}->{to} {chunk:?} n={n} block={block:?}"
                    );
                }
            }
        }
    }
}

/// The default filter's passband is flat. A sine of amplitude `a` below the
/// band edge reads `a` (the analytic gain is 1) at the 44.1 and 48 kHz pairs
/// listed here: within 0.005 dB from 100 Hz to 20 kHz, and at 0.93 of the
/// lower Nyquist within 0.01 dB (44.1 <-> 48) or 0.1 dB (96 -> 44.1, whose
/// steeper decimation puts the filter's knee closer). The response does not
/// depend on the chunk size, so the default is measured.
///
/// Measured when this was written (dB, 0.93 Nyquist = 20.5 kHz): 44.1 -> 48
/// -0.0007, 48 -> 44.1 -0.0018, 96 -> 44.1 -0.0875; at 0.97 Nyquist
/// (21.4 kHz) 44.1 -> 48 and 48 -> 44.1 read -0.88 dB, and the edge (-6 dB) is
/// at 0.977 Nyquist (`upsampling_cuts_at_the_input_band_edge`).
#[test]
fn the_passband_is_flat_to_the_band_edge() {
    for (from, to, at_edge_db) in [
        (44_100.0, 48_000.0, 0.01),
        (48_000.0, 44_100.0, 0.01),
        (96_000.0, 44_100.0, 0.1),
    ] {
        let nyquist = f64::min(from, to) / 2.0;
        let probes = [
            (100.0, 0.005),
            (1_000.0, 0.005),
            (10_000.0, 0.005),
            (18_000.0, 0.005),
            (20_000.0, 0.005),
            (0.93 * nyquist, at_edge_db),
        ];
        for (f, bound) in probes {
            let out = resample(
                &[tone(f, from, from as usize, 0.5)],
                from as u32,
                to as u32,
                ChunkSize::default(),
            );
            let x = &out[0];
            let (a, b) = (x.len() / 4, 3 * x.len() / 4);
            let db = 20.0 * (level_at(x, to, f, a, b) / 0.5).log10();
            assert!(
                db.abs() < bound,
                "{from}->{to}: {f:.0} Hz reads {db:+.4} dB, bound {bound}"
            );
        }
    }
}

/// Two rates that are one whole rate in Hz are no conversion, and the stated
/// length is then the input's, to the frame; a rate the converter refuses is
/// not an identity, so `new` is the one to say so.
#[test]
fn rates_of_one_whole_hz_are_no_conversion() {
    let hz = |r: f64| SampleRate::new(r);
    assert!(!StreamingResampler::converts(hz(44_100.0), hz(44_100.2)));
    assert!(StreamingResampler::converts(hz(44_100.0), hz(48_000.0)));
    assert!(StreamingResampler::converts(hz(f64::NAN), hz(f64::NAN)));
    assert_eq!(
        StreamingResampler::output_frames(hz(44_100.0), hz(44_100.2), Samples(1_000_000)),
        Samples(1_000_000)
    );
}
