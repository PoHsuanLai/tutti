//! The reverb: any [`Family`] as a graph node (design 022 section 5).
//!
//! [`FamilyNode<F>`] is the one node over every reverb algorithm. It is generic
//! rather than boxed: a [`Family`] has a `Controls` type and a [`Recurrence`] of its
//! own, and erasing them would bring back a settings bag. The node holds the
//! design, its realization and the algorithm's [`Param`](tutti_types::Param) cells ([`Params`]),
//! and per block it reads the cells once, designs a [`Member`](Family::Member) when the
//! controls changed, moves the realization to it ([`Realized::retarget`]), renders, and
//! tells the graph how long the room will still ring ([`Io::report_tail`]).
//!
//! The node is wet only: its blend with its input, for use as an insert, is the
//! graph's mix (`Editor::set_mix`, doc 013 M4), which aligns the input by the
//! realization's latency (never its pre-delay, which is part of the wet sound).
//! [`FreeverbNode`] is the General MIDI send of the SoundFont player.

use core::ops::Range as Frames;

use tutti_dsp::{
    Cadence, Capacity, Channels, Effort, Family, Feed, FreeverbControls, FreeverbFamily, Grows,
    MAX_CHANNELS, NetworkCapacity, Reach, Realized, Recurrence, Ring, Setup, System, TAIL_FLOOR,
};
use tutti_graph::{
    Bus, BusRole, Buses, Cost, Cx, HasParam, HasParams, InBus, IntoNode, Io, Node, NodeParts,
    OutBus, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_types::{Latency, MaxBlock, Mix, SampleRate, Samples, Seconds, Tail};

use crate::params::{At, ControlSet, HasControl, HasControls, Params};

// The node measures its tail to `TAIL_FLOOR`: the level the wet signal has fallen to
// when the declared tail ends, and below which the node parks, 60 dB under what it
// carried, the figure the General MIDI reverb has always declared. Measured against
// the old, uncut Freeverb at `input_end + tail`, what the cut discards is more than
// 100 dB below the rendered peak: about -112 dBFS RMS after a 2 s noise burst,
// -128 dBFS after a 50 ms one, and nothing after an impulse (already flushed).

/// A tail of this many frames never ends.
const UNBOUNDED: u64 = u64::MAX;

/// What a [`FamilyNode`] declares of a family's ring-out, and whether it parks on it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rings {
    /// The family says how long it still rings ([`Family::Member`]'s
    /// [`System::tail`]): the node tracks it
    /// ([`Io::report_tail`]), cuts the realization off past it and goes idle. Only a family
    /// sampled once per block ([`NodeFamily::SAMPLING`] is [`Cadence::BLOCK`]) parks.
    Tracks,
    /// It rings, but for how long is a numeric search too dear to repeat per block (a
    /// filter): the node declares [`Tail::Unknown`] and never narrows or parks.
    Unknown,
    /// It does not ring (a memoryless clip, a gain whose state is only a gain): the node
    /// declares [`Tail::None`] and never parks. It runs on silent input, so a detector
    /// that must keep tracking a key sees every block.
    Silent,
    /// It rings for at most this long, a ceiling the family knows when it is built (the
    /// General MIDI chorus: its longest delay, since it has no feedback): the node
    /// declares [`Tail::Finite`] of it at the prepared rate and never narrows, cuts or
    /// parks (the graph's silence skip reads the ceiling). A ceiling errs long.
    Ceiling(TailCeiling),
}

/// The longest a family can ring, as a fixed ceiling ([`Rings::Ceiling`]): a time that is
/// finite and not negative.
///
/// A range-checked value (CLAUDE.md, "Range-checked values"): [`new`](Self::new) refuses a
/// NaN, an infinite or a negative time and never clamps, since the ceiling becomes the
/// [`Tail::Finite`] the graph reads, and an unchecked one would declare a tail no node has.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct TailCeiling(Seconds);

/// A refused tail ceiling: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} is not a tail ceiling: it must be finite and not negative")]
pub struct TailCeilingError(pub Seconds);

