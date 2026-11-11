use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Db, Seconds};

use super::*;
use crate::contract::cases::design as cases;
use crate::contract::{Anchor, DesignCase, Hook, LtiCase, ReverbCase};
use crate::network::Band;
use crate::testing::flaws::{Mutant, map_absorption, map_fdn};
use crate::{Lti as _, System as _};

fn controls(room: f32, damp: f32) -> FreeverbControls {
    FreeverbControls {
        room: Mix(room),
        damp: Mix(damp),
    }
}

impl DesignCase for FreeverbDesign {
    fn reachable(&self) -> Vec<FreeverbControls> {
        let steps = [0.0, 0.25, 0.5, 0.75, 1.0];
        let mut all: Vec<_> = steps
            .iter()
            .flat_map(|&r| steps.iter().map(move |&d| controls(r, d)))
            .collect();
        let mut rng = StdRng::seed_from_u64(22);
        all.extend((0..200).map(|_| controls(rng.gen_range(0.0..=1.0), rng.gen_range(0.0..=1.0))));
        all
    }

    fn hostile(&self) -> Vec<FreeverbControls> {
        let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 7.0];
        nasty
            .iter()
            .flat_map(|&v| [controls(v, 0.5), controls(0.5, v), controls(v, v)])
            .collect()
    }

    fn musical_delay(&self, n: &Network) -> usize {
        n.min_delay() as usize
    }

    /// Freeverb has no size, diffusion, pre-delay, freeze, width or wet
    /// control, so those hooks are empty, and its network never sets a
    /// pre-delay, a width or a wet gain.
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[
            (Hook::SizePairs, "Freeverb has no size control"),
            (Hook::DiffusionPairs, "Freeverb has no diffusion control"),
            (Hook::PreDelayPairs, "Freeverb has no pre-delay"),
            (Hook::FreezePairs, "Freeverb cannot freeze"),
            (Hook::WidthPairs, "Freeverb has no width control"),
            (Hook::WetPairs, "Freeverb has no wet gain"),
        ]
    }
}

impl ReverbCase for FreeverbDesign {
    /// 15%. The measured figure is the energy slope 40 to 65 dB down, where
    /// a bank of combs of 1116 to 1640 frames (a 1.47 ratio in rate) has
    /// not yet settled on the slowest: it lands 5 to 11% under the network's
    /// `rt60`, which is the slowest comb's (design 022 asked for 10%; the
    /// bank's own spread needs more).
    fn tolerance(&self) -> f32 {
        0.15
    }

    /// The RT60 of the slowest comb (1617 + 23 = 1640 frames at 44.1 kHz,
    /// scaled to the rate) at the gain Freeverb gives a band: the loop
    /// gain `room * 0.28 + 0.7` times the one-pole `(1 - d) / (1 - d z^-1)`
    /// (`d = damp * 0.4`) at DC, a quarter of the rate and Nyquist.
    fn requested_rt60(
        &self,
        c: &FreeverbControls,
        rate: SampleRate,
        band: Band,
    ) -> Option<Seconds> {
        let longest = (rate.get() / 44_100.0 * 1640.0).round();
        let loop_gain = f64::from(c.room.get() * 0.28 + 0.7);
        let d = f64::from(c.damp.get() * 0.4);
        let pole = match band {
            Band::Low => 1.0,
            Band::Mid => (1.0 - d) / libm::sqrt(1.0 + d * d),
            Band::High => (1.0 - d) / (1.0 + d),
        };
        let g = loop_gain * pole;
        Some(Seconds(
            (-3.0 * longest / (rate.get() * libm::log10(g))) as f32 as f64,
        ))
    }

    fn decay_pairs(&self) -> Vec<(FreeverbControls, FreeverbControls)> {
        vec![
            (controls(0.2, 0.5), controls(0.5, 0.5)),
            (controls(0.5, 0.5), controls(1.0, 0.5)),
            (controls(0.0, 0.0), controls(1.0, 1.0)),
        ]
    }
    // Freeverb has no size, diffusion, pre-delay or freeze control, so the
    // size, diffusion and pre-delay pairs are empty and `frozen` is false:
    // those directions of the cases are vacuous for it (`vacant` says so).
    // The freeze case still asserts that no control reaches a lossless loop.
}

