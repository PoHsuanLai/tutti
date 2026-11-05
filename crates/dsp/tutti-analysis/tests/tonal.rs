//! Chroma and key detection end to end, on synthesized music.
//!
//! Every tone is a few harmonics at amplitudes `1/h`, written straight from
//! `sin` in f64. Each test here was confirmed to fail against the mutation its
//! comment names, then restored.

use rand::{Rng, SeedableRng, rngs::StdRng};
use tutti_analysis::{
    ChromaConfig, Error, FftScratch, Key, KeyProfile, Mode, PitchClass, StftGeometry, Tuning,
    chroma, estimate_key,
};
use tutti_types::{Cents, Samples};

const SR: f64 = 22_050.0;

/// C4, in hertz.
const C4: f64 = 261.625_565_300_598_6;

/// The frequency `semitones` above C4.
fn hz(semitones: i32) -> f64 {
    C4 * (f64::from(semitones) / 12.0).exp2()
}

/// Adds a note of `partials` harmonics, `detune` cents off, to `out` from
/// sample `at` for `secs` seconds.
fn note(out: &mut [f32], at: usize, secs: f64, f0: f64, partials: usize, detune: f64) {
    let f0 = f0 * (detune / 1200.0).exp2();
    let n = ((SR * secs) as usize).min(out.len() - at);
    for (i, slot) in out[at..at + n].iter_mut().enumerate() {
        let t = i as f64 / SR;
        let s: f64 = (1..=partials)
            .map(|h| (std::f64::consts::TAU * f0 * h as f64 * t).sin() / h as f64)
            .sum();
        *slot += (0.2 * s) as f32;
    }
}

/// `notes` (semitones above C4) in turn, `secs` each.
fn melody(notes: &[i32], secs: f64, partials: usize, detune: f64) -> Vec<f32> {
    let each = (SR * secs) as usize;
    let mut x = vec![0.0; each * notes.len()];
    for (i, &n) in notes.iter().enumerate() {
        note(&mut x, i * each, secs, hz(n), partials, detune);
    }
    x
}

fn chord_tones(out: &mut [f32], at: usize, secs: f64, root: i32, minor: bool, partials: usize) {
    for interval in [0, if minor { 3 } else { 4 }, 7] {
        note(out, at, secs, hz(root + interval), partials, 0.0);
    }
}

fn geometry(window: usize, hop: usize) -> StftGeometry {
    StftGeometry::new(SR, Samples(window), Samples(hop)).unwrap()
}

fn config() -> ChromaConfig {
    ChromaConfig::new(geometry(4096, 1024)).unwrap()
}

fn mean_chroma(x: &[f32], cfg: &ChromaConfig) -> [f32; 12] {
    chroma(x, cfg, &mut FftScratch::new()).unwrap().mean()
}

fn key_of(x: &[f32], cfg: &ChromaConfig) -> Key {
    let c = chroma(x, cfg, &mut FftScratch::new()).unwrap();
    estimate_key(c.grid(), KeyProfile::Krumhansl).unwrap().key
}

/// The classes of the `n` largest columns, ascending.
fn top(c: &[f32; 12], n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..12).collect();
    order.sort_by(|&a, &b| c[b].total_cmp(&c[a]));
    let mut t = order[..n].to_vec();
    t.sort_unstable();
    t
}

const C_MAJOR_SCALE: [i32; 8] = [0, 2, 4, 5, 7, 9, 11, 12];

/// Harmonic A minor, from A and back to A: A B C D E F G# A A.
const A_MINOR_SCALE: [i32; 9] = [-3, -1, 0, 2, 4, 5, 8, 9, 9];

/// A C-E-G triad of bare sines has its energy at classes 0, 4 and 7 and
/// nowhere else but window sidelobes: with one partial a peak folds only to
/// itself, and a Hann sidelobe is at most -31 dB, which is 10^-3.1 of a peak's
/// energy, with at most two on each of three sines. With the default four
/// subharmonic votes the only strays are the third subharmonics (A from E,
/// F from C), each weighted `0.6^2` of its peak, under the 0.36 the largest
/// column exceeds.
///
/// Mutation checked: folding every peak to `class + 1` fails the classes.
/// (The subharmonic weights are pinned by `a_peak_votes_for_its_subharmonics_
/// down_to_the_band_floor` in `chroma`: here the largest column's growth with
/// the weights hides them from the 0.36 bound.)
#[test]
fn a_triad_of_sines_has_chroma_at_its_classes() {
    let mut x = vec![0.0; SR as usize * 2];
    chord_tones(&mut x, 0, 2.0, 0, false, 1);

    let only = mean_chroma(&x, &config().with_harmonics(1).unwrap());
    assert_eq!(top(&only, 3), [0, 4, 7]);
    let sidelobes = 6.0 * 10f32.powf(-3.1);
    for (class, &v) in only.iter().enumerate() {
        if [0, 4, 7].contains(&class) {
            assert!(v > 0.9, "{class} {v}");
        } else {
            assert!(v <= sidelobes, "{class} {v}");
        }
    }

    let voted = mean_chroma(&x, &config());
    assert_eq!(top(&voted, 3), [0, 4, 7]);
    for (class, &v) in voted.iter().enumerate() {
        if ![0, 4, 7].contains(&class) {
            assert!(v <= 0.36, "{class} {v}");
        }
    }
    assert_eq!(voted[0], 1.0);
}

