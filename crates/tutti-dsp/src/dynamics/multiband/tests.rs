use core::f64::consts::PI;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{BusTopology, CompressionRatio, Seconds};

use super::*;
use crate::contract::{FamilyCase, Hook, RecurrenceCase, built};
use crate::testing::amplitude_at;
use crate::{Complex32, Complex64, Lti, MAX_LANES, RealFft, Realized};

const RATE: SampleRate = SampleRate(48_000.0);
/// The longest block `run` hands the runner.
const CHUNK: usize = 256;
/// Frames a glide takes at [`RATE`]: `ceil(0.005 * 48000)`.
const GLIDE: usize = 240;
/// Frames for the crossover's sections and the followers to settle on a constant.
const SETTLE: usize = 24_000;

fn design() -> MultibandFamily {
    MultibandFamily::new(BusTopology::STEREO, 3).unwrap()
}

fn mono(bands: usize) -> MultibandFamily {
    MultibandFamily::new(BusTopology::MONO, bands).unwrap()
}

fn hz(freqs: &[f32]) -> CrossoverControls {
    let mut frequencies = [Hz(0.0); 5];
    for (f, &x) in frequencies.iter_mut().zip(freqs) {
        *f = Hz(x);
    }
    CrossoverControls { frequencies }
}

fn band(
    threshold: f32,
    knee: f32,
    ratio: f32,
    attack: f64,
    release: f64,
    makeup: f32,
) -> BandSettings {
    BandSettings {
        compressor: CompressorControls {
            threshold: Db(threshold),
            knee: Db(knee),
            ratio: CompressionRatio(ratio),
            attack: Seconds(attack),
            release: Seconds(release),
            makeup: Db(makeup),
        },
        bypass: false,
        solo: false,
    }
}

/// Every band at unity ratio and no makeup, the splits at `freqs`: nothing is reduced.
fn unity(freqs: &[f32]) -> MultibandSettings {
    MultibandSettings {
        crossover: hz(freqs),
        bands: [band(-18.0, 0.0, 1.0, 0.01, 0.1, 0.0); MAX_BANDS],
    }
}

impl FamilyCase for MultibandFamily {
    fn capacity(&self, _: SampleRate) {}

    fn reachable(&self) -> Vec<MultibandSettings> {
        let mut all = Vec::new();
        for threshold in [-40.0, -18.0, 0.0] {
            for ratio in [1.0, 4.0, 20.0] {
                for (attack, release) in [(0.0, 0.0), (0.005, 0.1)] {
                    let b = band(threshold, 6.0, ratio, attack, release, 3.0);
                    all.push(MultibandSettings {
                        crossover: hz(&[200.0, 3_000.0, 9_000.0]),
                        bands: [b; MAX_BANDS],
                    });
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(9);
        for _ in 0..48 {
            let mut set = unity(&[
                rng.gen_range(60.0..300.0),
                rng.gen_range(400.0..3_000.0),
                rng.gen_range(4_000.0..12_000.0),
            ]);
            for b in &mut set.bands {
                *b = BandSettings {
                    bypass: rng.gen_bool(0.3),
                    solo: rng.gen_bool(0.3),
                    ..band(
                        rng.gen_range(-60.0..0.0),
                        rng.gen_range(0.0..12.0),
                        rng.gen_range(1.0..20.0),
                        rng.gen_range(0.0..0.05),
                        rng.gen_range(0.0..0.5),
                        rng.gen_range(-12.0..12.0),
                    )
                };
            }
            all.push(set);
        }
        all
    }

    /// A band below the floors, a control that is not finite, and splits out of order, not
    /// finite or above Nyquist: each is a member the family can run.
    fn hostile(&self) -> Vec<MultibandSettings> {
        let mut all = Vec::new();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut set = unity(&[200.0, 3_000.0]);
            set.bands[1] = band(bad, bad, bad, f64::from(bad), f64::from(bad), bad);
            all.push(set);
            all.push(unity(&[bad, 3_000.0]));
        }
        all.push(unity(&[3_000.0, 200.0]));
        all.push(unity(&[100_000.0, 200_000.0]));
        let mut set = unity(&[200.0, 3_000.0]);
        set.bands[0] = band(-18.0, -6.0, 0.25, -1.0, -1.0, 0.0);
        all.push(set);
        all
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(
            Hook::Extremes,
            "every follower is a one-pole inside the unit circle and the crossover is \
             stable at every split: there is no loop to close on its limit",
        )]
    }
}

impl RecurrenceCase for MultibandFamily {
    fn probes(&self) -> Vec<MultibandSettings> {
        let mut soloed = unity(&[300.0, 4_000.0]);
        soloed.bands[0] = BandSettings {
            solo: true,
            ..band(-30.0, 0.0, 3.0, 0.002, 0.05, 2.0)
        };
        vec![
            MultibandSettings {
                crossover: hz(&[250.0, 2_500.0]),
                bands: [band(-24.0, 6.0, 4.0, 0.003, 0.08, 3.0); MAX_BANDS],
            },
            unity(&[120.0, 1_500.0]),
            soloed,
        ]
    }

