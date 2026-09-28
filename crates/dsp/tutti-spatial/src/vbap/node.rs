use super::error::Result;
use tutti_core::fold_frame_to_mono;
use tutti_core::ChannelLayout;
use tutti_core::{Azimuth, Elevation, Param, SampleRate, Spread, StereoWidth, Tail};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

use super::panner::{BlockGains, VbapPanner};
use crate::fork::{fresh_fork_parts, FreshFork};
use crate::layout::speaker_channel_map;
use crate::SpatialTarget;

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
/// use tutti_core::{Azimuth, Elevation, SampleRate, Samples};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_spatial::VbapPannerNode;
///
/// let panner = VbapPannerNode::quad()?;
/// panner.set_position(Azimuth(45.0), Elevation(0.0)); // front-left
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
/// built from a named preset of the underlying `vbap` crate's builder, which is
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
/// **The speaker gains always sum to unit energy** — `Σ gain² == 1.0` — for
/// every bearing, every height, every spread and every supported layout. A
/// source panned in a full circle holds a constant perceived level; it never
/// fades out and never has a hole in it.
///
/// That is stronger than textbook VBAP. VBAP places a source
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
/// Plain VBAP (as the `vbap` crate computes it) would instead lose energy on
/// a stereo pair from 45° outward and fall silent from 150° through 210°, so
/// a pan automated through 180° would make the source disappear.
///
/// **The law holds at block edges, not strictly inside a moving block.** The
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
    /// from. `None` until the first block after construction, a clone or a
    /// [`reset`](Node::reset): the ramp then starts from the smoother's
    /// current position instead, so there is no stale vector to glide from.
    ramp_from: Option<BlockGains>,
    /// The last finite azimuth, elevation, spread and width the cells held. The
    /// de-zipper smoother is recursive, so one NaN or ±∞ bearing would leave
    /// it NaN for good; a non-finite write reads as unchanged instead.
    good: [f32; 4],
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
#[derive(Clone)]
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

/// `value` if finite (and remembered in `slot`), else what `slot` last held.
#[inline]
fn hold_finite(slot: &mut f32, value: f32) -> f32 {
    if value.is_finite() {
        *slot = value;
    }
    *slot
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

        let (azimuth, elevation) = self.controls.target.load();
        let spread = self.controls.spread.load();
        new_panner.set_position(azimuth, elevation);
        new_panner.set_spread(spread);
        // The fresh panner's smoother is built at 48 kHz; without this a clone
        // of a 96 kHz node would run its 50 ms de-zipper in 25 ms until it was
        // prepared again.
        new_panner.set_sample_rate(self.sample_rate);

        Self {
            panner: new_panner,
            layout: self.layout,
            // Shared, not snapshotted: a clone is the fork's template, which
            // must read the cells as they are when the fork is taken.
            controls: self.controls.clone(),
            sample_rate: self.sample_rate,
            speaker_of_channel: self.speaker_of_channel.clone(),
            // The clone's smoother is fresh, so its ramp must start from it
            // rather than from the original's last gains.
            ramp_from: None,
            good: self.good,
        }
    }
}

impl VbapPannerNode {
    /// Creates a 2-out panner over the stereo speaker pair (±30°).
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn stereo() -> Result<Self> {
        let panner = VbapPanner::stereo()?;
        Ok(Self::from_panner(panner, ChannelLayout::STEREO))
    }

