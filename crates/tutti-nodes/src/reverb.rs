//! The reverb: any [`Design`] as a graph node (design 022 section 5).
//!
//! [`DesignNode<D>`] is the one node over every reverb algorithm. It is generic
//! rather than boxed: a [`Design`] has a `Controls` type and a [`Realization`] of its
//! own, and erasing them would bring back a settings bag. The node holds the
//! design, its realization and the algorithm's [`Param`] cells ([`ControlParams`]),
//! and per block it reads the cells once, designs a [`Model`](Design::Model) when the
//! controls changed, moves the realization to it ([`Realization::retarget`]), renders, and
//! tells the graph how long the room will still ring ([`Io::report_tail`]).
//!
//! The node is wet only unless its **mix** ([`UnitParam::Wet`], default 1) is
//! lowered: then it blends the dry input (aligned to the realization's latency, never
//! to its pre-delay) against the wet one, for use as an insert.
//! [`FreeverbNode`] is the General MIDI send of the SoundFont player.

use core::ops::Range as Frames;

use tutti_dsp::{
    Cadence, Ceiling, Channels, Design, Effect, Effort, FreeverbControls, FreeverbDesign,
    MAX_CHANNELS, Range, Realization, Ring, Setup, Sizes, System,
};
use tutti_graph::{
    Cost, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, ParamSetBuilder, Prepare, Shape,
    Status,
};
use tutti_types::{
    Db, Latency, MaxBlock, Mix, Param, SampleRate, Samples, Tail, UnitAddr, UnitParam,
};

use crate::blend::{Blend, DryWet};
use crate::ramp::{LastGood, block_driven};

/// The level the wet signal has fallen to when the declared tail ends, and below
/// which the node parks: 60 dB under what it carried, the figure the General MIDI
/// reverb has always declared. Measured against the old, uncut Freeverb at
/// `input_end + tail`, what the cut discards is more than 100 dB below the
/// rendered peak: about -112 dBFS RMS after a 2 s noise burst, -128 dBFS after a
/// 50 ms one, and nothing after an impulse (already flushed).
const TAIL_FLOOR: Db = Db(-60.0);

/// A tail of this many frames never ends.
const UNBOUNDED: u64 = u64::MAX;

/// Where in a block a node reads its controls, and what it last designed from.
#[derive(Clone, Copy, Debug)]
pub struct At<C> {
    /// The frame of the block to read a driven port at.
    pub frame: usize,
    /// The block's length: what a cell's glide ramps over.
    pub block: usize,
    /// The controls the model was designed from at the start of the block, where
    /// a cell's glide starts; `None` before the first design (after a prepare),
    /// when there is nothing to glide from.
    pub applied: Option<C>,
}

/// The algorithm's [`Param`] cells and how a block reads them.
///
/// `tutti-dsp` holds no `Param` (it is a library of algorithms over plain
/// values), so the cells live here, one struct per [`Design`]: the cells the
/// algorithm exposes, their addresses ([`addrs`](Self::addrs)), and the
/// function from a block's port values to the algorithm's
/// [`Controls`](Self::Controls).
pub trait ControlParams: Clone + Send + 'static {
    /// The algorithm's controls: the design's own `Controls`.
    type Controls: Copy + PartialEq + Send;

    /// The addresses of the params the graph may modulate, in port order after
    /// the blend's (the node's mix, port 0, for a [`DryWet`]), which are the node's
    /// and no algorithm's. A method,
    /// not a constant: a design that can freeze has a freeze param and one that
    /// cannot has none.
    fn addrs(&self) -> &'static [UnitAddr];

    /// Adds each cell to `set` under its address.
    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder;

    /// The base of the `k`th address ([`Node::param_base`]): its cell's value.
    fn base(&self, k: usize) -> Option<f32>;

    /// The controls at `at.frame`: each cell, or the value its port carries
    /// when the graph modulates it, read through `LastGood` so a non-finite
    /// write reads as unchanged. `first` is the port of the first address: the
    /// number of the blend's. A design sampled once per block is read at the
    /// block's last frame and ignores the rest of `at`.
    fn read(&mut self, io: &Io<'_>, first: usize, at: At<Self::Controls>) -> Self::Controls;

    /// Whether a port drives a control that [`read`](Self::read) samples
    /// per frame, so the controls can differ between frames of the block. `false`
    /// (the default) for a design read once per block.
    fn driven(&self, io: &Io<'_>, first: usize) -> bool {
        let _ = (io, first);
        false
    }

    /// The controls that ring longest among the values a modulated port takes
    /// anywhere in the block, which can differ from the last frame's: the node
    /// reports its tail from them, so a decay that falls through the block is
    /// not reported short. `None` when nothing is modulated.
    fn longest(&self, io: &Io<'_>, first: usize) -> Option<Self::Controls> {
        let _ = (io, first);
        None
    }

    /// Stops sharing the cells, keeping their values: a fork's own.
    fn detach(&mut self);
}

