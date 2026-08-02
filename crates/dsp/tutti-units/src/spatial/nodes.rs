use crate::Result;
use tutti_core::AudioUnit;
use tutti_core::ChannelLayout;
use tutti_core::{
    Azimuth, BufferMut, BufferRef, Elevation, Param, SampleRate, SignalFrame, Spread, StereoWidth,
};

use super::vbap_panner::SpatialPanner;

/// Azimuth/elevation pair as typed parameters. Both spatial panner nodes
/// carry exactly this pair; grouping them here names the concept and lets
/// nodes forward a single field through their Clone impls.
///
/// The two fields are *different types* on purpose: a bearing wraps (190
/// degrees is 170 to the right) and a height saturates (past straight up, you
/// stop). They were one `Degrees` until the two behaviours had to diverge.
#[derive(Clone)]
pub struct SpatialTarget {
    pub azimuth: Param<Azimuth>,
    pub elevation: Param<Elevation>,
}

impl SpatialTarget {
    pub fn new() -> Self {
        Self {
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
        }
    }

    /// The typed pair, for the panners' trigonometry.
    ///
    /// Typed rather than raw: both panners immediately rebuild `Azimuth(..)` /
    /// `Elevation(..)` from what they receive here (that is where the two
    /// long-way-around bugs lived), so handing out bare floats only created a
    /// strip/re-wrap round trip that a caller could get wrong in between.
    #[inline]
    pub fn load(&self) -> (Azimuth, Elevation) {
        (self.azimuth.load(), self.elevation.load())
    }

    /// Store a bearing/height pair, normalized on the way in.
    ///
    /// Each coordinate is constrained the way its own space requires: the
    /// bearing wraps onto the circle, the height clamps at the poles. Taking
    /// the two newtypes is what makes the pairing unmixable — with a raw
    /// `(f32, f32)` a caller could swap them and the compiler would agree.
    pub fn store(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.azimuth.store(azimuth.into().wrap());
        self.elevation
            .store(Elevation::new_clamped(elevation.into().get()));
    }

    pub fn reset_origin(&self) {
        self.store(Azimuth::FRONT, Elevation::LEVEL);
    }
}

impl Default for SpatialTarget {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a VBAP gain index to the file/output channel it belongs in, for a given
/// layout — because the VBAP speaker order is NOT the file channel order once
/// LFE enters the picture.
///
/// VBAP presets are pure *spatialized* speakers with no LFE (the vbap crate's
/// 5.1/7.1 presets are literally 5.0/7.0 — "LFE handled separately"), and their
/// order is `[L, R, C, surrounds…]`. The file/interchange order (SMPTE / WAV
/// `WAVEFORMATEXTENSIBLE`) is `[FL, FR, C, LFE, SL, SR, …]` with LFE at index 3.
/// So a straight gain-i → channel-i write puts the surrounds one slot early and
/// leaves a hole. This returns `map[i] = file channel for VBAP speaker i`; the
/// LFE channel is deliberately absent (it is fed a separate low-passed send by
/// [`build_surround_mix`](super::build_surround_mix), not by the panner).
///
/// - **Stereo / Quad**: identity — no LFE, order already matches.
/// - **5.1** (6ch, VBAP `[L,R,C,Ls,Rs]`): `[0,1,2,4,5]` — skip LFE at 3.
/// - **7.1** (8ch, VBAP `[L,R,C,Lss,Rss,Lrs,Rrs]`): `[0,1,2,4,5,6,7]` — skip LFE.
/// - **Atmos 7.1.4** (12ch): the 7.1 base skips LFE, then the 4 height channels
///   follow at 8..12: `[0,1,2,4,5,6,7,8,9,10,11]`.
/// - Any other width: identity (best effort).
fn speaker_channel_map(layout: ChannelLayout) -> Vec<usize> {
    match layout.count() {
        6 => vec![0, 1, 2, 4, 5],
        8 => vec![0, 1, 2, 4, 5, 6, 7],
        12 => vec![0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11],
        n => (0..n as usize).collect(),
    }
}

/// The LFE (`.1`) output channel index for a layout, if it has one. LFE lives at
/// channel 3 in the 5.1 / 7.1 / 7.1.4 file order (SMPTE / WAV). Layouts without
/// an LFE (mono / stereo / quad) return `None`.
///
/// LFE is *not* a panned speaker (see [`speaker_channel_map`]); this is the
/// channel [`build_surround_mix`](super::build_surround_mix) feeds with a
/// separate low-passed bass-management send.
pub(crate) fn lfe_channel(layout: ChannelLayout) -> Option<usize> {
    match layout.count() {
        6 | 8 | 12 => Some(3),
        _ => None,
    }
}

/// VBAP multichannel panner (stereo/quad/5.1/7.1/Atmos).
/// Position controlled via lock-free atomics for RT-safe automation.
pub struct SpatialPannerNode {
    panner: SpatialPanner,
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
    /// Gain-index → output-channel scatter map (see [`speaker_channel_map`]).
    /// Precomputed per layout so the RT path just indexes it.
    channel_map: Vec<usize>,
}

impl Clone for SpatialPannerNode {
    fn clone(&self) -> Self {
        let mut new_panner = match self.layout.count() {
            2 => SpatialPanner::stereo().expect("stereo preset"),
            4 => SpatialPanner::quad().expect("quad preset"),
            6 => SpatialPanner::surround_5_1().expect("5.1 preset"),
            8 => SpatialPanner::surround_7_1().expect("7.1 preset"),
            12 => SpatialPanner::atmos_7_1_4().expect("Atmos preset"),
            _ => SpatialPanner::stereo().expect("stereo fallback"),
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

impl SpatialPannerNode {
    pub fn stereo() -> Result<Self> {
        let panner = SpatialPanner::stereo()?;
        Ok(Self::from_panner(panner, ChannelLayout::STEREO))
    }

    pub fn quad() -> Result<Self> {
        let panner = SpatialPanner::quad()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(4u16)))
    }

    pub fn surround_5_1() -> Result<Self> {
        let panner = SpatialPanner::surround_5_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(6u16)))
    }

