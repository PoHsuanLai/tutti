//! [`FreeverbDesign`]: [`Freeverb`] as a [`Design`].
//!
//! The network describes the linear filters of the reverb in `reverb.rs`: per
//! channel eight parallel damped combs (a diagonal [`Fdn`] of one-pole lines,
//! the right channel's 23 frames longer) feeding four series all-passes in
//! [`AllPassForm::Freeverb`] (Freeverb's own, not the unit-magnitude lattice),
//! at Freeverb's tunings and constants. What it leaves out is the realization's
//! flush of any value under 1e-6 to zero, a nonlinearity far below the tail's
//! floor that no query models. The comb loop gain is `room * 0.28 + 0.7` and the
//! damping `damp * 0.4`, computed in the same `f32` operations as
//! [`Freeverb`]'s constants, so the General MIDI controls (room 0.5, damp 0.5)
//! give today's coefficients to the bit. The realization is [`Freeverb`] itself
//! ([`Realization`](crate::Realization)): it reads those two numbers from the network, and
//! this file changes no processing.

use core::num::NonZeroU32;

use tutti_types::{Amplitude, ChannelLayout, Mix, SampleRate};

use crate::network::{
    Absorption, AllPass, AllPassForm, Bounds, Fdn, Gains, Inline, Line, MAX_CHANNELS, MAX_LINES,
    Matrix, Modulation, Network, OutputMap, Stage,
};
use crate::reverb::Freeverb;
use crate::{Cadence, Ceiling, Channels, Design, MaxDecay, Sizes};

const COMBS_LEFT: [usize; 8] = [
    Freeverb::CF_TUNING_L1,
    Freeverb::CF_TUNING_L2,
    Freeverb::CF_TUNING_L3,
    Freeverb::CF_TUNING_L4,
    Freeverb::CF_TUNING_L5,
    Freeverb::CF_TUNING_L6,
    Freeverb::CF_TUNING_L7,
    Freeverb::CF_TUNING_L8,
];
const COMBS_RIGHT: [usize; 8] = [
    Freeverb::CF_TUNING_R1,
    Freeverb::CF_TUNING_R2,
    Freeverb::CF_TUNING_R3,
    Freeverb::CF_TUNING_R4,
    Freeverb::CF_TUNING_R5,
    Freeverb::CF_TUNING_R6,
    Freeverb::CF_TUNING_R7,
    Freeverb::CF_TUNING_R8,
];
const ALL_PASSES: [[usize; 2]; 4] = [
    [Freeverb::APF_TUNING_L1, Freeverb::APF_TUNING_R1],
    [Freeverb::APF_TUNING_L2, Freeverb::APF_TUNING_R2],
    [Freeverb::APF_TUNING_L3, Freeverb::APF_TUNING_R3],
    [Freeverb::APF_TUNING_L4, Freeverb::APF_TUNING_R4],
];

// The network is built by pushing into fixed lists; these are what makes the
// pushes infallible.
const _: () = assert!(COMBS_LEFT.len() + COMBS_RIGHT.len() <= MAX_LINES);
const _: () = assert!(2 <= MAX_CHANNELS);

/// Freeverb's two controls, as positions on `0..1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreeverbControls {
    /// The room size: scales the comb loop gain, `room * 0.28 + 0.7`.
    pub room: Mix,
    /// The damping: the comb's one-pole, `damp * 0.4`.
    pub damp: Mix,
}

impl FreeverbControls {
    /// The General MIDI reverb: room 0.5, damping 0.5.
    pub const GENERAL_MIDI: FreeverbControls = FreeverbControls {
        room: Mix(0.5),
        damp: Mix(0.5),
    };
}

impl Default for FreeverbControls {
    fn default() -> Self {
        Self::GENERAL_MIDI
    }
}

/// The [`Design`] of [`Freeverb`]: mono in, stereo out, two controls.
///
/// A control outside `0..1` is clamped, and a non-finite one takes the General
/// MIDI value, so every control gives a stable network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeverbDesign;

fn position(mix: Mix, default: f32) -> f32 {
    let v = mix.get();
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        default
    }
}

impl Design for FreeverbDesign {
    type Controls = FreeverbControls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Freeverb;

    // A retarget lands at once and nothing ramps: the render is a function of
    // the stream whatever the blocks (the design is still sampled per block, as
    // for the FDN, doc 024 Q27).
    const CADENCE: Cadence = Cadence::stream(NonZeroU32::MIN);