impl TailCeiling {
    /// A ceiling of `seconds`, or `None` when it is not finite or is negative. Never
    /// clamps.
    #[must_use]
    pub const fn new(seconds: Seconds) -> Option<Self> {
        let s = seconds.get();
        if s.is_finite() && s >= 0.0 {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// A ceiling for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses `seconds`.
    #[must_use]
    pub const fn of(seconds: Seconds) -> Self {
        match Self::new(seconds) {
            Some(c) => c,
            None => panic!("a tail ceiling is finite and not negative"),
        }
    }

    /// The ceiling in seconds.
    #[must_use]
    pub const fn get(self) -> Seconds {
        self.0
    }
}

impl TryFrom<Seconds> for TailCeiling {
    type Error = TailCeilingError;

    fn try_from(seconds: Seconds) -> Result<Self, TailCeilingError> {
        Self::new(seconds).ok_or(TailCeilingError(seconds))
    }
}

impl From<TailCeiling> for Seconds {
    fn from(ceiling: TailCeiling) -> Seconds {
        ceiling.0
    }
}

/// A [`Family`] a [`FamilyNode`] can run: the design plus the cells of its
/// controls.
pub trait NodeFamily: Family + 'static {
    /// The cells: the family's addresses and the conversion between its controls and
    /// the `f32`s the ports carry.
    type Set: ControlSet<Controls = Self::Controls>;

    /// What the node declares of the ring-out and whether it parks on it ([`Rings`]).
    /// A family that tracks it ([`Rings::Tracks`], the default) must be sampled once per
    /// block ([`SAMPLING`](Self::SAMPLING) is [`Cadence::BLOCK`]). It is the instance's,
    /// not the type's: a family built with no feedback can declare a ceiling where the
    /// same family with feedback declares none.
    fn rings(&self) -> Rings {
        Rings::Tracks
    }

    /// The grid on which the node reads its controls and designs: one design per
    /// block (the default), or every `n` frames when a control moves inside the
    /// block. A stream grid is counted from the last reset ([`Cadence::steps`]), so
    /// what it designs does not depend on where the blocks fall.
    /// Separate from [`Family::CADENCE`], the grid the render is a function of: a
    /// reverb whose render is a function of the stream is still designed once per
    /// block.
    const SAMPLING: Cadence = Cadence::BLOCK;

    /// The control set, for a node whose controls start at `initial`. Control side.
    fn set(&self, initial: &Self::Controls) -> Self::Set;

    /// The capacity the node's runner is prepared with at `rate`: `()` for a family
    /// that does not grow, and for a [`Grows`] one [`grown`](Self::grown). Control side.
    fn capacity(&self, rate: SampleRate) -> <Self::Recurrence as Recurrence>::Capacity;

    /// What a [`Grows`] family's [`Grows::reach`] gives at `rate`, over the family's
    /// default [`Domain`](tutti_dsp::Domain) with the tail measured to [`TAIL_FLOOR`] ([`Reach::at`]): such a
    /// family's [`capacity`](Self::capacity).
    ///
    /// # Panics
    ///
    /// For a `rate` that is not positive and finite ([`Reach::at`] refuses it; the node
    /// replaces such a rate before it asks).
    fn grown(&self, rate: SampleRate) -> <Self::Recurrence as Recurrence>::Capacity
    where
        Self: Grows,
    {
        let reach = Reach::<Self::Domain>::at(rate).expect("a positive finite rate");
        self.reach(&reach)
    }
}

/// Freeverb's controls: the room size and the damping, as positions on `0..1`.
#[derive(Clone, Copy, Debug)]
pub struct FreeverbSet;

control_set! {
    FreeverbSet(_s): FreeverbControls = FreeverbControls::GENERAL_MIDI;
    live: (all);
    RoomSize => Mix = room,
    Damping => Mix = damp;

    fn defaults(&self) -> FreeverbControls {
        FreeverbControls::GENERAL_MIDI
    }

    /// A bigger room and less damping ring longer.
    fn longest(&self, lowest: &FreeverbControls, highest: &FreeverbControls) -> FreeverbControls {
        FreeverbControls {
            room: highest.room,
            damp: lowest.damp,
        }
    }
}

impl NodeFamily for FreeverbFamily {
    type Set = FreeverbSet;

    fn set(&self, _: &FreeverbControls) -> FreeverbSet {
        FreeverbSet
    }