    /// Creates a 4-out panner over the quad field (FL/FR/RL/RR, no LFE).
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
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
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn surround_5_1() -> Result<Self> {
        let panner = VbapPanner::surround_5_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(6u16)))
    }

    /// Creates an 8-out panner over the 7.1 field. Like 5.1, the preset is 7.0
    /// and the LFE channel is fed separately.
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
    /// geometry is rejected.
    pub fn surround_7_1() -> Result<Self> {
        let panner = VbapPanner::surround_7_1()?;
        Ok(Self::from_panner(panner, ChannelLayout::from(8u16)))
    }

    /// Creates a 12-out panner over the 7.1.4 Atmos bed: 7.1 plus four height
    /// speakers.
    ///
    /// # Errors
    /// Returns [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
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
    /// [`VbapError::Vbap`](crate::vbap::VbapError::Vbap) if the preset
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
            good: [0.0, 0.0, Spread::POINT.get(), StereoWidth::NATURAL.get()],
        }
    }

    /// Returns the node's controls: a handle sharing its cells, the same one
    /// inserting the node hands back.
    pub fn controls(&self) -> VbapPannerControls {
        self.controls.clone()
    }

    /// Sets the source's position in degrees, lock-free. See
    /// [`VbapPannerControls::set_position`].
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.controls.set_position(azimuth, elevation);
    }

    /// Returns the commanded bearing in [`Azimuth`] degrees: the target, not
    /// the smoothed position the panner is currently at.
    pub fn azimuth(&self) -> Azimuth {
        self.controls.azimuth()
    }

    /// Returns the commanded height in [`Elevation`] degrees: the target, not
    /// the smoothed position the panner is currently at.
    pub fn elevation(&self) -> Elevation {
        self.controls.elevation()
    }

    /// Sets the VBAP diffusion, clamped to [`Spread`]'s `0..1`. See
    /// [`VbapPannerControls::set_spread`].
    pub fn set_spread(&self, spread: impl Into<Spread>) {
        self.controls.set_spread(spread);
    }

    /// Returns the current [`Spread`], `0..1`.
    pub fn spread(&self) -> Spread {
        self.controls.spread()
    }

    /// Sets the mid/side width applied to a stereo input, clamped to
    /// [`StereoWidth`]. See [`VbapPannerControls::set_width`].
    pub fn set_width(&self, width: impl Into<StereoWidth>) {
        self.controls.set_width(width);
    }

    /// Returns the current [`StereoWidth`].
    pub fn width(&self) -> StereoWidth {
        self.controls.width()
    }

    /// Returns the output channel count: the speaker layout's width.
    pub fn num_channels(&self) -> usize {
        self.layout.count() as usize
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.controls.target.load();
        let azimuth = Azimuth(hold_finite(&mut self.good[0], azimuth.get()));
        let elevation = Elevation(hold_finite(&mut self.good[1], elevation.get()));
        let spread = Spread(hold_finite(
            &mut self.good[2],
            self.controls.spread.load().get(),
        ));
        self.panner.set_position(azimuth, elevation);
        self.panner.set_spread(spread);
    }

    /// The ramp for a block of `frames`: from where the previous block ended
    /// to the gains at this block's last frame.
    ///
    /// Everything the solve depends on — target, spread, width — is read here,
    /// once per block. The end point is remembered as the next block's start,
    /// which is what keeps consecutive blocks continuous: a block boundary is
    /// a point on the ramp, never a step.
    fn block_gains(&mut self, frames: usize) -> (BlockGains, BlockGains) {
        self.sync_position();
        let width = StereoWidth(hold_finite(
            &mut self.good[3],
            self.controls.width.load().get(),
        ));
        let from = match self.ramp_from {
            Some(gains) => gains,
            None => self.panner.gains_now(width),
        };
        let to = self.panner.solve_block(width, frames);
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
/// and its **last frame is exactly `to`** — written as a subtraction of a
/// zero term, which is exact for any `from`, rather than
/// `from + (to - from) * 1.0`, which is exact only when the two are within a
/// factor of two. That is what makes a one-frame block the per-frame solve
/// bit for bit, and a block edge the exact solve.
///
/// When both ends came from the mono-fold branch the pair is folded
/// (`fold(l, r) * g`); otherwise each end is expressed as a (left,
/// right) gain pair — [`BlockGains::pair`] — so a width crossing the
/// mono-fold threshold between blocks crossfades instead of stepping.
#[inline]
fn render_channel(
    from: &BlockGains,
    to: &BlockGains,
    speaker: Option<usize>,
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
    if from.mono && to.mono {
        let (g_from, g_to) = (from.a[s], to.a[s]);
        let delta = g_to - g_from;
        for (i, ((o, &l), &r)) in out.iter_mut().zip(left).zip(right).enumerate() {
            let gain = g_to - delta * remaining(i);
            *o = fold_frame_to_mono(&[l, r]) * gain;
        }
    } else {
        let (a_from, b_from) = from.pair(s);
        let (a_to, b_to) = to.pair(s);
        let (da, db) = (a_to - a_from, b_to - b_from);
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
        Shape::audio(ChannelLayout::STEREO, self.layout).with_tail(Tail::Unknown)
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
                left,
                right,
                &mut out[..size],
            );
        }
        Status::Modified
    }

    fn reset(&mut self) {
        // `sync_position` first: the commanded position lives both in
        // `controls.target` (what `set_position` writes) and in the inner
        // panner's own cells (what the smoother is seeded from). Without the
        // join, `set_position` → `reset` would seed the ramp at the panner's
        // stale bearing and glide from there. Dropping `ramp_from` discards
        // the previous block's gains along with the ramp.
        self.sync_position();
        self.panner.reset_state();
        self.ramp_from = None;
    }
}

