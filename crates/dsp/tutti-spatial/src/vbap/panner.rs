use super::error::Result;
use core::sync::atomic::Ordering;
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::RtScratch;
use tutti_core::{
    fold_frame_to_mono, ArcDegrees, Azimuth, Elevation, SampleRate, Spread, StereoWidth,
};
use vbap::VBAPanner;

use crate::AngleSmoother;

/// Maximum number of speakers supported (Atmos 7.1.4).
const MAX_SPEAKERS: usize = 12;

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
    /// Sized to the layout's speaker count on construction; reused per
    /// sample so the RT path never allocates.
    gains_scratch_a: RtScratch<f64>,
    /// Second scratch buffer for the stereo-width branch, which needs
    /// two gain sets (one per virtual source).
    gains_scratch_b: RtScratch<f64>,
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
            gains_scratch_a: RtScratch::new(speaker_count),
            gains_scratch_b: RtScratch::new(speaker_count),
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

    pub(crate) fn compute_gains(&mut self) -> (usize, [f32; MAX_SPEAKERS]) {
        let target_azimuth = self.azimuth_target.load(Ordering::Acquire);
        let target_elevation = self.elevation_target.load(Ordering::Acquire);

        let (smoothed_azimuth, smoothed_elevation) = self
            .smoother
            .step(Azimuth(target_azimuth), Elevation(target_elevation));

        // RT invariant: `solve_gains` must reach `compute_gains_into`, not
        // `compute_gains`. The latter allocates a fresh `Vec<f64>` per call
        // (and is `#[deprecated]` in vbap 0.1.2). Backstop:
        // `tutti-spatial/tests/rt_no_alloc.rs::vbap_panner_stereo_process_is_allocation_free`.
        let speaker_count = self.gains_scratch_a.capacity();
        // `solve_gains` takes its two inputs explicitly rather than `&self`:
        // the scratch is borrowed mutably out of this same struct, so a
        // `&self` receiver would not co-exist with it.
        let (panner, has_rear) = (&self.panner, self.has_rear_speakers);
        let scratch = self.gains_scratch_a.active(speaker_count);
        Self::solve_gains(
            panner,
            has_rear,
            smoothed_azimuth,
            smoothed_elevation,
            scratch,
        );

        let count = speaker_count.min(MAX_SPEAKERS);
        let mut gains = [0.0f32; MAX_SPEAKERS];
        for (i, &g) in scratch.iter().enumerate().take(count) {
            gains[i] = g as f32;
        }

        self.apply_spread(&mut gains, count);
        (count, gains)
    }

    pub(crate) fn process_mono_into(&mut self, sample: f32, output: &mut [f32]) {
        let (count, gains) = self.compute_gains();
        for (out, &gain) in output.iter_mut().zip(&gains[..count]) {
            *out = sample * gain;
        }
    }

    /// Pan a stereo frame, spreading the two channels into two virtual sources
    /// one `width`-scaled offset either side of the bearing.
    ///
    /// # Spread applies here, and used not to
    ///
    /// The `width > 0` branch called the upstream solver directly and never
    /// reached [`apply_spread`](Self::apply_spread), so a spread set on a node
    /// fed any non-zero width silently did nothing — a parameter the inspector
    /// shows, the document saves and the engine ignores, with no error anywhere.
    /// It is applied now, once per virtual source.
    ///
    /// That is the correct reading of the two controls rather than a
    /// convenience: `width` and `spread` are orthogonal. Width says how far
    /// apart the two virtual sources sit; spread says how far each one is
    /// smeared across the speaker field. Neither is a special case of the other,
    /// so neither may suppress the other — which is what the old code did, in
    /// one direction only. Covered by
    /// `tests/vbap_energy_sweep.rs::spread_reaches_the_stereo_width_path`.
    ///
    /// The `width < 0.001` branch folds to mono and delegates to
    /// [`process_mono_into`](Self::process_mono_into), which reaches
    /// `apply_spread` the ordinary way; that half was always correct.
    pub(crate) fn process_stereo_into(
        &mut self,
        left: f32,
        right: f32,
        width: StereoWidth,
        output: &mut [f32],
    ) {
        // `left`/`right` stay bare — they are audio samples, not measurements.
        // Unwrapped once: the angle offset below scales the scalar.
        let width = width.get().max(0.0);

        if width < 0.001 {
            // The engine's one fold rather than a local `* 0.5`: identical at
            // width 2 (`fold_frame_to_mono`'s stereo arm IS the average), but it
            // is the same constant this crate's other folds use, in one place.
            let mono = fold_frame_to_mono(&[left, right]);
            self.process_mono_into(mono, output);
            return;
        }

        let target_azimuth = self.azimuth_target.load(Ordering::Acquire);
        let target_elevation = self.elevation_target.load(Ordering::Acquire);

        let (smoothed_azimuth, smoothed_elevation) = self
            .smoother
            .step(Azimuth(target_azimuth), Elevation(target_elevation));

        // The two virtual sources sit one offset either side of the bearing.
        // `rotate_by`, not `+`: the sum crosses the seam. At azimuth 170 with
        // full width the left source is at 185, which *is* -175 — the same
        // wraparound `set_position` normalizes on store, re-introduced here by
        // adding to the already-wrapped value. Doing it on the raw `f32` was
        // the escape the omitted `Azimuth + ArcDegrees` exists to prevent, and
        // the "types stop at the trigonometry" note below covers only `elev`.
        let angle_offset = ArcDegrees(15.0 * width);
        let azimuth_a = smoothed_azimuth.rotate_by(angle_offset);
        let azimuth_b = smoothed_azimuth.rotate_by(-angle_offset);

        // Two pre-allocated scratch buffers — one for each virtual source.
        // Both go through `solve_gains`, so each virtual source carries unit
        // energy on its own; without that, the width branch would reintroduce
        // exactly the rear-arc silence the mono branch no longer has.
        let count_a = self.gains_scratch_a.capacity();
        let count_b = self.gains_scratch_b.capacity();
        let (panner, has_rear) = (&self.panner, self.has_rear_speakers);

        let scratch_a = self.gains_scratch_a.active(count_a);
        Self::solve_gains(panner, has_rear, azimuth_a, smoothed_elevation, scratch_a);
        let scratch_b = self.gains_scratch_b.active(count_b);
        Self::solve_gains(panner, has_rear, azimuth_b, smoothed_elevation, scratch_b);

        // Spread applies here too. It used not to: this branch called the
        // upstream solver directly and never reached `apply_spread`, so setting
        // a spread on a node fed a stereo pair at any non-zero width silently
        // did nothing — a parameter the inspector shows, the document saves and
        // the engine ignores. Spread and width are orthogonal controls (width
        // separates the two virtual sources, spread smears each one across the
        // speaker field), so the fix is to apply spread to each source rather
        // than to let one control suppress the other.
        let count = count_a.min(MAX_SPEAKERS);
        let mut gains_a = [0.0f32; MAX_SPEAKERS];
        let mut gains_b = [0.0f32; MAX_SPEAKERS];
        for (i, &g) in self
            .gains_scratch_a
            .active_ref(count_a)
            .iter()
            .enumerate()
            .take(count)
        {
            gains_a[i] = g as f32;
        }
        for (i, &g) in self
            .gains_scratch_b
            .active_ref(count_b)
            .iter()
            .enumerate()
            .take(count)
        {
            gains_b[i] = g as f32;
        }
        self.apply_spread(&mut gains_a, count);
        self.apply_spread(&mut gains_b, count);

        for (i, out) in output.iter_mut().enumerate() {
            let gain_l = gains_a.get(i).copied().unwrap_or(0.0);
            let gain_r = gains_b.get(i).copied().unwrap_or(0.0);
            *out = left * gain_l + right * gain_r;
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
