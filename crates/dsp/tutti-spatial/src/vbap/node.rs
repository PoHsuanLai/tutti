use super::error::Result;
use tutti_core::AudioUnit;
use tutti_core::ChannelLayout;
use tutti_core::{
    Azimuth, BufferMut, BufferRef, Elevation, Param, SampleRate, SignalFrame, Spread, StereoWidth,
};

use super::panner::VbapPanner;
use crate::layout::speaker_channel_map;
use crate::SpatialTarget;

/// VBAP multichannel panner over a fixed speaker layout, with position driven
/// by lock-free atomics so automation is RT-safe.
///
/// # The layouts are a closed set
///
/// Five widths are supported — stereo, quad, 5.1, 7.1 and 7.1.4 Atmos — each
/// built from a named preset of the underlying `vbap` crate's builder, which is
/// what supplies the speaker *positions*. VBAP pans across the two or three
/// speakers surrounding a direction, so it needs that arrangement and not
/// merely a channel count; a bare width does not determine one.
///
/// [`for_layout`](Self::for_layout) therefore refuses an unrecognized width with
/// [`VbapError::UnsupportedSpeakerLayout`] rather than substituting a nearby
/// layout, so a caller picks a real arrangement instead of silently rendering
/// for a different one. That refusal is also what keeps this type's `Clone`
/// correct — see the note on the impl.
///
/// # The energy law
///
/// **The speaker gains always sum to unit energy** — `Σ gain² == 1.0` — for
/// every bearing, every height, every spread and every supported layout. A
/// source panned in a full circle holds a constant perceived level; it never
/// fades out and never has a hole in it.
///
/// That is stronger than textbook VBAP, deliberately. VBAP places a source
/// inside the two or three speakers surrounding it, and a direction that no
/// speaker tuple surrounds has no solution: an array with a gap fades a source
/// out as it crosses the gap. Two such gaps exist here — the rear arc of a
/// stereo pair, which has no speaker behind the listener at all, and everything
/// below an Atmos bed. This node **collapses** a source in either onto the
/// nearest direction the array can render, at full energy, rather than fading
/// it:
///
/// - A **front-only** layout (stereo) has no front/back axis, so a rear bearing
///   is mirrored about the lateral axis — 135° renders as 45°, 180° as
///   front-centre. Anything still outside the ±30° pair hard-pans.
/// - Any layout, at a **height it has no speakers for**, renders at the nearest
///   height it does: a source under an Atmos bed comes from the bed.
///
/// The alternative was measured and rejected. Left to the upstream `vbap`
/// crate, which normalizes the tuple solution before clamping negative gains
/// away, a stereo pair lost energy from 45° outward and was *completely silent*
/// from 150° through 210° — a caller automating a pan through 180° heard the
/// source disappear. Details and the full before/after table:
/// [`VbapPanner::solve_gains`](super::panner) and
/// `tests/vbap_energy_sweep.rs`.
///
/// Note that LFE is not one of the gains: the panner never feeds it
/// ([`build_vbap_mix`](super::build_vbap_mix) sends it a separate low-passed
/// feed), so it reads zero and is outside the law above.
///
/// [`VbapError::UnsupportedSpeakerLayout`]: crate::vbap::VbapError::UnsupportedSpeakerLayout
pub struct VbapPannerNode {
    panner: VbapPanner,
    layout: ChannelLayout,
    target: SpatialTarget,
    /// VBAP diffusion, `0..1`: how many speakers a point source is smeared
    /// across. See [`Spread`] — it is not a `Mix`, because it blends nothing.
    spread: Param<Spread>,
    /// Mid/side stereo width, `0..` — 1.0 is unchanged, above 1.0 is wider
    /// than the source. NOT an `Amplitude` despite the matching range: it
    /// scales the SIDE component against the mid. See [`StereoWidth`].
    width: Param<StereoWidth>,
    sample_rate: SampleRate,
    scratch_output: Vec<f32>,
    /// Gain-index → output-channel scatter map (see [`crate::layout`]).
    /// Precomputed per layout so the RT path just indexes it.
    channel_map: Vec<usize>,
}

