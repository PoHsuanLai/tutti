use super::error::Result;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::{Azimuth, Elevation, Param, SampleRate, Spread, StereoWidth, Tail};
use tutti_types::{ChannelLayout, Fold};

use super::panner::{BlockGains, VbapPanner};
use crate::SpatialTarget;
use crate::fork::{FreshFork, fresh_fork_parts};
use crate::layout::speaker_channel_map;
use crate::target::hold_finite;

/// The last finite value each control cell held, for [`hold_finite`].
#[derive(Clone, Copy, Debug)]
struct LastFinite {
    azimuth: f32,
    elevation: f32,
    spread: f32,
    width: f32,
}

/// A VBAP loudspeaker panner: stereo in, one output per speaker of a fixed
/// layout.
///
/// Build it from a preset ([`stereo`](Self::stereo), [`quad`](Self::quad),
/// [`surround_5_1`](Self::surround_5_1), [`surround_7_1`](Self::surround_7_1),
/// [`atmos_7_1_4`](Self::atmos_7_1_4)) or from a channel count with
/// [`for_layout`](Self::for_layout). It is a graph node
/// (`tutti_graph::Node`); inserting it hands back its
/// [`VbapPannerControls`], which move the source lock-free while it renders.
/// Position changes are de-zippered over 50 ms. A mono source presents the
/// same sample on both inputs. Rendering allocates nothing and reports no
/// latency.
///
/// # Examples
///
/// ```
/// use tutti_types::{Azimuth, Elevation, SampleRate, Samples};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_spatial::VbapPannerNode;
///
/// let panner = VbapPannerNode::quad()?;
/// panner.controls().set_position(Azimuth(45.0), Elevation(0.0)); // front-left
///
/// let mut solo = Solo::new(panner, Prepare::new(SampleRate(48_000.0), Samples(64)));
/// let out = solo.render_input(&[&[1.0; 64], &[1.0; 64]]);
/// assert_eq!(out.len(), 4); // one channel per speaker
/// # Ok::<(), tutti_spatial::VbapError>(())
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
/// [`for_layout`](Self::for_layout) therefore refuses an unrecognized width with
/// [`VbapError::UnsupportedSpeakerLayout`] rather than substituting a nearby
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
/// ([`build_vbap_mix`](super::build_vbap_mix) sends it a separate low-passed
/// feed), so it reads zero and is outside the law above.
///
/// [`VbapError::UnsupportedSpeakerLayout`]: crate::vbap::VbapError::UnsupportedSpeakerLayout
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
    /// The position, spread and width cells — the same ones the
    /// [`VbapPannerControls`] an insert hands back write.
    controls: VbapPannerControls,
    sample_rate: SampleRate,
    /// Output-channel → speaker gather map, the inverse of the layout's
    /// speaker → file-channel map (see [`crate::layout`]). `None` marks a
    /// channel no speaker feeds — the LFE, which the panner leaves silent.
    /// Precomputed per layout so the RT path just indexes it, and inverted so
    /// the render can run channel-outer over each output's planar slice.
    speaker_of_channel: Vec<Option<usize>>,
    /// The gains the previous block ended on, which this block ramps away
    /// from. `None` until the first block after construction or a
    /// [`reset`](Node::reset) (which a fork runs): the ramp then starts from the smoother's
    /// current position instead, so there is no stale vector to glide from.
    ramp_from: Option<BlockGains>,
    /// The stereo input folded to mono, for the mono-fold branch. Built at
    /// construction.
    fold: Fold,
    /// The last finite azimuth, elevation, spread and width the cells held. The
    /// de-zipper smoother is recursive, so one NaN or ±∞ bearing would leave
    /// it NaN for good; a non-finite write reads as unchanged instead.
    good: LastFinite,
}