/// A [`Design`] a [`DesignNode`] can run: the design plus the cells of its
/// controls and what the node does with the input around it.
pub trait NodeDesign: Design<Realization: Effect<<Self as Design>::Model>> + 'static {
    /// The cells.
    type Params: ControlParams<Controls = Self::Controls>;

    /// What the node does with its input: [`DryWet`] for a reverb used as an
    /// insert, [`NoBlend`](crate::NoBlend) for a family that is only ever the
    /// wet signal.
    type Blend: Blend;

    /// Whether the node parks: it tracks how long the model still rings
    /// ([`Io::report_tail`]), cuts the realization off past it and goes idle. A
    /// design that does not declares [`Tail::Unknown`] and never narrows or parks
    /// (a filter: its ring-out is a numeric search, too dear to repeat per block).
    /// Only a design sampled once per block ([`SAMPLING`](Self::SAMPLING) is
    /// [`Cadence::BLOCK`]) parks.
    const PARKS: bool = true;

    /// The grid on which the node reads its controls and designs: one design per
    /// block (the default), or every `n` frames when a control moves inside the
    /// block. Separate from [`Design::CADENCE`], the grid the render is a function
    /// of: a reverb whose render is a function of the stream is still designed
    /// once per block.
    const SAMPLING: Cadence = Cadence::BLOCK;

    /// Cells that hold `initial`. Control side; allocates.
    fn params(&self, initial: Self::Controls) -> Self::Params;

    /// Whether `target` is far enough from `applied`, the controls the model
    /// was last designed from, to design again: the dead-band. Exact inequality by
    /// default.
    fn moved(applied: &Self::Controls, target: &Self::Controls) -> bool {
        applied != target
    }
}

/// Freeverb's cells: the room size and the damping, as positions on `0..1`.
#[derive(Clone, Debug)]
pub struct FreeverbParams {
    room: Param<Mix>,
    damp: Param<Mix>,
    good: [LastGood; 2],
}

impl ControlParams for FreeverbParams {
    type Controls = FreeverbControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        const ADDRS: [UnitAddr; 2] = [
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Damping),
        ];
        &ADDRS
    }

    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        let addrs = self.addrs();
        set.param(addrs[0], self.room.as_atomic())
            .param(addrs[1], self.damp.as_atomic())
    }

    fn base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.room.load().get()),
            1 => Some(self.damp.load().get()),
            _ => None,
        }
    }

    fn read(&mut self, io: &Io<'_>, first: usize, _: At<FreeverbControls>) -> FreeverbControls {
        let room = block_driven(io, first).unwrap_or_else(|| self.room.load().get());
        let damp = block_driven(io, first + 1).unwrap_or_else(|| self.damp.load().get());
        FreeverbControls {
            room: Mix(self.good[0].read(room)),
            damp: Mix(self.good[1].read(damp)),
        }
    }

    /// A bigger room and less damping ring longer: the highest room and the
    /// lowest damping the block's frames take.
    fn longest(&self, io: &Io<'_>, first: usize) -> Option<FreeverbControls> {
        let room = io.param(first).frames();
        let damp = io.param(first + 1).frames();
        if room.is_none() && damp.is_none() {
            return None;
        }
        let extreme = |frames: Option<&[f32]>, cell: f32, pick: fn(f32, f32) -> f32| {
            frames
                .into_iter()
                .flatten()
                .copied()
                .filter(|v| v.is_finite())
                .fold(cell, pick)
        };
        Some(FreeverbControls {
            room: Mix(extreme(room, self.room.load().get(), f32::max)),
            damp: Mix(extreme(damp, self.damp.load().get(), f32::min)),
        })
    }

    fn detach(&mut self) {
        self.room.detach();
        self.damp.detach();
    }
}

