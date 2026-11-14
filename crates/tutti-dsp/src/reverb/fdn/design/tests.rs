use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use super::*;
use crate::contract::{FamilyCase, Hook, LtiCase, RecurrenceCase, ReverbCase};
use crate::reverb::network::{Band, Fdn, Matrix, Network, Stage};
use crate::{Lti as _, Realized, System as _};

fn freezable() -> FdnFamily {
    FdnFamily::new(8, MaxDecay::DEFAULT.get(), true).unwrap()
}

fn base() -> FdnControls {
    FdnControls::DEFAULT
}

fn with(f: impl FnOnce(&mut FdnControls)) -> FdnControls {
    let mut c = base();
    f(&mut c);
    c
}

impl FamilyCase for FdnFamily {
    fn capacity(&self, rate: SampleRate) -> NetworkCapacity {
        self.reach(&self.reach_at(rate))
    }

    fn reachable(&self) -> Vec<FdnControls> {
        let mut all = Vec::new();
        for low in [0.2, 1.0, 8.0] {
            for mid in [0.2, 1.0, 8.0] {
                for high in [0.2, 1.0, 8.0] {
                    for size in [0.0, 0.5, 1.0] {
                        for diffusion in [0.0, 0.5, 1.0] {
                            for freeze in [false, true] {
                                for pre in [0.0, 0.1] {
                                    all.push(FdnControls {
                                        decay: [Seconds(mid), Seconds(low), Seconds(high)],
                                        size: Mix(size),
                                        diffusion: Mix(diffusion),
                                        pre_delay: Seconds(pre),
                                        freeze,
                                        ..base()
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(22);
        for _ in 0..200 {
            all.push(FdnControls {
                decay: [0; 3].map(|_| Seconds(rng.random_range(0.05..8.0))),
                size: Mix(rng.random_range(0.0..=1.0)),
                diffusion: Mix(rng.random_range(0.0..=1.0)),
                pre_delay: Seconds(rng.random_range(0.0..=0.5)),
                modulation: Depth::of(rng.random_range(-1.0..=1.0)),
                rate: Hz(rng.random_range(0.05..=5.0)),
                width: StereoWidth(rng.random_range(0.0..=2.0)),
                wet: Db(rng.random_range(-60.0..=12.0)),
                freeze: rng.random_bool(0.2),
            });
        }
        all
    }

    fn hostile(&self) -> Vec<FdnControls> {
        let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 40.0, 0.0];
        nasty
            .iter()
            .map(|&v| FdnControls {
                decay: [Seconds::from_f32(v); 3],
                size: Mix(v),
                diffusion: Mix(v),
                pre_delay: Seconds::from_f32(v),
                modulation: Depth::clamped(v),
                rate: Hz(v),
                width: StereoWidth(v),
                wet: Db(v),
                freeze: false,
            })
            .collect()
    }

    fn musical_delay(&self, n: &Network) -> usize {
        n.min_delay() as usize
    }

    /// The figures of the FDN have no closed form to compare to (the lines
    /// are mixed): its response is held to the realization's, in the
    /// `response` case.
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(Hook::Figures, "a mixing FDN has no closed-form response")]
    }
}

impl LtiCase for FdnFamily {
    fn marginal(&self, c: &FdnControls) -> bool {
        self.freezable && c.freeze
    }

    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        (trips * self.capacity(rate).line) as usize
    }
}

impl ReverbCase for FdnFamily {
    fn tolerance(&self) -> f32 {
        0.15
    }

    /// Deep: a train of echoes (parallel combs) decays in steps, and a fit
    /// over a wide range averages them.
    fn fit_depth(&self) -> (f64, f64) {
        (20.0, 100.0)
    }

    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        match band {
            Band::Low => 0.0,
            Band::Mid => (libm::sqrt(300.0 * 4_000.0) / rate.get()) as f32,
            Band::High => 0.5,
        }
    }

    fn freeze_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
        vec![(base(), with(|c| c.freeze = true))]
    }

    fn width_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
        vec![(base(), with(|c| c.width = StereoWidth::MONO))]
    }

    fn wet_pairs(&self) -> Vec<(FdnControls, FdnControls, f32)> {
        vec![
            (base(), with(|c| c.wet = Db(-6.0206)), 0.5),
            (base(), with(|c| c.wet = Db(f32::NEG_INFINITY)), 0.0),
        ]
    }

    /// `None` where the controls ask for band gains whose response exceeds 1
    /// (a mid band ringing far longer than a neighbour): the design scales them
    /// to peak at 0.9999, as the kernel does, so the decay asked for is not the
    /// decay the network has, and there is no figure to hold it to.
    fn requested_rt60(&self, c: &FdnControls, rate: SampleRate, band: Band) -> Option<Seconds> {
        let n = self.member(c, rate);
        if let Some(Stage::Fdn(f)) = n.stages.first()
            && let Some(l) = f.lines.first()
            && !(self.freezable && c.freeze)
        {
            let rt = |i: usize| c.decay[i].clamp(MIN_DECAY, self.max_decay);
            let g = [1usize, 0, 2].map(|i| crate::loop_gain(l.length, rt(i), rate).get());
            let pole = CROSSOVER.map(|f| {
                libm::exp(-std::f64::consts::TAU * f64::from(f.get()) / rate.get()) as f32
            });
            if crate::reverb::fdn::recurrence::peak_response(g, pole) > 1.0 {
                return None;
            }
        }
        Some(if self.freezable && c.freeze {
            Seconds(f64::INFINITY)
        } else {
            c.decay[match band {
                Band::Mid => 0,
                Band::Low => 1,
                Band::High => 2,
            }]
        })
    }

    fn frozen(&self, c: &FdnControls) -> bool {
        self.freezable && c.freeze
    }

    fn pre_delay(&self, c: &FdnControls) -> Seconds {
        c.pre_delay
    }

    fn pre_delay_pairs(&self) -> Vec<(FdnControls, FdnControls, Seconds)> {
        vec![(
            with(|c| c.modulation = Depth::of(0.0)),
            with(|c| {
                c.modulation = Depth::of(0.0);
                c.pre_delay = Seconds(0.1);
            }),
            Seconds(0.1),
        )]
    }

    fn decay_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
        [(0.2, 1.0), (1.0, 8.0)]
            .map(|(a, b)| {
                (
                    with(|c| c.decay = [Seconds(a); 3]),
                    with(|c| c.decay = [Seconds(b); 3]),
                )
            })
            .to_vec()
    }

    fn size_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
        [(0.0, 0.5), (0.5, 1.0)]
            .map(|(a, b)| {
                (
                    with(|c| {
                        c.size = Mix(a);
                        c.modulation = Depth::of(0.0);
                    }),
                    with(|c| {
                        c.size = Mix(b);
                        c.modulation = Depth::of(0.0);
                    }),
                )
            })
            .to_vec()
    }