    fn extremes(&self) -> Vec<MultibandSettings> {
        Vec::new()
    }

    /// A makeup that moves and a bypass and a solo that flip (each must glide), and a
    /// split that moves a little.
    fn steps(&self) -> Vec<(MultibandSettings, MultibandSettings)> {
        let base = MultibandSettings {
            crossover: hz(&[250.0, 2_500.0]),
            bands: [band(-24.0, 6.0, 4.0, 0.01, 0.1, 0.0); MAX_BANDS],
        };
        let with = |f: &dyn Fn(&mut MultibandSettings)| {
            let mut s = base;
            f(&mut s);
            s
        };
        vec![
            (base, with(&|s| s.bands[1].compressor.makeup = Db(6.0))),
            (base, with(&|s| s.bands[0].bypass = true)),
            (base, with(&|s| s.bands[2].solo = true)),
            (base, with(&|s| s.crossover = hz(&[300.0, 2_200.0]))),
        ]
    }
}

crate::family_contract!(design());
crate::recurrence_contract!(design(), BlockIndependent);

type Runner = Realized<MultibandRecurrence>;

fn runner(f: &MultibandFamily, c: &MultibandSettings) -> Runner {
    built(f, c, RATE).0
}

/// `audio` (one slice per channel) through `k` in blocks of [`CHUNK`], band `b`'s
/// threshold lane `lanes[b]` where given.
fn run(k: &mut Runner, audio: &[&[f32]], lanes: &[Option<&[f32]>]) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + CHUNK).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut feed = Feed::new(&a);
        for (i, lane) in lanes.iter().enumerate() {
            if let Some(lane) = lane {
                feed = feed.with_lane(i, &lane[at..end]);
            }
        }
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        k.process(&feed, &mut outs, end - at);
        at = end;
    }
    out
}

fn mean(x: &[f32]) -> f64 {
    x.iter().map(|&v| f64::from(v)).sum::<f64>() / x.len() as f64
}

fn tone(freq: f64, amplitude: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| amplitude * libm::sin(2.0 * PI * freq * n as f64 / RATE.get()) as f32)
        .collect()
}

/// The reduction in dB of the static curve at a hard knee, from its definition.
fn hard(level_db: f64, threshold: f64, ratio: f64) -> f64 {
    (1.0 - 1.0 / ratio) * (level_db - threshold).max(0.0)
}

fn db(amplitude: f64) -> f64 {
    20.0 * libm::log10(amplitude)
}

fn linear(db: f64) -> f64 {
    libm::pow(10.0, db / 20.0)
}

/// Band `b`'s complex response at `f`, from the model.
fn response(m: &MultibandMember, band: usize, f: f32) -> Complex64 {
    let mut g = [Complex64::new(0.0, 0.0)];
    m.crossover().frequency_response(band, &[Hz(f)], &mut g);
    g[0]
}

/// The family refuses a band count outside 2 to 4 and an empty layout, and answers for
/// the bands it has.
#[test]
fn the_family_has_two_to_four_bands() {
    for bands in [0, 1, 5, 6] {
        assert!(
            matches!(
                MultibandFamily::new(BusTopology::STEREO, bands),
                Err(CrossoverError::Bands { min: 2, max: 4, .. })
            ),
            "{bands}"
        );
    }
    assert!(MultibandFamily::new(BusTopology::EMPTY, 3).is_err());
    let f = MultibandFamily::new(BusTopology::STEREO, 4).unwrap();
    let m = f.member(&MultibandSettings::DEFAULT, RATE);
    assert_eq!((f.bands(), m.bands()), (4, 4));
    assert!(m.compressor(3).is_some() && m.compressor(4).is_none());
    assert_eq!(f.channels().output_width(), 2);
    // One lane a band at the widest.
    const { assert!(MAX_LANES >= MultibandFamily::MAX_BANDS) };
}

