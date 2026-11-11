//! The reverb: any [`Design`] as a graph node (design 022 section 5).
//!
//! [`DesignNode<D>`] is the one node over every reverb algorithm. It is generic
//! rather than boxed: a [`Design`] has a `Controls` type and a [`Realization`] of its
//! own, and erasing them would bring back a settings bag. The node holds the
//! design, its realization and the algorithm's [`Param`](tutti_types::Param) cells ([`Params`]),
//! and per block it reads the cells once, designs a [`Model`](Design::Model) when the
//! controls changed, moves the realization to it ([`Realization::retarget`]), renders, and
//! tells the graph how long the room will still ring ([`Io::report_tail`]).
//!
//! The node is wet only: its blend with its input, for use as an insert, is the
//! graph's mix (`Editor::set_mix`, doc 013 M4), which aligns the input by the
//! realization's latency (never its pre-delay, which is part of the wet sound).
//! [`FreeverbNode`] is the General MIDI send of the SoundFont player.

use core::ops::Range as Frames;

use tutti_dsp::{
    Cadence, Ceiling, Channels, Design, Effect, Effort, FreeverbControls, FreeverbDesign,
    MAX_CHANNELS, Range, Realization, Ring, Setup, Sizes, System,
};
use tutti_graph::{
    BusOut, Cost, Cx, IntoNode, Io, Node, NodeParts, OutBus, ParamNode, ParamSet, Prepare, Shape,
    Status,
};
use tutti_types::{Db, Latency, MaxBlock, Mix, SampleRate, Samples, Tail, UnitAddr, UnitParam};

use crate::buses::main_bus;

use crate::params::{At, ControlSet, Params};

/// The level the wet signal has fallen to when the declared tail ends, and below
/// which the node parks: 60 dB under what it carried, the figure the General MIDI
/// reverb has always declared. Measured against the old, uncut Freeverb at
/// `input_end + tail`, what the cut discards is more than 100 dB below the
/// rendered peak: about -112 dBFS RMS after a 2 s noise burst, -128 dBFS after a
/// 50 ms one, and nothing after an impulse (already flushed).
const TAIL_FLOOR: Db = Db(-60.0);

/// A tail of this many frames never ends.
const UNBOUNDED: u64 = u64::MAX;

/// A [`Design`] a [`DesignNode`] can run: the design plus the cells of its
/// controls.
pub trait NodeDesign: Design<Realization: Effect<<Self as Design>::Model>> + 'static {
    /// The cells: the family's addresses and the conversion between its controls and
    /// the `f32`s the ports carry.
    type Set: ControlSet<Controls = Self::Controls>;

    /// Whether the node parks: it tracks how long the model still rings
    /// ([`Io::report_tail`]), cuts the realization off past it and goes idle. A
    /// design that does not declares [`Tail::Unknown`] and never narrows or parks
    /// (a filter: its ring-out is a numeric search, too dear to repeat per block).
    /// Only a design sampled once per block ([`SAMPLING`](Self::SAMPLING) is
    /// [`Cadence::BLOCK`]) parks.
    const PARKS: bool = true;

    /// The grid on which the node reads its controls and designs: one design per
    /// block (the default), or every `n` frames when a control moves inside the
    /// block. A stream grid is counted from the last reset ([`Cadence::steps`]), so
    /// what it designs does not depend on where the blocks fall.
    /// Separate from [`Design::CADENCE`], the grid the render is a function of: a
    /// reverb whose render is a function of the stream is still designed once per
    /// block.
    const SAMPLING: Cadence = Cadence::BLOCK;

    /// The control set, for a node whose controls start at `initial`. Control side.
    fn set(&self, initial: &Self::Controls) -> Self::Set;
}

/// Freeverb's controls: the room size and the damping, as positions on `0..1`.
#[derive(Clone, Copy, Debug)]
pub struct FreeverbSet;

impl ControlSet for FreeverbSet {
    type Controls = FreeverbControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        const ADDRS: [UnitAddr; 2] = [
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Damping),
        ];
        &ADDRS
    }

    fn defaults(&self) -> FreeverbControls {
        FreeverbControls::GENERAL_MIDI
    }

    fn flatten(&self, c: &FreeverbControls, out: &mut [f32]) {
        out.copy_from_slice(&[c.room.get(), c.damp.get()]);
    }

    fn assemble(&self, v: &[f32]) -> FreeverbControls {
        FreeverbControls {
            room: Mix(v[0]),
            damp: Mix(v[1]),
        }
    }

    /// A bigger room and less damping ring longer.
    fn longest(&self, lowest: &FreeverbControls, highest: &FreeverbControls) -> FreeverbControls {
        FreeverbControls {
            room: highest.room,
            damp: lowest.damp,
        }
    }
}