    pub fn surround_7_1() -> Result<Self> {
        let panner = SpatialPanner::surround_7_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(8u16)))
    }

    pub fn atmos_7_1_4() -> Result<Self> {
        let panner = SpatialPanner::atmos_7_1_4()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(12u16)))
    }

    /// Build a panner sized to a [`tutti_types::ChannelLayout`] — the count-based
    /// width vocabulary the export / master side speaks — by dispatching to the
    /// matching VBAP preset. This is the single place the count→preset mapping
    /// lives, so reconcilers and graph builders call it instead of re-matching.
    ///
    /// Errors with [`Error::UnsupportedSpeakerLayout`] for a width that has no
    /// preset (only 2/4/6/8/12 are defined). The node keeps the count enum and
    /// resolves it to a VBAP speaker preset internally.
    pub fn for_layout(layout: tutti_types::ChannelLayout) -> Result<Self> {
        match layout.count() {
            2 => Self::stereo(),
            4 => Self::quad(),
            6 => Self::surround_5_1(),
            8 => Self::surround_7_1(),
            12 => Self::atmos_7_1_4(),
            n => Err(crate::Error::UnsupportedSpeakerLayout(n)),
        }
    }

    fn from_panner(panner: SpatialPanner, layout: ChannelLayout) -> Self {
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

    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Set spread factor (0.0 = point source, 1.0 = diffuse)
    pub fn set_spread(&self, spread: impl Into<Spread>) {
        self.spread.store(Spread::new_clamped(spread.into().get()));
    }

    pub fn spread(&self) -> Spread {
        self.spread.load()
    }

    /// Set stereo width for stereo input mode (0.0 = mono, 1.0 = full stereo)
    pub fn set_width(&self, width: impl Into<StereoWidth>) {
        self.width
            .store(StereoWidth::new_clamped(width.into().get()));
    }

    pub fn width(&self) -> StereoWidth {
        self.width.load()
    }

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

impl AudioUnit for SpatialPannerNode {
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
            // feeds LFE; build_surround_mix feeds it a separate low-passed send.
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
        crate::node_id::SPATIAL_PANNER_BASE_ID | (self.layout.count() as u64)
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
    fn test_spatial_panner_tick() {
        let mut panner = SpatialPannerNode::stereo().unwrap();
        panner.set_position(0.0, 0.0);

        let input = [1.0f32, 1.0f32];
        let mut output = [0.0f32; 2];

        panner.tick(&input, &mut output);

        assert!(output[0] > 0.0);
        assert!(output[1] > 0.0);
    }

    #[test]
    fn test_spatial_panner_clone() {
        let panner = SpatialPannerNode::surround_5_1().unwrap();
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
    fn test_spatial_clone_shares_atomics() {
        let panner = SpatialPannerNode::stereo().unwrap();
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
