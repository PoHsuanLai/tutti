use crate::Result;
use tutti_core::AudioUnit;
use tutti_core::{BufferMut, BufferRef, Degrees, Linear, Param, SignalFrame};

use super::binaural_panner::BinauralPanner;
use super::vbap_panner::SpatialPanner;

/// Azimuth/elevation pair as typed parameters. Both spatial panner nodes
/// carry exactly this pair; grouping them here names the concept and lets
/// nodes forward a single field through their Clone impls.
#[derive(Clone)]
pub struct SpatialTarget {
    pub azimuth: Param<Degrees>,
    pub elevation: Param<Degrees>,
}

impl SpatialTarget {
    pub fn new() -> Self {
        Self {
            azimuth: Param::new(Degrees(0.0)),
            elevation: Param::new(Degrees(0.0)),
        }
    }

    #[inline]
    pub fn load(&self) -> (f32, f32) {
        (self.azimuth.load().0, self.elevation.load().0)
    }

    pub fn store(&self, azimuth: f32, elevation: f32) {
        self.azimuth.store(Degrees(azimuth));
        self.elevation.store(Degrees(elevation));
    }

    pub fn reset_origin(&self) {
        self.store(0.0, 0.0);
    }
}

impl Default for SpatialTarget {
    fn default() -> Self {
        Self::new()
    }
}

/// VBAP multichannel panner (stereo/quad/5.1/7.1/Atmos).
/// Position controlled via lock-free atomics for RT-safe automation.
pub struct SpatialPannerNode {
    panner: SpatialPanner,
    num_outputs: usize,
    target: SpatialTarget,
    spread: Param<Linear>,
    width: Param<Linear>,
    sample_rate: f32,
    scratch_output: Vec<f32>,
}

impl Clone for SpatialPannerNode {
    fn clone(&self) -> Self {
        let mut new_panner = match self.num_outputs {
            2 => SpatialPanner::stereo().expect("stereo preset"),
            4 => SpatialPanner::quad().expect("quad preset"),
            6 => SpatialPanner::surround_5_1().expect("5.1 preset"),
            8 => SpatialPanner::surround_7_1().expect("7.1 preset"),
            12 => SpatialPanner::atmos_7_1_4().expect("Atmos preset"),
            _ => SpatialPanner::stereo().expect("stereo fallback"),
        };

        let (azimuth, elevation) = self.target.load();
        let spread = self.spread.load().0;
        new_panner.set_position(azimuth, elevation);
        new_panner.set_spread(spread);

        Self {
            panner: new_panner,
            num_outputs: self.num_outputs,
            target: self.target.clone(),
            spread: self.spread.handle(),
            width: self.width.handle(),
            sample_rate: self.sample_rate,
            scratch_output: vec![0.0; self.num_outputs],
        }
    }
}

impl SpatialPannerNode {
    pub fn stereo() -> Result<Self> {
        let panner = SpatialPanner::stereo()?;
        Ok(Self::from_panner(panner, 2))
    }

    pub fn quad() -> Result<Self> {
        let panner = SpatialPanner::quad()?;
        Ok(Self::from_panner(panner, 4))
    }

    pub fn surround_5_1() -> Result<Self> {
        let panner = SpatialPanner::surround_5_1()?;
        Ok(Self::from_panner(panner, 6))
    }

    pub fn surround_7_1() -> Result<Self> {
        let panner = SpatialPanner::surround_7_1()?;
        Ok(Self::from_panner(panner, 8))
    }

    pub fn atmos_7_1_4() -> Result<Self> {
        let panner = SpatialPanner::atmos_7_1_4()?;
        Ok(Self::from_panner(panner, 12))
    }

    fn from_panner(panner: SpatialPanner, num_outputs: usize) -> Self {
        Self {
            panner,
            num_outputs,
            target: SpatialTarget::new(),
            spread: Param::new(Linear(0.0)),
            width: Param::new(Linear(1.0)),
            sample_rate: 48000.0,
            scratch_output: vec![0.0; num_outputs],
        }
    }