impl NodeDesign for FreeverbDesign {
    type Set = FreeverbSet;

    fn set(&self, _: &FreeverbControls) -> FreeverbSet {
        FreeverbSet
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
/// Per block the node reads its cells once ([`Params`]), and when
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
/// its controls are a [`ParamSet`] over the
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
    params: Params<D::Set>,
    mode: Mode<D::Controls>,
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
    /// Every output by handle in port order: the main bus, then the outputs
    /// past it (a split's bands after the first).
    outs: OutBuses,
    /// Frames of ring-out after the last non-silent input frame that the
    /// settings in force since may still need.
    ring: u64,
    /// Frames since the last non-silent input frame, up to the end of the
    /// previous block (`UNBOUNDED` when there never was one).
    since: u64,
    /// Whether the realization holds exactly silence (reset, nothing since).
    silent: bool,
    /// Frames rendered since the last reset: where a stream grid's points fall.
    position: u64,
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

    /// A reverb of `design` with no cells: its controls are
    /// `controls` for good, so its tail is the exact figure of that one
    /// network, not a worst case. Allocates.
    #[must_use]
    pub fn fixed(design: D, controls: D::Controls) -> Self {
        Self::build(design, controls, Mode::Fixed(controls))
    }

    fn build(design: D, controls: D::Controls, mode: Mode<D::Controls>) -> Self {
        let rate = SampleRate::DEFAULT;
        let channels = design.channels();
        let outs = OutBuses::of(&ports(channels));
        let realization = design.realization();
        let params = Params::new(design.set(&controls), &controls);
        let (network, longest) = (Box::new(design.blank(rate)), Box::new(design.blank(rate)));
        let mut node = Self {
            design,
            realization,
            params,
            mode,
            network,
            longest,
            applied: None,
            current: 0,
            ceiling: 0,
            rate,
            channels,
            outs,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
            position: 0,
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
        self.clear();
    }

    /// Back to silence: no ring-out owed, no input heard.
    fn clear(&mut self) {
        self.realization.reset();
        self.silent = true;
        self.ring = 0;
        self.since = UNBOUNDED;
        self.position = 0;
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
    fn cut(&mut self, io: &mut Io<'_>) -> usize {
        let size = io.frames();
        let inputs = usize::from(self.channels.input.count());
        let mut current = self.current;
        if matches!(self.mode, Mode::Live)
            && let Some(longest) = self.params.longest(io)
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
        let channels = outs.buses(self.outs.get()).flat_map(BusOut::into_channels);
        for (slot, ch) in output.iter_mut().zip(channels) {
            *slot = &mut ch[frames.clone()];
        }
        let width = usize::from(self.channels.output.count());
        self.realization
            .process(&input[..inputs], &mut output[..width]);
    }

    /// Renders the block one step of the sampling grid at a time, the block
    /// beginning at stream frame `position`: each step that begins a segment reads
    /// the controls at its solve frame, and one that moved past the dead-band of
    /// the last design designs and retargets before it renders. The steps that did
    /// not move render together, in one call, with the model held; the rest of a
    /// segment an earlier block began is rendered with the model that block set.
    fn render_steps(&mut self, io: &mut Io<'_>, position: u64) {
        let size = io.frames();
        // A cell glides from where the block began, whatever the steps designed,
        // and arrives at its value on the block's last solve.
        let applied = self.applied;
        let span = D::SAMPLING
            .steps(position, size, applied.is_none())
            .filter(|step| step.grid_point)
            .map(|step| step.solve_at + 1)
            .last()
            .unwrap_or(size);
        let mut held = 0;
        for step in D::SAMPLING.steps(position, size, applied.is_none()) {
            if !step.grid_point {
                continue;
            }
            let at = At {
                frame: step.solve_at,
                block: span,
                applied,
                rate: self.rate,
            };
            let target = self.params.read(io, at);
            if self
                .applied
                .as_ref()
                .is_none_or(|applied| self.params.moved(applied, &target))
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

impl DesignNode<FreeverbDesign> {
    /// The General MIDI reverb send: Freeverb at room 0.5 and damping 0.5, mono
    /// in and stereo out, wet only. Its controls are fixed, so its tail is the
    /// exact figure of that one network, and it declares no cells.
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

/// A design's ports: its main buses, and a split's bands past the first as
/// ports past them.
fn ports(channels: Channels) -> Shape {
    Shape::buses(main_bus(channels.input), main_bus(channels.main_output()))
        .with_ports(channels.input, channels.output)
}

/// A design's output handles in port order: the main bus, and the outputs
/// past it when there are any.
#[derive(Clone, Copy, Debug)]
struct OutBuses {
    handles: [OutBus; 2],
    count: usize,
}

impl OutBuses {
    fn of(shape: &Shape) -> Self {
        let main = shape.output_bus(0).expect("a design has an output");
        match shape.spare_out() {
            Some(rest) => Self {
                handles: [main, rest],
                count: 2,
            },
            None => Self {
                handles: [main, main],
                count: 1,
            },
        }
    }

    fn get(&self) -> &[OutBus] {
        &self.handles[..self.count]
    }
}

impl<D: NodeDesign> Node for DesignNode<D> {
    fn shape(&self) -> Shape {
        let addrs = match self.mode {
            Mode::Live => self.params.addrs(),
            Mode::Fixed(_) => &[],
        };
        ports(self.channels)
            .with_cost(cost(self.realization.effort()))
            .with_latency(Latency::new(self.realization.latency()))
            .with_tail(if D::PARKS {
                tail_of(self.ceiling)
            } else {
                Tail::Unknown
            })
            .with_param_addrs(addrs)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.retune(p.sample_rate(), p.max_block());
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let position = self.position;
        self.position += size as u64;

        // The controls at the block's last frame, and whether the design moves
        // inside the block: a driven port, or a cell that moved past the dead-band
        // (it glides across the block, one design per step of the grid).
        let target = match self.mode {
            Mode::Fixed(c) => c,
            Mode::Live => self.params.read(
                &io,
                At {
                    frame: size - 1,
                    block: size,
                    applied: self.applied,
                    rate: self.rate,
                },
            ),
        };
        let moved = |applied: &D::Controls| self.params.moved(applied, &target);
        let stepped = D::SAMPLING != Cadence::BLOCK
            && matches!(self.mode, Mode::Live)
            && (self.params.driven(&io) || self.applied.as_ref().is_some_and(moved));
        if !stepped && self.applied.as_ref().is_none_or(moved) {
            self.apply(target);
        }

        let cut = if D::PARKS { self.cut(&mut io) } else { size };

        // Past its tail the room is cut off and parked: its state is below the
        // floor, and the input has been silent for longer than the realization's
        // latency, so the output is silent.
        if cut == 0 {
            if !self.silent {
                self.realization.reset();
                self.silent = true;
            }
            return Status::Idle;
        }

        if stepped {
            self.render_steps(&mut io, position);
        } else {
            self.render(&mut io, 0..cut);
        }
        if cut < size {
            let (_, mut outs) = io.split();
            for ch in outs.buses(self.outs.get()).flat_map(BusOut::into_channels) {
                ch[cut..size].fill(0.0);
            }
            if !self.silent {
                self.realization.reset();
                self.silent = true;
            }
        }

        Status::Modified
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match self.mode {
            Mode::Live => self.params.base(k),
            Mode::Fixed(_) => None,
        }
    }
}

/// A clone shares the cells and starts silent: a copy of the realization with its
/// state empty ([`Realization::fresh`]).
impl<D: NodeDesign> Clone for DesignNode<D> {
    fn clone(&self) -> Self {
        Self {
            design: self.design.clone(),
            realization: self.realization.fresh(),
            params: self.params.clone(),
            mode: self.mode,
            network: self.network.clone(),
            longest: self.longest.clone(),
            applied: self.applied,
            current: self.current,
            ceiling: self.ceiling,
            rate: self.rate,
            channels: self.channels,
            outs: self.outs,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
            position: 0,
        }
    }
}

impl<D: NodeDesign> ParamNode for DesignNode<D> {
    /// In a live node the algorithm's cells; a fixed one has none.
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder();
        match self.mode {
            Mode::Live => self.params.bind(set).build(),
            Mode::Fixed(_) => set.build(),
        }
    }

    /// A clone with every cell detached (at its value now) and the room empty.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
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