    fn diffusion_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
        [(0.0, 0.5), (0.0, 1.0)]
            .map(|(a, b)| {
                (
                    with(|c| c.diffusion = Mix(a)),
                    with(|c| c.diffusion = Mix(b)),
                )
            })
            .to_vec()
    }
}

mod freezable_design {
    use super::*;
    crate::family_contract!(freezable(), Grows, Lti, reverb);
}

mod plain_design {
    use super::*;
    crate::family_contract!(FdnFamily::default(), Grows, Lti, reverb);
}

impl RecurrenceCase for FdnFamily {
    fn probes(&self) -> Vec<FdnControls> {
        vec![
            base(),
            with(|c| {
                c.decay = [Seconds(3.0), Seconds(3.5), Seconds(1.5)];
                c.diffusion = Mix(1.0);
                c.pre_delay = Seconds(0.05);
                c.modulation = Depth::of(0.5);
            }),
            with(|c| {
                c.decay = [Seconds(1.0), Seconds(1.5), Seconds(0.5)];
                c.size = Mix(0.2);
                c.diffusion = Mix(0.3);
            }),
            with(|c| {
                c.diffusion = Mix(0.0);
                c.modulation = Depth::of(0.0);
            }),
        ]
    }

    fn static_probes(&self) -> Vec<FdnControls> {
        self.probes()
            .into_iter()
            .map(|c| FdnControls {
                modulation: Depth::of(0.0),
                pre_delay: Seconds(0.02),
                ..c
            })
            .collect()
    }

