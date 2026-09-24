use super::error::Result;
use core::sync::atomic::Ordering;
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::RtScratch;
use tutti_core::{ArcDegrees, Azimuth, Elevation, SampleRate, Spread, StereoWidth};
use vbap::VBAPanner;

use crate::AngleSmoother;

/// Maximum number of speakers supported (Atmos 7.1.4).
pub(crate) const MAX_SPEAKERS: usize = 12;

/// How many times [`VbapPanner::solve_gains`] may retreat a source's elevation
/// toward the horizontal plane before giving up.
///
/// The retreat halves the distance to the plane each round, so the bound is a
/// number of halvings and not an angle. Measured over a 5°×5° grid of every
/// preset, the worst case is **two** rounds (Atmos, far below the array); the
/// 2D rings need at most one, and only exactly at the poles. Eight is therefore
/// slack rather than a budget — it exists so the loop is bounded on the audio
/// thread, not because any real bearing approaches it.
const MAX_ELEVATION_RETREATS: u32 = 8;

/// Below this, a gain vector's sum of squares is treated as zero and
/// renormalizing it would divide by ~0. Upstream `vbap` uses the same
/// threshold for the same decision.
const ENERGY_EPSILON: f32 = 1e-10;

/// VBAP panner internals. Use `VbapPannerNode` instead.
pub(crate) struct VbapPanner {
    panner: VBAPanner,
    azimuth_target: Arc<AtomicF32>,
    elevation_target: Arc<AtomicF32>,
    smoother: AngleSmoother,
    spread: Spread,
    /// Whether this layout has a speaker behind the listener (any `|azimuth|`
    /// past 90°), which decides whether [`solve_gains`](Self::solve_gains)
    /// folds a rear bearing into the front hemisphere.
    ///
    /// Derived from the speaker positions rather than the channel count: the
    /// property that matters is "can this array place a rear image", and only
    /// the geometry answers it. Of the five presets, stereo is the one that
    /// cannot.
    has_rear_speakers: bool,
    /// Pre-allocated scratch used by [`VBAPanner::compute_gains_into`].
    /// Sized to the layout's speaker count on construction; reused per solve
    /// so the RT path never allocates. One buffer serves both virtual sources
    /// of the stereo-width branch: each solve is copied out into an f32
    /// array before the next one starts.
    gains_scratch: RtScratch<f64>,
}

impl VbapPanner {
    fn new_with_layout(panner: VBAPanner) -> Self {
        let sample_rate = SampleRate(48000.0);
        let speaker_count = panner.num_speakers();
        let has_rear_speakers = panner.speakers().iter().any(|s| s.azimuth().abs() > 90.0);
        Self {
            panner,
            azimuth_target: Arc::new(AtomicF32::new(0.0)),
            elevation_target: Arc::new(AtomicF32::new(0.0)),
            smoother: AngleSmoother::new(sample_rate),
            spread: Spread::POINT,
            has_rear_speakers,
            gains_scratch: RtScratch::new(speaker_count),
        }
    }

