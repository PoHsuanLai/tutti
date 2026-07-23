use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame};

use tutti_core::{Linear, Param, SampleRate, Seconds};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpolationMode {
    None,
    #[default]
    Linear,
    CubicHermite,
}

#[derive(Clone)]
pub struct DelayLine {
    pub(crate) buffer: Vec<f32>,
    write_pos: usize,
    max_delay_samples: usize,
}

impl DelayLine {
    pub fn new(max_delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay_samples + 1],
            write_pos: 0,
            max_delay_samples,
        }
    }

    pub fn from_seconds(
        max_delay_secs: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        let max_delay_secs = max_delay_secs.into().get();
        let sample_rate = sample_rate.into().get();
        let samples = (max_delay_secs * sample_rate as f32).ceil() as usize;
        Self::new(samples)
    }

    pub fn push_sample(&mut self, sample: f32) {
        self.buffer[self.write_pos] = sample;
        self.write_pos += 1;
        if self.write_pos >= self.buffer.len() {
            self.write_pos = 0;
        }
    }

    pub fn read_sample(&self, delay_samples: f32, mode: InterpolationMode) -> f32 {
        let delay = delay_samples.clamp(0.0, self.max_delay_samples as f32);
        match mode {
            InterpolationMode::None => {
                let idx = delay.round() as usize;
                self.read_at(idx)
            }
            InterpolationMode::Linear => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                s0 + frac * (s1 - s0)
            }
            InterpolationMode::CubicHermite => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let sm1 = self.read_at(floor.saturating_sub(1));
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                let s2 = self.read_at(floor + 2);
                let c0 = s0;
                let c1 = 0.5 * (s1 - sm1);
                let c2 = sm1 - 2.5 * s0 + 2.0 * s1 - 0.5 * s2;
                let c3 = 0.5 * (s2 - sm1) + 1.5 * (s0 - s1);
                ((c3 * frac + c2) * frac + c1) * frac + c0
            }
        }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    #[inline]
    fn read_at(&self, delay_samples: usize) -> f32 {
        let len = self.buffer.len();
        let idx = (self.write_pos + len - 1 - delay_samples.min(self.max_delay_samples)) % len;
        self.buffer[idx]
    }
}

/// Mono delay with feedback. 1 input (audio), 1 output.
/// Delay time can be modulated via the typed parameter handles.
pub struct DelayLineNode {
    delay: DelayLine,
    delay_time: Param<Seconds>,
    feedback: Param<Linear>,
    mix: Param<Linear>,
    interpolation: InterpolationMode,
    sample_rate: f64,
    max_delay_secs: f32,
}

impl DelayLineNode {
    pub fn new(
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Linear>,
    ) -> Self {
        let max_delay_secs = max_delay_secs.into().get();
        let delay_secs = delay_secs.into();
        let feedback = Linear(feedback.into().get().clamp(0.0, 0.99));
        Self {
            delay: DelayLine::from_seconds(max_delay_secs, DEFAULT_SR),
            delay_time: Param::new(delay_secs),
            feedback: Param::new(feedback),
            mix: Param::new(Linear(1.0)),
            interpolation: InterpolationMode::Linear,
            sample_rate: DEFAULT_SR,
            max_delay_secs,
        }
    }

    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    pub fn delay_time(&self) -> Arc<AtomicF32> {
        self.delay_time.as_atomic()
    }

    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn set_delay_time(&self, secs: impl Into<Seconds>) {
        self.delay_time.store(Seconds(secs.into().get().max(0.0)));
    }

    pub fn set_feedback(&self, fb: impl Into<Linear>) {
        self.feedback
            .store(Linear(fb.into().get().clamp(0.0, 0.99)));
    }

    pub fn set_mix(&self, mix: impl Into<Linear>) {
        self.mix.store(Linear(mix.into().get().clamp(0.0, 1.0)));
    }

    #[inline]
    fn process_sample(&mut self, input: f32, delay_samples: f32, fb: f32, mix: f32) -> f32 {
        let feedback_tap = self
            .delay
            .read_sample(delay_samples.max(1.0) - 1.0, self.interpolation);
        self.delay.push_sample(input + feedback_tap * fb);
        let delayed = self.delay.read_sample(delay_samples, self.interpolation);
        input * (1.0 - mix) + delayed * mix
    }
}