impl LtiCase for FreeverbDesign {
    /// `|H_L(0)| = 0.015 sum_left 1 / (1 - g_loop)`: at DC each comb is
    /// `0.015 z^-D / (1 - g_loop z^-D)` with the one-pole at its DC gain (the
    /// loop gain `g_loop = room * 0.28 + 0.7`), the eight left combs add, and
    /// the four all-passes have unit gain at DC for a feedback of 0.5. The right
    /// channel is the same sum over its eight combs.
    fn anchors(&self) -> Vec<Anchor<FreeverbControls>> {
        let rate = SampleRate(48_000.0);
        [(0.5, 0.5), (0.0, 0.5), (1.0, 0.2), (0.9, 1.0)]
            .into_iter()
            .flat_map(|(room, damp)| {
                let loop_gain = f64::from(room * 0.28 + 0.7);
                let dc = 8.0 * f64::from(Freeverb::FIXED_GAIN) / (1.0 - loop_gain);
                (0..2).map(move |output| Anchor {
                    controls: controls(room, damp),
                    rate,
                    output,
                    freq: tutti_types::Hz(0.0),
                    gain: dc,
                    tolerance: 1e-4,
                })
            })
            .collect()
    }

    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        (trips * self.bounds(&self.ceiling(rate)).line) as usize
    }
}

crate::design_contract!(FreeverbDesign, Lti, reverb);

fn mutant(f: fn(&mut Network)) -> Mutant<FreeverbDesign> {
    Mutant::new(FreeverbDesign, f)
}

#[track_caller]
fn fails(result: Result<(), String>, why: &str) {
    let e = result.expect_err("the case should fail on the defective design");
    assert!(e.contains(why), "{e}");
}

/// Each case of the contract fails on a Freeverb with the defect its
/// mutation names (the cases' mutation records, run every time).
#[test]
fn a_defective_freeverb_fails_its_case() {
    // stable: a gain above 1.
    let m = mutant(|n| map_absorption(n, |_| Absorption::Gain(Amplitude(1.02))));
    fails(cases::stable(&m), "unstable");

    // rt60: every comb gain scaled by 1.02.
    let m = mutant(|n| {
        map_absorption(n, |a| match a {
            Absorption::OnePole { gain, damp } => Absorption::OnePole {
                gain: gain * 1.02,
                damp,
            },
            a => a,
        })
    });
    fails(cases::rt60_matches(&m), "rt60");

    // band order: the low and high bands' gains swapped.
    let m = mutant(|n| {
        map_absorption(n, |a| Absorption::Bands {
            low: a.gain(Band::High),
            mid: a.gain(Band::Mid),
            high: a.gain(Band::Low),
            crossover: [tutti_types::Hz(250.0), tutti_types::Hz(4000.0)],
        })
    });
    fails(cases::band_order(&m), "bands");

    // tail monotone in decay: the room runs backwards.
    let m = mutant(|n| {
        map_absorption(n, |a| match a {
            Absorption::OnePole { gain, damp } => Absorption::OnePole {
                gain: Amplitude(1.68 - gain.get()),
                damp,
            },
            a => a,
        })
    });
    fails(cases::tail_monotone(&m), "tail fell");

    // lossless iff frozen: every gain 1 although nothing froze it.
    let m = mutant(|n| map_absorption(n, |_| Absorption::Gain(Amplitude(1.0))));
    fails(cases::lossless_iff_frozen(&m), "lossless");

    // density: no stages, so no modes.
    let m = mutant(|n| n.stages = Inline::new());
    fails(cases::density(&m), "modal density");

    // bounds: one line short.
    let mut m = mutant(|_| {});
    m.bounds = |b| Bounds {
        lines: b.lines - 1,
        ..b
    };
    fails(cases::bounds_dominate(&m), "does not dominate");

    // pure: the design reads a counter, as a clock would be read.
    let m = mutant(|n| {
        thread_local! { static CALLS: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) }; }
        n.wet = Db(CALLS.with(|c| {
            c.set(c.get() + 1.0);
            c.get()
        }));
    });
    fails(cases::pure(&m), "differ");

    // no direct path: a line of length 0.
    let m = mutant(|n| map_fdn(n, |f| f.lines.as_mut_slice()[0].length = 0.0));
    fails(cases::no_direct_path(&m), "direct path");

    // pre-delay: a frame where there is none.
    let m = mutant(|n| n.pre_delay = tutti_types::Samples(n.pre_delay.get() + 1));
    fails(cases::pre_delay(&m), "pre-delay");
}