/// The live controls of a [`VbapPannerNode`]: its position, [`Spread`] and
/// [`StereoWidth`], shared with the node and every clone of these controls.
///
/// Inserting the node hands these back ([`IntoNode::Controls`]);
/// [`VbapPannerNode::controls`] returns them too. Every method is lock-free
/// and may be called from any thread while the node renders; a write lands
/// on the node's next block, which reads every cell once.
///
/// Its own type rather than a [`tutti_graph::ParamSet`]: `UnitParam` has no
/// bearing, height, spread or width to address these by, and a position is
/// a pair (a bearing wraps, a height saturates), which a single `f32` address
/// would split.
#[derive(Clone, Debug)]
pub struct VbapPannerControls {
    target: SpatialTarget,
    /// VBAP diffusion, `0..1`: how many speakers a point source is smeared
    /// across. See [`Spread`] — it is not a `Mix`, because it blends nothing.
    spread: Param<Spread>,
    /// Mid/side stereo width, `0..` — 1.0 is unchanged, above 1.0 is wider
    /// than the source. NOT an `Amplitude` despite the matching range: it
    /// scales the SIDE component against the mid. See [`StereoWidth`].
    width: Param<StereoWidth>,
}

impl VbapPannerControls {
    fn new() -> Self {
        Self {
            target: SpatialTarget::new(),
            spread: Param::new(Spread::POINT),
            width: Param::new(StereoWidth::NATURAL),
        }
    }

    /// Sets the source's position, in degrees. Lock-free.
    ///
    /// - `azimuth`: bearing, wraps onto the circle (0 = front, 90 = left, -90 = right)
    /// - `elevation`: height, clamped to -90..90 (0 = ear level, positive = up)
    ///
    /// The panner glides to the new position over 50 ms.
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.target.store(azimuth, elevation);
    }

    /// Returns the commanded bearing in [`Azimuth`] degrees: the target, not
    /// the smoothed position the panner is currently at.
    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    /// Returns the commanded height in [`Elevation`] degrees: the target, not
    /// the smoothed position the panner is currently at.
    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Sets the VBAP diffusion, clamped to [`Spread`]'s `0..1`: 0 is a point
    /// source, 1 smears it across the whole speaker field. Lock-free.
    pub fn set_spread(&self, spread: impl Into<Spread>) {
        self.spread.store(Spread::new_clamped(spread.into().get()));
    }

    /// Returns the current [`Spread`], `0..1`.
    pub fn spread(&self) -> Spread {
        self.spread.load()
    }

    /// Sets the mid/side width applied to a stereo input, clamped to
    /// [`StereoWidth`]: 0 is mono, 1 unchanged, above 1 wider than the source.
    /// Lock-free.
    pub fn set_width(&self, width: impl Into<StereoWidth>) {
        self.width
            .store(StereoWidth::new_clamped(width.into().get()));
    }

    /// Returns the current [`StereoWidth`].
    pub fn width(&self) -> StereoWidth {
        self.width.load()
    }

    /// Stop sharing every cell, keeping the values (see `Param::detach`):
    /// what a fork's copy does, so it renders the placement it was taken at.
    fn detach(&mut self) {
        self.target.detach();
        self.spread.detach();
        self.width.detach();
    }
}

impl VbapPannerNode {
    /// Creates a 2-out panner over the stereo speaker pair (±30°).
    ///
    /// # Errors
    /// Returns [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
    pub fn stereo() -> Result<Self> {
        let panner = VbapPanner::stereo()?;
        Ok(Self::from_panner(panner, ChannelLayout::STEREO))
    }

