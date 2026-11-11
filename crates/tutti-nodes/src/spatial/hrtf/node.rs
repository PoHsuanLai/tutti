//! [`HrtfBinauralNode`] — the binaural graph node, 2 in / 2 out.
//!
//! Construction needs a measured HRIR sphere, so [`HrtfBinauralNode::new`]
//! takes the dataset bytes.

use tutti_dsp::{HRTF_LATENCY, HRTF_STEP, HrtfBinaural, HrtfBinauralError};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::Latency;
use tutti_types::{Azimuth, Direction, Elevation, Mix, Param, SampleRate, Samples, Tail};
use tutti_types::{ChannelLayout, UnitParam};

use crate::ramp::{LastGood, block_driven};
use crate::spatial::target::AngleSmoother;

/// The params an [`HrtfBinauralNode`] lets the graph modulate, in port order.
pub const HRTF_BINAURAL_PARAMS: [UnitParam; 3] =
    [UnitParam::Azimuth, UnitParam::Elevation, UnitParam::Wet];

/// A binaural panner for headphones: FFT convolution against a measured HRIR
/// sphere, stereo in, stereo out.
///
/// Build it with [`new`](Self::new) from the dataset bytes. The input pair is
/// folded to mono, placed at the commanded bearing and height, and blended
/// against the (equally delayed) dry mono by the blend ([`UnitParam::Wet`]:
/// 1.0, the default, is full HRTF, 0.0 the dry mono).
/// It is a graph node (`tutti_graph::Node` and [`ParamNode`]); inserting it
/// hands back its [`ParamSet`] ([`UnitParam::Azimuth`],
/// [`Elevation`](UnitParam::Elevation) and [`Wet`](UnitParam::Wet)), which
/// move the source lock-free while it renders, and a graph's control lanes
/// may drive all three ([`HRTF_BINAURAL_PARAMS`]).
/// Position changes are de-zippered over 50 ms, and each step of the glide
/// crossfades between the filters on either side of it. The filter for a
/// direction is blended across the sphere face that direction crosses. A
/// non-finite bearing or height (NaN, ±∞) is ignored: the source stays at the
/// last finite one, and the next finite position moves it as usual.
///
/// Output lags input by 128 samples (one convolution step), on both the wet
/// and the dry half. The node declares this as latency, so the graph's delay
/// compensation lines the other paths up with it. Rendering a block
/// allocates nothing, takes no lock and cannot panic on any sphere `new`
/// accepted; preparing at a new rate resamples the dataset and allocates.
#[derive(Debug)]
pub struct HrtfBinauralNode {
    panner: HrtfBinaural,
    /// The de-zipper, stepped once per convolution step: the node's, the
    /// convolver renders the direction it returns.
    aim: AngleSmoother,
    /// Bearing, wrapped onto the circle when read.
    azimuth: Param<Azimuth>,
    /// Height, clamped to the poles when read.
    elevation: Param<Elevation>,
    /// The blend between the HRTF-rendered signal and the dry mono centre,
    /// `0..1`. Not the VBAP panner's width, a mid/side
    /// [`StereoWidth`](tutti_types::StereoWidth).
    blend: Param<Mix>,
    sample_rate: SampleRate,
    /// The last finite azimuth and elevation the cells held. The de-zipper
    /// smoother is recursive, so one NaN or ±∞ coordinate would leave it NaN
    /// for good; a non-finite write reads as unchanged instead.
    good: Held,
}

/// The last finite value each control cell held.
#[derive(Clone, Copy, Debug)]
struct Held {
    azimuth: LastGood,
    elevation: LastGood,
    blend: LastGood,
}

impl Clone for HrtfBinauralNode {
    /// Shares the controls (a clone is the fork's template, which must read
    /// the cells as they are when the fork is taken) and the parsed sphere;
    /// the renderer gets its own, cleared, streaming state.
    fn clone(&self) -> Self {
        Self {
            panner: self.panner.fresh(self.aim.current()),
            aim: self.aim.clone(),
            azimuth: self.azimuth.clone(),
            elevation: self.elevation.clone(),
            blend: self.blend.clone(),
            sample_rate: self.sample_rate,
            good: self.good,
        }
    }
}