/// With every band at unity ratio and no makeup the bands sum to the crossover's
/// all-pass: unit magnitude at every frequency, read from an impulse, for 2, 3 and 4
/// bands; and the sum is not the input (the all-pass rings).
#[test]
fn unity_bands_sum_to_the_crossovers_all_pass() {
    const N: usize = 1 << 15;
    let mut impulse = vec![0.0f32; N];
    impulse[0] = 1.0;
    for (bands, freqs) in [
        (2, vec![1_000.0f32]),
        (3, vec![200.0, 3_000.0]),
        (4, vec![120.0, 1_000.0, 6_000.0]),
    ] {
        let mut k = runner(&mono(bands), &unity(&freqs));
        let out = run(&mut k, &[&impulse], &[])[0].clone();
        let fft = RealFft::new(N);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(&out, &mut bins, &mut scratch);
        for (i, b) in bins.iter().enumerate().skip(1) {
            let magnitude = f64::from(b.re).hypot(f64::from(b.im));
            assert!(
                (magnitude - 1.0).abs() < 2e-4,
                "{bands} bands, bin {i}: {magnitude}"
            );
        }
        let ring: f32 = out[1..64].iter().map(|v| v.abs()).sum();
        assert!(
            ring > 0.1,
            "{bands} bands: the sum rings {ring}, not an input copy"
        );
    }
}

/// Loud content in band 0 is reduced by the compressor's static curve and a quiet
/// band is left alone.
///
/// A constant 0.5 (-6.02 dB) is all band 0 once the crossover settles, and the one input
/// whose detector reading is exact: at -20 dB, 4:1 the reduction is
/// `(-6.02 + 20) * 3/4` dB, with a 2 dB makeup on it. The tone is 0.01 (-40 dB), under
/// every band's -18 dB threshold. The meter is read over the settled window alone (it
/// holds each band's deepest since it was last taken, and the step's own transient
/// overshoots the curve a little).
#[test]
fn a_loud_band_follows_the_static_curve_and_a_quiet_band_is_left_alone() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-20.0, 0.0, 4.0, 0.001, 0.001, 2.0);
    let len = SETTLE + 9_600;
    let x: Vec<f32> = tone(5_000.0, 0.01, len).iter().map(|s| 0.5 + s).collect();
    let mut k = runner(&mono(3), &set);
    let mut out = run(&mut k, &[&x[..SETTLE]], &[]);
    k.take_reading();
    let rest = run(&mut k, &[&x[SETTLE..]], &[]);
    out[0].extend_from_slice(&rest[0]);
    let reduction = hard(db(0.5), -20.0, 4.0);
    let want = 0.5 * linear(2.0 - reduction);
    let dc = mean(&out[0][SETTLE..]);
    assert!(
        (dc - want).abs() < 5e-6 * want,
        "band 0 settled at {dc}, the curve says {want}"
    );
    let amp = amplitude_at(&out[0][SETTLE..], 5_000.0, RATE.get());
    assert!((amp - 0.01).abs() < 2e-5, "the quiet band is {amp}");
    // The meter over the window: band 0 sits on the curve, the others reduce nothing.
    let reading = k.take_reading();
    let deepest = f64::from(reading.band(0).get());
    assert!(
        (deepest - reduction).abs() < 1e-3,
        "{deepest} vs {reduction}"
    );
    assert!(reading.band(1).get() < 1e-3 && reading.band(2).get() < 1e-3);
}

/// A bypassed band passes unreduced and without makeup, and meters nothing; releasing the
/// bypass compresses it again.
#[test]
fn a_bypassed_band_passes_unprocessed() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.001, 6.0);
    set.bands[0].bypass = true;
    let f = mono(3);
    let mut k = runner(&f, &set);
    let x = vec![0.5f32; SETTLE + 9_600];
    let out = run(&mut k, &[&x], &[]);
    assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < 1e-6);
    assert_eq!(k.take_reading().band(0), Db::UNITY);
    set.bands[0].bypass = false;
    k.retarget(&f.member(&set, RATE));
    let out = run(&mut k, &[&x], &[]);
    // Over -30 dB at 8:1 the 0.5 DC is reduced by `(30 - 6.02) * 7/8`, with the makeup.
    let reduction = hard(db(0.5), -30.0, 8.0);
    let want = 0.5 * linear(6.0 - reduction);
    assert!(
        (mean(&out[0][SETTLE..]) - want).abs() < 1e-5,
        "{}",
        mean(&out[0][SETTLE..])
    );
}

