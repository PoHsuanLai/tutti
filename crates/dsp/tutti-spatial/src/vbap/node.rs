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

    fn reset(&mut self) {
        self.target.reset_origin();
        self.spread.store(Spread::POINT);
        self.width.store(StereoWidth::NATURAL);
        self.panner.set_position(Azimuth::FRONT, Elevation::LEVEL);
        self.panner.set_spread(Spread::POINT);
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

    #[test]
    fn vbap_panner_clone() {
        let panner = VbapPannerNode::surround_5_1().unwrap();
        panner.set_position(45.0, 15.0);
        panner.set_spread(0.3);

        let cloned = panner.clone();

        assert_eq!(cloned.num_channels(), panner.num_channels());
        // Compared via `.get()`: the angular units omit `Sub` on purpose (a
        // circle has no ends), so a difference is taken in the scalar space.
        assert!((cloned.azimuth().get() - panner.azimuth().get()).abs() < 0.001);
        assert!((cloned.elevation().get() - panner.elevation().get()).abs() < 0.001);
        assert!((cloned.spread().get() - panner.spread().get()).abs() < 0.001);
    }

    #[test]
    fn vbap_clone_shares_atomics() {
        let panner = VbapPannerNode::stereo().unwrap();
        let cloned = panner.clone();

        // Setting position on original should be visible from clone
        panner.set_position(90.0, 45.0);
        assert!((cloned.azimuth().get() - 90.0).abs() < 0.001);
        assert!((cloned.elevation().get() - 45.0).abs() < 0.001);

        // And vice versa
        cloned.set_position(-60.0, 10.0);
        assert!((panner.azimuth().get() - (-60.0)).abs() < 0.001);
        assert!((panner.elevation().get() - 10.0).abs() < 0.001);
    }
}