impl NodeDesign for FreeverbDesign {
    type Params = FreeverbParams;
    type Blend = DryWet;

    fn params(&self, initial: FreeverbControls) -> FreeverbParams {
        let fallback = FreeverbControls::GENERAL_MIDI;
        FreeverbParams {
            room: Param::new(initial.room),
            damp: Param::new(initial.damp),
            good: [
                LastGood::new(initial.room.get(), fallback.room.get()),
                LastGood::new(initial.damp.get(), fallback.damp.get()),
            ],
        }
    }
}

/// The Freeverb as a graph node: mono in, stereo out.
pub type FreeverbNode = DesignNode<FreeverbDesign>;

/// Whether a node's controls can change.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode<C> {
    /// The algorithm's cells are read every block and the graph may modulate
    /// them: the tail ceiling is the design's worst case.
    Live,
    /// The controls are these and never change: the node has no cells but its
    /// mix, and declares the exact tail of this one network.
    Fixed(C),
}

/// A reverb of any [`NodeDesign`] as a graph node, [`Design::channels`] wide.
///
/// # Controls
///
/// Per block the node reads its cells once ([`ControlParams::read`]), and when
/// the controls differ from the last block's it designs the [`Model`](Design::Model) **in
/// place** into one it owns ([`Design::design_into`]: a network is about 56 KB,
/// too large to build on an audio thread) and calls [`Realization::retarget`], which
/// moves the realization toward it. An unmodulated, unchanged reverb designs once.
/// The audio thread is the only place that sees a modulated value (a lane
/// writes the port, never the cell), so it is where the design runs, and it
/// allocates nothing.
///
/// # In a graph
///
/// A graph node ([`IntoNode`], through [`tutti_graph::param_parts`]): inserted,
/// its controls are a [`ParamSet`] over the mix ([`UnitParam::Wet`]) and the
/// algorithm's cells, and a fork starts from the values set with a
/// [`Realization::fresh`] realization, silent (the graph's preroll warms it).
///
/// **Latency** is the realization's, never a pre-delay. **The tail** is a ceiling
/// ([`Shape::tail`], the design's worst case at the longest decay) that the
/// node narrows each block with [`Io::report_tail`] to what its current
/// network rings, held at the longer figure until a shorter decay has been
/// heard out. Once its input has been silent for the reported tail the node
/// cuts the room off, resets the realization and parks ([`Status::Idle`]): the state
/// is below -60 dB, and an idle node is a fresh one when it wakes.
pub struct DesignNode<D: NodeDesign> {
    design: D,
    realization: D::Realization,
    params: D::Params,
    mode: Mode<D::Controls>,
    /// The dry path and its controls.
    blend: D::Blend,
    /// The model the realization was last retargeted to: designed in place.
    network: Box<D::Model>,
    /// The longest-ringing model of a modulated block, for the tail only.
    longest: Box<D::Model>,
    /// The controls `network` is the design of.
    applied: Option<D::Controls>,
    /// The tail of `network`, in frames at `rate`.
    current: u64,
    /// The ceiling: what `Shape::tail` declares, in frames.
    ceiling: u64,
    rate: SampleRate,
    channels: Channels,
    /// Frames of ring-out after the last non-silent input frame that the
    /// settings in force since may still need.
    ring: u64,
    /// Frames since the last non-silent input frame, up to the end of the
    /// previous block (`UNBOUNDED` when there never was one).
    since: u64,
    /// Whether the realization holds exactly silence (reset, nothing since).
    silent: bool,
}

impl<D: NodeDesign> std::fmt::Debug for DesignNode<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesignNode")
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("ceiling", &self.ceiling)
            .field("ring", &self.ring)
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