    fn channels(&self) -> Channels {
        Channels {
            input: ChannelLayout::MONO,
            output: ChannelLayout::STEREO,
        }
    }

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    /// A [`Freeverb`] at the default rate; the node's `prepare` re-tunes it.
    fn realization(&self) -> Freeverb {
        Freeverb::new(SampleRate::DEFAULT)
    }

    fn design_into(&self, controls: &FreeverbControls, rate: SampleRate, out: &mut Network) {
        // `Freeverb::new` rounds the rate to whole hertz and scales the tunings
        // from it, so the lines are those of a reverb built at `rate`.
        let rate = SampleRate(rate.get().round());
        let room = position(controls.room, FreeverbControls::GENERAL_MIDI.room.get());
        let damp = position(controls.damp, FreeverbControls::GENERAL_MIDI.damp.get());
        // The same operations, in the same order, as `Freeverb::ROOM_FEEDBACK`
        // and `Freeverb::DAMPING`.
        let absorption = Absorption::OnePole {
            gain: Amplitude(room * 0.28 + 0.7),
            damp: damp * 0.4,
        };

        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for &tuning in COMBS_LEFT.iter().chain(&COMBS_RIGHT) {
            let line = Line {
                length: Freeverb::scale_tuning(rate, tuning) as f32,
                modulation: Modulation::NONE,
                absorption,
                diffuser: None,
            };
            let _ = lines.push(line);
        }
        let n = lines.len();

        // Left row: the first eight lines; right row: the next eight.
        let mut left = Gains::filled(0.0, n).unwrap_or_default();
        let mut right = Gains::filled(0.0, n).unwrap_or_default();
        for (i, (l, r)) in left
            .as_mut_slice()
            .iter_mut()
            .zip(right.as_mut_slice())
            .enumerate()
        {
            if i < COMBS_LEFT.len() {
                *l = 1.0;
            } else {
                *r = 1.0;
            }
        }
        let mut rows = Inline::new();
        let _ = rows.push(left);
        let _ = rows.push(right);

        out.clear(rate);
        let _ = out.stages.push(Stage::Fdn(Fdn {
            lines,
            matrix: Matrix::Diagonal,
            input: Gains::filled(Freeverb::FIXED_GAIN, n).unwrap_or_default(),
            output: OutputMap { rows },
        }));
        for [l, r] in ALL_PASSES {
            let all_pass = |tuning| AllPass {
                delay: Freeverb::scale_tuning(rate, tuning) as f32,
                gain: Freeverb::ALLPASS_FEEDBACK,
                modulation: Modulation::NONE,
                form: AllPassForm::Freeverb,
            };
            let mut channels = Inline::new();
            let _ = channels.push(all_pass(l));
            let _ = channels.push(all_pass(r));
            let _ = out.stages.push(Stage::AllPass(channels));
        }
    }

    /// Freeverb's lengths and stages are constants and its decay is longest at
    /// room 1 (the low band's loop gain is the room's alone), so the worst case
    /// is that design. `max_decay` does not apply: Freeverb has no decay
    /// control, and its longest RT60 is what room 1 makes it.
    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        let worst = FreeverbControls {
            room: Mix(1.0),
            damp: Mix(0.0),
        };
        Bounds::measure(&self.design(&worst, ceiling.rate()), ceiling.floor())
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tutti_types::{Db, Seconds};

    use super::*;
    use crate::contract::cases::design as cases;
    use crate::contract::flaws::{Mutant, map_absorption, map_fdn};
    use crate::contract::{Anchor, DesignCase, Hook, LtiCase, ReverbCase};
    use crate::network::Band;
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
            all.extend(
                (0..200).map(|_| controls(rng.gen_range(0.0..=1.0), rng.gen_range(0.0..=1.0))),
            );
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
        use crate::contract::flaws::Lying;
        use crate::network::{Tap, Taps};
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
    ///
    /// Mutations (run): `form: AllPassForm::Freeverb` -> `Lattice` fails the
    /// form assertion; `* 0.28 + 0.7` written `* 0.28 + 0.70001` in the design
    /// fails the first assertion; `0.4` -> `0.41` fails the second.
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
    ///
    /// Mutation (run): `COMBS_RIGHT` taking `CF_TUNING_L*` fails the right
    /// combs; skipping the `round` of the rate fails the 44 100.4 Hz case.
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
    ///
    /// Mutation (run): a spread of 0 fails the right lengths; swapping the two
    /// rows of `C` fails the row check.
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
    ///
    /// Mutation (run): `ROOM_FEEDBACK` 0.84 -> 0.85 moves the RT60 and fails.
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
    ///
    /// Mutation (run): `Mix(1.0)` -> `Mix(0.5)` in `bounds` fails the
    /// dominance case (and this one).
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
    ///
    /// Mutation (run): dropping the `is_finite` test lets NaN through: the
    /// first assertion fails.
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
}