    /// Set position in degrees (thread-safe, lock-free)
    ///
    /// - `azimuth`: Horizontal angle (-180 to 180, 0 = front, 90 = left, -90 = right)
    /// - `elevation`: Vertical angle (-90 to 90, 0 = ear level, positive = up)
    pub fn set_position(&self, azimuth: f32, elevation: f32) {
        self.target.store(azimuth, elevation);
    }

    pub fn azimuth(&self) -> f32 {
        self.target.azimuth.load().0
    }

    pub fn elevation(&self) -> f32 {
        self.target.elevation.load().0
    }

    /// Set spread factor (0.0 = point source, 1.0 = diffuse)
    pub fn set_spread(&self, spread: f32) {
        self.spread.store(Linear(spread.clamp(0.0, 1.0)));
    }

    pub fn spread(&self) -> f32 {
        self.spread.load().0
    }

    /// Set stereo width for stereo input mode (0.0 = mono, 1.0 = full stereo)
    pub fn set_width(&self, width: f32) {
        self.width.store(Linear(width.max(0.0)));
    }

    pub fn width(&self) -> f32 {
        self.width.load().0
    }

    pub fn num_channels(&self) -> usize {
        self.num_outputs
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.target.load();
        let spread = self.spread.load().0;
        self.panner.set_position(azimuth, elevation);
        self.panner.set_spread(spread);
    }
}

impl AudioUnit for SpatialPannerNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        self.num_outputs
    }

    fn reset(&mut self) {
        self.target.reset_origin();
        self.spread.store(Linear(0.0));
        self.width.store(Linear(1.0));
        self.panner.set_position(0.0, 0.0);
        self.panner.set_spread(0.0);
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get() as f32;
        self.panner.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.sync_position();

        let width = self.width.load().0;

        let left = input.first().copied().unwrap_or(0.0);
        let right = input.get(1).copied().unwrap_or(left);
        self.panner.process_stereo_into(left, right, width, output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.sync_position();

        let width = self.width.load().0;
        let num_outputs = self.num_outputs;

        // scratch_output is pre-sized to num_outputs in from_panner and Clone.
        // num_outputs is fixed for the node's lifetime, so this never grows at RT.
        debug_assert_eq!(self.scratch_output.len(), num_outputs);

        for i in 0..size {
            let left = input.at_f32(0, i);
            let right = if input.channels() > 1 {
                input.at_f32(1, i)
            } else {
                left
            };

            self.panner
                .process_stereo_into(left, right, width, &mut self.scratch_output);

            for (ch, &sample) in self.scratch_output.iter().enumerate() {
                if ch < num_outputs {
                    output.set_f32(ch, i, sample);
                }
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::SPATIAL_PANNER_BASE_ID | (self.num_outputs as u64)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.num_outputs);
        for i in 0..self.num_outputs {
            output.set(i, input.at(0));
        }
        output
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

/// ITD/ILD binaural panner for headphone 3D audio.
/// Position controlled via lock-free atomics for RT-safe automation.
pub struct BinauralPannerNode {
    panner: BinauralPanner,
    target: SpatialTarget,
    width: Param<Linear>,
    sample_rate: f32,
}

impl Clone for BinauralPannerNode {
    fn clone(&self) -> Self {
        let mut new_panner = BinauralPanner::new(self.sample_rate);
        let (azimuth, elevation) = self.target.load();
        new_panner.set_position(azimuth, elevation);

        Self {
            panner: new_panner,
            target: self.target.clone(),
            width: self.width.handle(),
            sample_rate: self.sample_rate,
        }
    }
}

impl BinauralPannerNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            panner: BinauralPanner::new(sample_rate),
            target: SpatialTarget::new(),
            width: Param::new(Linear(1.0)),
            sample_rate,
        }
    }

    /// Azimuth in degrees (-180..180, 0=front, 90=left), elevation (-90..90, 0=ear level).
    /// Lock-free.
    pub fn set_position(&self, azimuth: f32, elevation: f32) {
        self.target.store(azimuth, elevation);
    }

    pub fn azimuth(&self) -> f32 {
        self.target.azimuth.load().0
    }

    pub fn elevation(&self) -> f32 {
        self.target.elevation.load().0
    }

    /// 0.0 = mono, 1.0 = full stereo, up to 2.0 for extra-wide.
    pub fn set_width(&self, width: f32) {
        self.width.store(Linear(width.clamp(0.0, 2.0)));
    }

    pub fn width(&self) -> f32 {
        self.width.load().0
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.target.load();
        self.panner.set_position(azimuth, elevation);
    }
}