impl<D: NodeDesign> DesignNode<D> {
    /// A reverb of `design`, its cells at `controls`, at the default rate (the
    /// graph's `prepare` re-tunes it). The graph may modulate the cells, so
    /// its tail ceiling is the design's worst case. Allocates: build it before
    /// the node goes live.
    #[must_use]
    pub fn live(design: D, controls: D::Controls) -> Self {
        Self::build(design, controls, Mode::Live)
    }

    /// A reverb of `design` with no cells but its mix: its controls are
    /// `controls` for good, so its tail is the exact figure of that one
    /// network, not a worst case. Allocates.
    #[must_use]
    pub fn fixed(design: D, controls: D::Controls) -> Self {
        Self::build(design, controls, Mode::Fixed(controls))
    }

    fn build(design: D, controls: D::Controls, mode: Mode<D::Controls>) -> Self {
        let rate = SampleRate::DEFAULT;
        let channels = design.channels();
        let realization = design.realization();
        let params = design.params(controls);
        let (network, longest) = (Box::new(design.blank(rate)), Box::new(design.blank(rate)));
        let mut node = Self {
            design,
            realization,
            params,
            mode,
            blend: D::Blend::default(),
            network,
            longest,
            applied: None,
            current: 0,
            ceiling: 0,
            rate,
            channels,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
        };
        node.retune(rate, MaxBlock::of(Samples(1)));
        node
    }

    /// Prepares the realization for `rate` and sizes everything that depends on it.
    fn retune(&mut self, rate: SampleRate, max_block: MaxBlock) {
        let ceiling = Ceiling::new(rate, D::Range::DEFAULT, TAIL_FLOOR)
            .or_else(|| Ceiling::new(SampleRate::DEFAULT, D::Range::DEFAULT, TAIL_FLOOR))
            .expect("the default rate is positive and finite");
        let bounds = self.design.bounds(&ceiling);
        self.rate = ceiling.rate();
        self.realization.prepare(
            &bounds,
            &Setup {
                rate: self.rate,
                max_block,
            },
        );
        self.applied = None;
        self.ceiling = match self.mode {
            Mode::Live => frames(bounds.tail(), self.rate),
            Mode::Fixed(controls) => {
                self.design
                    .design_into(&controls, self.rate, &mut self.network);
                frames(self.network.tail(TAIL_FLOOR), self.rate)
            }
        };
        let inputs = usize::from(self.channels.input.count());
        self.blend
            .prepare(self.realization.latency(), inputs, max_block);
        self.clear();
    }

    /// Back to silence: no ring-out owed, no input heard.
    fn clear(&mut self) {
        self.realization.reset();
        self.blend.clear();
        self.silent = true;
        self.ring = 0;
        self.since = UNBOUNDED;
    }

    /// Designs `controls` into the node's network and retargets the realization.
    fn apply(&mut self, controls: D::Controls) {
        self.design
            .design_into(&controls, self.rate, &mut self.network);
        self.realization.retarget(&self.network);
        if D::PARKS {
            self.current = frames(self.network.tail(TAIL_FLOOR), self.rate);
        }
        self.applied = Some(controls);
    }