/// The realization contract over Freeverb: the realization is `Freeverb` itself, held to
/// its network's analytic figures.
#[cfg(test)]
mod realization_tests {
    use super::*;
    use crate::contract::RealizationCase;
    use crate::contract::cases::realization as cases;
    use crate::contract::flaws::{Defect, Defective};
    use crate::{Realization, Setup};

    fn controls(room: f32, damp: f32) -> FreeverbControls {
        FreeverbControls {
            room: Mix(room),
            damp: Mix(damp),
        }
    }

    impl RealizationCase for FreeverbDesign {
        fn probes(&self) -> Vec<FreeverbControls> {
            vec![
                controls(0.5, 0.5),
                controls(0.0, 0.5),
                controls(0.9, 0.2),
                controls(0.5, 1.0),
            ]
        }

        fn extremes(&self) -> Vec<FreeverbControls> {
            vec![controls(1.0, 0.0), controls(1.0, 1.0)]
        }

        fn steps(&self) -> Vec<(FreeverbControls, FreeverbControls)> {
            vec![
                (controls(0.5, 0.5), controls(0.6, 0.5)),
                (controls(0.9, 0.0), controls(0.2, 1.0)),
            ]
        }
    }

    crate::realization_contract!(FreeverbDesign, Lti, reverb, BlockIndependent);

    fn defective(defect: Defect) -> Defective<FreeverbDesign> {
        Defective {
            inner: FreeverbDesign,
            defect,
            tweak: |_| {},
        }
    }

    #[track_caller]
    fn fails(result: Result<(), String>, why: &str) {
        let e = result.expect_err("the case should fail on the defective realization");
        assert!(e.contains(why), "{e}");
    }

    /// Each case of the contract fails on a Freeverb with the defect its
    /// mutation names (the cases' mutation records, run every time).
    #[test]
    fn a_defective_realization_fails_its_case() {
        // rt60: a loop gain 5% too high rings 39% longer.
        let m = defective(Defect::LoopGain(1.05));
        fails(cases::rt60(&m), "measured RT60");
        // tail: 3% too high rings 2.4 times as long at room 0.9.
        let m = defective(Defect::LoopGain(1.03));
        fails(cases::tail_covers(&m), "dB");
        // stability: a gain that never stops growing.
        fails(cases::stability(&defective(Defect::Runaway)), "");
        // band order: no damping, every band like the lowest.
        let m = defective(Defect::NoDamping);
        fails(cases::band_order(&m), "bands");
        // arrival: a frame late, and a latency that claims a frame.
        fails(
            cases::arrival(&defective(Defect::LateByOne)),
            "first output",
        );
        fails(
            cases::arrival(&defective(Defect::FalseLatency)),
            "first output",
        );
        // latency: one frame more than the network has.
        fails(cases::latency(&defective(Defect::FalseLatency)), "latency");
        // wet only: the input leaks to the output.
        fails(
            cases::wet_only(&defective(Defect::DryLeak)),
            "shortest path",
        );
        // decorrelated: both channels alike.
        fails(
            cases::decorrelated(&defective(Defect::MonoOutput)),
            "correlate",
        );
        // click free: a gain of 4 applied whole at the retarget.
        fails(
            cases::click_free(&defective(Defect::UnslewedGain(4.0))),
            "jump",
        );
        // A modest step too: 1.5 times fails the 3 times allowance (measured
        // margins of the clean realizations: Freeverb 1.98, the FDN 1.51 times the
        // steady jump).
        fails(
            cases::click_free(&defective(Defect::UnslewedGain(1.5))),
            "jump",
        );
        // line lengths: the longest comb 5% long (inside the RT60 tolerance).
        let m = Defective {
            inner: FreeverbDesign,
            defect: Defect::None,
            tweak: |k| k.lengthen_last_comb(1722),
        };
        fails(cases::line_lengths(&m), "first echo");
        // reset that does nothing; fresh that keeps the state; a gain that
        // depends on the block.
        fails(cases::reset(&defective(Defect::NoReset)), "reset");
        fails(cases::fresh(&defective(Defect::FreshKeepsState)), "fresh()");
        fails(
            cases::block_independent(&defective(Defect::BlockDependent)),
            "blocks of",
        );
    }