impl HrtfBinauralNode {
    /// Creates a renderer from HRIR sphere bytes (for example an embedded
    /// IRCAM `.bin`, in the format `hrir_sphere_builder` writes), aimed straight
    /// ahead at full HRTF blend.
    ///
    /// The sphere is parsed once and resampled to `sample_rate`. The graph
    /// prepares the node at the device rate before its first block,
    /// resampling again if that differs. Either rate is held to at most
    /// [`MAX_PREPARED_RATE`](tutti_dsp::MAX_PREPARED_RATE) for the sphere.
    /// Clones and forks share the parsed sphere. Allocates; call it on the
    /// control thread.
    ///
    /// # Errors
    ///
    /// Returns an [`HrtfBinauralError`] if the bytes are not a well-formed
    /// HRIR sphere. Every index, length and value is checked here, so nothing
    /// malformed reaches the audio thread.
    pub fn new(
        hrir_bytes: &[u8],
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self, HrtfBinauralError> {
        let sample_rate = sample_rate.into();
        let aim = AngleSmoother::new(sample_rate, Samples(HRTF_STEP));
        Ok(Self {
            panner: HrtfBinaural::new(hrir_bytes, sample_rate, aim.current())?,
            aim,
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
            blend: Param::new(Mix::WET),
            sample_rate,
            good: Held {
                azimuth: LastGood::fixed(Azimuth::FRONT.get()),
                elevation: LastGood::fixed(Elevation::LEVEL.get()),
                blend: LastGood::fixed(Mix::WET.get()),
            },
        })
    }

    /// The position cells, read once (`driven` is the block's azimuth and
    /// elevation where a lane drives them): a non-finite coordinate held at
    /// the last finite one, the bearing wrapped and the height clamped.
    #[inline]
    fn placement(&mut self, [azimuth, elevation]: [Option<f32>; 2]) -> Direction {
        let azimuth = self
            .good
            .azimuth
            .read(azimuth.unwrap_or_else(|| self.azimuth.load().get()));
        let elevation = self
            .good
            .elevation
            .read(elevation.unwrap_or_else(|| self.elevation.load().get()));
        Direction::new(Azimuth(azimuth).wrap(), Elevation::new_clamped(elevation))
    }
}

/// A graph node: stereo in, stereo out.
///
/// # Latency and tail
///
/// The step's delay, 128 samples, is declared as the node's processing
/// latency, true of both outputs at every blend, so the graph's delay
/// compensation lines the rest of the mix up with it. The tail excludes that
/// delay: it is the response length less one, a fixed size at a rate, so it
/// is exact rather than an estimate.
///
/// # Reset clears time, not placement
///
/// [`reset`](Node::reset) clears the input history, the step in flight and
/// the de-zipper ramp. Position and blend are caller-set configuration and
/// survive, and the ramp is seated on the commanded position, so the first
/// block after a reset already renders there.
impl Node for HrtfBinauralNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
            .with_params(&HRTF_BINAURAL_PARAMS)
            .with_latency(Latency::new(Samples(HRTF_LATENCY)))
            .with_tail(match self.panner.ring_out() {
                0 => Tail::None,
                n => Tail::Finite(Samples(n)),
            })
            // An exception to the nearest-class rule (doc 013, "The cost
            // model and coarsening"). The FFT work lands all at once in the
            // block that completes a 128-frame step: at 64-frame blocks that
            // block carries twice the mean, past the Heavy boundary for a
            // moving source with 512-tap HRIRs, and the schedule must price
            // the worst block, not the mean. Longer HRIRs cost more.
            .with_cost(tutti_graph::Cost::Heavy)
    }

    /// Resamples the sphere to the graph's rate (allocates; control thread).
    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        let target = self.placement([None; 2]);
        self.aim.set_sample_rate(self.sample_rate);
        let aim = &mut self.aim;
        self.panner.set_sample_rate(self.sample_rate, || {
            aim.reset_to_target(target);
            aim.current()
        });
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // The cells are the one copy of the commanded position and blend; the
        // renderer reads them here, once per block, a lane that drives one
        // overriding it at the block's last frame (`block_driven`).
        let target = self.placement([block_driven(&io, 0), block_driven(&io, 1)]);
        // `Mix::new_clamped` passes a NaN through, so the blend is held too.
        let blend = Mix(self
            .good
            .blend
            .read(block_driven(&io, 2).unwrap_or_else(|| self.blend.load().get())));
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        let mut outs = outputs.iter_mut();
        // The shape declares two outputs, so both are always present.
        if let (Some(out_left), Some(out_right)) = (outs.next(), outs.next()) {
            let aim = &mut self.aim;
            self.panner.process(
                blend,
                (left, right),
                (&mut out_left[..size], &mut out_right[..size]),
                || aim.step(target),
            );
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let target = self.placement([None; 2]);
        self.aim.reset_to_target(target);
        self.panner.reset_state(self.aim.current());
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.azimuth.load().get()),
            1 => Some(self.elevation.load().get()),
            2 => Some(self.blend.load().get()),
            _ => None,
        }
    }
}

impl ParamNode for HrtfBinauralNode {
    /// The bearing, height and blend.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Azimuth, self.azimuth.cell())
            .param(UnitParam::Elevation, self.elevation.cell())
            .param(UnitParam::Wet, self.blend.cell())
            .build()
    }

    /// A clone (a rebuilt renderer) with its three cells detached at their
    /// values now, and reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.azimuth.detach();
        fork.elevation.detach();
        fork.blend.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for HrtfBinauralNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
