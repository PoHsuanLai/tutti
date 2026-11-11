use super::error::{Result, VbapError};
use tutti_dsp::{SpeakerLayout, VbapGains};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{Azimuth, Direction, Elevation, Param, SampleRate, Spread, StereoWidth, Tail};
use tutti_types::{ChannelLayout, ChannelTopology, Fold, UnitParam};

use super::panner::VbapPanner;
use crate::buses::main_bus;
use crate::ramp::{LastGood, block_driven};

/// The params a [`VbapPannerNode`] lets the graph modulate, in port order.
pub const VBAP_PANNER_PARAMS: [UnitParam; 4] = [
    UnitParam::Azimuth,
    UnitParam::Elevation,
    UnitParam::Spread,
    UnitParam::StereoWidth,
];

/// The last finite value each control cell held.
#[derive(Clone, Copy, Debug)]
struct Held {
    azimuth: LastGood,
    elevation: LastGood,
    spread: LastGood,
    width: LastGood,
}

/// A VBAP loudspeaker panner: stereo in, one output per speaker of a fixed
/// layout.
///
/// Build it from a preset ([`stereo`](Self::stereo), [`quad`](Self::quad),
/// [`surround_5_1`](Self::surround_5_1), [`surround_7_1`](Self::surround_7_1),
/// [`atmos_7_1_4`](Self::atmos_7_1_4)), or from a topology
/// ([`for_topology`](Self::for_topology), with
/// [`ChannelTopology::for_width`] for a bare count). It is a graph node
/// (`tutti_graph::Node` and [`ParamNode`]); inserting it hands back its
/// [`ParamSet`] ([`UnitParam::Azimuth`], [`Elevation`](UnitParam::Elevation),
/// [`Spread`](UnitParam::Spread) and [`StereoWidth`](UnitParam::StereoWidth)),
/// which move the source lock-free while it renders, and a graph's control
/// lanes may drive all four ([`VBAP_PANNER_PARAMS`]).
/// Position changes are de-zippered over 50 ms. A mono source presents the
/// same sample on both inputs. Rendering allocates nothing and reports no
/// latency.
///
/// # Examples
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_types::{SampleRate, Samples, UnitParam};
/// use tutti_graph::{ParamNode, Prepare, Solo};
/// use tutti_nodes::VbapPannerNode;
///
/// let panner = VbapPannerNode::quad();
/// panner.param_set().set(UnitParam::Azimuth, 45.0); // front-left
///
/// let mut solo = Solo::new(panner, Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(64))));
/// let out = solo.render_input(&[&[1.0; 64], &[1.0; 64]]);
/// assert_eq!(out.len(), 4); // one channel per speaker
/// ```
///
/// # The layouts are a closed set
///
/// Five widths are supported — stereo, quad, 5.1, 7.1 and 7.1.4 Atmos — each
/// built from a named preset of `tutti_dsp::SpeakerLayout`, which is
/// what supplies the speaker *positions*. VBAP pans across the two or three
/// speakers surrounding a direction, so it needs that arrangement and not
/// merely a channel count; a bare width does not determine one.
///
/// [`for_topology`](Self::for_topology) therefore refuses an unrecognized topology with
/// [`VbapError::NoBed`] rather than substituting a nearby
/// layout, so a caller picks a real arrangement instead of silently rendering
/// for a different one.
///
/// # The energy law
///
/// The speaker gains sum to unit energy for every bearing, height, spread and
/// supported layout, including the directions no speaker tuple surrounds;
/// [`SpeakerLayout::gains_into`](tutti_dsp::SpeakerLayout::gains_into) says how.
///
/// That law holds at block edges, not strictly inside a moving block. The
/// gains are solved once per block, at its last frame, and ramped *linearly*
/// from the previous block's solve — a chord between two unit-energy vectors,
/// not the arc between them — so while the source moves, the frames inside a
/// block sit slightly below unit energy (by `1 - cos(Δ/2)` for a gain vector
/// turning through `Δ` in one block; a 64-frame block at the 50 ms de-zipper
/// keeps `Δ` to a few degrees, a dip of well under 0.1 dB). A held source is
/// exact everywhere. Longer blocks (up to the graph's `Prepare::max_block`;
/// an export renders 1024) widen `Δ` and so deepen the dip for a source
/// moving inside one block.
///
/// Note that LFE is not one of the gains: the panner never feeds it
/// (a low-passed send, wired by the caller, feeds it), so it reads zero and is outside the law above.
///
/// [`VbapError::NoBed`]: crate::spatial::VbapError::NoBed
///
/// # Clone
///
/// A clone shares the node's control cells (so a clone taken as a fork's
/// template reads the placement as it is when the fork is taken) and copies
/// its de-zipper state. A fork detaches the cells and resets the copy.
#[derive(Clone, Debug)]
pub struct VbapPannerNode {
    panner: VbapPanner,
    layout: ChannelLayout,
    /// Bearing, wrapped onto the circle when read.
    azimuth: Param<Azimuth>,
    /// Height, clamped to the poles when read.
    elevation: Param<Elevation>,
    /// VBAP diffusion, `0..1`: how many speakers a point source is smeared
    /// across. See [`Spread`]: it is not a `Mix`, because it blends nothing.
    spread: Param<Spread>,
    /// Mid/side stereo width, `0..`: 1.0 is unchanged, above 1.0 is wider
    /// than the source. NOT an `Amplitude` despite the matching range: it
    /// scales the SIDE component against the mid. See [`StereoWidth`].
    width: Param<StereoWidth>,
    sample_rate: SampleRate,
    /// Output-channel → speaker gather map, derived from the bed's topology
    /// (see `speaker_of_channel`). `None` marks a channel no speaker feeds:
    /// the LFE, which the panner leaves silent. Precomputed per layout so the
    /// RT path just indexes it, so the render can run channel-outer over each
    /// output's planar slice.
    speaker_of_channel: Vec<Option<usize>>,
    /// The gains the previous block ended on, which this block ramps away
    /// from. `None` until the first block after construction or a
    /// [`reset`](Node::reset) (which a fork runs): the ramp then starts from the smoother's
    /// current position instead, so there is no stale vector to glide from.
    ramp_from: Option<VbapGains>,
    /// The stereo input folded to mono, for the mono-fold branch. Built at
    /// construction.
    fold: Fold,
    /// The last finite azimuth, elevation, spread and width the cells held. The
    /// de-zipper smoother is recursive, so one NaN or ±∞ bearing would leave
    /// it NaN for good; a non-finite write reads as unchanged instead.
    good: Held,
}

