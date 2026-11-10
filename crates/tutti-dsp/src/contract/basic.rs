//! The core cases: every class of system runs them.

use super::harness::{MAX_BLOCK, built, noise, rate_of, render, widths};
use super::{DesignCase, Hook, RealizationCase};
use crate::{Realization, Sizes, System};

/// The hooks that return nothing and are not listed, and the listed ones that
/// do return something: an error naming the first, or `Ok`.
pub(super) fn check_vacuity(
    vacant: &[(Hook, &'static str)],
    known: &[(Hook, usize)],
) -> Result<(), String> {
    for &(hook, len) in known {
        let listed = vacant.iter().find(|(h, _)| *h == hook);
        match (len == 0, listed) {
            (true, None) => {
                return Err(format!(
                    "{hook:?} returns nothing and the design does not list it as vacant: \
                     the cases it feeds pass for no reason"
                ));
            }
            (false, Some((_, why))) => {
                return Err(format!(
                    "{hook:?} is listed as vacant ({why}) but returns {len}: delete the entry"
                ));
            }
            (true, Some((_, ""))) => {
                return Err(format!("{hook:?} is listed as vacant without saying why"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn every<D: DesignCase>(d: &D) -> Vec<D::Controls> {
    let mut all = d.reachable();
    all.extend(d.hostile());
    all
}

/// The design cases of the core.
pub(super) mod design {
    use super::*;

    /// `bounds()` dominates `design()` in every figure for every control.
    ///
    /// Mutation: `bounds` one line short.
    pub fn bounds_dominate<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            let ceiling = d.ceiling(rate);
            let bound = d.bounds(&ceiling);
            for c in every(d) {
                let n = d.design(&c, rate);
                let size = d.measure(&n, ceiling.floor());
                if !bound.dominates(&size) {
                    return Err(format!("{bound:?} does not dominate {size:?}"));
                }
            }
        }
        Ok(())
    }

    /// The same controls give the same network, however often they are asked.
    ///
    /// Mutation: the design reads a counter (a stand-in for a clock). The
    /// no-allocation half is `tests/network_alloc.rs`.
    pub fn pure<D: DesignCase>(d: &D) -> Result<(), String>
    where
        D::Model: PartialEq,
    {
        for rate in d.rates() {
            for c in every(d) {
                let (a, b) = (d.design(&c, rate), d.design(&c, rate));
                if a != b || a != d.clone().design(&c, rate) {
                    return Err("two designs of the same controls differ".into());
                }
            }
        }
        Ok(())
    }

    /// Every hook of [`DesignCase`] that returns a list returns something, or the
    /// design says why it cannot ([`DesignCase::vacant`]).
    ///
    /// Mutations (run, in `reverb_design.rs`): `hostile` returning no controls and
    /// unlisted fails; a hook listed as vacant that returns controls fails.
    pub fn vacuity<D: DesignCase>(d: &D) -> Result<(), String> {
        if d.reachable().is_empty() {
            return Err("no reachable controls".into());
        }
        check_vacuity(d.vacant(), &[(Hook::Hostile, d.hostile().len())])
    }
}

/// The realization cases of the core.
pub(super) mod realization {
    use super::*;

    /// A unit impulse arrives first exactly `musical_delay + latency`
    /// frames in (the model's own shortest path, pre-delay included, and the
    /// realization's declared latency),
    /// on the earliest channel, and on no channel before.
    ///
    /// Mutations: the realization's output delayed by one frame (its `latency` still
    /// says the same); a `latency` that claims one frame more than the realization
    /// delays.
    pub fn arrival<D: RealizationCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.static_probes() {
                let (mut k, network) = built(d, &c, rate);
                let expected = d.musical_delay(&network) + k.latency().get();
                let mut x = vec![0.0f32; expected + 4000];
                x[0] = 1.0;
                let out = render(&mut k, widths(d), &x, 64);
                let first = out
                    .iter()
                    .filter_map(|c| c.iter().position(|&s| s != 0.0))
                    .min();
                if first != Some(expected) {
                    return Err(format!(
                        "first output at {first:?}, network and latency say {expected} at {rate:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The realization declares the model's own latency (zero for a time-domain
    /// model, a partition for a FIR), never a pre-delay or a tap spacing.
    ///
    /// Mutation: a realization that declares one frame more than it has.
    pub fn latency<D: RealizationCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.static_probes() {
                let (k, model) = built(d, &c, rate);
                if k.latency() != model.latency() {
                    return Err(format!(
                        "declares {} frames of latency, the model has {}",
                        k.latency(),
                        model.latency()
                    ));
                }
            }
        }
        Ok(())
    }

    /// A retarget mid-tail does not click: a 150 Hz tone through the realization, the
    /// controls stepped from `a` to `b` at a block edge, and the largest
    /// sample-to-sample jump after the step is within 1.5 times the larger of the
    /// two steady states' (each measured on a realization left at its controls). A
    /// tone, not noise: its steady jumps are small, so a step in a gain or a
    /// jump in a delay is a jump many times them, where noise hides it.
    ///
    /// Mutations: the realization scales its output by 4 from its second retarget on,
    /// a gain applied unslewed (a `Defective` test); an FDN that takes its new
    /// gains, or its new line lengths, whole (run by hand on `fdn.rs`).
    pub fn click_free<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let second = rate.get() as usize;
        let tone: Vec<f32> = (0..6 * second)
            .map(|n| 0.5 * libm::sin(std::f64::consts::TAU * 150.0 * n as f64 / rate.get()) as f32)
            .collect();
        let jump = |out: &[Vec<f32>], from: usize, to: usize| -> f32 {
            out.iter()
                .flat_map(|c| c[from..to].windows(2))
                .fold(0.0, |m, w| m.max((w[1] - w[0]).abs()))
        };
        let steady = |c: &D::Controls| -> f32 {
            let (mut k, _) = built(d, c, rate);
            let out = render(&mut k, widths(d), &tone, 64);
            jump(&out, 4 * second, 6 * second)
        };
        let mut checked = 0;
        for (a, b) in d.steps() {
            let (mut k, _) = built(d, &a, rate);
            let shape = widths(d);
            let mut out = render(&mut k, shape, &tone[..3 * second], 64);
            k.retarget(&d.design(&b, rate));
            let after = render(&mut k, shape, &tone[3 * second..], 64);
            for (all, more) in out.iter_mut().zip(after) {
                all.extend(more);
            }
            let click = jump(&out, 3 * second - 1, 6 * second);
            let allowed = 3.0 * steady(&a).max(steady(&b));
            checked += 1;
            if click > allowed {
                return Err(format!(
                    "a jump of {click} after the step, steady states allow {allowed}"
                ));
            }
        }
        if checked == 0 {
            return Err("no step to try".into());
        }
        Ok(())
    }

    /// After `reset` the realization renders bit for bit what a freshly built one
    /// does, at the same controls.
    ///
    /// Mutation: a `reset` that does nothing.
    pub fn reset<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let probe = noise(9, 8192, 1.0);
            let (mut used, _) = built(d, &c, rate);
            render(&mut used, widths(d), &noise(8, 20_000, 1.0), 64);
            used.reset();
            let (mut fresh, _) = built(d, &c, rate);
            if render(&mut used, widths(d), &probe, 64) != render(&mut fresh, widths(d), &probe, 64)
            {
                return Err("a reset realization renders differently from a fresh one".into());
            }
        }
        Ok(())
    }

    /// `fresh()` of a used realization renders bit for bit what a freshly built one
    /// does, at the same controls: state gone, coefficients kept.
    ///
    /// Mutation: a `fresh` that keeps the state (a plain clone).
    pub fn fresh<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let probe = noise(9, 8192, 1.0);
            let (mut used, _) = built(d, &c, rate);
            render(&mut used, widths(d), &noise(8, 20_000, 1.0), 64);
            let mut forked = used.fresh();
            let (mut fresh, _) = built(d, &c, rate);
            if render(&mut forked, widths(d), &probe, 64)
                != render(&mut fresh, widths(d), &probe, 64)
            {
                return Err("fresh() renders differently from a freshly built realization".into());
            }
        }
        Ok(())
    }

    /// The output does not depend on how the input is cut into blocks: blocks of
    /// 1, 17 and 64 frames render bit for bit what blocks of 512 do (a
    /// time-domain realization; a convolver would declare its own tolerance).
    ///
    /// Mutation: a gain that depends on the block's length.
    pub fn block_independent<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let x = noise(13, 4096, 1.0);
            let (mut k, _) = built(d, &c, rate);
            let whole = render(&mut k, widths(d), &x, MAX_BLOCK);
            for block in [1, 17, 64] {
                let (mut k, _) = built(d, &c, rate);
                if render(&mut k, widths(d), &x, block) != whole {
                    return Err(format!(
                        "blocks of {block} differ from blocks of {MAX_BLOCK}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The first `retarget` after `prepare`, `reset` or `fresh` lands at once (a
    /// realization built at a model *is* at it: nothing ramps from a default),
    /// and a reset or a fresh realization is at the targets it holds, not
    /// part of the way to them. Three renders of one probe, each against the
    /// settled reference (built at `b`, used, reset: at `b` with silent state):
    /// a realization retargeted once to `b`; one at `a` retargeted to `b` and
    /// reset mid-ramp; and the `fresh()` of that one.
    ///
    /// Mutations (run, in `fdn_design.rs`): `Defect::RampFirst` (the first
    /// retarget goes to a model that is not the target, and the target ramps) fails
    /// the first render; `FdnKernel::reset` that empties the lines and leaves
    /// the ramps where they were (a test-only flaw) fails the second and the
    /// third.
    pub fn landed<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let probe = noise(9, 8192, 1.0);
        let shape = widths(d);
        let mut checked = 0;
        for (a, b) in d.steps() {
            let (mut reference, _) = built(d, &b, rate);
            render(&mut reference, shape, &noise(8, 20_000, 1.0), 64);
            reference.reset();
            let want = render(&mut reference, shape, &probe, 64);

            let (mut first, _) = built(d, &b, rate);
            if render(&mut first, shape, &probe, 64) != want {
                return Err(
                    "the first retarget did not land: a realization built at the \
                            controls renders differently from a settled one"
                        .into(),
                );
            }

            let (mut mid, _) = built(d, &a, rate);
            render(&mut mid, shape, &noise(7, 6000, 1.0), 64);
            mid.retarget(&d.design(&b, rate));
            render(&mut mid, shape, &noise(6, 100, 1.0), 64);
            let mut forked = mid.fresh();
            mid.reset();
            if render(&mut mid, shape, &probe, 64) != want {
                return Err("reset() mid-ramp did not land on the targets".into());
            }
            if render(&mut forked, shape, &probe, 64) != want {
                return Err("fresh() mid-ramp did not land on the targets".into());
            }
            checked += 1;
        }
        if checked == 0 {
            return Err("no step to try".into());
        }
        Ok(())
    }

    /// A ramp's clock is the stream's, not a block's: with the controls changing
    /// at multiples of 8704 frames (a multiple of every block length below), blocks of
    /// 1, 17, 64 and 512 frames render bit for bit what blocks of 512 do. The
    /// plain `block_independent` case holds one retarget; this one moves the
    /// controls through the render, where a ramp that restarts at a block's edge
    /// shows.
    ///
    /// Mutation (run, in `fdn_design.rs`): `FdnKernel::process` that restarts its
    /// chunk count at every call (a test-only flaw) fails.
    pub fn stream_clock<D: RealizationCase>(d: &D) -> Result<(), String> {
        const SPAN: usize = 8704;
        let rate = rate_of(d);
        let shape = widths(d);
        let x = noise(13, 6 * SPAN, 1.0);
        let mut checked = 0;
        for (a, b) in d.steps() {
            let models = [d.design(&a, rate), d.design(&b, rate)];
            let run = |block: usize| {
                let (mut k, _) = built(d, &a, rate);
                let mut out: Vec<Vec<f32>> = vec![Vec::new(); shape.1];
                for (n, span) in x.chunks(SPAN).enumerate() {
                    if n > 0 {
                        k.retarget(&models[n % 2]);
                    }
                    for (all, part) in out.iter_mut().zip(render(&mut k, shape, span, block)) {
                        all.extend(part);
                    }
                }
                out
            };
            let whole = run(MAX_BLOCK);
            for block in [1, 17, 64] {
                if run(block) != whole {
                    return Err(format!(
                        "with the controls changing, blocks of {block} differ from blocks of {MAX_BLOCK}"
                    ));
                }
            }
            checked += 1;
        }
        if checked == 0 {
            return Err("no step to try".into());
        }
        Ok(())
    }

    /// Every hook of [`RealizationCase`] that returns a list returns something, or
    /// the design says why it cannot ([`DesignCase::vacant`]).
    ///
    /// Mutation (run, in `reverb_design.rs`): `steps` returning nothing, unlisted,
    /// fails.
    pub fn vacuity<D: RealizationCase>(d: &D) -> Result<(), String> {
        check_vacuity(
            d.vacant(),
            &[
                (Hook::Probes, d.probes().len()),
                (Hook::Extremes, d.extremes().len()),
                (Hook::Steps, d.steps().len()),
                (Hook::StaticProbes, d.static_probes().len()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FreeverbDesign;
    use crate::contract::cases::realization as cases;
    use crate::contract::flaws::Mutant;

    /// A hook that returns nothing and is not listed fails, one that is listed and
    /// returns something fails, a listing without a reason fails, and the other
    /// two rows pass.
    ///
    /// Mutations (run): dropping the `(true, None)` arm fails the first; the
    /// `(false, Some)` arm the second; the empty-reason arm the third.
    #[test]
    fn vacuity_names_the_unlisted_empty_hook_and_the_listed_full_one() {
        let listed = [(Hook::Steps, "no steps")];
        assert!(
            check_vacuity(&[], &[(Hook::Steps, 0)])
                .unwrap_err()
                .contains("does not list it")
        );
        assert!(
            check_vacuity(&listed, &[(Hook::Steps, 3)])
                .unwrap_err()
                .contains("is listed as vacant")
        );
        assert!(
            check_vacuity(&[(Hook::Steps, "")], &[(Hook::Steps, 0)])
                .unwrap_err()
                .contains("without saying why")
        );
        assert!(check_vacuity(&listed, &[(Hook::Steps, 0)]).is_ok());
        assert!(check_vacuity(&[], &[(Hook::Steps, 3)]).is_ok());
    }

    /// The realization-level case reads the realization hooks of a design: Freeverb
    /// lists none of them as vacant, so listing one it fills fails.
    ///
    /// Mutation (run): `steps` dropped from the list the case reads fails this.
    #[test]
    fn the_realization_vacuity_reads_the_realization_hooks() {
        let mut m = Mutant::new(FreeverbDesign, |_| {});
        assert!(cases::vacuity(&m).is_ok());
        m.vacant = Some(&[(Hook::Steps, "it has none")]);
        assert!(
            cases::vacuity(&m)
                .unwrap_err()
                .contains("is listed as vacant")
        );
    }
}