    /// The controls the model was last designed from, for a node's own tests.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> Option<D::Controls> {
        self.applied
    }

    /// How many frames of the block may still sound, and the tail it reports:
    /// the tail owed is measured from the last non-silent input frame, the longer
    /// figure held until a shorter decay has been heard out.
    fn cut(&mut self, io: &mut Io<'_>, first: usize) -> usize {
        let size = io.frames();
        let inputs = usize::from(self.channels.input.count());
        let mut current = self.current;
        if matches!(self.mode, Mode::Live)
            && let Some(longest) = self.params.longest(io, first)
            && Some(longest) != self.applied
        {
            self.design
                .design_into(&longest, self.rate, &mut self.longest);
            current = current.max(frames(self.longest.tail(TAIL_FLOOR), self.rate));
        }

        // A tail that was unbounded (a freeze) and is not now rings out from
        // here: the energy it held is in the loop, owed to the settings in force.
        if self.ring == UNBOUNDED && current != UNBOUNDED {
            self.ring = current;
            self.since = 0;
        }
        let last_input = (0..inputs)
            .filter_map(|c| io.input(c)[..size].iter().rposition(|&s| s != 0.0))
            .max();
        let since = self.since;
        match last_input {
            Some(i) => {
                self.ring = self.ring.saturating_sub(since.saturating_add(1 + i as u64));
                self.since = (size - 1 - i) as u64;
                self.silent = false;
            }
            None => self.since = since.saturating_add(size as u64),
        }
        self.ring = self.ring.max(current).min(self.ceiling);
        io.report_tail(tail_of(self.ring));

        // The realization's latency before the tail starts (a tail excludes
        // latency).
        let owed = if self.ring == UNBOUNDED {
            UNBOUNDED
        } else {
            self.ring
                .saturating_add(self.realization.latency().get() as u64)
        };
        let live = if owed == UNBOUNDED {
            size as u64
        } else {
            match last_input {
                Some(i) => (i as u64).saturating_add(owed).saturating_add(1),
                None => owed.saturating_sub(since),
            }
        };
        live.min(size as u64) as usize
    }

    /// Renders `frames` of the block through the realization.
    fn render(&mut self, io: &mut Io<'_>, frames: Frames<usize>) {
        if frames.is_empty() {
            return;
        }
        let inputs = usize::from(self.channels.input.count());
        let (ins, mut outs) = io.split();
        let mut input: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, slot) in input.iter_mut().enumerate().take(inputs) {
            *slot = &ins.get(c)[frames.clone()];
        }
        let mut output: [&mut [f32]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        for (slot, ch) in output.iter_mut().zip(outs.iter_mut()) {
            *slot = &mut ch[frames.clone()];
        }
        let width = usize::from(self.channels.output.count());
        self.realization
            .process(&input[..inputs], &mut output[..width]);
    }

    /// Renders the block one step of the sampling grid at a time: each step reads
    /// the controls at its solve frame, and one that moved past the dead-band of
    /// the last design designs and retargets before it renders. The steps that did not move
    /// render together, in one call, with the model held.
    fn render_steps(&mut self, io: &mut Io<'_>, first: usize) {
        let size = io.frames();
        // A cell glides from where the block began, whatever the steps designed.
        let applied = self.applied;
        let mut held = 0;
        for step in D::SAMPLING.steps(size, applied.is_none()) {
            let at = At {
                frame: step.solve_at,
                block: size,
                applied,
            };
            let target = self.params.read(io, first, at);
            if self
                .applied
                .as_ref()
                .is_none_or(|applied| D::moved(applied, &target))
            {
                self.render(io, held..step.frames.start);
                self.apply(target);
                self.render(io, step.frames.clone());
                held = step.frames.end;
            }
        }
        self.render(io, held..size);
    }
}

impl<D: NodeDesign<Blend = DryWet>> DesignNode<D> {
    /// The shared cell of the mix: `0` is the dry input alone, `1` (the default)
    /// the room alone.
    #[must_use]
    pub fn mix(&self) -> Param<Mix> {
        self.blend.mix()
    }
}

impl DesignNode<FreeverbDesign> {
    /// The General MIDI reverb send: Freeverb at room 0.5 and damping 0.5, mono
    /// in and stereo out, wet only. Its controls are fixed, so its tail is the
    /// exact figure of that one network, and it declares no cells but the mix.
    #[must_use]
    pub fn general_midi() -> Self {
        Self::fixed(FreeverbDesign, FreeverbControls::GENERAL_MIDI)
    }
}

impl Default for DesignNode<FreeverbDesign> {
    fn default() -> Self {
        Self::general_midi()
    }
}

/// A tail in whole frames at `rate`, `UNBOUNDED` when it never ends.
fn frames(tail: Ring, rate: SampleRate) -> u64 {
    match tail {
        Ring::Finite(s) if s.get().is_finite() => (s.get() * rate.get()).ceil() as u64,
        _ => UNBOUNDED,
    }
}

fn tail_of(frames: u64) -> Tail {
    if frames == UNBOUNDED {
        Tail::Unbounded
    } else {
        Tail::Finite(Samples(usize::try_from(frames).unwrap_or(usize::MAX)))
    }
}