impl VbapPannerNode {
    /// Creates a 2-out panner over the stereo speaker pair (±30°).
    pub fn stereo() -> Self {
        Self::on_bed(ChannelLayout::STEREO, SpeakerLayout::stereo())
    }

    /// Creates a 4-out panner over the quad field (FL/FR/RL/RR, no LFE).
    pub fn quad() -> Self {
        Self::on_bed(ChannelLayout::QUAD, SpeakerLayout::quad())
    }

    /// Creates a 6-out panner over the 5.1 field. The preset is 5.0: the LFE
    /// channel is left silent here and fed separately by
    /// the caller's wiring.
    pub fn surround_5_1() -> Self {
        Self::on_bed(ChannelLayout::from(6u16), SpeakerLayout::surround_5_1())
    }

    /// Creates an 8-out panner over the 7.1 field. Like 5.1, the preset is 7.0
    /// and the LFE channel is fed separately.
    pub fn surround_7_1() -> Self {
        Self::on_bed(ChannelLayout::from(8u16), SpeakerLayout::surround_7_1())
    }

    /// Creates a 12-out panner over the 7.1.4 Atmos bed: 7.1 plus four height
    /// speakers.
    pub fn atmos_7_1_4() -> Self {
        Self::on_bed(ChannelLayout::from(12u16), SpeakerLayout::atmos_7_1_4())
    }