    /// The `Lti` realization cases fail on a Freeverb with the defect their
    /// mutation names (and the measured figures of the clean one, with the
    /// margins, are in the cases' own docs).
    #[test]
    fn a_defective_freeverb_fails_the_lti_realization_cases() {
        use crate::contract::flaws::Lying;
        // linear: a saturation breaks additivity; a gain that moves with the
        // frame is linear and breaks shift invariance.
        fails(cases::linear(&defective(Defect::Tanh)), "additivity");
        fails(
            cases::linear(&defective(Defect::PeriodicGain)),
            "shift invariance",
        );
        // response: the output 10% high from the second retarget on, and every
        // comb gain 5% high.
        fails(
            cases::response(&defective(Defect::UnslewedGain(1.1))),
            "impulse response's spectrum",
        );
        fails(
            cases::response(&defective(Defect::LoopGain(1.05))),
            "impulse response's spectrum",
        );
        // decay: every comb gain 5% high decays at 72% of the radius's rate.
        fails(
            cases::decay(&defective(Defect::LoopGain(1.05))),
            "spectral radius",
        );
        // tail tightness: a model whose tail is twice the truth.
        let lying = Lying {
            inner: FreeverbDesign,
            tail: 2.0,
            hinf: 1.0,
            l1: 1.0,
        };
        fails(cases::tail_tight(&lying), "the tail to");
    }

    /// At the General MIDI controls `retarget` leaves the combs on exactly the
    /// coefficients a freshly built `Freeverb` starts with, bit for bit, so the
    /// realization of the General MIDI send renders what the reverb always did: the
    /// processing loops are untouched and the figures they read are the same
    /// bits. (An invariant of the coefficients, not a render compared with
    /// another render.)
    ///
    /// Mutation (run): `1_f32 - damp` written `1.0001 - damp` in `set_combs`
    /// fails the `damp2` comparison; `ROOM_FEEDBACK` moved by one ulp in the
    /// design fails the first.
    #[test]
    fn retargeting_to_the_general_midi_network_changes_no_coefficient() {
        let rate = SampleRate(44_100.0);
        let fresh = Freeverb::new(rate);
        // Start from other figures so the retarget is seen to set them.
        let mut realization = Freeverb::new(rate);
        realization.retarget(&FreeverbDesign.design(&controls(0.9, 0.9), rate));
        assert_ne!(realization.figures().comb, fresh.figures().comb);
        realization.retarget(&FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, rate));
        let (a, b) = (realization.figures().comb, fresh.figures().comb);
        assert_eq!(
            (a.0.to_bits(), a.1.to_bits(), a.2.to_bits()),
            (b.0.to_bits(), b.1.to_bits(), b.2.to_bits())
        );
    }

    /// A network the realization does not understand leaves it as it was: no
    /// recirculating stage, a stage that is not a one-pole, and figures that are
    /// not a stable loop.
    ///
    /// Mutation (run): dropping the `feedback < 1` range test lets the unstable
    /// network through and fails the last assertion.
    #[test]
    fn a_network_it_does_not_understand_is_ignored() {
        let rate = SampleRate(44_100.0);
        let mut realization = Freeverb::new(rate);
        let before = realization.figures().comb;
        realization.retarget(&Network::new(rate));
        let mut flat = FreeverbDesign.design(&controls(0.9, 0.9), rate);
        crate::contract::flaws::map_absorption(&mut flat, |_| Absorption::Gain(Amplitude(0.5)));
        realization.retarget(&flat);
        let mut unstable = FreeverbDesign.design(&controls(0.9, 0.9), rate);
        crate::contract::flaws::map_absorption(&mut unstable, |_| Absorption::OnePole {
            gain: Amplitude(1.5),
            damp: 0.2,
        });
        realization.retarget(&unstable);
        assert_eq!(realization.figures().comb, before);
    }

    /// A realization prepared at another rate keeps the coefficients it was
    /// retargeted to, and its lines are those of a `Freeverb` built at that rate.
    ///
    /// Mutation (run): `prepare` returning early leaves the 44 100 Hz lines and
    /// fails the lengths.
    #[test]
    fn prepare_retunes_the_lines_and_keeps_the_coefficients() {
        let mut realization = FreeverbDesign.realization();
        realization.retarget(&FreeverbDesign.design(&controls(0.9, 0.2), SampleRate::DEFAULT));
        let want = realization.figures().comb;
        let rate = SampleRate(96_000.0);
        let ceiling = Ceiling::at(rate).unwrap();
        realization.prepare(
            &FreeverbDesign.bounds(&ceiling),
            &Setup {
                rate,
                max_block: tutti_types::MaxBlock::of(tutti_types::Samples(64)),
            },
        );
        assert_eq!(realization.figures().comb, want);
        assert_eq!(
            realization.figures().combs_left,
            Freeverb::new(rate).figures().combs_left
        );
    }
}