    fn capacity(&self, rate: SampleRate) -> NetworkCapacity {
        self.grown(rate)
    }
}

/// The Freeverb as a graph node: mono in, stereo out.
pub type FreeverbNode = FamilyNode<FreeverbFamily>;

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

/// A reverb of any [`NodeFamily`] as a graph node, [`Family::channels`] wide.
///
/// # Controls
///
/// Per block the node reads its cells once ([`Params`]), and when
/// the controls differ from the last block's it designs the [`Member`](Family::Member) **in
/// place** into one it owns ([`Family::member_into`]: a network is about 56 KB,
/// too large to build on an audio thread) and calls [`Realized::retarget`], which
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
/// [`Realized::fresh`] realization, silent (the graph's preroll warms it).
///
/// **Latency** is the member's plus the realization's, never a pre-delay. **The tail** is a ceiling
/// ([`Shape::tail`], the design's worst case at the longest decay) that the
/// node narrows each block with [`Io::report_tail`] to what its current
/// network rings, held at the longer figure until a shorter decay has been
/// heard out. Once its input has been silent for the reported tail the node
/// cuts the room off, resets the realization and parks ([`Status::Idle`]): the state
/// is below -60 dB, and an idle node is a fresh one when it wakes.
pub struct FamilyNode<F: NodeFamily> {
    family: F,
    realization: Realized<F::Recurrence>,
    params: Params<F::Set>,
    mode: Mode<F::Controls>,
    /// The model the realization was last retargeted to: designed in place.
    network: Box<F::Member>,
    /// The longest-ringing model of a modulated block, for the tail only.
    longest: Box<F::Member>,
    /// The controls `network` is the design of.
    applied: Option<F::Controls>,
    /// The tail of `network`, in frames at `rate`.
    current: u64,
    /// The ceiling: what `Shape::tail` declares, in frames.
    ceiling: u64,
    /// The processing latency the node declares: the family's own (a member's
    /// [`System::latency`], the blank's, so it does not move with a control) and what the
    /// realization adds to it ([`Realized::latency`]).
    latency: Samples,
    rate: SampleRate,
    channels: Channels,
    /// The output bus, every band of it.
    out: OutBus,
    /// The side-chain key bus of a keyed family ([`Channels::keyed`]).
    key: Option<InBus>,
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

impl<F: NodeFamily> std::fmt::Debug for FamilyNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FamilyNode")
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("ceiling", &self.ceiling)
            .field("ring", &self.ring)
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

impl<F: NodeFamily> FamilyNode<F> {
    /// A reverb of `design`, its cells at `controls`, at the default rate (the
    /// graph's `prepare` re-tunes it). The graph may modulate the cells, so
    /// its tail ceiling is the design's worst case. Allocates: build it before
    /// the node goes live.
    #[must_use]
    pub fn live(family: F, controls: F::Controls) -> Self {
        Self::build(family, controls, Mode::Live)
    }

    /// A reverb of `design` with no cells: its controls are
    /// `controls` for good, so its tail is the exact figure of that one
    /// network, not a worst case. Allocates.
    #[must_use]
    pub fn fixed(family: F, controls: F::Controls) -> Self {
        Self::build(family, controls, Mode::Fixed(controls))
    }

