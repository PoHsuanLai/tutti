//! [`HrtfBinauralNode`] — the binaural graph node, 2 in / 2 out.
//!
//! Construction needs a measured HRIR sphere, so [`HrtfBinauralNode::new`]
//! takes the dataset bytes.

use tutti_dsp::{HRTF_LATENCY, HRTF_STEP, HrtfBinaural, HrtfBinauralError};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::ChannelLayout;
use tutti_types::Latency;
use tutti_types::{Azimuth, Elevation, Mix, Param, SampleRate, Samples, Tail};

use crate::SpatialTarget;
use crate::fork::{FreshFork, fresh_fork_parts};
use crate::target::AngleSmoother;
use crate::target::hold_finite;

/// A binaural panner for headphones: FFT convolution against a measured HRIR
/// sphere, stereo in, stereo out.
///
/// Build it with [`new`](Self::new) from the dataset bytes. The input pair is
/// folded to mono, placed at the commanded bearing and height, and blended
/// against the (equally delayed) dry mono by [`HrtfBinauralControls::set_blend`].
/// It is a graph node (`tutti_graph::Node`); inserting it hands back its
/// [`HrtfBinauralControls`], which move the source lock-free while it renders.
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
///
/// Available with the `hrtf` feature.
#[derive(Debug)]
pub struct HrtfBinauralNode {
    panner: HrtfBinaural,
    /// The de-zipper, stepped once per convolution step: the node's, the
    /// convolver renders the direction it returns.
    aim: AngleSmoother,
    /// The position and blend cells — the same ones the
    /// [`HrtfBinauralControls`] an insert hands back write.
    controls: HrtfBinauralControls,
    sample_rate: SampleRate,
    /// The last finite azimuth and elevation the cells held. The de-zipper
    /// smoother is recursive, so one NaN or ±∞ coordinate would leave it NaN
    /// for good; a non-finite write reads as unchanged instead.
    good: LastFinite,
}

/// The last finite value each control cell held, for [`hold_finite`].
#[derive(Clone, Copy, Debug)]
struct LastFinite {
    azimuth: f32,
    elevation: f32,
    blend: f32,
}

/// The live controls of an [`HrtfBinauralNode`]: its position and its
/// HRTF/dry blend, shared with the node (and every clone of these controls).
///
/// Inserting the node hands these back ([`IntoNode::Controls`]);
/// [`HrtfBinauralNode::controls`] returns them too. Every method is lock-free
/// and may be called from any thread while the node renders; a write lands
/// on the node's next block.
///
/// Its own type rather than a [`tutti_graph::ParamSet`]: `UnitParam` has no
/// bearing or height, and a position is a pair (a bearing wraps, a height
/// saturates) that one `f32` address would split.
#[derive(Clone, Debug)]
pub struct HrtfBinauralControls {
    target: SpatialTarget,
    blend: Param<Mix>,
}

impl HrtfBinauralControls {
    /// Sets the source's position in degrees: bearing (wraps; 0 = front,
    /// 90 = left) and height (clamped to -90..90; 0 = ear level). Lock-free.
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.target.store(azimuth, elevation);
    }

    /// Returns the commanded bearing in [`Azimuth`] degrees: the target, not
    /// the smoothed direction the convolver is currently rendering.
    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    /// Returns the commanded height in [`Elevation`] degrees: the target, not
    /// the smoothed direction the convolver is currently rendering.
    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Sets the blend between the HRTF-rendered signal and the dry mono
    /// center, clamped to `0..1`: 1.0 (the default) is full HRTF, 0.0 the
    /// dry mono. Lock-free.
    ///
    /// Not to be confused with the VBAP panner's `set_width`, a mid/side
    /// [`StereoWidth`](tutti_types::StereoWidth).
    pub fn set_blend(&self, blend: impl Into<Mix>) {
        self.blend.store(Mix::new_clamped(blend.into().get()));
    }

    /// Returns the current HRTF/dry [`Mix`], `0..1`.
    pub fn blend(&self) -> Mix {
        self.blend.load()
    }

    /// Stop sharing every cell, keeping the values (see `Param::detach`):
    /// what a fork's copy does, so it renders the placement it was taken at.
    fn detach(&mut self) {
        self.target.detach();
        self.blend.detach();
    }
}

impl Clone for HrtfBinauralNode {
    /// Shares the controls (a clone is the fork's template, which must read
    /// the cells as they are when the fork is taken) and the parsed sphere;
    /// the renderer gets its own, cleared, streaming state.
    fn clone(&self) -> Self {
        Self {
            panner: self.panner.fresh(self.aim.current()),
            aim: self.aim.clone(),
            controls: self.controls.clone(),
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
            controls: HrtfBinauralControls {
                target: SpatialTarget::new(),
                blend: Param::new(Mix::WET),
            },
            sample_rate,
            good: LastFinite {
                azimuth: Azimuth::FRONT.get(),
                elevation: Elevation::LEVEL.get(),
                blend: Mix::WET.get(),
            },
        })
    }

    /// Returns the node's controls: a handle sharing its cells, the same one
    /// inserting the node hands back.
    pub fn controls(&self) -> HrtfBinauralControls {
        self.controls.clone()
    }

    /// The position cells, read once: a non-finite coordinate held at the
    /// last finite one.
    #[inline]
    fn placement(&mut self) -> (Azimuth, Elevation) {
        let (azimuth, elevation) = self.controls.target.load();
        (
            Azimuth(hold_finite(&mut self.good.azimuth, azimuth.get())),
            Elevation(hold_finite(&mut self.good.elevation, elevation.get())),
        )
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
        let target = self.placement();
        self.aim.set_sample_rate(self.sample_rate);
        let aim = &mut self.aim;
        self.panner.set_sample_rate(self.sample_rate, || {
            aim.reset_to_target(target.0, target.1);
            aim.current()
        });
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // The controls' cells are the one copy of the commanded position and
        // blend; the renderer reads them here, once per block.
        let target = self.placement();
        // `Mix::new_clamped` passes a NaN through, so the blend is held too.
        let blend = Mix(hold_finite(
            &mut self.good.blend,
            self.controls.blend.load().get(),
        ));
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
                || aim.step(target.0, target.1),
            );
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let target = self.placement();
        self.aim.reset_to_target(target.0, target.1);
        self.panner.reset_state(self.aim.current());
    }
}

impl FreshFork for HrtfBinauralNode {
    /// A clone (a rebuilt renderer) with its position and blend cells
    /// detached at their values now, and reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.controls.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`HrtfBinauralControls`], and a fork that shares
/// nothing with it and starts from the placement last set through them.
impl IntoNode for HrtfBinauralNode {
    type Controls = HrtfBinauralControls;

    fn into_parts(self) -> NodeParts<HrtfBinauralControls> {
        let controls = self.controls();
        fresh_fork_parts(self, controls)
    }
}

#[cfg(test)]
mod tests;