impl AudioUnit for BinauralPannerNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.target.reset_origin();
        self.width.store(Linear(1.0));
        self.panner = BinauralPanner::new(self.sample_rate);
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate as f32;
        self.panner = BinauralPanner::new(self.sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.sync_position();

        let width = self.width.load().0;

        let left = input.first().copied().unwrap_or(0.0);
        let right = input.get(1).copied().unwrap_or(left);

        let (out_left, out_right) = self.panner.process_stereo(left, right, width);

        if output.len() >= 2 {
            output[0] = out_left;
            output[1] = out_right;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.sync_position();

        let width = self.width.load().0;

        for i in 0..size {
            let left = input.at_f32(0, i);
            let right = if input.channels() > 1 {
                input.at_f32(1, i)
            } else {
                left
            };

            let (out_left, out_right) = self.panner.process_stereo(left, right, width);
            output.set_f32(0, i, out_left);
            output.set_f32(1, i, out_right);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::BINAURAL_PANNER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(2);
        output.set(0, input.at(0));
        output.set(1, input.at(0));
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
    fn test_binaural_panner_tick() {
        let mut panner = BinauralPannerNode::new(48000.0);
        panner.set_position(90.0, 0.0);

        let input = [1.0f32, 1.0f32];
        let mut output = [0.0f32; 2];
        for _ in 0..100 {
            panner.tick(&input, &mut output);
        }

        panner.tick(&input, &mut output);
        assert!(
            output[0] > output[1],
            "Left should be louder for left position"
        );
    }

    #[test]
    fn test_spatial_panner_clone() {
        let panner = SpatialPannerNode::surround_5_1().unwrap();
        panner.set_position(45.0, 15.0);
        panner.set_spread(0.3);

        let cloned = panner.clone();

        assert_eq!(cloned.num_channels(), panner.num_channels());
        assert!((cloned.azimuth() - panner.azimuth()).abs() < 0.001);
        assert!((cloned.elevation() - panner.elevation()).abs() < 0.001);
        assert!((cloned.spread() - panner.spread()).abs() < 0.001);
    }

    #[test]
    fn test_binaural_panner_clone() {
        let panner = BinauralPannerNode::new(44100.0);
        panner.set_position(-45.0, 10.0);
        panner.set_width(0.5);

        let cloned = panner.clone();

        assert!((cloned.azimuth() - panner.azimuth()).abs() < 0.001);
        assert!((cloned.elevation() - panner.elevation()).abs() < 0.001);
        assert!((cloned.width() - panner.width()).abs() < 0.001);
    }

    #[test]
    fn test_spatial_clone_shares_atomics() {
        let panner = SpatialPannerNode::stereo().unwrap();
        let cloned = panner.clone();

        // Setting position on original should be visible from clone
        panner.set_position(90.0, 45.0);
        assert!((cloned.azimuth() - 90.0).abs() < 0.001);
        assert!((cloned.elevation() - 45.0).abs() < 0.001);

        // And vice versa
        cloned.set_position(-60.0, 10.0);
        assert!((panner.azimuth() - (-60.0)).abs() < 0.001);
        assert!((panner.elevation() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_binaural_clone_shares_atomics() {
        let panner = BinauralPannerNode::new(48000.0);
        let cloned = panner.clone();

        panner.set_position(90.0, 45.0);
        assert!((cloned.azimuth() - 90.0).abs() < 0.001);
        assert!((cloned.elevation() - 45.0).abs() < 0.001);

        cloned.set_position(-60.0, 10.0);
        assert!((panner.azimuth() - (-60.0)).abs() < 0.001);
        assert!((panner.elevation() - 10.0).abs() < 0.001);
    }
}