impl Clone for VbapPannerNode {
    fn clone(&self) -> Self {
        // The arms below must stay in lockstep with `for_layout`, which is what
        // keeps the `_` arm unreachable: every public constructor routes through
        // one of the five presets, and any other width is refused there as
        // `UnsupportedSpeakerLayout`. That gating is the only thing standing
        // between the fallback and a silent bug — a sixth layout added to
        // `for_layout` and not here would clone a 16-channel node into a stereo
        // one, changing its output width with no error anywhere.
        let mut new_panner = match self.layout.count() {
            2 => VbapPanner::stereo().expect("stereo preset"),
            4 => VbapPanner::quad().expect("quad preset"),
            6 => VbapPanner::surround_5_1().expect("5.1 preset"),
            8 => VbapPanner::surround_7_1().expect("7.1 preset"),
            12 => VbapPanner::atmos_7_1_4().expect("Atmos preset"),
            _ => VbapPanner::stereo().expect("stereo fallback"),
        };

        let (azimuth, elevation) = self.target.load();
        let spread = self.spread.load();
        new_panner.set_position(azimuth, elevation);
        new_panner.set_spread(spread);

        Self {
            panner: new_panner,
            layout: self.layout,
            target: self.target.clone(),
            spread: self.spread.handle(),
            width: self.width.handle(),
            sample_rate: self.sample_rate,
            scratch_output: vec![0.0; self.layout.count() as usize],
            channel_map: self.channel_map.clone(),
        }
    }
}

impl VbapPannerNode {
    /// A 2-out panner over the stereo speaker pair.
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn stereo() -> Result<Self> {
        let panner = VbapPanner::stereo()?;
        Ok(Self::from_panner(panner, ChannelLayout::STEREO))
    }