/// While any band is soloed the output is the sum of the soloed bands alone: the model's
/// band responses at the tone, summed as complex numbers.
#[test]
fn solo_leaves_only_the_soloed_bands() {
    let f = mono(3);
    let freqs = [300.0f32, 3_000.0];
    let (tone_hz, len) = (3_500.0f32, SETTLE + 9_600);
    let x = tone(f64::from(tone_hz), 1.0, len);
    for soloed in [
        [true, false, false],
        [false, true, false],
        [true, true, false],
        [false, false, true],
    ] {
        let mut set = unity(&freqs);
        for (b, &s) in set.bands.iter_mut().zip(&soloed) {
            b.solo = s;
        }
        let m = f.member(&set, RATE);
        let mut want = Complex64::new(0.0, 0.0);
        for (b, &s) in soloed.iter().enumerate() {
            assert_eq!(m.is_audible(b), s);
            if s {
                want += response(&m, b, tone_hz);
            }
        }
        let mut k = runner(&f, &set);
        let out = run(&mut k, &[&x], &[]);
        let got = amplitude_at(&out[0][SETTLE..], f64::from(tone_hz), RATE.get());
        assert!(
            (got - want.norm()).abs() < 1e-4,
            "{soloed:?}: {got} vs {}",
            want.norm()
        );
    }
}

/// A bypass that flips glides linearly over 5 ms and lands: on a settled DC the output
/// is the blend `g (1 - w) + w` of the compressed gain `g` and unity, `w` the weight
/// `k / 240` at frame `k - 1` of the glide.
#[test]
fn a_moved_bypass_glides_over_five_milliseconds() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.001, 0.0);
    let f = mono(3);
    let mut k = runner(&f, &set);
    let x = vec![0.5f32; SETTLE + 2 * GLIDE];
    let before = run(&mut k, &[&x[..SETTLE]], &[]);
    let held = f64::from(*before[0].last().unwrap());
    set.bands[0].bypass = true;
    k.retarget(&f.member(&set, RATE));
    let out = run(&mut k, &[&x[SETTLE..]], &[])[0].clone();
    for (j, &y) in out.iter().take(GLIDE).enumerate() {
        let w = (j + 1) as f64 / GLIDE as f64;
        let want = held * (1.0 - w) + 0.5 * w;
        assert!(
            (f64::from(y) - want).abs() < 2e-4,
            "frame {j}: {y} vs {want}"
        );
    }
    for (j, &y) in out.iter().enumerate().skip(GLIDE) {
        assert!((y - 0.5).abs() < 1e-5, "frame {j} after the glide: {y}");
    }
    assert!(
        held < 0.45,
        "the band was compressing ({held}), or this proves nothing"
    );
}

/// A solo that moves mutes the other band over 5 ms: the muted band's DC is its settled
/// level times the audible weight `1 - k / 240`, and lands on silence.
#[test]
fn a_moved_solo_glides_the_mute_over_five_milliseconds() {
    let set = unity(&[200.0, 3_000.0]);
    let f = mono(3);
    let mut k = runner(&f, &set);
    let x = vec![0.5f32; SETTLE + 2 * GLIDE];
    let before = run(&mut k, &[&x[..SETTLE]], &[]);
    let held = f64::from(*before[0].last().unwrap());
    let mut soloed = set;
    soloed.bands[1].solo = true;
    k.retarget(&f.member(&soloed, RATE));
    let out = run(&mut k, &[&x[SETTLE..]], &[])[0].clone();
    for (j, &y) in out.iter().take(GLIDE).enumerate() {
        let w = (j + 1) as f64 / GLIDE as f64;
        let want = held * (1.0 - w);
        assert!(
            (f64::from(y) - want).abs() < 2e-4,
            "frame {j}: {y} vs {want}"
        );
    }
    for (j, &y) in out.iter().enumerate().skip(GLIDE) {
        assert!(y.abs() < 1e-5, "frame {j} after the glide: {y}");
    }
    assert!((held - 0.5).abs() < 1e-5, "{held}");
}