    fn extremes(&self) -> Vec<FdnControls> {
        let hard = |f: fn(&mut FdnControls)| {
            with(|c| {
                c.size = Mix(1.0);
                c.modulation = Depth::of(1.0);
                c.rate = Hz(5.0);
                f(c)
            })
        };
        vec![
            hard(|c| c.decay = [Seconds(8.0); 3]),
            hard(|c| c.decay = [Seconds(8.0), Seconds(8.0), Seconds(0.05)]),
            hard(|c| c.decay = [Seconds(8.0), Seconds(0.05), Seconds(0.05)]),
            hard(|c| {
                c.decay = [Seconds(8.0); 3];
                c.diffusion = Mix(0.0);
            }),
        ]
    }

    fn steps(&self) -> Vec<(FdnControls, FdnControls)> {
        vec![
            (base(), with(|c| c.decay = [Seconds(6.0); 3])),
            (base(), with(|c| c.size = Mix(0.9))),
            (base(), with(|c| c.diffusion = Mix(0.1))),
            // A pre-delay that is not a whole number of cycles of the 150 Hz
            // tone the case plays.
            (base(), with(|c| c.pre_delay = Seconds(0.0123))),
            (base(), with(|c| c.wet = Db(-12.0))),
            (base(), with(|c| c.freeze = true)),
        ]
    }
}

crate::recurrence_contract!(freezable(), Lti, reverb, BlockIndependent);

/// The FDN's declared cadence is its chunk clock, so it names the
/// `BlockIndependent` group.
#[test]
fn the_fdn_declares_its_stream_grid() {
    assert_eq!(
        FdnFamily::CADENCE,
        Cadence::stream(NonZeroU32::new(CHUNK as u32).unwrap())
    );
}

/// No hook of the FDN is vacuous: every case that Freeverb leaves empty has
/// something to run here, so a case that stops running fails this test.
#[test]
fn no_hook_of_the_fdn_is_vacuous() {
    let d = freezable();
    assert!(!ReverbCase::freeze_pairs(&d).is_empty());
    assert!(!ReverbCase::width_pairs(&d).is_empty());
    assert!(!ReverbCase::wet_pairs(&d).is_empty());
    assert!(!ReverbCase::pre_delay_pairs(&d).is_empty());
    assert!(!ReverbCase::size_pairs(&d).is_empty());
    assert!(!ReverbCase::diffusion_pairs(&d).is_empty());
    assert!(ReverbCase::frozen(&d, &with(|c| c.freeze = true)));
    assert!(ReverbCase::pre_delay(&d, &with(|c| c.pre_delay = Seconds(0.1))).get() > 0.0);
}

fn design_of(d: &FdnFamily, c: &FdnControls) -> Network {
    d.member(c, SampleRate::SR_48K)
}

fn fdn_of(n: &Network) -> &Fdn {
    let Some(Stage::Fdn(f)) = n.stages.first() else {
        panic!("one FDN stage")
    };
    f
}

/// Diffusion is the reflection vector: none couples no line (the network
/// says so), full is the Householder matrix, and between is a unit vector
/// that leans from one axis toward the flat vector, so the loop is
/// orthogonal at every setting.
#[test]
fn diffusion_is_the_reflection_vector() {
    let d = freezable();
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(0.0)))).matrix,
        Matrix::Diagonal
    );
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(1.0)))).matrix,
        Matrix::Householder
    );
    let mut last_axis = f32::INFINITY;
    for diffusion in [0.1, 0.25, 0.5, 0.75, 0.9] {
        let n = design_of(&d, &with(|c| c.diffusion = Mix(diffusion)));
        let Matrix::Reflection(v) = &fdn_of(&n).matrix else {
            panic!("a reflection at {diffusion}")
        };
        assert_eq!(v.len(), 8);
        let unit: f64 = v.iter().map(|&x| f64::from(x).powi(2)).sum();
        assert!((unit - 1.0).abs() < 1e-6, "{diffusion}: |v|^2 = {unit}");
        assert!(
            v[0] < last_axis,
            "the first line's share falls as the mix grows"
        );
        last_axis = v[0];
    }
}