    /// A 4-out panner over the quad field (FL/FR/RL/RR, no LFE).
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn quad() -> Result<Self> {
        let panner = VbapPanner::quad()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(4u16)))
    }

    /// A 6-out panner over the 5.1 field. The preset is literally 5.0 — LFE is
    /// left silent here and fed separately by
    /// [`build_vbap_mix`](super::build_vbap_mix).
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn surround_5_1() -> Result<Self> {
        let panner = VbapPanner::surround_5_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(6u16)))
    }

    /// An 8-out panner over the 7.1 field. Like 5.1, the preset is 7.0 and LFE
    /// is fed separately.
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn surround_7_1() -> Result<Self> {
        let panner = VbapPanner::surround_7_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(8u16)))
    }

    /// A 12-out panner over the 7.1.4 Atmos bed — 7.1 plus four height speakers.
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn atmos_7_1_4() -> Result<Self> {
        let panner = VbapPanner::atmos_7_1_4()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(12u16)))
    }

    /// Build a panner sized to a [`tutti_types::ChannelLayout`] — the count-based
    /// width vocabulary the export / master side speaks — by dispatching to the
    /// matching VBAP preset. This is the single place the count→preset mapping
    /// lives, so reconcilers and graph builders call it instead of re-matching.
    ///
    /// The node keeps the count enum and resolves it to a VBAP speaker preset
    /// internally.
    ///
    /// # Errors
    /// Returns [`VbapError::UnsupportedSpeakerLayout`](crate::vbap::VbapError::UnsupportedSpeakerLayout)
    /// for a width that has no preset — only 2/4/6/8/12 are defined.
    pub fn for_layout(layout: tutti_types::ChannelLayout) -> Result<Self> {
        match layout.count() {
            2 => Self::stereo(),
            4 => Self::quad(),
            6 => Self::surround_5_1(),
            8 => Self::surround_7_1(),
            12 => Self::atmos_7_1_4(),
            n => Err(crate::vbap::VbapError::UnsupportedSpeakerLayout(n)),
        }
    }

    fn from_panner(panner: VbapPanner, layout: ChannelLayout) -> Self {
        Self {
            panner,
            layout,
            target: SpatialTarget::new(),
            spread: Param::new(Spread::POINT),
            width: Param::new(StereoWidth::NATURAL),
            sample_rate: SampleRate::SR_48K,
            scratch_output: vec![0.0; layout.count() as usize],
            channel_map: speaker_channel_map(layout),
        }
    }

    /// Set position (thread-safe, lock-free).
    ///
    /// - `azimuth`: bearing, wraps onto the circle (0 = front, 90 = left, -90 = right)
    /// - `elevation`: height, saturates at the poles (0 = ear level, positive = up)
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.target.store(azimuth, elevation);
    }

    /// The commanded bearing in [`Azimuth`] degrees — the target, not the
    /// smoothed position the panner is currently at.
    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    /// The commanded height in [`Elevation`] degrees — the target, not the
    /// smoothed position the panner is currently at.
    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Set the VBAP diffusion, clamped to [`Spread`]'s `0..1`: 0 is a point
    /// source, 1 smears it across the whole speaker field. Lock-free.
    pub fn set_spread(&self, spread: impl Into<Spread>) {
        self.spread.store(Spread::new_clamped(spread.into().get()));
    }

    /// The current [`Spread`], `0..1`.
    pub fn spread(&self) -> Spread {
        self.spread.load()
    }

    /// Set the mid/side width applied to a stereo input, clamped to
    /// [`StereoWidth`]: 0 is mono, 1 unchanged, above 1 wider than the source.
    /// Lock-free.
    pub fn set_width(&self, width: impl Into<StereoWidth>) {
        self.width
            .store(StereoWidth::new_clamped(width.into().get()));
    }

    /// The current [`StereoWidth`].
    pub fn width(&self) -> StereoWidth {
        self.width.load()
    }

    /// Output channel count — the speaker layout's width, and what
    /// `AudioUnit::outputs` reports.
    pub fn num_channels(&self) -> usize {
        self.layout.count() as usize
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.target.load();
        let spread = self.spread.load();
        self.panner.set_position(azimuth, elevation);
        self.panner.set_spread(spread);
    }
}