/// Detection is linked across channels: one reduction from the loudest channel of the
/// band, applied to both.
#[test]
fn detection_is_linked_across_channels() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-20.0, 0.0, 4.0, 0.001, 0.001, 0.0);
    let len = SETTLE + 4_800;
    let (loud, quiet) = (vec![0.5f32; len], vec![0.05f32; len]);
    let g = linear(-hard(db(0.5), -20.0, 4.0));
    // Whichever channel is the loud one.
    for (left, right, loud_side) in [(&loud, &quiet, 0), (&quiet, &loud, 1)] {
        let mut k = runner(&design(), &set);
        let out = run(&mut k, &[left, right], &[]);
        let (l, r) = (mean(&out[0][SETTLE..]), mean(&out[1][SETTLE..]));
        let (want_l, want_r) = if loud_side == 0 {
            (0.5, 0.05)
        } else {
            (0.05, 0.5)
        };
        assert!(
            (l - want_l * g).abs() < 1e-5 * g,
            "loud {loud_side}: left {l}"
        );
        // The quiet channel alone is under the threshold (-26 dB) and would not be reduced.
        assert!(
            (r - want_r * g).abs() < 1e-5 * g,
            "loud {loud_side}: right {r}"
        );
    }
}

/// A band's follower is the compressor's one-pole: after one attack time on a settled
/// level the reduction has closed `1 - 1/e` of its way to the curve.
///
/// The level is a settled constant and the step is the threshold lane dropping from
/// +20 dB (nothing to reduce) to -30 dB, so the target reduction jumps at once and the
/// crossover has no transient of its own to blur it.
#[test]
fn a_bands_follower_closes_one_minus_one_over_e_in_one_attack() {
    let mut set = unity(&[200.0, 3_000.0]);
    let attack = 0.01;
    set.bands[0] = band(0.0, 0.0, 8.0, attack, 1.0, 0.0);
    let mut k = runner(&mono(3), &set);
    run(
        &mut k,
        &[&vec![0.5f32; SETTLE]],
        &[Some(&vec![20.0f32; SETTLE])],
    );
    k.take_reading();
    let frames = (attack * RATE.get()) as usize;
    run(
        &mut k,
        &[&vec![0.5f32; frames]],
        &[Some(&vec![-30.0f32; frames])],
    );
    let reached = f64::from(k.take_reading().band(0).get());
    let curve = hard(db(0.5), -30.0, 8.0);
    let want = curve * (1.0 - 1.0 / core::f64::consts::E);
    assert!((reached - want).abs() < 2e-3 * curve, "{reached} vs {want}");
}

/// The meter holds the deepest reduction since it was last taken, over every block in
/// between and not the last frame's: a loud block and the silent one after it read as the
/// loud one's; and the read starts again from nothing (a second with no frames between
/// is 0, and once the follower has released, the next reads 0 too).
#[test]
fn the_meter_reports_the_deepest_reduction_since_the_last_read() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.005, 0.0);
    let mut k = runner(&mono(3), &set);
    run(&mut k, &[&vec![0.5f32; SETTLE]], &[]);
    k.take_reading();
    run(&mut k, &[&vec![0.5f32; 4_800]], &[]);
    run(&mut k, &[&vec![0.0f32; 4_800]], &[]);
    let curve = hard(db(0.5), -30.0, 8.0);
    let both = f64::from(k.take_reading().band(0).get());
    assert!((both - curve).abs() < 1e-3, "{both} vs {curve}");
    assert_eq!(k.take_reading().band(0), Db::UNITY);
    // The follower released during the silence: nothing is held over from before the read.
    run(&mut k, &[&vec![0.0f32; 4_800]], &[]);
    assert!(k.take_reading().band(0).get() < 1e-3);
    // Past the last band there is nothing to read.
    assert_eq!(k.take_reading().band(7), Db::UNITY);
}

/// Each band reads its own threshold lane per frame: driving band 0's lane low reduces
/// band 0 and no other, and a frame that is not finite falls back to the member's.
#[test]
fn a_band_reads_its_own_threshold_lane() {
    let mut set = unity(&[200.0, 3_000.0]);
    for b in &mut set.bands {
        *b = band(0.0, 0.0, 4.0, 0.001, 0.05, 0.0);
    }
    let f = mono(3);
    let len = SETTLE;
    let x = vec![0.5f32; len];
    let low = vec![-30.0f32; len];
    let mut k = runner(&f, &set);
    run(&mut k, &[&x], &[Some(&low)]);
    let curve = hard(db(0.5), -30.0, 4.0);
    let reading = k.take_reading();
    let d0 = f64::from(reading.band(0).get());
    assert!((d0 - curve).abs() < 0.05, "{d0} vs {curve}");
    assert!(reading.band(1).get() < 1e-3 && reading.band(2).get() < 1e-3);
    // A lane that is NaN reads as the member's threshold (0 dB): nothing to reduce.
    let nan = vec![f32::NAN; len];
    let mut k = runner(&f, &set);
    run(&mut k, &[&x], &[Some(&nan)]);
    assert_eq!(k.take_reading().band(0), Db::UNITY);
}