    /// Creates a panner for the bed `topology` names: the engine's order for
    /// 2 (stereo), 4 (quad), 6 (5.1), 8 (7.1) or 12 (7.1.4) channels.
    ///
    /// # Errors
    /// Returns [`VbapError::NoBed`] for a
    /// topology that is not one of those five beds.
    pub fn for_topology(topology: &ChannelTopology) -> Result<Self> {
        let layout = topology.layout();
        let speakers = match layout.count() {
            2 => SpeakerLayout::stereo(),
            4 => SpeakerLayout::quad(),
            6 => SpeakerLayout::surround_5_1(),
            8 => SpeakerLayout::surround_7_1(),
            12 => SpeakerLayout::atmos_7_1_4(),
            _ => return Err(VbapError::NoBed(topology.clone())),
        };
        if ChannelTopology::assumed(layout).as_ref() != Some(topology) {
            return Err(VbapError::NoBed(topology.clone()));
        }
        Ok(Self::on_bed(layout, speakers))
    }

    /// A panner over `speakers`, whose channels run in the engine's order for
    /// `layout`. The bed picks the angles; the order picks the channels.
    fn on_bed(layout: ChannelLayout, speakers: SpeakerLayout) -> Self {
        let topology = ChannelTopology::for_width(layout);
        Self {
            panner: VbapPanner::new(speakers),
            layout,
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
            spread: Param::new(Spread::POINT),
            width: Param::new(StereoWidth::NATURAL),
            sample_rate: SampleRate::SR_48K,
            speaker_of_channel: speaker_of_channel(&topology),
            ramp_from: None,
            fold: Fold::widths(ChannelLayout::STEREO, ChannelLayout::MONO)
                .expect("stereo folds to mono"),
            good: Held {
                azimuth: LastGood::fixed(Azimuth::FRONT.get()),
                elevation: LastGood::fixed(Elevation::LEVEL.get()),
                spread: LastGood::fixed(Spread::POINT.get()),
                width: LastGood::fixed(StereoWidth::NATURAL.get()),
            },
        }
    }

    /// The output channels: the speaker layout's width.
    pub fn channels(&self) -> ChannelLayout {
        self.layout
    }

    /// The position and spread cells, read once (`driven` is the block's
    /// azimuth, elevation and spread where a lane drives them): non-finite
    /// values held at the last finite one, the bearing wrapped and the height
    /// and spread clamped.
    #[inline]
    fn placement(&mut self, [azimuth, elevation, spread]: [Option<f32>; 3]) -> (Direction, Spread) {
        let azimuth = self
            .good
            .azimuth
            .read(azimuth.unwrap_or_else(|| self.azimuth.load().get()));
        let elevation = self
            .good
            .elevation
            .read(elevation.unwrap_or_else(|| self.elevation.load().get()));
        let spread = self
            .good
            .spread
            .read(spread.unwrap_or_else(|| self.spread.load().get()));
        (
            Direction::new(Azimuth(azimuth).wrap(), Elevation::new_clamped(elevation)),
            Spread::new_clamped(spread),
        )
    }

    /// The ramp for a block of `frames`: from where the previous block ended
    /// to the gains at this block's last frame.
    ///
    /// Everything the solve depends on — target, spread, width — is read here,
    /// once per block. The end point is remembered as the next block's start,
    /// which is what keeps consecutive blocks continuous: a block boundary is
    /// a point on the ramp, never a step.
    fn block_gains(&mut self, frames: usize, driven: [Option<f32>; 4]) -> (VbapGains, VbapGains) {
        let [azimuth, elevation, spread, width] = driven;
        let (aim, spread) = self.placement([azimuth, elevation, spread]);
        let width = StereoWidth(
            self.good
                .width
                .read(width.unwrap_or_else(|| self.width.load().get())),
        );
        let from = match self.ramp_from {
            Some(gains) => gains,
            None => self.panner.gains_now(width, spread),
        };
        let to = self.panner.solve_block(aim, width, spread, frames);
        self.ramp_from = Some(to);
        (from, to)
    }
}

/// Which speaker feeds each channel of the bed `topology`: the preset's
/// speakers are its channels in order with the LFE left out, so a channel's
/// speaker is the count of non-LFE channels before it, and the LFE's is `None`.
/// Allocates, so it runs at construction only.
fn speaker_of_channel(topology: &ChannelTopology) -> Vec<Option<usize>> {
    let mut next = 0;
    topology
        .positions()
        .iter()
        .map(|position| {
            (!position.is_lfe()).then(|| {
                next += 1;
                next - 1
            })
        })
        .collect()
}