/// The cases the `Lti` group and the vacuity case added fail on a Freeverb with
/// the defect their mutation names.
#[test]
fn a_defective_freeverb_fails_the_lti_design_cases() {
    use crate::network::{Tap, Taps};
    use crate::testing::flaws::Lying;
    let louder = |n: &mut Network| {
        map_absorption(n, |a| match a {
            Absorption::OnePole { gain, damp } => Absorption::OnePole {
                gain: gain * 1.1,
                damp,
            },
            a => a,
        })
    };
    // radius and no rescale: every comb gain 10% high takes the loudest room
    // over 1, and its absorption peak with it.
    fails(cases::radius(&mutant(louder)), "spectral radius");
    fails(cases::no_rescale(&mutant(louder)), "rescale");
    // anchors: the input gain (Freeverb's FIXED_GAIN) 10% high moves the DC gain.
    let m = mutant(|n| {
        map_fdn(n, |f| {
            f.input.as_mut_slice().iter_mut().for_each(|g| *g *= 1.1)
        })
    });
    fails(cases::anchors(&m), "|H| is");
    // ports: a third output channel the design does not declare.
    let m = mutant(|n| {
        let mut taps = Taps::default();
        taps.taps
            .push(Tap {
                delay: 0.0,
                gain: 0.0,
                channel: 2,
            })
            .unwrap();
        let _ = n.stages.push(Stage::Taps(taps));
    });
    fails(cases::ports(&m), "outputs");
    // norms: a model whose hinf_norm is a tenth of its own bound is under the
    // peak (the bound is 2 to 4 times the peak, so a half would not be), and
    // one 50 times it is far over (checked where the grid resolves the poles).
    let low = Lying {
        inner: FreeverbDesign,
        tail: 1.0,
        hinf: 0.1,
        l1: 1.0,
    };
    fails(cases::norms(&low), "over hinf_norm");
    let high = Lying {
        inner: FreeverbDesign,
        tail: 1.0,
        hinf: 50.0,
        l1: 50.0,
    };
    fails(cases::norms(&high), "times the peak");
    // vacuity: a hook that returns nothing and is not listed, a hook that is
    // listed and returns something.
    let mut m = mutant(|_| {});
    m.vacant = Some(&[]);
    fails(cases::reverb_vacuity(&m), "returns nothing");
    m.vacant = Some(&[(Hook::Hostile, "it is not")]);
    fails(cases::vacuity(&m), "is listed as vacant");
}