    fn build(family: F, controls: F::Controls, mode: Mode<F::Controls>) -> Self {
        let rate = SampleRate::DEFAULT;
        let channels = family.channels();
        let shape = ports(channels);
        let out = shape.output_bus(0).expect("a design has an output");
        let key = channels
            .key()
            .map(|_| shape.input_bus(1).expect("a keyed design's key is bus 1"));
        let realization = family.realized();
        let params = Params::new(family.set(&controls), &controls);
        let (network, longest) = (Box::new(family.blank(rate)), Box::new(family.blank(rate)));
        let mut node = Self {
            family,
            realization,
            params,
            mode,
            network,
            longest,
            applied: None,
            current: 0,
            ceiling: 0,
            latency: Samples::ZERO,
            rate,
            channels,
            out,
            key,
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
        self.rate = if rate.get().is_finite() && rate.get() > 0.0 {
            rate
        } else {
            SampleRate::DEFAULT
        };
        let capacity = self.family.capacity(self.rate);
        self.realization.prepare(
            &capacity,
            &Setup {
                rate: self.rate,
                max_block,
            },
        );
        self.applied = None;
        // Latency has one home per cause (design 024 section 2.7.2): the member's and the
        // realization's, summed here once. It is the blank member's: a latency that moved
        // with a control would be a shape the graph cannot follow (the contract's `latency`
        // case holds every member to it).
        self.latency = Samples(
            self.family
                .blank(self.rate)
                .latency()
                .get()
                .saturating_add(self.realization.latency().get()),
        );
        self.ceiling = match self.mode {
            Mode::Live => frames(capacity.tail(), self.rate),
            Mode::Fixed(controls) => {
                self.family
                    .member_into(&controls, self.rate, &mut self.network);
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
    fn apply(&mut self, controls: F::Controls) {
        self.family
            .member_into(&controls, self.rate, &mut self.network);
        self.realization.retarget(&self.network);
        if self.family.rings() == Rings::Tracks {
            self.current = frames(self.network.tail(TAIL_FLOOR), self.rate);
        }
        self.applied = Some(controls);
    }

    /// The controls the model was last designed from, for a node's own tests.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> Option<F::Controls> {
        self.applied
    }

    /// How many frames of the block may still sound, and the tail it reports:
    /// the tail owed is measured from the last non-silent input frame, the longer
    /// figure held until a shorter decay has been heard out.
    fn cut(&mut self, io: &mut Io<'_>) -> usize {
        let size = io.frames();
        let inputs = self.channels.input_width();
        let mut current = self.current;
        if matches!(self.mode, Mode::Live)
            && let Some(longest) = self.params.longest(io)
            && Some(longest) != self.applied
        {
            self.family
                .member_into(&longest, self.rate, &mut self.longest);
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

        // The latency before the tail starts (a tail excludes latency).
        let owed = if self.ring == UNBOUNDED {
            UNBOUNDED
        } else {
            self.ring.saturating_add(self.latency.get() as u64)
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

    /// Renders `frames` of the block through the realization: the main input, the
    /// key bus of a keyed family and the lanes of the controls the family reads per frame
    /// ([`ControlRead::Fast`](crate::ControlRead::Fast)), each cut to `frames`.
    fn render(&mut self, io: &mut Io<'_>, frames: Frames<usize>) {
        if frames.is_empty() {
            return;
        }
        let inputs = self.channels.input_width();
        let lanes = self.params.lanes(io);
        let (ins, mut outs) = io.split();
        let mut input: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, slot) in input.iter_mut().enumerate().take(inputs) {
            *slot = &ins.get(c)[frames.clone()];
        }
        let mut key: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        let mut keys = 0;
        if let Some(bus) = self.key {
            for (slot, ch) in key.iter_mut().zip(ins.bus(bus).channels()) {
                *slot = &ch[frames.clone()];
                keys += 1;
            }
        }
        let mut output: [&mut [f32]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        let channels = outs.bus(self.out).into_channels();
        for (slot, ch) in output.iter_mut().zip(channels) {
            *slot = &mut ch[frames.clone()];
        }
        let width = self.channels.output_width();
        let mut feed = Feed::new(&input[..inputs]).with_key(&key[..keys]);
        for (i, lane) in lanes.iter().enumerate() {
            if let Some(lane) = lane {
                feed = feed.with_lane(i, &lane[frames.clone()]);
            }
        }
        self.realization
            .process(&feed, &mut output[..width], frames.len());
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
        let span = F::SAMPLING
            .steps(position, size, applied.is_none())
            .filter(|step| step.grid_point)
            .map(|step| step.solve_at + 1)
            .last()
            .unwrap_or(size);
        let mut held = 0;
        for step in F::SAMPLING.steps(position, size, applied.is_none()) {
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

impl FamilyNode<FreeverbFamily> {
    /// The General MIDI reverb send: Freeverb at room 0.5 and damping 0.5, mono
    /// in and stereo out, wet only. Its controls are fixed, so its tail is the
    /// exact figure of that one network, and it declares no cells.
    #[must_use]
    pub fn general_midi() -> Self {
        Self::fixed(FreeverbFamily, FreeverbControls::GENERAL_MIDI)
    }
}

impl Default for FamilyNode<FreeverbFamily> {
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

/// A design's ports: its input bus (and, when it is keyed, the side-chain key bus after
/// it, as wide), and its outputs as one main bus of its bands (one, but for a split).
fn ports(channels: Channels) -> Shape {
    let bands = u8::try_from(channels.band_count()).expect("a design's bands fit a bus");
    let output =
        Bus::banded(channels.output(), BusRole::Main, bands).expect("a design has an output band");
    let main = Buses::main(channels.input());
    let inputs = match channels.key() {
        None => main,
        Some(key) => main
            .with(Bus::new(key, BusRole::Sidechain))
            .expect("a main bus and a key are two buses"),
    };
    Shape::buses(inputs, Buses::NONE.with(output).expect("one bus"))
}

impl<F: NodeFamily> Node for FamilyNode<F> {
    fn shape(&self) -> Shape {
        let addrs = match self.mode {
            Mode::Live => self.params.addrs(),
            Mode::Fixed(_) => &[],
        };
        ports(self.channels)
            .with_cost(cost(self.realization.effort()))
            .with_latency(Latency::new(self.latency))
            .with_tail(match self.family.rings() {
                Rings::Tracks => tail_of(self.ceiling),
                Rings::Unknown => Tail::Unknown,
                Rings::Silent => Tail::None,
                Rings::Ceiling(c) => Tail::Finite(c.get().to_samples_ceil(self.rate)),
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
        let moved = |applied: &F::Controls| self.params.moved(applied, &target);
        let stepped = F::SAMPLING != Cadence::BLOCK
            && matches!(self.mode, Mode::Live)
            && (self.params.driven(&io) || self.applied.as_ref().is_some_and(moved));
        if !stepped && self.applied.as_ref().is_none_or(moved) {
            self.apply(target);
        }

        let cut = if self.family.rings() == Rings::Tracks {
            self.cut(&mut io)
        } else {
            size
        };

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
            for ch in outs.bus(self.out).into_channels() {
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
/// state empty ([`Realized::fresh`]).
impl<F: NodeFamily> Clone for FamilyNode<F> {
    fn clone(&self) -> Self {
        Self {
            family: self.family.clone(),
            realization: self.realization.fresh(),
            params: self.params.clone(),
            mode: self.mode,
            network: self.network.clone(),
            longest: self.longest.clone(),
            applied: self.applied,
            current: self.current,
            ceiling: self.ceiling,
            latency: self.latency,
            rate: self.rate,
            channels: self.channels,
            out: self.out,
            key: self.key,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
            position: 0,
        }
    }
}

impl<F: NodeFamily> ParamNode for FamilyNode<F> {
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

/// A design node has the params its [`ControlSet`] declares, in the units its
/// `Controls` hold them in.
impl<F: NodeFamily, const P: u16> HasParam<P> for FamilyNode<F>
where
    F::Set: HasControl<P>,
{
    type Unit = <F::Set as HasControl<P>>::Unit;
}

impl<F: NodeFamily, const P: u16> HasParams<P> for FamilyNode<F> where F::Set: HasControls<P> {}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts from
/// the values last set through it ([`tutti_graph::param_parts`]).
impl<F: NodeFamily> IntoNode for FamilyNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tail_ceiling_tests {
    use tutti_fixtures::{RangeContract, check_range_contract};
    use tutti_types::Seconds;

    use super::TailCeiling;

    // The shape `check_range_contract` checks is serde's; `TailCeiling` has none in the
    // library (nothing persists it), so the check runs on the same `try_from` shape here.
    impl serde::Serialize for TailCeiling {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }

    impl<'de> serde::Deserialize<'de> for TailCeiling {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Self::try_from(<Seconds as serde::Deserialize>::deserialize(d)?)
                .map_err(serde::de::Error::custom)
        }
    }

    /// `TailCeiling` keeps the range-checked shape: `new` refuses a NaN, an infinite or a
    /// negative time and never clamps, `of` agrees, and `TryFrom` carries the refused raw,
    /// so no declared `Tail::Finite` comes from a time that is not one.
    #[test]
    fn a_tail_ceiling_keeps_the_range_checked_shape() {
        check_range_contract(&RangeContract {
            new: TailCeiling::new,
            get: TailCeiling::get,
            of: Some(TailCeiling::of),
            valid: &[Seconds(0.0), Seconds(0.005), Seconds(60.0)],
            invalid: &[
                Seconds(-1e-9),
                Seconds(-1.0),
                Seconds(f64::NAN),
                Seconds(f64::INFINITY),
                Seconds(f64::NEG_INFINITY),
            ],
        });
        assert_eq!(
            TailCeiling::try_from(Seconds(f64::NAN)).map_err(|e| e.0.get().is_nan()),
            Err(true)
        );
    }
}