    /// Creates a 4-out panner over the quad field (FL/FR/RL/RR, no LFE).
    ///
    /// # Errors
    /// Returns [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
    pub fn quad() -> Result<Self> {
        let panner = VbapPanner::quad()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(4u16)))
    }

    /// Creates a 6-out panner over the 5.1 field. The preset is 5.0: the LFE
    /// channel is left silent here and fed separately by
    /// [`build_vbap_mix`](super::build_vbap_mix).
    ///
    /// # Errors
    /// Returns [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
    pub fn surround_5_1() -> Result<Self> {
        let panner = VbapPanner::surround_5_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(6u16)))
    }

    /// Creates an 8-out panner over the 7.1 field. Like 5.1, the preset is 7.0
    /// and the LFE channel is fed separately.
    ///
    /// # Errors
    /// Returns [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
    pub fn surround_7_1() -> Result<Self> {
        let panner = VbapPanner::surround_7_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(8u16)))
    }

    /// Creates a 12-out panner over the 7.1.4 Atmos bed: 7.1 plus four height
    /// speakers.
    ///
    /// # Errors
    /// Returns [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
    pub fn atmos_7_1_4() -> Result<Self> {
        let panner = VbapPanner::atmos_7_1_4()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(12u16)))
    }

    /// Creates a panner for a [`tutti_types::ChannelLayout`] by its channel
    /// count: 2 (stereo), 4 (quad), 6 (5.1), 8 (7.1) or 12 (7.1.4).
    ///
    /// # Errors
    /// Returns [`VbapError::UnsupportedSpeakerLayout`](crate::vbap::VbapError::UnsupportedSpeakerLayout)
    /// for a width that has no preset, and
    /// [`VbapError::Geometry`](crate::vbap::VbapError::Geometry) if the preset
    /// geometry is rejected.
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
            controls: VbapPannerControls::new(),
            sample_rate: SampleRate::SR_48K,
            speaker_of_channel: speaker_of_channel(layout),
            ramp_from: None,
            fold: Fold::widths(ChannelLayout::STEREO, ChannelLayout::MONO)
                .expect("stereo folds to mono"),
            good: LastFinite {
                azimuth: 0.0,
                elevation: 0.0,
                spread: Spread::POINT.get(),
                width: StereoWidth::NATURAL.get(),
            },
        }
    }

    /// Returns the node's controls: a handle sharing its cells, the same one
    /// inserting the node hands back.
    pub fn controls(&self) -> VbapPannerControls {
        self.controls.clone()
    }

    /// Returns the output channel count: the speaker layout's width.
    pub fn num_channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// The position and spread cells, read once: non-finite values held at
    /// the last finite one, the bearing wrapped and the height and spread
    /// clamped.
    #[inline]
    fn placement(&mut self) -> (Azimuth, Elevation, Spread) {
        let (azimuth, elevation) = self.controls.target.load();
        let azimuth = hold_finite(&mut self.good.azimuth, azimuth.get());
        let elevation = hold_finite(&mut self.good.elevation, elevation.get());
        let spread = hold_finite(&mut self.good.spread, self.controls.spread.load().get());
        (
            Azimuth(azimuth).wrap(),
            Elevation::new_clamped(elevation),
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
    fn block_gains(&mut self, frames: usize) -> (BlockGains, BlockGains) {
        let (azimuth, elevation, spread) = self.placement();
        let width = StereoWidth(hold_finite(
            &mut self.good.width,
            self.controls.width.load().get(),
        ));
        let from = match self.ramp_from {
            Some(gains) => gains,
            None => self.panner.gains_now(width, spread),
        };
        let to = self
            .panner
            .solve_block((azimuth, elevation), width, spread, frames);
        self.ramp_from = Some(to);
        (from, to)
    }
}

/// Invert the layout's speaker → file-channel map into channel → speaker.
///
/// A later speaker mapped to the same channel wins. Allocates, so it runs at
/// construction only.
fn speaker_of_channel(layout: ChannelLayout) -> Vec<Option<usize>> {
    let channels = layout.count() as usize;
    let mut inverse = vec![None; channels];
    for (speaker, &ch) in speaker_channel_map(layout).iter().enumerate() {
        if ch < channels {
            inverse[ch] = Some(speaker);
        }
    }
    inverse
}

/// Render one output channel for one block: speaker `speaker`'s gain ramped
/// linearly from `from` to `to`, applied to the stereo pair.
///
/// Frame `i` of `n` uses `to - (to - from) * (n - 1 - i) / n`, so the ramp
/// starts one step past `from` (which the previous block already played)
/// and its last frame is exactly `to` — written as a subtraction of a
/// zero term, which is exact for any `from`, rather than
/// `from + (to - from) * 1.0`, which is exact only when the two are within a
/// factor of two. That is what makes a one-frame block the per-frame solve
/// bit for bit, and a block edge the exact solve.
///
/// When both ends came from the mono-fold branch the pair is folded
/// (`mono * g`, `mono` the stereo-to-mono `fold` of each frame of `left` and
/// `right`); otherwise each end is expressed as a (left,
/// right) gain pair — [`BlockGains::pair`] — so a width crossing the
/// mono-fold threshold between blocks crossfades instead of stepping.
#[inline]
fn render_channel(
    from: &BlockGains,
    to: &BlockGains,
    speaker: Option<usize>,
    fold: &Fold,
    left: &[f32],
    right: &[f32],
    out: &mut [f32],
) {
    let Some(s) = speaker else {
        out.fill(0.0);
        return;
    };
    let n = out.len();
    let inv_n = 1.0 / n as f32;
    let remaining = |i: usize| (n - 1 - i) as f32 * inv_n;
    // A delta of exactly `+0.0` makes every ramp gain `g_to - (+0.0 * t)`,
    // which is `g_to` bit for bit (`t` is finite and non-negative; `-0.0`
    // is excluded because `-0.0 - -0.0` is `+0.0`), so a held gain skips the
    // ramp arithmetic with the same bits.
    if from.mono && to.mono {
        let (g_from, g_to) = (from.a[s], to.a[s]);
        let delta = g_to - g_from;
        let mono = |l: f32, r: f32| {
            let mut m = [0.0];
            fold.frame(&[l, r], &mut m);
            m[0]
        };
        if delta.to_bits() == 0 {
            for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                *o = mono(l, r) * g_to;
            }
            return;
        }
        for (i, ((o, &l), &r)) in out.iter_mut().zip(left).zip(right).enumerate() {
            *o = mono(l, r) * (g_to - delta * remaining(i));
        }
    } else {
        let (a_from, b_from) = from.pair(s);
        let (a_to, b_to) = to.pair(s);
        let (da, db) = (a_to - a_from, b_to - b_from);
        if da.to_bits() == 0 && db.to_bits() == 0 {
            for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                *o = l * a_to + r * b_to;
            }
            return;
        }
        for (i, ((o, &l), &r)) in out.iter_mut().zip(left).zip(right).enumerate() {
            let t = remaining(i);
            *o = l * (a_to - da * t) + r * (b_to - db * t);
        }
    }
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
        Shape::audio(ChannelLayout::STEREO, self.layout)
            .with_tail(Tail::Unknown)
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
    /// LFE; `build_vbap_mix` feeds it a separate low-passed send. A block of
    /// one frame collapses the ramp to its end point, which is the exact
    /// per-frame solve (pinned by `tests/vbap_block_gains.rs`).
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let (from, to) = self.block_gains(size);
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        for (ch, out) in outputs.iter_mut().enumerate() {
            render_channel(
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
        let (azimuth, elevation, _) = self.placement();
        self.panner.reset_state(azimuth, elevation);
        self.ramp_from = None;
    }
}

impl FreshFork for VbapPannerNode {
    /// A clone with its position, spread and width cells detached (at their
    /// values now) and its ramp reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.controls.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`VbapPannerControls`], and a fork that shares nothing
/// with it and starts from the placement last set through them.
impl IntoNode for VbapPannerNode {
    type Controls = VbapPannerControls;

    fn into_parts(self) -> NodeParts<VbapPannerControls> {
        let controls = self.controls();
        fresh_fork_parts(self, controls)
    }
}

#[cfg(test)]
mod tests;