/// A graph node: stereo in, the layout's width out, no latency.
///
/// # Reset clears time, not placement
///
/// [`reset`](Node::reset) clears the de-zipper ramp only. Position, spread
/// and width are caller-set configuration and survive: a host resets between
/// clips to clear a tail, and a reset that re-aimed would silently move every
/// spatialised source to front-centre — and, since clones share these cells,
/// the *live* node's source too.
///
/// The ramp is seated on the commanded position, so the first block after a
/// reset already renders there rather than gliding in from front-centre.
///
/// # Tail
///
/// [`Tail::Unknown`], deliberately not [`Tail::None`]: the executor skips a
/// `Tail::None` node on a silent block, and a skipped block would not step the
/// position de-zipper, so a source moved during silence would resume from a
/// stalled ramp instead of the settled bearing.
impl Node for VbapPannerNode {
    fn shape(&self) -> Shape {
        // A stereo pair in and the bed's speakers out, so the graph's mix and
        // bypass fold the dry pair onto the bed by speaker.
        Shape::buses(main_bus(ChannelLayout::STEREO), main_bus(self.layout))
            .with_ports(ChannelLayout::STEREO, self.layout)
            .with_tail(Tail::Unknown)
            .with_params(&VBAP_PANNER_PARAMS)
            // A ramped gain pair per output channel and frame, and the
            // smoother stepped per frame: measured 10.9 ns per frame and
            // channel at stereo, 4.3 at 5.1 while the source moves, against
            // a gain's 0.2 and `SvfFilterNode`'s 7.2 (doc 013, "The cost
            // model and coarsening"). A held source costs about 1.5 and 0.6.
            .with_cost(tutti_graph::Cost::Moderate)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        self.panner.set_sample_rate(self.sample_rate);
    }

    /// Solves the gains once for the block and ramps into them.
    ///
    /// Each output channel is rendered over its own planar slice, reading the
    /// speaker that feeds it (LFE: none, so silence). The panner never feeds
    /// LFE; the caller feeds it a separate low-passed send. A block of
    /// one frame collapses the ramp to its end point, which is the exact
    /// per-frame solve (pinned by `a_position_change_lands_in_the_next_block_as_a_ramp` in
    /// `tests/vbap_block_gains.rs`).
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        // A lane that drives a param overrides its cell for the block, at the
        // block's last frame (`block_driven`).
        let driven = [0, 1, 2, 3].map(|k| block_driven(&io, k));
        let (from, to) = self.block_gains(size, driven);
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        for (ch, out) in outputs.iter_mut().enumerate() {
            VbapGains::render_channel(
                &from,
                &to,
                self.speaker_of_channel[ch],
                &self.fold,
                left,
                right,
                &mut out[..size],
            );
        }
        Status::Modified
    }

    fn reset(&mut self) {
        // Seats the smoother on the position the cells hold now, so a
        // `set_position` → `reset` with no block between resumes there.
        // Dropping `ramp_from` discards the previous block's gains along with
        // the ramp.
        let (aim, _) = self.placement([None; 3]);
        self.panner.reset_state(aim);
        self.ramp_from = None;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.azimuth.load().get()),
            1 => Some(self.elevation.load().get()),
            2 => Some(self.spread.load().get()),
            3 => Some(self.width.load().get()),
            _ => None,
        }
    }
}

impl ParamNode for VbapPannerNode {
    /// The bearing, height, spread and width.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Azimuth, self.azimuth.cell())
            .param(UnitParam::Elevation, self.elevation.cell())
            .param(UnitParam::Spread, self.spread.cell())
            .param(UnitParam::StereoWidth, self.width.cell())
            .build()
    }

    /// A clone with its four cells detached (at their values now) and its
    /// ramp reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.azimuth.detach();
        fork.elevation.detach();
        fork.spread.detach();
        fork.width.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for VbapPannerNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