/// Moving every note up `k` semitones moves the chroma classes and the key
/// up exactly `k`, for every `k`.
///
/// Mutation checked: adding 1 to every folded class fails at every `k`.
#[test]
fn everything_rotates_by_the_transposition() {
    let mut fft = FftScratch::new();
    for k in 0..12 {
        let mut x = vec![0.0; SR as usize * 2];
        chord_tones(&mut x, 0, 2.0, k, true, 4);
        let cfg = config();
        let c = chroma(&x, &cfg, &mut fft).unwrap();
        let mut want = [
            k as usize % 12,
            (k as usize + 3) % 12,
            (k as usize + 7) % 12,
        ];
        want.sort_unstable();
        assert_eq!(top(&c.mean(), 3), want, "k {k}");

        let scale: Vec<i32> = C_MAJOR_SCALE.iter().map(|n| n + k).collect();
        let key = key_of(&melody(&scale, 0.5, 4, 0.0), &cfg);
        assert_eq!(
            key,
            Key {
                tonic: PitchClass::from_semitones_above_c(k as u8),
                mode: Mode::Major
            },
            "k {k}"
        );
    }
}

/// A C major scale up an octave is C major; the harmonic A minor scale, which
/// holds the same white keys but for the leading tone and which dwells on A,
/// is A minor and not C major. The runner-up of each is its relative.
///
/// Mutation checked: giving `KeyProfile::Krumhansl` the major ratings for
/// minor reads the A minor scale as a major key and fails.
#[test]
fn scales_name_their_keys() {
    let cfg = config();
    assert_eq!(
        key_of(&melody(&C_MAJOR_SCALE, 0.5, 4, 0.0), &cfg),
        Key {
            tonic: PitchClass::C,
            mode: Mode::Major
        }
    );
    assert_eq!(
        key_of(&melody(&A_MINOR_SCALE, 0.5, 4, 0.0), &cfg),
        Key {
            tonic: PitchClass::A,
            mode: Mode::Minor
        }
    );
}

/// Folding is by the tuning: a scale 30 cents sharp still resolves, and the
/// estimate reads +30 to within the error of its peaks. With bare sines the
/// only error is the interpolation's, at most 0.02 bin (measured 0.016), at
/// the lowest note (C4): `1200 log2(1 + 0.02 w / f)` cents. With four partials
/// the third partial of an equal-tempered note is 1.955 cents sharp of the
/// grid (3/2 against 2^(7/12)), which bounds the pull of every partial of
/// order 1 to 4 (the others are on the grid).
///
/// Mutation checked: negating the estimate fails both bounds. Thirty cents
/// rounds home either way, so the key does not depend on the estimate; that
/// the tuning moves the fold is pinned by `a_fixed_tuning_moves_the_fold` in
/// `chroma`.
#[test]
fn a_detuned_scale_resolves_and_its_tuning_is_read() {
    let cfg = config();
    let width = SR / 4096.0;
    let interpolation = 1200.0 * (1.0 + 0.02 * width / C4).log2();
    for (partials, tolerance) in [(1, interpolation), (4, interpolation + 1.955)] {
        let x = melody(&C_MAJOR_SCALE, 0.5, partials, 30.0);
        let c = chroma(&x, &cfg, &mut FftScratch::new()).unwrap();
        let tuning = f64::from(c.tuning().get());
        assert!((tuning - 30.0).abs() <= tolerance, "{partials}: {tuning}");
        let key = estimate_key(c.grid(), KeyProfile::Krumhansl).unwrap().key;
        assert_eq!(
            key,
            Key {
                tonic: PitchClass::C,
                mode: Mode::Major
            }
        );
    }
    // Fixed tuning is reported as given.
    let fixed = cfg.with_tuning(Tuning::Fixed(Cents(12.0))).unwrap();
    let c = chroma(&melody(&[0], 0.5, 1, 0.0), &fixed, &mut FftScratch::new()).unwrap();
    assert_eq!(c.tuning(), Cents(12.0));
}

/// Silence has no pitch anywhere: zero chroma and no key. Noise has chroma
/// and a key with far less confidence than a scale's. NaN, infinity and a
/// buffer shorter than a window are errors.
///
/// Mutation checked: skipping the flat-chroma check in the key reads silence
/// as a key and fails; a key confidence fixed at certain fails the noise
/// comparison.
#[test]
fn silence_noise_and_bad_input() {
    let cfg = config();
    let mut fft = FftScratch::new();
    let silent = chroma(&vec![0.0; SR as usize], &cfg, &mut fft).unwrap();
    assert!(silent.grid().as_slice().iter().all(|&v| v == 0.0));
    assert_eq!(
        estimate_key(silent.grid(), KeyProfile::Temperley).unwrap_err(),
        Error::NoTonalContent
    );

    let mut rng = StdRng::seed_from_u64(3);
    let noise: Vec<f32> = (0..SR as usize * 4)
        .map(|_| rng.gen_range(-0.3..0.3))
        .collect();
    let noisy = chroma(&noise, &cfg, &mut fft).unwrap();
    assert!(noisy.grid().as_slice().iter().all(|v| v.is_finite()));
    let sure = |x: &[f32]| {
        let c = chroma(x, &cfg, &mut FftScratch::new()).unwrap();
        estimate_key(c.grid(), KeyProfile::Krumhansl)
            .unwrap()
            .confidence
            .get()
    };
    assert!(sure(&noise) < 0.5 * sure(&melody(&C_MAJOR_SCALE, 0.5, 4, 0.0)));

    let mut bad = noise.clone();
    bad[5000] = f32::NAN;
    assert_eq!(
        chroma(&bad, &cfg, &mut fft).unwrap_err(),
        Error::NonFiniteInput
    );
    bad[5000] = f32::INFINITY;
    assert_eq!(
        chroma(&bad, &cfg, &mut fft).unwrap_err(),
        Error::NonFiniteInput
    );
    assert!(matches!(
        chroma(&noise[..1000], &cfg, &mut fft),
        Err(Error::InsufficientInput { .. })
    ));
}
