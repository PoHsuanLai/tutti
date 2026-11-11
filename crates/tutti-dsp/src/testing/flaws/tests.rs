use super::*;
use crate::contract::cases::design as cases;

crate::design_contract!(ToyFdn, Lti, reverb);

/// Asserts a case failed, with a message containing `why`.
fn fails(result: Result<(), String>, why: &str) {
    let e = result.expect_err("the case should fail on the defective design");
    println!("{e}");
    assert!(e.contains(why), "{e}");
}

fn toy(f: fn(&mut Network)) -> Mutant<ToyFdn> {
    Mutant::new(ToyFdn, f)
}

/// Each case fails on a design with the defect its mutation names. These
/// are the mutation records of the cases, run on every test run.
#[test]
fn a_defective_design_fails_its_case() {
    // stable: a gain above 1.
    let m = toy(|n| map_absorption(n, |_| Absorption::Gain(Amplitude(1.02))));
    fails(cases::stable(&m), "unstable");

    // rt60 matches: every gain scaled by 0.98.
    let m = toy(|n| {
        map_absorption(n, |a| match a {
            Absorption::Bands {
                low,
                mid,
                high,
                crossover,
            } => Absorption::Bands {
                low: low * 0.98,
                mid: mid * 0.98,
                high: high * 0.98,
                crossover,
            },
            a => a,
        })
    });
    fails(cases::rt60_matches(&m), "rt60");

    // band order: low and high gains swapped.
    let m = toy(|n| {
        map_absorption(n, |a| match a {
            Absorption::Bands {
                low,
                mid,
                high,
                crossover,
            } => Absorption::Bands {
                low: high,
                mid,
                high: low,
                crossover,
            },
            a => a,
        })
    });
    fails(cases::band_order(&m), "bands");

    // tail monotone in decay: the gains run backwards (1 - g).
    let m = toy(|n| {
        map_absorption(n, |a| match a {
            Absorption::Bands {
                low,
                mid,
                high,
                crossover,
            } => Absorption::Bands {
                low: Amplitude(0.99 - low.get().min(0.99)),
                mid: Amplitude(0.99 - mid.get().min(0.99)),
                high: Amplitude(0.99 - high.get().min(0.99)),
                crossover,
            },
            a => a,
        })
    });
    fails(cases::tail_monotone(&m), "tail fell");

    // lossless iff frozen: one band of a frozen network at 0.999.
    let m = toy(|n| {
        map_absorption(n, |a| match a {
            Absorption::Bands {
                low,
                mid,
                high: Amplitude(1.0),
                crossover,
            } => Absorption::Bands {
                low,
                mid,
                high: Amplitude(0.999),
                crossover,
            },
            a => a,
        })
    });
    fails(cases::lossless_iff_frozen(&m), "lossless");

    // density, size: lengths ignore size.
    let m = toy(|n| {
        map_fdn(n, |f| {
            for (l, base) in f.lines.as_mut_slice().iter_mut().zip(TOY_LENGTHS) {
                l.length = base;
            }
        });
        for s in n.stages.as_mut_slice() {
            if let Stage::AllPass(a) = s {
                for ap in a.as_mut_slice() {
                    ap.delay = 142.0;
                }
            }
        }
    });
    fails(cases::density(&m), "modal density");

    // density, diffusion: the matrix is always diagonal.
    let m = toy(|n| map_fdn(n, |f| f.matrix = Matrix::Diagonal));
    fails(cases::density(&m), "echo density");

    // bounds: one line short.
    let mut m = toy(|_| {});
    m.bounds = |b| NetworkSizes {
        lines: b.lines - 1,
        ..b
    };
    fails(cases::bounds_dominate(&m), "does not dominate");

    // pure: the design reads a counter, as a clock would be read.
    let m = toy(|n| {
        thread_local! { static CALLS: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) }; }
        n.wet = Db(CALLS.with(|c| {
            c.set(c.get() + 1.0);
            c.get()
        }));
    });
    fails(cases::pure(&m), "differ");

    // no direct path: a line of length 0.
    let m = toy(|n| map_fdn(n, |f| f.lines.as_mut_slice()[0].length = 0.0));
    fails(cases::no_direct_path(&m), "direct path");

    // pre-delay: one frame more.
    let m = toy(|n| n.pre_delay = Samples(n.pre_delay.get() + 1));
    fails(cases::pre_delay(&m), "pre-delay");
}

/// The cases are not vacuous on `ToyFdn`: it asks for RT60s, bands, pairs.
#[test]
fn the_toy_design_exercises_every_hook() {
    let d = ToyFdn;
    assert!(!d.decay_pairs().is_empty() && !d.size_pairs().is_empty());
    assert!(!d.diffusion_pairs().is_empty() && !d.pre_delay_pairs().is_empty());
    assert!(d.reachable().iter().any(|c| c.freeze));
    assert!(
        d.requested_rt60(&ToyControls::BASE, SampleRate(48_000.0), Band::Mid)
            .is_some()
    );
}

// `loop_gain` is used by `Absorption::from_rt60`; keep the import honest.
#[test]
fn the_toy_gains_are_the_formula() {
    let n = ToyFdn.design(&ToyControls::BASE, SampleRate(48_000.0));
    let Stage::Fdn(f) = &n.stages[0] else {
        panic!()
    };
    let l = f.lines[0];
    assert_eq!(
        l.absorption.gain(Band::Mid),
        loop_gain(l.length, Seconds(1.0), SampleRate(48_000.0))
    );
}