impl FreshFork for VbapPannerNode {
    /// A clone with its position, spread and width cells detached (at their
    /// values now) and its ramp reset. The inner panner's own target cells
    /// are already private to each clone (`Clone` builds a fresh panner).
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
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use tutti_graph::contract::{drive, prepared};

    const RATE: SampleRate = SampleRate(48_000.0);

    /// `node`, prepared at 48 kHz for blocks of up to 64 frames.
    fn prep(node: VbapPannerNode) -> VbapPannerNode {
        prepared(node, RATE, 64)
    }

    /// One block of one frame, written to `out`.
    fn tick(node: &mut VbapPannerNode, input: &[f32; 2], out: &mut [f32]) {
        tick_at(node, RATE, input, out);
    }

    fn tick_at(node: &mut VbapPannerNode, rate: SampleRate, input: &[f32; 2], out: &mut [f32]) {
        let rendered = drive(node, rate, &[&input[..1], &input[1..]], &[]);
        for (o, c) in out.iter_mut().zip(rendered) {
            *o = c[0];
        }
    }

    #[test]
    fn vbap_panner_tick() {
        let mut panner = prep(VbapPannerNode::stereo().unwrap());
        panner.set_position(0.0, 0.0);

        let input = [1.0f32, 1.0f32];
        let mut output = [0.0f32; 2];

        tick(&mut panner, &input, &mut output);

        assert!(output[0] > 0.0);
        assert!(output[1] > 0.0);
    }