/// At the General MIDI controls the network carries `Freeverb`'s own
/// constants to the bit: the comb loop gain, the damping, the all-pass
/// feedback and the input gain.
#[test]
fn the_general_midi_controls_give_todays_coefficients_to_the_bit() {
    let n = FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, SampleRate(44_100.0));
    let Stage::Fdn(fdn) = &n.stages[0] else {
        panic!("the first stage is the combs")
    };
    for line in fdn.lines.iter() {
        let Absorption::OnePole { gain, damp } = line.absorption else {
            panic!()
        };
        assert_eq!(gain.get().to_bits(), Freeverb::ROOM_FEEDBACK.to_bits());
        assert_eq!(damp.to_bits(), Freeverb::DAMPING.to_bits());
    }
    assert!(
        fdn.input
            .iter()
            .all(|&g| g.to_bits() == Freeverb::FIXED_GAIN.to_bits())
    );
    for stage in &n.stages[1..] {
        let Stage::AllPass(a) = stage else {
            panic!("then the all-passes")
        };
        assert!(
            a.iter()
                .all(|a| a.gain.to_bits() == Freeverb::ALLPASS_FEEDBACK.to_bits())
        );
        // Freeverb's own all-pass, not the lattice (see `AllPassForm`).
        assert!(a.iter().all(|a| a.form == AllPassForm::Freeverb));
    }
    // The literal figures too, so the constants themselves are pinned: the
    // loop gain 0.84, the damping 0.2, the all-pass feedback 0.5, the input 0.015.
    assert_eq!(Freeverb::ROOM_FEEDBACK, 0.84);
    assert_eq!(Freeverb::DAMPING, 0.2);
    assert_eq!(Freeverb::ALLPASS_FEEDBACK, 0.5);
    assert_eq!(Freeverb::FIXED_GAIN, 0.015);
}

/// The network's lines are those of a `Freeverb` built at the same rate,
/// and its coefficients are the ones in the combs, at several rates (the
/// rate is rounded to whole hertz as `Freeverb::new` does).
#[test]
fn the_network_has_the_lines_of_a_freeverb_built_at_the_rate() {
    for hz in [
        22_050.0, 44_100.0, 44_100.4, 48_000.0, 88_200.0, 96_000.0, 192_000.0,
    ] {
        let rate = SampleRate(hz);
        let reverb = Freeverb::new(rate);
        let figures = reverb.figures();
        let n = FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, rate);
        assert_eq!(n.rate, SampleRate(hz.round()));
        let Stage::Fdn(fdn) = &n.stages[0] else {
            panic!()
        };
        let lengths: Vec<usize> = fdn.lines.iter().map(|l| l.length as usize).collect();
        let want: Vec<usize> = figures
            .combs_left
            .iter()
            .chain(&figures.combs_right)
            .copied()
            .collect();
        assert_eq!(lengths, want, "{hz} Hz");
        assert_eq!(fdn.matrix, Matrix::Diagonal);
        for (stage, (l, r)) in n.stages[1..].iter().zip(
            figures
                .all_passes_left
                .iter()
                .zip(&figures.all_passes_right),
        ) {
            let Stage::AllPass(a) = stage else { panic!() };
            assert_eq!(
                (a[0].delay as usize, a[1].delay as usize),
                (*l, *r),
                "{hz} Hz"
            );
        }
        let (feedback, damp1, damp2) = figures.comb;
        let Absorption::OnePole { gain, damp } = fdn.lines[0].absorption else {
            panic!()
        };
        assert_eq!((gain.get(), damp), (feedback, damp1));
        assert_eq!(damp2, 1.0 - damp);
        assert_eq!(a_gain(&n), figures.all_pass_feedback);
    }
}

fn a_gain(n: &Network) -> f32 {
    let Stage::AllPass(a) = &n.stages[1] else {
        panic!()
    };
    a[0].gain
}