/// A freeze holds every gain at exactly one, shuts the input, stops the
/// modulation and makes the network lossless with an unbounded tail; a
/// design that is not freezable ignores it.
#[test]
fn a_freeze_is_lossless_and_takes_no_input() {
    let (d, plain) = (freezable(), FdnFamily::default());
    let frozen = with(|c| c.freeze = true);
    let n = design_of(&d, &frozen);
    let f = fdn_of(&n);
    assert!(n.is_lossless() && n.is_stable());
    assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
    assert!(f.input.iter().all(|&g| g == 0.0), "the input is shut");
    assert!(f.lines.iter().all(|l| l.modulation.depth == 0.0));
    for l in f.lines.iter() {
        for band in crate::reverb::network::Band::ALL {
            assert_eq!(l.absorption.gain(band).get().to_bits(), 1.0f32.to_bits());
        }
    }
    assert_eq!(design_of(&plain, &frozen), design_of(&plain, &base()));
    assert!(!design_of(&plain, &frozen).is_lossless());
}

/// Lengths are whole frames, grow with the size and scale with the rate, so
/// a settled network delays by whole frames and a frozen one reads no
/// fraction.
#[test]
fn lengths_are_whole_frames_that_grow_with_size_and_rate() {
    let d = FdnFamily::default();
    let mut last = vec![0.0f32; 8];
    for size in [0.0, 0.3, 0.6, 1.0] {
        let n = design_of(&d, &with(|c| c.size = Mix(size)));
        let lengths: Vec<f32> = fdn_of(&n).lines.iter().map(|l| l.length).collect();
        assert!(lengths.iter().all(|l| l.fract() == 0.0), "{lengths:?}");
        assert!(lengths.iter().zip(&last).all(|(a, b)| a > b));
        last = lengths;
    }
    let at = |hz| d.member(&base(), SampleRate::of(hz));
    let (a, b) = (at(44_100.0), at(88_200.0));
    for (x, y) in fdn_of(&a).lines.iter().zip(fdn_of(&b).lines.iter()) {
        assert!((y.length - 2.0 * x.length).abs() <= 1.0);
    }
}

/// The configuration refuses what the network cannot hold.
#[test]
fn a_configuration_refuses_what_it_cannot_hold() {
    let ok = Seconds(8.0);
    assert!(FdnFamily::new(8, ok, false).is_some());
    assert!(FdnFamily::new(16, ok, true).is_some());
    for lines in [0, 1, 7, 17, 18] {
        assert!(FdnFamily::new(lines, ok, false).is_none(), "{lines}");
    }
    for bad in [0.0, 0.01, 60.001, f32::NAN, f32::INFINITY] {
        assert!(
            FdnFamily::new(8, Seconds::from_f32(bad), false).is_none(),
            "{bad}"
        );
    }
}

/// At diffusion 0 the lines do not couple: each is its own comb with a
/// positive loop gain, so the response's sum (its value at DC) is
/// `wet * sum_j c_j b_j / (1 - g_j)`, with `g_j` the line's DC loop gain. A
/// sign on one line's loop would make its term `c b / (1 + g)`, a small
/// fraction of the figure at a long decay.
#[test]
fn a_diagonal_network_is_the_identity_a_comb_a_line_with_a_positive_sign() {
    use crate::Setup;
    let rate = SampleRate::SR_48K;
    let d = freezable();
    let mut k = d.realized();
    k.prepare(
        &d.reach(&Reach::at(rate)),
        &Setup {
            rate,
            max_block: tutti_types::MaxBlock::of(tutti_types::Samples(64)),
        },
    );
    let net = d.member(
        &with(|c| {
            c.decay = [Seconds(2.0); 3];
            c.diffusion = Mix(0.0);
            c.modulation = Depth::of(0.0);
            c.pre_delay = Seconds(0.0);
            c.width = StereoWidth(1.0);
        }),
        rate,
    );
    let Some(Stage::Fdn(f)) = net.stages.iter().find(|s| matches!(s, Stage::Fdn(_))) else {
        panic!("an FDN stage");
    };
    assert_eq!(f.matrix, Matrix::Diagonal);
    let expected: f64 = (0..f.lines.len())
        .map(|j| {
            let b = f64::from(f.input[j]);
            let c = f64::from(f.output.rows[0][j]);
            let g = f64::from(f.lines[j].absorption.gain(Band::Low).get());
            b * c / (1.0 - g)
        })
        .sum::<f64>()
        * f64::from(net.wet.to_amplitude().get());
    k.retarget(&net);
    let mut sum = 0.0f64;
    for block in 0..(10 * 48_000 / 64) {
        let mut x = [0.0f32; 64];
        if block == 0 {
            x[0] = 1.0;
        }
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        k.process(&crate::Feed::new(&[&x]), &mut [&mut l, &mut r], 64);
        sum += l.iter().map(|&v| f64::from(v)).sum::<f64>();
    }
    assert!(
        (sum - expected).abs() < 0.01 * expected.abs(),
        "the response sums to {sum}, the identity's comb bank to {expected}"
    );
}