fn cost(effort: Effort) -> Cost {
    match effort {
        Effort::Light => Cost::Light,
        Effort::Moderate => Cost::Moderate,
        Effort::Heavy => Cost::Heavy,
    }
}

impl<D: NodeDesign> Node for DesignNode<D> {
    fn shape(&self) -> Shape {
        let mut addrs = D::Blend::ADDRS.to_vec();
        if matches!(self.mode, Mode::Live) {
            addrs.extend_from_slice(self.params.addrs());
        }
        Shape::audio(self.channels.input, self.channels.output)
            .with_cost(cost(self.realization.effort()))
            .with_latency(Latency::new(self.realization.latency()))
            .with_tail(if D::PARKS {
                tail_of(self.ceiling)
            } else {
                Tail::Unknown
            })
            .with_param_addrs(&addrs)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.retune(p.sample_rate(), p.max_block());
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let first = D::Blend::ADDRS.len();

        // The controls at the block's last frame, and whether the design moves
        // inside the block: a driven port, or a cell that moved past the dead-band
        // (it glides across the block, one design per step of the grid).
        let target = match self.mode {
            Mode::Fixed(c) => c,
            Mode::Live => self.params.read(
                &io,
                first,
                At {
                    frame: size - 1,
                    block: size,
                    applied: self.applied,
                },
            ),
        };
        let moved = |applied: &D::Controls| D::moved(applied, &target);
        let stepped = D::SAMPLING != Cadence::BLOCK
            && matches!(self.mode, Mode::Live)
            && (self.params.driven(&io, first) || self.applied.as_ref().is_some_and(moved));
        if !stepped && self.applied.as_ref().is_none_or(moved) {
            self.apply(target);
        }

        let cut = if D::PARKS {
            self.cut(&mut io, first)
        } else {
            size
        };

        // Past its tail the room is cut off and parked: its state is below the
        // floor, and the input has been silent for longer than the realization's
        // latency, so the dry path holds only silence too and the output is
        // silent.
        if cut == 0 {
            if !self.silent {
                self.realization.reset();
                self.silent = true;
            }
            return Status::Idle;
        }

        // The blend reads its controls only now: an idle block above read none.
        self.blend.begin(&io);

        if stepped {
            self.render_steps(&mut io, first);
        } else {
            self.render(&mut io, 0..cut);
        }
        if cut < size {
            let (_, mut outs) = io.split();
            for ch in outs.iter_mut() {
                ch[cut..size].fill(0.0);
            }
            if !self.silent {
                self.realization.reset();
                self.silent = true;
            }
        }

        let (ins, mut outs) = io.split();
        self.blend.finish(&ins, &mut outs, size);
        Status::Modified
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        let first = D::Blend::ADDRS.len();
        match (k, self.mode) {
            (k, _) if k < first => self.blend.base(k),
            (k, Mode::Live) => self.params.base(k - first),
            _ => None,
        }
    }
}

/// A clone shares the cells and starts silent: a copy of the realization with its
/// state empty ([`Realization::fresh`]).
impl<D: NodeDesign> Clone for DesignNode<D> {
    fn clone(&self) -> Self {
        let mut copy = Self {
            design: self.design.clone(),
            realization: self.realization.fresh(),
            params: self.params.clone(),
            mode: self.mode,
            blend: self.blend.clone(),
            network: self.network.clone(),
            longest: self.longest.clone(),
            applied: self.applied,
            current: self.current,
            ceiling: self.ceiling,
            rate: self.rate,
            channels: self.channels,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
        };
        copy.blend.clear();
        copy
    }
}

impl<D: NodeDesign> ParamNode for DesignNode<D> {
    /// The blend's (the mix of a [`DryWet`]), and in a live node the algorithm's
    /// cells.
    fn param_set(&self) -> ParamSet {
        let set = self.blend.bind(ParamSet::builder());
        match self.mode {
            Mode::Live => self.params.bind(set).build(),
            Mode::Fixed(_) => set.build(),
        }
    }

    /// A clone with every cell detached (at its value now) and the room empty.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.blend.detach();
        fork.params.detach();
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts from
/// the values last set through it ([`tutti_graph::param_parts`]).
impl<D: NodeDesign> IntoNode for DesignNode<D> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}