    pub(crate) fn stereo() -> Result<Self> {
        let panner = VBAPanner::builder().stereo().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn quad() -> Result<Self> {
        let panner = VBAPanner::builder().quad().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn surround_5_1() -> Result<Self> {
        let panner = VBAPanner::builder().surround_5_1().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn surround_7_1() -> Result<Self> {
        let panner = VBAPanner::builder().surround_7_1().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn atmos_7_1_4() -> Result<Self> {
        let panner = VBAPanner::builder().atmos_7_1_4().build()?;
        Ok(Self::new_with_layout(panner))
    }

    /// Set position in degrees (smoothed over 50ms)
    ///
    /// Uses VBAP angle convention:
    /// - `azimuth`: Horizontal angle (-180 to 180, 0 = front, 90 = left, -90 = right)
    /// - `elevation`: Vertical angle (-90 to 90, 0 = ear level, positive = up)
    pub(crate) fn set_position(&mut self, azimuth: Azimuth, elevation: Elevation) {
        // Azimuth WRAPS, elevation SATURATES. These two lines used to be the
        // same `clamp`, which is right for a height and wrong for a bearing:
        // 190 degrees became 180 (hard left) when it is 170 to the right.
        //
        // The atomics stay raw `f32`: they are the lock-free control→RT
        // boundary, which a newtype cannot cross. Normalizing here means the
        // stored float is always already wrapped/clamped.
        self.azimuth_target
            .store(azimuth.wrap().get(), Ordering::Release);
        self.elevation_target.store(
            Elevation::new_clamped(elevation.get()).get(),
            Ordering::Release,
        );
    }

    /// Set spread factor (0.0 = point source, 1.0 = diffuse)
    pub(crate) fn set_spread(&mut self, spread: Spread) {
        self.spread = Spread::new_clamped(spread.get());
    }

    /// Retune the position smoothers so the 50ms de-zipper ramp holds at any
    /// sample rate (the smoothers are built at 48kHz in `new_with_layout`).
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.smoother.set_sample_rate(sample_rate);
    }

    /// Drop the in-flight de-zipper ramp, leaving the commanded position and
    /// spread untouched.
    ///
    /// The smoother is this panner's only per-block history; the two target
    /// atomics and `spread` are caller-set configuration, which is why nothing
    /// here writes them.
    pub(crate) fn reset_state(&mut self) {
        let azimuth = Azimuth(self.azimuth_target.load(Ordering::Acquire));
        let elevation = Elevation(self.elevation_target.load(Ordering::Acquire));
        self.smoother.reset_to_target(azimuth, elevation);
    }

    /// Fold a bearing into the front hemisphere by mirroring it about the
    /// lateral (left–right) axis: 135° becomes 45°, 180° becomes 0°.
    ///
    /// Only correct for an array with **no rear speaker**. A stereo pair has no
    /// front/back axis to resolve — both speakers are ahead of the listener, so
    /// there is no amplitude difference that could place a source behind. The
    /// nearest honest rendering of "behind me and to the left" on such an array
    /// is "to the left", which is what the mirror produces, and it is what the
    /// image does anyway once the listener turns their head.
    ///
    /// For an array that *does* surround the listener the mirror would be a
    /// bug, not a fallback: a source at 135° has a real rear-left speaker to
    /// come out of, and folding it to 45° would move it to the front. Hence the
    /// [`has_rear_speakers`](Self::has_rear_speakers) gate at the call site.
    #[inline]
    fn fold_to_front(azimuth: Azimuth) -> Azimuth {
        let deg = azimuth.wrap().get();
        if deg.abs() > 90.0 {
            // `Azimuth` deliberately omits arithmetic operators; the mirror is
            // a reflection rather than a rotation, so it is computed on the
            // scalar and rebuilt. `wrap` above guarantees `-180..180`, so the
            // result lands in `-90..90` and needs no second wrap.
            Azimuth(deg.signum() * (180.0 - deg.abs()))
        } else {
            azimuth.wrap()
        }
    }

    /// The crate's own gain solve: upstream `vbap`, corrected.
    ///
    /// Writes unit-energy gains into `scratch[..num_speakers]` — the sum of
    /// squares is 1.0 for **every** bearing and height, on every supported
    /// layout. That is the energy law this crate guarantees, and it is not the
    /// one upstream provides.
    ///
    /// # Why the correction is needed
    ///
    /// `VBAPanner::compute_gains_into` normalizes the tuple solution and *then*
    /// clamps negative gains to zero (`(gain * norm).max(0.0)`,
    /// `vbap-0.1.1/src/panner.rs:144`). For a source inside the winning tuple
    /// no gain is negative and the order does not matter. For a source outside
    /// every tuple the clamp fires **after** the energy has been budgeted
    /// across gains that are then discarded, so the source quietly loses that
    /// energy — and where every gain is negative it is discarded entirely and
    /// the source is silent.
    ///
    /// Measured before this correction: a stereo pair held unity to 30°, then
    /// decayed (0.933 at 45°, 0.500 at 90°, 0.067 at 135°) and was **completely
    /// silent from 150° through 210°**. Atmos 7.1.4 leaked energy in the
    /// horizontal plane too — 0.800 dead ahead, 0.661 at 120°.
    ///
    /// # The three steps, and why each is load-bearing
    ///
    /// 1. **Fold**, for a front-only array. See
    ///    [`fold_to_front`](Self::fold_to_front). Folding *alone* is not enough:
    ///    it leaves 90° at half energy, because 90° is already in the front
    ///    hemisphere and is outside the ±30° pair regardless.
    /// 2. **Retreat in elevation**, when the array has no geometry at all for
    ///    the requested height. Upstream's clamp has by then reduced the gain
    ///    vector to all zeros, so there is nothing left to renormalize — the
    ///    sign information the clamp destroyed cannot be recovered from
    ///    outside. Halving the elevation toward the horizontal plane and
    ///    re-solving finds the nearest height the array can actually render.
    ///    This is what covers the region below an Atmos bed (no speaker sits
    ///    under the listener) and the exact poles of a 2D ring (where the
    ///    horizontal projection of the direction vector is the zero vector).
    /// 3. **Renormalize**. This is clamp-then-normalize completed: upstream
    ///    already clamped, so rescaling what survived to unit energy is the
    ///    normalization it should have done second. Renormalizing *alone* is
    ///    not enough either: it leaves 150°–210° silent on stereo, because
    ///    there both gains were negative and zero times any scale is zero.
    ///
    /// The audible consequence of the completed law is that a source outside
    /// the array's coverage **hard-pans at full energy** to the nearest
    /// speaker(s) instead of fading out. A front-only pair genuinely cannot
    /// place a rear image, so some collapse is unavoidable; collapsing to the
    /// nearest speaker is the choice that keeps a panning automation audible
    /// all the way round, which fading to silence does not.
    /// Takes `panner` and `has_rear_speakers` explicitly rather than `&self`:
    /// every caller passes a `scratch` borrowed mutably out of the same struct,
    /// which a `&self` receiver would not co-exist with.
    fn solve_gains(
        panner: &VBAPanner,
        has_rear_speakers: bool,
        azimuth: Azimuth,
        elevation: Elevation,
        scratch: &mut [f64],
    ) {
        let azimuth = if has_rear_speakers {
            azimuth.wrap()
        } else {
            Self::fold_to_front(azimuth)
        };

        // The types stop at the trigonometry: vbap takes bare f64 degrees.
        let azimuth_deg = azimuth.get() as f64;
        let mut elevation_deg = Elevation::new_clamped(elevation.get()).get() as f64;

        for _ in 0..MAX_ELEVATION_RETREATS {
            panner.compute_gains_into(azimuth_deg, elevation_deg, scratch);

            let sum_sq: f64 = scratch.iter().map(|g| g * g).sum();
            if sum_sq > ENERGY_EPSILON as f64 {
                let norm = 1.0 / sum_sq.sqrt();
                for gain in scratch.iter_mut() {
                    *gain *= norm;
                }
                return;
            }

            // Nothing survived the clamp at this height. Halve the distance to
            // the horizontal plane, which every supported layout covers, and
            // try again. Snapping the last sliver to exactly 0 rather than
            // halving forever is what keeps the bound at two rounds in practice.
            elevation_deg *= 0.5;
            if elevation_deg.abs() < 1e-4 {
                elevation_deg = 0.0;
            }
        }

        // Unreachable for the five presets: the horizontal plane is covered by
        // all of them, so the retreat terminates well inside the bound. Left as
        // silence rather than a panic because this is the audio thread, and a
        // layout added later that somehow has no horizontal coverage should
        // produce a quiet node rather than take the process down.
        debug_assert!(
            false,
            "VBAP gain solve found no energy after {MAX_ELEVATION_RETREATS} elevation retreats"
        );
        scratch.fill(0.0);
    }

    /// Apply spread to `gains[..count]` in place, holding the sum of squares at
    /// 1.0 — the same energy law [`solve_gains`](Self::solve_gains) establishes,
    /// preserved across the diffusion blend rather than newly imposed by it.
    #[inline]
    fn apply_spread(&self, gains: &mut [f32], count: usize) {
        // Unwrapped once here: the blend below is interpolation arithmetic on
        // the scalar, which `Spread` deliberately does not define operators for.
        let spread = self.spread.get();
        if spread <= 0.0 {
            return;
        }
        let equal_gain = 1.0 / (count as f32).sqrt();
        for gain in &mut gains[..count] {
            *gain = *gain * (1.0 - spread) + equal_gain * spread;
        }
        let sum_sq: f32 = gains[..count].iter().map(|g| g * g).sum();
        if sum_sq > 0.0 {
            let norm = 1.0 / sum_sq.sqrt();
            for gain in &mut gains[..count] {
                *gain *= norm;
            }
        }
    }

    /// Unit-energy gains for one source at `(azimuth, elevation)`, spread
    /// applied. Entries past the layout's speaker count are zero.
    fn source_gains(&mut self, azimuth: Azimuth, elevation: Elevation) -> [f32; MAX_SPEAKERS] {
        // RT invariant: `solve_gains` must reach `compute_gains_into`, not
        // `compute_gains`. The latter allocates a fresh `Vec<f64>` per call
        // (and is `#[deprecated]` in vbap 0.1.2). Backstop:
        // `tutti-spatial/tests/rt_no_alloc.rs::vbap_panner_stereo_process_is_allocation_free`.
        let speaker_count = self.gains_scratch.capacity();
        // `solve_gains` takes its two inputs explicitly rather than `&self`:
        // the scratch is borrowed mutably out of this same struct, so a
        // `&self` receiver would not co-exist with it.
        let (panner, has_rear) = (&self.panner, self.has_rear_speakers);
        let scratch = self.gains_scratch.active(speaker_count);
        Self::solve_gains(panner, has_rear, azimuth, elevation, scratch);

        let count = speaker_count.min(MAX_SPEAKERS);
        let mut gains = [0.0f32; MAX_SPEAKERS];
        for (i, &g) in scratch.iter().enumerate().take(count) {
            gains[i] = g as f32;
        }
        self.apply_spread(&mut gains, count);
        gains
    }

    /// The gains for one frame at the smoothed position `(azimuth, elevation)`.
    ///
    /// `width < 0.001` folds the pair to mono and pans one source; otherwise
    /// the two channels become two virtual sources one `width`-scaled offset
    /// either side of the bearing.
    ///
    /// # Spread applies to both virtual sources, and used not to
    ///
    /// The `width > 0` branch once called the upstream solver directly and
    /// never reached [`apply_spread`](Self::apply_spread), so a spread set on a
    /// node fed any non-zero width silently did nothing — a parameter the
    /// inspector shows, the document saves and the engine ignores. `width` and
    /// `spread` are orthogonal: width says how far apart the two virtual
    /// sources sit, spread how far each one is smeared across the speaker
    /// field, so neither may suppress the other. Both go through
    /// [`source_gains`](Self::source_gains), which also gives each virtual
    /// source unit energy on its own — without that, the width branch would
    /// reintroduce exactly the rear-arc silence the mono branch no longer has.
    /// Covered by `tests/vbap_energy_sweep.rs::spread_reaches_the_stereo_width_path`.
    fn frame_gains(
        &mut self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
    ) -> BlockGains {
        // Unwrapped once: the angle offset below scales the scalar.
        let width = width.get().max(0.0);
        if width < 0.001 {
            return BlockGains {
                mono: true,
                a: self.source_gains(azimuth, elevation),
                b: [0.0; MAX_SPEAKERS],
            };
        }
        // The two virtual sources sit one offset either side of the bearing.
        // `rotate_by`, not `+`: the sum crosses the seam. At azimuth 170 with
        // full width the left source is at 185, which *is* -175 — the same
        // wraparound `set_position` normalizes on store, re-introduced here by
        // adding to the already-wrapped value.
        let angle_offset = ArcDegrees(15.0 * width);
        BlockGains {
            mono: false,
            a: self.source_gains(azimuth.rotate_by(angle_offset), elevation),
            b: self.source_gains(azimuth.rotate_by(-angle_offset), elevation),
        }
    }

    /// The gains at the smoother's **current** position, without advancing
    /// it — the starting point of the first ramp after a construction, clone
    /// or reset.
    pub(crate) fn gains_now(&mut self, width: StereoWidth) -> BlockGains {
        let (azimuth, elevation) = self.smoother.current();
        self.frame_gains(azimuth, elevation, width)
    }

    /// Advance the de-zipper `frames` steps and solve the gains **once**, at
    /// the block's last frame.
    ///
    /// The smoother still steps once per frame, exactly as when the gains were
    /// solved per frame, so the 50 ms ramp keeps its timing and the returned
    /// set is bit-for-bit the one the per-frame solver produced at that frame.
    /// What changed is only that the frames in between are no longer solved:
    /// the node ramps linearly towards this set instead (design doc 013,
    /// "`VbapPannerNode` `process`" — two VBAP solves per frame was the
    /// largest per-node waste it found).
    ///
    /// The target atomics are read once here. The per-frame path read them
    /// every frame, but they are written once per block (by the node's
    /// `sync_position`), so every frame saw the same value.
    pub(crate) fn solve_block(&mut self, width: StereoWidth, frames: usize) -> BlockGains {
        let target_azimuth = Azimuth(self.azimuth_target.load(Ordering::Acquire));
        let target_elevation = Elevation(self.elevation_target.load(Ordering::Acquire));
        let mut smoothed = self.smoother.current();
        for _ in 0..frames {
            smoothed = self.smoother.step(target_azimuth, target_elevation);
        }
        self.frame_gains(smoothed.0, smoothed.1, width)
    }
}

/// One frame's speaker gains, as the start or end point of a block's ramp.
///
/// `mono` records which branch produced them: a mono set is one gain vector
/// (`a`) applied to the folded pair, a stereo set is two — `a` for the left
/// virtual source, `b` for the right. Entries past the speaker count are zero.
#[derive(Clone, Copy)]
pub(crate) struct BlockGains {
    pub(crate) mono: bool,
    pub(crate) a: [f32; MAX_SPEAKERS],
    pub(crate) b: [f32; MAX_SPEAKERS],
}

impl BlockGains {
    /// Speaker `s`'s gains as a (left, right) pair, whatever the branch.
    ///
    /// A mono set weighs each channel by half its gain — `fold(l, r) * g` is
    /// `l * g/2 + r * g/2` up to rounding — which is what lets a ramp cross
    /// from one branch to the other without a step.
    #[inline]
    pub(crate) fn pair(&self, s: usize) -> (f32, f32) {
        if self.mono {
            let half = self.a[s] * 0.5;
            (half, half)
        } else {
            (self.a[s], self.b[s])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset the stereo path applies to each virtual source, extracted so
    /// the seam behaviour is testable without a speaker layout.
    fn virtual_sources(bearing: Azimuth, width: f32) -> (Azimuth, Azimuth) {
        let offset = ArcDegrees(15.0 * width);
        (bearing.rotate_by(offset), bearing.rotate_by(-offset))
    }

    #[test]
    fn the_stereo_spread_wraps_instead_of_leaving_the_canonical_range() {
        // Near the rear seam at full width, one source crosses 180. It used to
        // be computed as a raw `f32` sum and handed to vbap as 185 degrees —
        // outside the -180..180 range every other path maintains.
        let (a, b) = virtual_sources(Azimuth(170.0), 1.0);
        assert_eq!(a, Azimuth(-175.0));
        assert_eq!(b, Azimuth(155.0));

        // The mirrored case, crossing the other way.
        let (a, b) = virtual_sources(Azimuth(-170.0), 1.0);
        assert_eq!(a, Azimuth(-155.0));
        assert_eq!(b, Azimuth(175.0));

        // Both stay in range for every bearing and width, which is the
        // property the raw-f32 form could not offer.
        for deg in -180..=180 {
            for w in [0.0, 0.25, 0.5, 1.0] {
                let (a, b) = virtual_sources(Azimuth(deg as f32), w);
                assert!(a.get() >= -180.0 && a.get() <= 180.0, "a={a:?}");
                assert!(b.get() >= -180.0 && b.get() <= 180.0, "b={b:?}");
            }
        }
    }

    #[test]
    fn the_spread_is_symmetric_about_the_bearing_away_from_the_seam() {
        // Front and centre: no wrapping involved, so the two sources should
        // straddle the bearing by exactly the offset. This is what the old
        // arithmetic got right, and it must keep working.
        let (a, b) = virtual_sources(Azimuth::FRONT, 1.0);
        assert_eq!(a, Azimuth(15.0));
        assert_eq!(b, Azimuth(-15.0));

        // Width scales the offset; zero width collapses both onto the bearing.
        let (a, b) = virtual_sources(Azimuth(30.0), 0.0);
        assert_eq!(a, Azimuth(30.0));
        assert_eq!(b, Azimuth(30.0));
    }
}