/// A frozen tail keeps its energy through a diffusion ramp from 0 to 1: the
/// reflection is a unit vector at every step, not a straight line between
/// two (which dips to `|v|^2` of 0.68 and is a contraction). The energy the
/// lines hold (their whole state while frozen) before and after the 20 ms
/// ramp agrees to 0.01 dB. (The output's level would not do: it is the
/// lines' energy over their lengths, and mixing moves energy between lines of
/// different length.)
#[test]
fn a_diffusion_ramp_keeps_a_frozen_tails_energy() {
    use crate::Setup;
    let rate = SampleRate::SR_48K;
    let d = freezable();
    let mut k = d.realized();
    k.prepare(
        &d.reach(&Reach::at(rate)),
        &Setup {
            rate,
            max_block: tutti_types::MaxBlock::of(tutti_types::Samples(64)),
        },
    );
    let at = |diffusion: f32, freeze: bool| {
        d.member(
            &with(|c| {
                c.diffusion = Mix(diffusion);
                c.freeze = freeze;
                c.modulation = Depth::of(0.0);
            }),
            rate,
        )
    };
    let run = |k: &mut Realized<FdnRecurrence>, frames: usize, noise: bool| {
        let mut rng = StdRng::seed_from_u64(5);
        let x: Vec<f32> = (0..frames)
            .map(|_| {
                if noise {
                    rng.random_range(-0.5..0.5)
                } else {
                    0.0
                }
            })
            .collect();
        for chunk in x.chunks(64) {
            let (mut l, mut r) = (vec![0.0f32; chunk.len()], vec![0.0f32; chunk.len()]);
            k.process(
                &crate::Feed::new(&[chunk]),
                &mut [&mut l, &mut r],
                chunk.len(),
            );
        }
    };
    k.retarget(&at(0.0, false));
    run(&mut k, 48_000, true);
    k.retarget(&at(0.0, true));
    run(&mut k, 24_000, false);
    let before = k.state_energy();
    k.retarget(&at(1.0, true));
    run(&mut k, 9_600, false);
    let db = 10.0 * (k.state_energy() / before).log10();
    assert!(
        db.abs() < 0.01,
        "the frozen energy moved {db:.4} dB through the ramp"
    );
}

/// The bounds hold a design's tail at 192 kHz too, where the rounding of the
/// reflection vector (applied every frame by `rt60_bound`) is a larger share
/// of a frame's decay: the margin is computed from the rate and the longest
/// decay, not a flat figure.
#[test]
fn the_bounds_dominate_where_rounding_matters() {
    let d = FdnFamily::default();
    let rate = SampleRate::SR_192K;
    let reach = Reach::at(rate);
    let bound = d.reach(&reach);
    for k in 1..100 {
        let c = FdnControls {
            decay: [d.max_decay; 3],
            size: Mix(1.0),
            diffusion: Mix(k as f32 / 100.0),
            pre_delay: MAX_PRE_DELAY,
            modulation: Depth::of(1.0),
            ..FdnControls::DEFAULT
        };
        let tail = d.member(&c, rate).tail(reach.floor());
        assert!(
            bound.tail.covers(tail),
            "diffusion {k}%: {tail:?} over {:?}",
            bound.tail
        );
    }
}
