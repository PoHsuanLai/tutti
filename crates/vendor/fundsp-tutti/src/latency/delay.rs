//! [`PdcDelay`] — the compensation delay node.

use super::PDC_DELAY_ID;
use crate::audiounit::AudioUnit;
use crate::buffer::{BufferMut, BufferRef};
use crate::signal::SignalFrame;
use core::any;
use tutti_types::{Samples, Tail};

/// A fixed delay line, inserted automatically to align signal paths.
///
/// Const-generic over channel count, matching the `[S; CH]` frame shape used by
/// [`tutti_types::io`]. The graph picks `PdcDelay<1>` for mono edges and
/// `PdcDelay<2>` for stereo.
///
/// # Reports no latency
///
/// [`route`](AudioUnit::route) returns a bare [`SignalFrame`], so this node's
/// own delay is invisible to latency analysis. That is deliberate and
/// load-bearing: a compensation delay is not signal-path latency, it *is* the
/// correction. Were it to report its delay, the next analysis would compensate
/// for the compensation and the graph would inflate without bound.
///
/// # Not retunable
///
/// The delay is fixed at construction. Every commit removes and rebuilds all
/// compensation delays from a fresh analysis, so there is no live-retune path
/// to keep working.
pub struct PdcDelay<const CH: usize> {
    ring: Vec<[f32; CH]>,
    write: usize,
}

impl<const CH: usize> Clone for PdcDelay<CH> {
    fn clone(&self) -> Self {
        Self::new(self.delay())
    }
}

impl<const CH: usize> PdcDelay<CH> {
    pub fn new(delay: Samples) -> Self {
        Self {
            ring: vec![[0.0; CH]; delay.get()],
            write: 0,
        }
    }

    pub fn delay(&self) -> Samples {
        Samples(self.ring.len())
    }

    /// Emit the frame written `delay` frames ago, and store `frame` in its place.
    ///
    /// The read and write slots coincide: with a ring exactly `delay` long, the
    /// oldest frame is always the one about to be overwritten.
    #[inline]
    fn step(&mut self, frame: [f32; CH]) -> [f32; CH] {
        if self.ring.is_empty() {
            return frame;
        }
        let out = self.ring[self.write];
        self.ring[self.write] = frame;
        self.write = (self.write + 1) % self.ring.len();
        out
    }
}

impl<const CH: usize> AudioUnit for PdcDelay<CH> {
    fn inputs(&self) -> usize {
        CH
    }

    fn outputs(&self) -> usize {
        CH
    }

    fn reset(&mut self) {
        self.ring.fill([0.0; CH]);
        self.write = 0;
    }

    fn set_sample_rate(&mut self, _sample_rate: crate::SampleRate) {}

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let frame = std::array::from_fn(|ch| input.get(ch).copied().unwrap_or(0.0));
        let delayed = self.step(frame);
        for (slot, value) in output.iter_mut().zip(delayed) {
            *slot = value;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let delayed = self.step(std::array::from_fn(|ch| input.at_f32(ch, i)));
            for (ch, value) in delayed.into_iter().enumerate() {
                output.set_f32(ch, i, value);
            }
        }
    }

    fn get_id(&self) -> u64 {
        PDC_DELAY_ID
    }

    fn as_any(&self) -> &dyn any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(CH)
    }

    /// The ring still holds `delay` frames when the input stops, and emits them
    /// before it falls silent.
    ///
    /// Reported, unlike this node's latency: the reason `route` hides the delay
    /// is that compensating a compensation would inflate without bound, and a
    /// tail carries no such feedback — a render that keeps pulling for these
    /// frames simply collects audio that is already in the line.
    fn tail(&mut self) -> Tail {
        match self.delay() {
            Samples(0) => Tail::None,
            delay => Tail::Finite(delay),
        }
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.ring.len() * core::mem::size_of::<[f32; CH]>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tick `unit` once per input frame, collecting the outputs.
    fn run<const CH: usize>(unit: &mut PdcDelay<CH>, frames: &[[f32; CH]]) -> Vec<[f32; CH]> {
        frames
            .iter()
            .map(|frame| {
                let mut out = [0.0; CH];
                unit.tick(frame, &mut out);
                out
            })
            .collect()
    }

    #[test]
    fn zero_delay_passes_through() {
        let mut unit = PdcDelay::<2>::new(Samples(0));
        assert_eq!(
            run(&mut unit, &[[1.0, 0.5], [2.0, 1.5]]),
            vec![[1.0, 0.5], [2.0, 1.5]]
        );
    }

    #[test]
    fn stereo_delay_emits_silence_then_the_input() {
        let mut unit = PdcDelay::<2>::new(Samples(2));
        let out = run(&mut unit, &[[1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]);
        assert_eq!(
            out,
            vec![[0.0, 0.0], [0.0, 0.0], [1.0, 1.0], [2.0, 2.0]],
            "two frames of silence, then the input delayed by two"
        );
    }

    #[test]
    fn mono_delay_emits_silence_then_the_input() {
        let mut unit = PdcDelay::<1>::new(Samples(3));
        let out = run(&mut unit, &[[1.0], [2.0], [3.0], [4.0], [5.0]]);
        assert_eq!(out, vec![[0.0], [0.0], [0.0], [1.0], [2.0]]);
    }

    #[test]
    fn channels_stay_independent() {
        let mut unit = PdcDelay::<2>::new(Samples(1));
        let out = run(&mut unit, &[[1.0, -1.0], [2.0, -2.0]]);
        assert_eq!(out, vec![[0.0, 0.0], [1.0, -1.0]]);
    }

    #[test]
    fn port_count_follows_the_channel_parameter() {
        assert_eq!(
            (
                PdcDelay::<1>::new(Samples(4)).inputs(),
                PdcDelay::<1>::new(Samples(4)).outputs()
            ),
            (1, 1)
        );
        assert_eq!(
            (
                PdcDelay::<2>::new(Samples(4)).inputs(),
                PdcDelay::<2>::new(Samples(4)).outputs()
            ),
            (2, 2)
        );
    }

    #[test]
    fn reports_no_latency() {
        // Load-bearing: a delay that reported its own delay would cause the
        // next analysis to compensate for the compensation.
        let mut unit = PdcDelay::<2>::new(Samples(512));
        assert_eq!(unit.latency(), None);
    }

    #[test]
    fn reset_clears_the_ring() {
        let mut unit = PdcDelay::<1>::new(Samples(2));
        run(&mut unit, &[[1.0], [2.0]]);
        unit.reset();
        assert_eq!(run(&mut unit, &[[9.0]]), vec![[0.0]]);
    }
}