/// The stereo spread: the right combs and all-passes are 23 frames longer
/// at 44.1 kHz, and the rows of `C` send the first eight lines to the left
/// and the next eight to the right. The tunings are Freeverb's (1116, 1188,
/// 1277, 1356, 1422, 1491, 1557, 1617 and 556, 441, 341, 225), so the total
/// delay is their sum, a figure fixed by the definition.
#[test]
fn the_tunings_and_the_stereo_spread_are_freeverbs() {
    let n = FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, SampleRate(44_100.0));
    let Stage::Fdn(fdn) = &n.stages[0] else {
        panic!()
    };
    let left = [
        1116.0f32, 1188.0, 1277.0, 1356.0, 1422.0, 1491.0, 1557.0, 1617.0,
    ];
    for (i, base) in left.iter().enumerate() {
        assert_eq!(fdn.lines[i].length, *base);
        assert_eq!(fdn.lines[8 + i].length, base + 23.0);
    }
    assert_eq!(fdn.output.rows.len(), 2);
    for (i, (l, r)) in fdn.output.rows[0]
        .iter()
        .zip(fdn.output.rows[1].iter())
        .enumerate()
    {
        assert_eq!((*l, *r), if i < 8 { (1.0, 0.0) } else { (0.0, 1.0) });
    }
    let aps = [556.0f32, 441.0, 341.0, 225.0];
    for (stage, base) in n.stages[1..].iter().zip(aps) {
        let Stage::AllPass(a) = stage else { panic!() };
        assert_eq!((a[0].delay, a[1].delay), (base, base + 23.0));
    }
    // 16 combs and 8 all-passes: 2 * (sum of the tunings) + 8 * 23 + 4 * 23.
    let total: f32 = 2.0 * (left.iter().sum::<f32>() + aps.iter().sum::<f32>()) + 12.0 * 23.0;
    assert_eq!(n.modal_density().to_f32(), total / 44_100.0);
}

/// The analytic figures of the GM reverb, from the definition: the slowest
/// comb is 1640 frames at a loop gain of 0.84, so its low-band RT60 is
/// `-3 * 1640 / (44100 * log10 0.84)` = 1.5 s; the one-pole at 0.2 leaves
/// the Nyquist band `0.84 * 0.8 / 1.2` = 0.56 of it per trip. The tail to
/// -60 dB is the longest line, the all-passes' trains and that RT60.
#[test]
fn the_general_midi_network_decays_as_the_definition_says() {
    let n = FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, SampleRate(44_100.0));
    let low = -3.0 * 1640.0 / (44_100.0 * libm::log10(0.84));
    assert!(
        (n.rt60(Band::Low).get() - low).abs() < 1e-5,
        "{:?}",
        n.rt60(Band::Low)
    );
    let high = -3.0 * 1640.0 / (44_100.0 * libm::log10(0.84 * 0.8 / 1.2));
    assert!((n.rt60(Band::High).get() - high).abs() < 1e-5);
    assert!(n.rt60(Band::High).get() < n.rt60(Band::Mid).get());
    assert!(n.rt60(Band::Mid).get() < n.rt60(Band::Low).get());
    assert!(n.is_stable() && !n.is_lossless());
    assert_eq!(n.latency(), tutti_types::Samples::ZERO);
    let Ring::Finite(tail) = n.tail(Db(-60.0)) else {
        panic!("finite")
    };
    assert!(tail.to_f32() > low as f32, "{tail:?} covers the decay");
}

use crate::Ring;

/// The bounds of Freeverb are the design at room 1, whatever the ceiling's
/// decay: it has no decay control to cap.
#[test]
fn the_bounds_are_the_largest_design() {
    let ceiling = Ceiling::at(SampleRate(48_000.0)).unwrap();
    let b = FreeverbDesign.bounds(&ceiling);
    let room_one = FreeverbDesign.design(&controls(1.0, 0.0), SampleRate(48_000.0));
    assert_eq!(b, Bounds::measure(&room_one, ceiling.floor()));
    assert_eq!((b.lines, b.stages, b.outputs), (16, 5, 2));
    let short = Ceiling::new(
        SampleRate(48_000.0),
        MaxDecay::new(Seconds(0.5)).unwrap(),
        Db(-60.0),
    )
    .unwrap();
    assert_eq!(FreeverbDesign.bounds(&short), b);
}

/// A non-finite control takes the General MIDI value and a control out of
/// range is clamped, so the design is total.
#[test]
fn a_hostile_control_is_made_safe() {
    let rate = SampleRate(44_100.0);
    let gm = FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, rate);
    assert_eq!(
        FreeverbDesign.design(&controls(f32::NAN, f32::INFINITY), rate),
        gm
    );
    assert_eq!(
        FreeverbDesign.design(&controls(9.0, -2.0), rate),
        FreeverbDesign.design(&controls(1.0, 0.0), rate)
    );
}