impl AudioUnit for VbapPannerNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        self.layout.count() as usize
    }

    /// Clears the de-zipper ramp only. Position, spread and width are
    /// caller-set configuration and survive.
    ///
    /// `AudioUnit::reset` resets *time*, not settings — an offline render
    /// (`bevy_tutti::export`) calls it on a freshly-cloned net to drop inherited
    /// filter memory and tails, and a host calls it between clips to clear a
    /// tail. Re-aiming here would silently move every spatialised source to
    /// front-centre in both cases, and `Clone` shares these atomics
    /// ([`Param::handle`]), so it would move the *live* node's source too.
    ///
    /// # The `sync_position` first is load-bearing
    ///
    /// The commanded position lives in **two** places: this node's
    /// [`SpatialTarget`], which [`set_position`](Self::set_position) writes, and
    /// the inner panner's own atomics, which the smoother is seeded from. The
    /// two are joined only by [`sync_position`](Self::sync_position) — and that
    /// used to run exclusively inside `tick`/`process`.
    ///
    /// So `set_position` → `reset` seeded the ramp at the panner's *stale*
    /// bearing: on a fresh node, front-centre. The first block after the reset
    /// then rendered `[0.707, 0.707]` at every azimuth and glided to the
    /// commanded bearing over the following 50 ms — a plausible-looking unity
    /// that pans nowhere, with no error raised. Pushing the target into the
    /// panner before seeding is what makes the first block after a reset
    /// already correct, which is the whole point of seating the smoother.
    fn reset(&mut self) {
        self.sync_position();
        self.panner.reset_state();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.panner.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.sync_position();

        let width = self.width.load();

        let left = input.first().copied().unwrap_or(0.0);
        let right = input.get(1).copied().unwrap_or(left);
        // Same speaker→file-channel scatter as `process` (see there): pan into
        // scratch in speaker order, then map to output channels, LFE left silent.
        self.panner
            .process_stereo_into(left, right, width, &mut self.scratch_output);
        for slot in output.iter_mut() {
            *slot = 0.0;
        }
        for (speaker, &ch) in self.channel_map.iter().enumerate() {
            if ch < output.len() {
                output[ch] = self.scratch_output[speaker];
            }
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.sync_position();

        let width = self.width.load();
        let num_outputs = self.layout.count() as usize;

        // scratch_output is pre-sized to num_outputs in from_panner and Clone.
        // num_outputs is fixed for the node's lifetime, so this never grows at RT.
        debug_assert_eq!(self.scratch_output.len(), num_outputs);

        // Hoisted once per block: is a second input channel present?
        let has_stereo_in = ChannelLayout::from(input.channels()).is_multi();

        for i in 0..size {
            let left = input.at_f32(0, i);
            let right = if has_stereo_in {
                input.at_f32(1, i)
            } else {
                left
            };

            // The panner writes its VBAP gains in *speaker* order into
            // scratch_output[0..num_speakers]. Scatter each to its file channel
            // via channel_map (which skips LFE), zeroing every output channel
            // first so unmapped channels (LFE) stay silent — the panner never
            // feeds LFE; build_vbap_mix feeds it a separate low-passed send.
            self.panner
                .process_stereo_into(left, right, width, &mut self.scratch_output);

            for ch in 0..num_outputs {
                output.set_f32(ch, i, 0.0);
            }
            for (speaker, &ch) in self.channel_map.iter().enumerate() {
                if ch < num_outputs {
                    output.set_f32(ch, i, self.scratch_output[speaker]);
                }
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::VBAP_PANNER_BASE_ID | (self.layout.count() as u64)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let num_outputs = self.layout.count() as usize;
        let mut output = SignalFrame::new(num_outputs);
        for i in 0..num_outputs {
            output.set(i, input.at(0));
        }
        output
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vbap_panner_tick() {
        let mut panner = VbapPannerNode::stereo().unwrap();
        panner.set_position(0.0, 0.0);

        let input = [1.0f32, 1.0f32];
        let mut output = [0.0f32; 2];

        panner.tick(&input, &mut output);

        assert!(output[0] > 0.0);
        assert!(output[1] > 0.0);
    }

    /// `AudioUnit::reset` resets time, not settings.
    ///
    /// The offline exporter clones the live net and calls `reset()` on it to
    /// drop inherited filter memory and tails; a host calls it between clips for
    /// the same reason. A reset that re-aimed would move every spatialised
    /// source to front-centre in both cases, with nothing to compare and no
    /// error raised.
    #[test]
    fn reset_keeps_the_authored_placement() {
        let mut panner = VbapPannerNode::surround_5_1().unwrap();
        panner.set_position(Azimuth(45.0), Elevation(15.0));
        panner.set_spread(Spread(0.3));
        panner.set_width(StereoWidth(1.5));

        panner.reset();

        assert!(
            (panner.azimuth().get() - 45.0).abs() < 0.001,
            "reset moved the bearing to {}",
            panner.azimuth().get()
        );
        assert!(
            (panner.elevation().get() - 15.0).abs() < 0.001,
            "reset moved the height to {}",
            panner.elevation().get()
        );
        assert!(
            (panner.spread().get() - 0.3).abs() < 0.001,
            "reset changed the spread to {}",
            panner.spread().get()
        );
        assert!(
            (panner.width().get() - 1.5).abs() < 0.001,
            "reset changed the width to {}",
            panner.width().get()
        );
    }

    /// The other half of the contract: the ramp *is* cleared, so the frame
    /// after a reset renders at the commanded position rather than sweeping in
    /// from wherever the smoother had got to.
    ///
    /// Asserted against a *settled* reference panner rather than against a
    /// channel inequality: the de-zipper starts at front-centre, so a few frames
    /// into a hard-left move both channels are still near-equal and either one
    /// may lead by a hair. "Equals the settled answer" is the property a seated
    /// smoother actually has, and it is the one that fails when the ramp is
    /// left in flight.
    ///
    /// # The setup deliberately does not tick before the reset
    ///
    /// The commanded position lives in the node's [`SpatialTarget`], which
    /// `set_position` writes, and in the inner panner's own atomics, which the
    /// smoother is seeded from. `sync_position` is the only join, and it used to
    /// run exclusively inside `tick`. So a `set_position` → `tick` → `reset`
    /// sequence passed even with the bug present: the leading `tick` had already
    /// pushed the bearing into the panner, so the reset seeded at the right
    /// place by accident.
    ///
    /// This test therefore resets with **no intervening tick**, which is the
    /// sequence a caller writes and the one that was broken. The `-90°` half is
    /// the mutation guard: at `+90°` a bug that seeds front-centre still leaves
    /// the left channel leading, so a one-sided assertion could pass for the
    /// wrong reason.
    ///
    /// Mutation: drop `sync_position()` from `VbapPannerNode::reset` → both
    /// halves fail, reading `[0.707, 0.707]` (front-centre) instead of a hard
    /// pan.
    #[test]
    fn reset_seats_the_smoother_on_the_commanded_position() {
        let input = [1.0f32, 1.0f32];

        for (bearing, lead, silent) in [(90.0f32, 0usize, 1usize), (-90.0, 1, 0)] {
            // Where the panner ends up once the 50 ms ramp has run out: a hard
            // pan, so the opposite channel is silent.
            let mut settled = VbapPannerNode::stereo().unwrap();
            settled.set_position(Azimuth(bearing), Elevation::LEVEL);
            let mut reference = [0.0f32; 2];
            for _ in 0..48_000 {
                settled.tick(&input, &mut reference);
            }
            assert!(
                reference[lead] > 0.9 && reference[silent] < 0.1,
                "the settled reference at {bearing} should be a hard pan, got {reference:?}"
            );

            // No tick between `set_position` and `reset`: the reset must find
            // the commanded bearing on its own.
            let mut panner = VbapPannerNode::stereo().unwrap();
            panner.set_position(Azimuth(bearing), Elevation::LEVEL);
            panner.reset();

            let mut after = [0.0f32; 2];
            panner.tick(&input, &mut after);
            assert!(
                (after[0] - reference[0]).abs() < 0.01 && (after[1] - reference[1]).abs() < 0.01,
                "at {bearing}deg, the first frame after a reset should already render \
                 at the commanded position: got {after:?}, settled is {reference:?}"
            );
        }
    }

    /// The ramp is genuinely in flight before a reset — otherwise
    /// [`reset_seats_the_smoother_on_the_commanded_position`] would hold
    /// trivially and prove nothing about the reset.
    ///
    /// Mutation: make `AngleSmoother::step` jump straight to the target (coeff
    /// 1.0) → fails, because the first frame would already be settled.
    #[test]
    fn the_de_zipper_ramp_is_in_flight_without_a_reset() {
        let input = [1.0f32, 1.0f32];

        let mut settled = VbapPannerNode::stereo().unwrap();
        settled.set_position(Azimuth(90.0), Elevation::LEVEL);
        let mut reference = [0.0f32; 2];
        for _ in 0..48_000 {
            settled.tick(&input, &mut reference);
        }

        let mut panner = VbapPannerNode::stereo().unwrap();
        panner.set_position(Azimuth(90.0), Elevation::LEVEL);
        let mut mid_ramp = [0.0f32; 2];
        panner.tick(&input, &mut mid_ramp);
        assert!(
            (mid_ramp[1] - reference[1]).abs() > 0.1,
            "one frame in, the ramp should still be far from settled, \
             got {mid_ramp:?} vs {reference:?}"
        );
    }

    /// `Clone` shares the position atomics, so a reset that wrote them would
    /// reach back through every handle — including the live node an offline
    /// render was cloned from.
    #[test]
    fn reset_on_a_clone_does_not_move_the_original() {
        let panner = VbapPannerNode::stereo().unwrap();
        panner.set_position(Azimuth(-60.0), Elevation(20.0));

        let mut cloned = panner.clone();
        cloned.reset();

        assert!(
            (panner.azimuth().get() - (-60.0)).abs() < 0.001,
            "resetting the clone moved the original to {}",
            panner.azimuth().get()
        );
        assert!(
            (panner.elevation().get() - 20.0).abs() < 0.001,
            "resetting the clone moved the original to {}",
            panner.elevation().get()
        );
    }

    /// **Every** separately shared cell survives a clone as a shared handle,
    /// not as a snapshot.
    ///
    /// `Clone` rebuilds the panner geometry but hands on `target`, `spread` and
    /// `width` by handle (`.clone()` / `.handle()`), so all three are asserted
    /// -- one per shared field. Covering only `target` leaves a hole the
    /// compiler cannot see: swapping `spread: self.spread.handle()` for
    /// `Param::new(spread)` is a snapshotting clone that still passes a
    /// position-only test, and `width` had no cover at all.
    ///
    /// Why sharing is the required behaviour rather than an implementation
    /// detail: the offline exporter clones the live net to render it, so a
    /// snapshotting clone would freeze the render at whatever spread or width
    /// happened to be set at clone time and silently ignore every later
    /// automation move. It is also the premise of
    /// `reset_on_a_clone_does_not_move_the_original` -- `reset` must not write
    /// these cells precisely because the write would reach back through every
    /// handle.
    ///
    /// Asserted in both directions per field: a handle is shared, not merely
    /// copied forward, so a write through either side must be visible from the
    /// other.
    #[test]
    fn vbap_clone_shares_atomics() {
        let panner = VbapPannerNode::stereo().unwrap();
        let cloned = panner.clone();

        // -- target: azimuth + elevation, set on the original.
        panner.set_position(90.0, 45.0);
        assert!(
            (cloned.azimuth().get() - 90.0).abs() < 0.001,
            "the clone did not see the original's bearing: {}",
            cloned.azimuth().get()
        );
        assert!(
            (cloned.elevation().get() - 45.0).abs() < 0.001,
            "the clone did not see the original's height: {}",
            cloned.elevation().get()
        );

        // And vice versa.
        cloned.set_position(-60.0, 10.0);
        assert!(
            (panner.azimuth().get() - (-60.0)).abs() < 0.001,
            "the original did not see the clone's bearing: {}",
            panner.azimuth().get()
        );
        assert!(
            (panner.elevation().get() - 10.0).abs() < 0.001,
            "the original did not see the clone's height: {}",
            panner.elevation().get()
        );

        // -- spread: its own cell, and the one the deleted `vbap_panner_clone`
        //    used to be the only cover for.
        panner.set_spread(Spread(0.3));
        assert!(
            (cloned.spread().get() - 0.3).abs() < 0.001,
            "the clone did not see the original's spread: {} -- a snapshotting \
             clone would freeze an offline render at the clone-time value",
            cloned.spread().get()
        );
        cloned.set_spread(Spread(0.8));
        assert!(
            (panner.spread().get() - 0.8).abs() < 0.001,
            "the original did not see the clone's spread: {}",
            panner.spread().get()
        );

        // -- width: its own cell too, and previously uncovered in either
        //    direction.
        panner.set_width(StereoWidth(1.5));
        assert!(
            (cloned.width().get() - 1.5).abs() < 0.001,
            "the clone did not see the original's width: {}",
            cloned.width().get()
        );
        cloned.set_width(StereoWidth(0.4));
        assert!(
            (panner.width().get() - 0.4).abs() < 0.001,
            "the original did not see the clone's width: {}",
            panner.width().get()
        );
    }
}
