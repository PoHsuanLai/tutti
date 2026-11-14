use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tutti_types::{BusTopology, Db, MaxBlock, SampleRate, Samples};

use super::*;
use crate::contract::{FamilyCase, Hook, RecurrenceCase, noise};
use crate::{Realized, Setup};

fn design() -> BrickwallFamily {
    BrickwallFamily::new(BusTopology::STEREO)
}

fn controls(db: f32) -> BrickwallControls {
    BrickwallControls { ceiling: Db(db) }
}

impl FamilyCase for BrickwallFamily {
    fn capacity(&self, _: SampleRate) {}

    fn reachable(&self) -> Vec<BrickwallControls> {
        let mut all: Vec<_> = [-96.0, -60.0, -24.0, -6.0, -0.3, 0.0, 3.0, 12.0]
            .into_iter()
            .map(controls)
            .collect();
        let mut rng = StdRng::seed_from_u64(24);
        all.extend((0..64).map(|_| controls(rng.random_range(-80.0..20.0))));
        all
    }

    /// `+-inf` are ceilings and a NaN has none: all three are members.
    fn hostile(&self) -> Vec<BrickwallControls> {
        [f32::NAN, f32::INFINITY, f32::NEG_INFINITY]
            .into_iter()
            .map(controls)
            .collect()
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(
            Hook::Extremes,
            "memoryless: there is no loop to close on its limit",
        )]
    }
}

impl RecurrenceCase for BrickwallFamily {
    fn probes(&self) -> Vec<BrickwallControls> {
        [-18.0, -6.0, -0.3, 3.0].into_iter().map(controls).collect()
    }

    fn extremes(&self) -> Vec<BrickwallControls> {
        Vec::new()
    }

    /// Steps whose ceilings are all above the contract's 0.5-amplitude tone: a clamp
    /// has no glide, so a step that clips the tone is a jump by definition (the
    /// ceiling lands whole), and the `click_free` case measures the tone's own jumps.
    fn steps(&self) -> Vec<(BrickwallControls, BrickwallControls)> {
        vec![
            (controls(0.0), controls(6.0)),
            (controls(-3.0), controls(-0.3)),
            (controls(-5.0), controls(3.0)),
        ]
    }
}

crate::family_contract!(design());
// A block-anchored family (`Cadence::BLOCK`: it has no slow control to sample); the
// block cut changes nothing because the clip is memoryless, which
// `the_render_is_the_clamp_at_any_block` states from the definition.
crate::recurrence_contract!(design());

/// The ceiling of `db` from its definition, `10^(db/20)`.
fn amplitude_of(db: f32) -> f64 {
    10f64.powf(f64::from(db) / 20.0)
}

/// `y = clamp(x, -c, c)`, from the definition.
fn definition(x: f32, c: f32) -> f32 {
    if x > c {
        c
    } else if x < -c {
        -c
    } else {
        x
    }
}

/// The member's ceiling is the amplitude of the controls' dB (to `f32`), `+inf` dB is
/// unbounded, `-inf` dB is zero, and a NaN is the default, 0 dB.
#[test]
fn the_member_ceiling_is_the_amplitude_of_the_decibels() {
    let family = design();
    let rate = SampleRate::SR_48K;
    for db in [-96.0f32, -24.0, -6.0, -0.3, 0.0, 3.0, 12.0] {
        let c = f64::from(family.member(&controls(db), rate).ceiling());
        assert!(
            (c / amplitude_of(db) - 1.0).abs() < 1e-6,
            "{db} dB: {c} against {}",
            amplitude_of(db)
        );
    }
    let ceiling = |db: f32| family.member(&controls(db), rate).ceiling();
    assert_eq!(ceiling(f32::INFINITY), f32::INFINITY);
    assert_eq!(ceiling(f32::NEG_INFINITY), 0.0);
    assert_eq!(ceiling(f32::NAN), 1.0, "a NaN ceiling is the default, 0 dB");
}