impl AudioUnit for DelayLineNode {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.delay.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.delay = DelayLine::from_seconds(self.max_delay_secs, sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let delay_samples = self.delay_time.load().get() * self.sample_rate as f32;
        let fb = self.feedback.load().get();
        let mix = self.mix.load().get();
        output[0] = self.process_sample(input[0], delay_samples, fb, mix);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let delay_samples = self.delay_time.load().get() * self.sample_rate as f32;
        let fb = self.feedback.load().get();
        let mix = self.mix.load().get();

        for i in 0..size {
            let in_s = input.at_f32(0, i);
            output.set_f32(0, i, self.process_sample(in_s, delay_samples, fb, mix));
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::DelayTime => self.set_delay_time(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::DELAY_LINE_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        let delay_samples = (self.delay_time.load().get() * self.sample_rate as f32) as f64;
        out.set(0, input.at(0).delay(delay_samples));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.delay.buffer.len() * core::mem::size_of::<f32>()
    }
}

impl Clone for DelayLineNode {
    fn clone(&self) -> Self {
        Self {
            delay: self.delay.clone(),
            delay_time: self.delay_time.handle(),
            feedback: self.feedback.handle(),
            mix: self.mix.handle(),
            interpolation: self.interpolation,
            sample_rate: self.sample_rate,
            max_delay_secs: self.max_delay_secs,
        }
    }
}

/// A left/right pair of `T`. Trivial helper, but removes `_l`/`_r` field
/// duplication across stereo DSP nodes.
#[derive(Debug, Clone)]
pub struct StereoPair<T> {
    pub l: T,
    pub r: T,
}

impl<T> StereoPair<T> {
    #[inline]
    pub const fn new(l: T, r: T) -> Self {
        Self { l, r }
    }
}

/// Stereo delay with per-channel delay times and cross-feedback.
///
/// # Port layout
///
/// The default node is 2-in / 2-out (audio on ports 0/1). For audio-rate
/// modulation it can grow optional param-input ports after the audio inputs
/// (see [`Self::with_param_inputs`]), in the order **feedback, then delay-time**.
/// A present `delay_time` port drives BOTH L and R delay times through one
/// shared input (the natural flanger/chorus control — the delay line
/// interpolates), while `feedback` overrides the feedback atomic. `cross_feedback`
/// and `mix` stay atomic-only. Each present port overrides its atomic per sample;
/// absent → a plain 2-in/2-out node, bit-identical to the unmodulated path and
/// zero added cost (the common case). Unconnected fundsp Net inputs read 0.0
/// every sample, so the ports are demand-built and never always-on.
pub struct StereoDelayLineNode {
    delays: StereoPair<DelayLine>,
    delay_time: StereoPair<Param<Seconds>>,
    feedback: Param<Linear>,
    cross_feedback: Param<Linear>,
    mix: Param<Linear>,
    interpolation: InterpolationMode,
    sample_rate: f64,
    max_delay_secs: f32,
    /// When true, a feedback param-input port follows the audio inputs and
    /// overrides [`Self::feedback`] per sample.
    mod_feedback: bool,
    /// When true, a delay-time param-input port follows the feedback port (or
    /// the audio inputs if `mod_feedback` is false) and overrides BOTH L/R
    /// delay times per sample, routed through the interpolating read.
    mod_delay_time: bool,
}

impl StereoDelayLineNode {
    pub fn new(
        max_delay_secs: impl Into<Seconds>,
        delay_l_secs: impl Into<Seconds>,
        delay_r_secs: impl Into<Seconds>,
        feedback: impl Into<Linear>,
    ) -> Self {
        let max_delay_secs = max_delay_secs.into().get();
        let delay_l_secs = delay_l_secs.into();
        let delay_r_secs = delay_r_secs.into();
        let feedback = Linear(feedback.into().get().clamp(0.0, 0.99));
        Self {
            delays: StereoPair::new(
                DelayLine::from_seconds(max_delay_secs, DEFAULT_SR),
                DelayLine::from_seconds(max_delay_secs, DEFAULT_SR),
            ),
            delay_time: StereoPair::new(Param::new(delay_l_secs), Param::new(delay_r_secs)),
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Linear(0.0)),
            mix: Param::new(Linear(1.0)),
            interpolation: InterpolationMode::Linear,
            sample_rate: DEFAULT_SR,
            max_delay_secs,
            mod_feedback: false,
            mod_delay_time: false,
        }
    }

    /// A delay with optional audio-rate `feedback` / `delay_time` param-input
    /// ports, appended after the two audio inputs in that order (feedback
    /// first). Each present port overrides its atomic per sample; the atomics
    /// still hold the base. A present `delay_time` port drives both L and R
    /// delay times (shared), through the interpolating read — the flanger /
    /// chorus path.
    pub fn with_param_inputs(
        max_delay_secs: impl Into<Seconds>,
        delay_l_secs: impl Into<Seconds>,
        delay_r_secs: impl Into<Seconds>,
        feedback: impl Into<Linear>,
        mod_feedback: bool,
        mod_delay_time: bool,
    ) -> Self {
        let mut node = Self::new(max_delay_secs, delay_l_secs, delay_r_secs, feedback);
        node.mod_feedback = mod_feedback;
        node.mod_delay_time = mod_delay_time;
        node
    }

    /// Input-port index of the feedback param input, if present (right after
    /// the two audio inputs).
    #[inline]
    pub fn feedback_port(&self) -> Option<usize> {
        self.mod_feedback.then_some(2)
    }

    /// Input-port index of the delay-time param input, if present (after the
    /// audio inputs and the feedback port).
    #[inline]
    pub fn delay_time_port(&self) -> Option<usize> {
        self.mod_delay_time
            .then_some(2 + self.mod_feedback as usize)
    }

    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    pub fn delay_time_l(&self) -> Arc<AtomicF32> {
        self.delay_time.l.as_atomic()
    }

    pub fn delay_time_r(&self) -> Arc<AtomicF32> {
        self.delay_time.r.as_atomic()
    }

    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    pub fn cross_feedback(&self) -> Arc<AtomicF32> {
        self.cross_feedback.as_atomic()
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn set_cross_feedback(&self, cf: impl Into<Linear>) {
        self.cross_feedback
            .store(Linear(cf.into().get().clamp(0.0, 0.99)));
    }

    /// Set both L and R delay times to the same value.
    pub fn set_delay_time(&self, secs: f32) {
        let v = Seconds(secs.clamp(0.0, self.max_delay_secs));
        self.delay_time.l.store(v);
        self.delay_time.r.store(v);
    }

    pub fn set_delay_time_l(&self, secs: f32) {
        self.delay_time
            .l
            .store(Seconds(secs.clamp(0.0, self.max_delay_secs)));
    }

    pub fn set_delay_time_r(&self, secs: f32) {
        self.delay_time
            .r
            .store(Seconds(secs.clamp(0.0, self.max_delay_secs)));
    }

    pub fn set_feedback(&self, fb: f32) {
        self.feedback.store(Linear(fb.clamp(0.0, 0.99)));
    }

    pub fn set_mix(&self, mix: f32) {
        self.mix.store(Linear(mix.clamp(0.0, 1.0)));
    }

    #[inline]
    fn snapshot_params(&self) -> StereoDelayParams {
        StereoDelayParams {
            dl: self.delay_time.l.load().get() * self.sample_rate as f32,
            dr: self.delay_time.r.load().get() * self.sample_rate as f32,
            fb: self.feedback.load().get(),
            cf: self.cross_feedback.load().get(),
            mix: self.mix.load().get(),
            interp: self.interpolation,
        }
    }

    /// Per-sample effective params for the modulated path: a present feedback /
    /// delay-time port overrides its atomic, applying the same clamp the setter
    /// uses. `cross_feedback` / `mix` are always read from their atomics. The
    /// shared delay-time port drives BOTH L and R (in samples), routed through
    /// the interpolating read. `read` reads input port `p`.
    #[inline]
    fn effective_params(&self, read: impl Fn(usize) -> f32) -> StereoDelayParams {
        let fb = self
            .feedback_port()
            .map_or_else(|| self.feedback.load().get(), |p| read(p).clamp(0.0, 0.99));
        let (dl, dr) = match self.delay_time_port() {
            Some(p) => {
                let secs = read(p).clamp(0.0, self.max_delay_secs);
                let samples = secs * self.sample_rate as f32;
                (samples, samples)
            }
            None => (
                self.delay_time.l.load().get() * self.sample_rate as f32,
                self.delay_time.r.load().get() * self.sample_rate as f32,
            ),
        };
        StereoDelayParams {
            dl,
            dr,
            fb,
            cf: self.cross_feedback.load().get(),
            mix: self.mix.load().get(),
            interp: self.interpolation,
        }
    }

    #[inline]
    fn process_sample(&mut self, in_l: f32, in_r: f32, p: &StereoDelayParams) -> (f32, f32) {
        let fb_l = self
            .delays
            .l
            .read_sample((p.dl.max(1.0) - 1.0).max(0.0), p.interp);
        let fb_r = self
            .delays
            .r
            .read_sample((p.dr.max(1.0) - 1.0).max(0.0), p.interp);

        self.delays.l.push_sample(in_l + fb_l * p.fb + fb_r * p.cf);
        self.delays.r.push_sample(in_r + fb_r * p.fb + fb_l * p.cf);

        let del_l = self.delays.l.read_sample(p.dl, p.interp);
        let del_r = self.delays.r.read_sample(p.dr, p.interp);

        (
            in_l * (1.0 - p.mix) + del_l * p.mix,
            in_r * (1.0 - p.mix) + del_r * p.mix,
        )
    }
}

struct StereoDelayParams {
    dl: f32,
    dr: f32,
    fb: f32,
    cf: f32,
    mix: f32,
    interp: InterpolationMode,
}

impl AudioUnit for StereoDelayLineNode {
    fn inputs(&self) -> usize {
        2 + self.mod_feedback as usize + self.mod_delay_time as usize
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.delays.l.reset();
        self.delays.r.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.delays.l = DelayLine::from_seconds(self.max_delay_secs, sample_rate);
        self.delays.r = DelayLine::from_seconds(self.max_delay_secs, sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Fast path: no param ports — block-rate snapshot, bit-identical to before.
        let params = if !self.mod_feedback && !self.mod_delay_time {
            self.snapshot_params()
        } else {
            self.effective_params(|p| input[p])
        };
        let (out_l, out_r) = self.process_sample(input[0], input[1], &params);
        output[0] = out_l;
        output[1] = out_r;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Fast path: no param ports — snapshot once per block, bit-identical.
        if !self.mod_feedback && !self.mod_delay_time {
            let params = self.snapshot_params();
            for i in 0..size {
                let (out_l, out_r) =
                    self.process_sample(input.at_f32(0, i), input.at_f32(1, i), &params);
                output.set_f32(0, i, out_l);
                output.set_f32(1, i, out_r);
            }
            return;
        }
        // Modulated path: read the active port(s) per sample.
        for i in 0..size {
            let params = self.effective_params(|p| input.at_f32(p, i));
            let (out_l, out_r) =
                self.process_sample(input.at_f32(0, i), input.at_f32(1, i), &params);
            output.set_f32(0, i, out_l);
            output.set_f32(1, i, out_r);
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::DelayTime => self.set_delay_time(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::STEREO_DELAY_LINE_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        let dl = (self.delay_time.l.load().get() * self.sample_rate as f32) as f64;
        let dr = (self.delay_time.r.load().get() * self.sample_rate as f32) as f64;
        out.set(0, input.at(0).delay(dl));
        out.set(1, input.at(1).delay(dr));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.delays.l.buffer.len() * core::mem::size_of::<f32>()
            + self.delays.r.buffer.len() * core::mem::size_of::<f32>()
    }
}

impl Clone for StereoDelayLineNode {
    fn clone(&self) -> Self {
        Self {
            delays: self.delays.clone(),
            delay_time: StereoPair::new(self.delay_time.l.handle(), self.delay_time.r.handle()),
            feedback: self.feedback.handle(),
            cross_feedback: self.cross_feedback.handle(),
            mix: self.mix.handle(),
            interpolation: self.interpolation,
            sample_rate: self.sample_rate,
            max_delay_secs: self.max_delay_secs,
            mod_feedback: self.mod_feedback,
            mod_delay_time: self.mod_delay_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delay_line_basic() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(0.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(2.0, InterpolationMode::None);
        assert!((val - 1.0).abs() < 0.001, "Expected 1.0, got {val}");
    }

    #[test]
    fn test_delay_line_linear_interpolation() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(0.0);
        dl.push_sample(1.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(1.5, InterpolationMode::Linear);
        assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
    }

    #[test]
    fn test_delay_line_cubic_interpolation() {
        let mut dl = DelayLine::new(10);
        for i in 0..5 {
            dl.push_sample(i as f32);
        }
        let val = dl.read_sample(1.5, InterpolationMode::CubicHermite);
        // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
        assert!(
            val > 2.0 && val < 4.0,
            "Cubic value {val} should be between recent samples"
        );
    }

    #[test]
    fn test_delay_line_reset() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(1.0);
        dl.reset();
        let val = dl.read_sample(1.0, InterpolationMode::None);
        assert!((val).abs() < 0.001, "After reset, should read 0.0");
    }

    #[test]
    fn test_delay_node_passthrough_no_feedback() {
        let mut node = DelayLineNode::new(1.0, 0.0, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(44100.0));
        node.set_mix(0.0);

        let mut output = [0.0f32];
        node.tick(&[0.5], &mut output);
        assert!(
            (output[0] - 0.5).abs() < 0.001,
            "Dry-only should pass through input"
        );
    }

    #[test]
    fn test_delay_node_echoes() {
        let sr = 1000.0;
        let delay_secs = 0.01; // 10 samples
        let mut node = DelayLineNode::new(1.0, delay_secs, 0.5);
        node.set_sample_rate(tutti_core::SampleRate(sr));

        // Send impulse
        let mut output = [0.0f32];
        node.tick(&[1.0], &mut output);

        // Advance past delay
        for _ in 0..9 {
            node.tick(&[0.0], &mut output);
        }

        // At sample 10, we should see the delayed signal
        node.tick(&[0.0], &mut output);
        assert!(
            output[0].abs() > 0.3,
            "Should hear echo at delay time, got {}",
            output[0]
        );
    }

    #[test]
    fn test_stereo_delay_independent_channels() {
        let sr = 1000.0;
        let mut node = StereoDelayLineNode::new(1.0, 0.005, 0.01, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(sr));

        let mut out = [0.0f32; 2];
        node.tick(&[1.0, 1.0], &mut out);

        // After 5 samples, left should echo; right should not yet
        for _ in 0..4 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let left_5 = out[0];

        // After 10 samples total, right should echo
        for _ in 0..4 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let right_10 = out[1];

        assert!(left_5.abs() > 0.5, "Left echo at 5 samples: {left_5}");
        assert!(right_10.abs() > 0.5, "Right echo at 10 samples: {right_10}");
    }

    #[test]
    fn test_stereo_delay_cross_feedback() {
        let sr = 1000.0;
        let mut node = StereoDelayLineNode::new(1.0, 0.01, 0.01, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(sr));
        node.set_cross_feedback(0.5);

        // Send impulse only on left
        let mut out = [0.0f32; 2];
        node.tick(&[1.0, 0.0], &mut out);

        // After delay, right channel should have cross-fed signal
        for _ in 0..9 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let right_at_delay = out[1];

        // After another delay period, right should have picked up left's cross-feedback
        for _ in 0..9 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);

        assert!(
            right_at_delay.abs() > 0.01 || out[1].abs() > 0.01,
            "Cross-feedback should produce signal in right channel"
        );
    }

    #[test]
    fn test_delay_node_footprint() {
        let node = DelayLineNode::new(2.0, 0.5, 0.5);
        assert!(node.footprint() > core::mem::size_of::<DelayLineNode>());
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    /// Tick a stereo delay sample by sample, appending `params` (the param-port
    /// values) after the two audio inputs each sample.
    fn process_stereo_delay(
        node: &mut dyn AudioUnit,
        l: &[f32],
        r: &[f32],
        params: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut out_l = vec![0.0f32; l.len()];
        let mut out_r = vec![0.0f32; r.len()];
        for i in 0..l.len() {
            let mut input = vec![l[i], r[i]];
            input.extend_from_slice(params);
            let mut output = [0.0f32; 2];
            node.tick(&input, &mut output);
            out_l[i] = output[0];
            out_r[i] = output[1];
        }
        (out_l, out_r)
    }

    #[test]
    fn stereo_delay_default_is_two_in_no_ports() {
        let u = StereoDelayLineNode::new(1.0, 0.01, 0.01, 0.5);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.feedback_port(), None);
        assert_eq!(u.delay_time_port(), None);
    }

    #[test]
    fn stereo_delay_param_port_arity_and_indices() {
        // feedback only → port 2 (delay-time absent).
        let f = StereoDelayLineNode::with_param_inputs(1.0, 0.01, 0.01, 0.5, true, false);
        assert_eq!(f.inputs(), 3);
        assert_eq!(f.feedback_port(), Some(2));
        assert_eq!(f.delay_time_port(), None);
        // delay-time only → port 2 (no feedback port before it).
        let d = StereoDelayLineNode::with_param_inputs(1.0, 0.01, 0.01, 0.5, false, true);
        assert_eq!(d.inputs(), 3);
        assert_eq!(d.feedback_port(), None);
        assert_eq!(d.delay_time_port(), Some(2));
        // both → feedback at 2, delay-time at 3.
        let b = StereoDelayLineNode::with_param_inputs(1.0, 0.01, 0.01, 0.5, true, true);
        assert_eq!(b.inputs(), 4);
        assert_eq!(b.feedback_port(), Some(2));
        assert_eq!(b.delay_time_port(), Some(3));
    }

    #[test]
    fn feedback_port_modulates() {
        // An impulse through the delay: a higher feedback via the port produces
        // a longer / louder tail than a low one. Proves the port drives the
        // feedback per sample.
        let sr = 1000.0;
        let delay_secs = 0.01; // 10 samples
        let n = 200;

        let run = |fb: f32| -> f32 {
            let mut node = StereoDelayLineNode::with_param_inputs(
                1.0, delay_secs, delay_secs, 0.0, true, false,
            );
            node.set_sample_rate(tutti_core::SampleRate(sr));
            let mut l = vec![0.0f32; n];
            let mut r = vec![0.0f32; n];
            l[0] = 1.0;
            r[0] = 1.0;
            // feedback held on port 2.
            let (out_l, _) = process_stereo_delay(&mut node, &l, &r, &[fb]);
            // Late-buffer energy (well past the first echo) — feedback governs
            // how much survives.
            out_l[50..].iter().map(|s| s * s).sum()
        };

        let low = run(0.1);
        let high = run(0.9);
        assert!(
            high > low * 2.0,
            "higher feedback via param port should yield a longer/louder tail: low={low}, high={high}"
        );
    }

    #[test]
    fn unmodulated_matches_held_constant() {
        // A modulated node whose ports are held at the atomic values must
        // produce the same output as a plain node — the modulated path is a
        // faithful superset.
        let sr = 1000.0;
        let delay_secs = 0.01;
        let fb = 0.5;
        let n = 256;

        let mut input_l = vec![0.0f32; n];
        let mut input_r = vec![0.0f32; n];
        for i in 0..n {
            input_l[i] = ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0;
            input_r[i] = ((i * 5 + 1) % 100) as f32 / 50.0 - 1.0;
        }

        let mut plain = StereoDelayLineNode::new(1.0, delay_secs, delay_secs, fb);
        plain.set_sample_rate(tutti_core::SampleRate(sr));
        let (plain_l, plain_r) = process_stereo_delay(&mut plain, &input_l, &input_r, &[]);

        // Both ports present, held at the atomic values (feedback then delay-time).
        let mut modn =
            StereoDelayLineNode::with_param_inputs(1.0, delay_secs, delay_secs, fb, true, true);
        modn.set_sample_rate(tutti_core::SampleRate(sr));
        let (mod_l, mod_r) = process_stereo_delay(&mut modn, &input_l, &input_r, &[fb, delay_secs]);

        for i in 0..n {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "L modulated-held diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
            assert!(
                (plain_r[i] - mod_r[i]).abs() < 1e-5,
                "R modulated-held diverges from plain at sample {i}: {} vs {}",
                plain_r[i],
                mod_r[i]
            );
        }
    }
}