    /// `Node::reset` resets time, not settings.
    ///
    /// A fork (an offline export's copy of the live graph) is reset before it
    /// renders, to drop inherited state; a host resets between clips for the
    /// same reason. A reset that re-aimed would move every spatialised
    /// source to front-centre in both cases, with nothing to compare and no
    /// error raised.
    #[test]
    fn reset_keeps_the_authored_placement() {
        let mut panner = prep(VbapPannerNode::surround_5_1().unwrap());
        panner.set_position(Azimuth(45.0), Elevation(15.0));
        panner.set_spread(Spread(0.3));
        panner.set_width(StereoWidth(1.5));

        Node::reset(&mut panner);

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
    /// smoother is seeded from. `sync_position` is the only join. A
    /// `set_position` → `tick` → `reset` sequence would pass even if `reset`
    /// skipped the join, because the leading `tick` already pushed the bearing
    /// into the panner.
    ///
    /// This test therefore resets with **no intervening tick**, which is the
    /// sequence a caller writes. The `-90°` half is
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
            let mut settled = prep(VbapPannerNode::stereo().unwrap());
            settled.set_position(Azimuth(bearing), Elevation::LEVEL);
            let mut reference = [0.0f32; 2];
            for _ in 0..48_000 {
                tick(&mut settled, &input, &mut reference);
            }
            assert!(
                reference[lead] > 0.9 && reference[silent] < 0.1,
                "the settled reference at {bearing} should be a hard pan, got {reference:?}"
            );

            // No tick between `set_position` and `reset`: the reset must find
            // the commanded bearing on its own.
            let mut panner = prep(VbapPannerNode::stereo().unwrap());
            panner.set_position(Azimuth(bearing), Elevation::LEVEL);
            Node::reset(&mut panner);

            let mut after = [0.0f32; 2];
            tick(&mut panner, &input, &mut after);
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

        let mut settled = prep(VbapPannerNode::stereo().unwrap());
        settled.set_position(Azimuth(90.0), Elevation::LEVEL);
        let mut reference = [0.0f32; 2];
        for _ in 0..48_000 {
            tick(&mut settled, &input, &mut reference);
        }

        let mut panner = prep(VbapPannerNode::stereo().unwrap());
        panner.set_position(Azimuth(90.0), Elevation::LEVEL);
        let mut mid_ramp = [0.0f32; 2];
        tick(&mut panner, &input, &mut mid_ramp);
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
        Node::reset(&mut cloned);

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
    /// detail: a clone is the fork source's template (`crate::fork`), taken at
    /// insert, so a snapshotting clone would freeze every later export at
    /// whatever spread or width happened to be set at insert and silently
    /// ignore every later move. (The fork itself detaches; see
    /// `tests/isolate_snapshots.rs`.) It is also the premise of
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

        // -- spread: its own cell.
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

    /// A clone keeps the original's sample rate, so its de-zipper ramp runs at
    /// the same speed: two fresh nodes at 96 kHz — the original and its clone —
    /// sweep to a new bearing identically.
    ///
    /// Mutation (run): dropping `new_panner.set_sample_rate(self.sample_rate)`
    /// from `Clone` leaves the clone's smoother at 48 kHz (the ramp twice as
    /// fast) and fails.
    #[test]
    fn a_clone_ramps_at_the_originals_sample_rate() {
        let rate = SampleRate(96_000.0);
        let mut original = prepared(VbapPannerNode::stereo().unwrap(), rate, 64);
        let mut clone = original.clone();
        original.set_position(Azimuth(60.0), Elevation::LEVEL);
        let (mut a, mut b) = ([0.0f32; 2], [0.0f32; 2]);
        for i in 0..2_000 {
            tick_at(&mut original, rate, &[1.0, 1.0], &mut a);
            tick_at(&mut clone, rate, &[1.0, 1.0], &mut b);
            assert_eq!(a, b, "frame {i}: the clone's ramp diverged");
        }
    }

    /// A non-finite position, spread or width never reaches the de-zipper
    /// smoother, whose state is recursive: the node keeps the last finite
    /// value, renders finite audio, and follows the next finite write.
    ///
    /// Mutation (each run, each fails): reading the azimuth, elevation, spread
    /// or width raw (no `hold_finite`) — the first two leave the smoother, and
    /// so every later frame, NaN.
    #[test]
    fn a_non_finite_control_never_reaches_the_smoother() {
        let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        for value in bad {
            for which in 0..4 {
                let mut node = prep(VbapPannerNode::surround_5_1().unwrap());
                node.set_position(Azimuth(30.0), Elevation::LEVEL);
                let (mut out, input) = ([0.0f32; 6], [0.5f32, -0.25]);
                let mut all_finite = true;
                let mut run = |node: &mut VbapPannerNode, frames: usize| {
                    for _ in 0..frames {
                        tick(node, &input, &mut out);
                        all_finite &= out.iter().all(|s| s.is_finite());
                    }
                };
                run(&mut node, 64);
                match which {
                    0 => node
                        .controls
                        .target
                        .azimuth
                        .as_atomic()
                        .store(value, Ordering::Release),
                    1 => node
                        .controls
                        .target
                        .elevation
                        .as_atomic()
                        .store(value, Ordering::Release),
                    2 => node
                        .controls
                        .spread
                        .as_atomic()
                        .store(value, Ordering::Release),
                    _ => node
                        .controls
                        .width
                        .as_atomic()
                        .store(value, Ordering::Release),
                }
                // Everything else moves in the same block.
                if which != 0 {
                    node.controls
                        .target
                        .azimuth
                        .as_atomic()
                        .store(-70.0, Ordering::Release);
                }
                if which != 2 {
                    node.controls
                        .spread
                        .as_atomic()
                        .store(0.4, Ordering::Release);
                }
                run(&mut node, 4_800);
                node.set_position(Azimuth(10.0), Elevation(5.0));
                node.set_spread(Spread(0.1));
                node.set_width(StereoWidth(1.0));
                run(&mut node, 4_800);
                assert!(all_finite, "control {which} = {value} reached the output");
            }
        }
    }

    /// The controls an insert hands back are the node's own cells: a move
    /// through them reaches the running node, which the graph owns.
    ///
    /// Mutation (run): `into_parts` hands back `VbapPannerControls::new()`
    /// (cells of its own) → the source stays front-centre, `[0.707, 0.707]`
    /// → fails.
    #[test]
    fn the_inserted_controls_move_the_running_node() {
        use tutti_graph::Solo;
        use tutti_types::Samples;

        let mut solo = Solo::new(
            VbapPannerNode::stereo().unwrap(),
            Prepare::new(RATE, Samples(64)),
        );
        solo.controls()
            .set_position(Azimuth(90.0), Elevation::LEVEL);
        // 100 ms: past the 50 ms de-zipper.
        let ones = vec![1.0f32; 4_800];
        let out = solo.render_input(&[&ones, &ones]);
        let (l, r) = (out[0][4_799], out[1][4_799]);
        assert!(
            l > 0.9 && r < 0.1,
            "a hard-left move through the controls should have landed: [{l}, {r}]"
        );
    }
}