/// The design diagram of the member: `y = clamp(x, -c, c)` over a grid and seeded
/// draws, and the four things that follow from it, each stated on its own: the
/// output never passes the ceiling (`|y| <= c`), the clip is odd (`f(-x) = -f(x)`),
/// idempotent (`f(f(x)) = f(x)`) and the identity inside the ceiling.
#[test]
fn the_member_is_the_clamp_and_its_four_consequences() {
    let family = design();
    let rate = SampleRate::SR_48K;
    let mut rng = StdRng::seed_from_u64(8);
    let mut xs: Vec<f32> = vec![0.0, -0.0, 1.0, -1.0, 0.5, 1e-30, 1e30, f32::MAX, f32::MIN];
    xs.extend((0..200).map(|_| rng.random_range(-4.0..4.0)));
    for c in family.reachable() {
        let m = family.member(&c, rate);
        let ceiling = m.ceiling();
        for &x in &xs {
            let y = m.apply(x);
            assert_eq!(y, definition(x, ceiling), "{c:?} at {x}");
            assert!(y.abs() <= ceiling, "{c:?} at {x}: {y} past the ceiling");
            assert_eq!(m.apply(-x), -y, "{c:?} is not odd at {x}");
            assert_eq!(m.apply(y), y, "{c:?} is not idempotent at {x}");
            if x.abs() <= ceiling {
                assert_eq!(y, x, "{c:?} changed {x}, inside the ceiling");
            }
        }
    }
}

/// `+inf` dB passes everything (a finite sample, an infinite one) and `-inf` mutes.
#[test]
fn infinite_ceilings_pass_everything_and_mute() {
    let family = design();
    let rate = SampleRate::SR_48K;
    let open = family.member(&controls(f32::INFINITY), rate);
    let shut = family.member(&controls(f32::NEG_INFINITY), rate);
    for x in [1e30f32, -3.0, 0.0, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(open.apply(x), x);
        assert_eq!(shut.apply(x).abs(), 0.0);
    }
}

/// A prepared runner at `controls`, rendering `x` on both channels, held or with
/// `lane` on lane 0, in blocks of `block`.
fn render_blocks(
    controls: BrickwallControls,
    x: &[f32],
    lane: Option<&[f32]>,
    block: usize,
) -> Vec<Vec<f32>> {
    let family = design();
    let rate = SampleRate::SR_48K;
    let mut k: Realized<BrickwallRecurrence> = family.realized(rate);
    k.prepare(
        &(),
        &Setup {
            rate,
            max_block: MaxBlock::of(Samples(512)),
        },
    );
    k.retarget(&family.member(&controls, rate));
    let mut out = vec![vec![0.0f32; x.len()]; 2];
    let mut start = 0;
    while start < x.len() {
        let end = (start + block).min(x.len());
        let ins = [&x[start..end]; 2];
        let mut feed = Feed::new(&ins);
        if let Some(lane) = lane {
            feed = feed.with_lane(0, &lane[start..end]);
        }
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        k.process(&feed, &mut outs, end - start);
        start = end;
    }
    out
}

/// The render is the clamp of the definition at the member's ceiling, whatever the
/// block cut (1, 17, 64, 512 frames) and on every channel.
#[test]
fn the_render_is_the_clamp_at_any_block() {
    let x = noise(5, 2048, 2.0);
    for db in [-18.0f32, -6.0, 0.0, 6.0] {
        let c = design().member(&controls(db), SampleRate::SR_48K).ceiling();
        for block in [1, 17, 64, 512] {
            for (ch, out) in render_blocks(controls(db), &x, None, block)
                .iter()
                .enumerate()
            {
                for (i, (&y, &x)) in out.iter().zip(&x).enumerate() {
                    assert_eq!(
                        y,
                        definition(x, c),
                        "{db} dB, block {block}, ch {ch}, frame {i}"
                    );
                }
            }
        }
    }
}

/// A fed ceiling is read **per frame**: frame `i` is clamped at the amplitude of
/// `lane[i]` dB, a NaN frame at the member's ceiling, and `+-inf` frames are
/// ceilings. Whatever the block cut.
#[test]
fn a_fed_ceiling_is_read_per_frame() {
    let x = noise(6, 1024, 2.0);
    let mut lane: Vec<f32> = (0..x.len()).map(|i| -((i % 40) as f32) * 0.75).collect();
    lane[3] = f32::NAN;
    lane[100] = f32::INFINITY;
    lane[101] = f32::NEG_INFINITY;
    let base = -6.0;
    let held = design()
        .member(&controls(base), SampleRate::SR_48K)
        .ceiling();
    for block in [1, 17, 64, 512] {
        for out in render_blocks(controls(base), &x, Some(&lane), block) {
            for (i, (&y, &x)) in out.iter().zip(&x).enumerate() {
                let want = if lane[i].is_nan() {
                    definition(x, held)
                } else {
                    definition(x, Db(lane[i]).to_amplitude().get())
                };
                assert_eq!(y, want, "block {block}, frame {i}, lane {}", lane[i]);
            }
        }
    }
}
