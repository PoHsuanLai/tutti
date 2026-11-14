//! The reverb: any [`Family`] as a graph node.
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
//! graph's mix (`Editor::set_mix`), which aligns the input by the
//! realization's latency (never its pre-delay, which is part of the wet sound).
//! [`FreeverbNode`] is the General MIDI send of the SoundFont player.

use core::ops::Range as Frames;

use tutti_dsp::{
    Cadence, Capacity, Channels, Effort, Family, Feed, FreeverbControls, FreeverbFamily, Grows,
    NetworkCapacity, Reach, Realized, Recurrence, Ring, Setup, System, TAIL_FLOOR,
};
use tutti_graph::{
    Bus, BusRole, Buses, Cost, Cx, HasParam, HasParams, InBus, IntoNode, Io, Node, NodeParts,
    OutBus, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_types::{Latency, MAX_BUS_CHANNELS, MaxBlock, Mix, SampleRate, Samples, Seconds, Tail};

use crate::params::{At, ControlSet, HasControl, HasControls, Params};

// The node measures its tail to `TAIL_FLOOR`: the level the wet signal has fallen to
// when the declared tail ends, and below which the node parks, 60 dB under what it
// carried.

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
/// A range-checked value: [`new`](Self::new) refuses a
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

/// A longest delay is a ceiling: a [`MaxDelay`](tutti_dsp::MaxDelay) is finite and above
/// zero, so this cannot fail.
impl From<tutti_dsp::MaxDelay> for TailCeiling {
    fn from(max: tutti_dsp::MaxDelay) -> Self {
        Self(max.get())
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
    ///
    /// On a stream grid the node renders a block a step at a time. It stops at each grid
    /// point, reading the controls at the step's solve frame, and at each corner of a
    /// port driving a slow control ([`ParamInput::corners`](tutti_graph::ParamInput::corners)),
    /// read on that frame. A stop that moved a control past its dead-band designs and
    /// retargets before the frames after it render; stops that did not move render
    /// together with the model held. A control read every frame (a fast one) has no
    /// corner. A block cadence designs once per block and takes no corner. The node
    /// solves on the stream clock (frames since its reset), because audio runs
    /// through it while the transport is stopped.
    ///
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
    fn grown(&self, rate: SampleRate) -> <Self::Recurrence as Recurrence>::Capacity
    where
        Self: Grows,
    {
        let reach = Reach::<Self::Domain>::at(rate);
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
    rig: Rig<F>,
    /// What [`prepare`](Node::prepare) builds from the rate it is given: `None` until the
    /// node is prepared, when nothing holds a value derived from a rate.
    tuned: Option<Tuned<F>>,
}

/// What a [`FamilyNode`] is without a rate: the design, its cells and its ports.
struct Rig<F: NodeFamily> {
    family: F,
    params: Params<F::Set>,
    mode: Mode<F::Controls>,
    channels: Channels,
    /// The output bus, every band of it.
    out: OutBus,
    /// The side-chain key bus of a keyed family ([`Channels::keyed`]).
    key: Option<InBus>,
}

/// A [`FamilyNode`] prepared for one rate and block size: the realization and everything
/// derived from the rate.
struct Tuned<F: NodeFamily> {
    realization: Realized<F::Recurrence>,
    rate: SampleRate,
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
        let mut d = f.debug_struct("FamilyNode");
        d.field("channels", &self.rig.channels);
        if let Some(t) = &self.tuned {
            d.field("rate", &t.rate)
                .field("ceiling", &t.ceiling)
                .field("ring", &t.ring)
                .field("since", &t.since);
        }
        d.finish_non_exhaustive()
    }
}

impl<F: NodeFamily> FamilyNode<F> {
    /// A reverb of `design`, its cells at `controls`. The graph may modulate the cells, so
    /// its tail ceiling is the design's worst case. It has no rate until the graph's
    /// `prepare` gives it one: it declares no latency and an unknown tail before, and
    /// renders silence. Allocates: build it before the node goes live.
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
        let channels = family.channels();
        let shape = ports(channels);
        let out = shape.output_bus(0).expect("a design has an output");
        let key = channels
            .key()
            .map(|_| shape.input_bus(1).expect("a keyed design's key is bus 1"));
        let params = Params::new(family.set(&controls), &controls);
        Self {
            rig: Rig {
                family,
                params,
                mode,
                channels,
                out,
                key,
            },
            tuned: None,
        }
    }

    /// The controls the model was last designed from, for a node's own tests.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> Option<F::Controls> {
        self.tuned.as_ref().and_then(|t| t.applied)
    }
}

impl<F: NodeFamily> Tuned<F> {
    /// Prepares a realization for `rate` and sizes everything that depends on it.
    fn new(rig: &mut Rig<F>, rate: SampleRate, max_block: MaxBlock) -> Self {
        let capacity = rig.family.capacity(rate);
        let mut realization = rig.family.realized(rate);
        realization.prepare(&capacity, &Setup { rate, max_block });
        // A block cadence's realization glides its own coefficients between the designs of
        // its blocks, so the reader lands a moved cell whole there.
        rig.params.retune(rate, F::SAMPLING != Cadence::BLOCK);
        let mut network = Box::new(rig.family.blank(rate));
        let longest = Box::new(rig.family.blank(rate));
        // Latency has one home per cause: the member's and the
        // realization's, summed here once. It is the blank member's: a latency that moved
        // with a control would be a shape the graph cannot follow (the contract's `latency`
        // case holds every member to it).
        let latency = Samples(
            network
                .latency()
                .get()
                .saturating_add(realization.latency().get()),
        );
        let ceiling = match rig.mode {
            Mode::Live => frames(capacity.tail(), rate),
            Mode::Fixed(controls) => {
                rig.family.member_into(&controls, rate, &mut network);
                frames(network.tail(TAIL_FLOOR), rate)
            }
        };
        let mut tuned = Self {
            realization,
            rate,
            network,
            longest,
            applied: None,
            current: 0,
            ceiling,
            latency,
            ring: 0,
            since: UNBOUNDED,
            silent: true,
            position: 0,
        };
        tuned.clear(rig);
        tuned
    }

    /// Back to silence: no ring-out owed, no input heard.
    fn clear(&mut self, rig: &mut Rig<F>) {
        self.realization.reset();
        self.silent = true;
        self.ring = 0;
        self.since = UNBOUNDED;
        self.position = 0;
        rig.params.land();
    }

    /// Designs `controls` into the node's network and retargets the realization.
    fn apply(&mut self, rig: &Rig<F>, controls: F::Controls) {
        rig.family
            .member_into(&controls, self.rate, &mut self.network);
        self.realization.retarget(&self.network);
        if rig.family.rings() == Rings::Tracks {
            self.current = frames(self.network.tail(TAIL_FLOOR), self.rate);
        }
        self.applied = Some(controls);
    }

    /// How many frames of the block may still sound, and the tail it reports:
    /// the tail owed is measured from the last non-silent input frame, the longer
    /// figure held until a shorter decay has been heard out.
    fn cut(&mut self, rig: &Rig<F>, io: &mut Io<'_>) -> usize {
        let size = io.frames();
        let inputs = rig.channels.input_width();
        let mut current = self.current;
        if matches!(rig.mode, Mode::Live)
            && let Some(longest) = rig.params.longest(io)
            && Some(longest) != self.applied
        {
            rig.family
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
    fn render(&mut self, rig: &Rig<F>, io: &mut Io<'_>, frames: Frames<usize>) {
        if frames.is_empty() {
            return;
        }
        let inputs = rig.channels.input_width();
        let lanes = rig.params.lanes(io);
        let (ins, mut outs) = io.split();
        let mut input: [&[f32]; MAX_BUS_CHANNELS] = [&[]; MAX_BUS_CHANNELS];
        for (c, slot) in input.iter_mut().enumerate().take(inputs) {
            *slot = &ins.get(c)[frames.clone()];
        }
        let mut key: [&[f32]; MAX_BUS_CHANNELS] = [&[]; MAX_BUS_CHANNELS];
        let mut keys = 0;
        if let Some(bus) = rig.key {
            for (slot, ch) in key.iter_mut().zip(ins.bus(bus).channels()) {
                *slot = &ch[frames.clone()];
                keys += 1;
            }
        }
        let mut output: [&mut [f32]; MAX_BUS_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        let channels = outs.bus(rig.out).into_channels();
        for (slot, ch) in output.iter_mut().zip(channels) {
            *slot = &mut ch[frames.clone()];
        }
        let width = rig.channels.output_width();
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
    fn render_steps(&mut self, rig: &mut Rig<F>, io: &mut Io<'_>, position: u64) {
        // Only a stream grid steps (`process`): a block cadence designs once, at the
        // block's last frame, so a step's solve is its first frame and a corner
        // inside a step is never read before the frames it designs.
        debug_assert!(F::SAMPLING != Cadence::BLOCK, "a block cadence never steps");
        let size = io.frames();
        // The stops: each grid point (cut at its step's start, read at its solve
        // frame), and each corner of a port driving a slow control (cut and read on
        // its frame), in order.
        let mut grid = F::SAMPLING
            .steps(position, size, self.applied.is_none())
            .filter(|step| step.grid_point)
            .map(|step| (step.frames.start, step.solve_at))
            .peekable();
        let mut held = 0;
        let mut from = 0;
        loop {
            let corner = rig.params.corner_from(io, from);
            let (cut, solve_at) = match (grid.peek().copied(), corner) {
                (Some((cut, solve)), Some(c)) if cut <= c => {
                    grid.next();
                    (cut, solve)
                }
                (_, Some(c)) => (c, c),
                (Some(g), None) => {
                    grid.next();
                    g
                }
                (None, None) => break,
            };
            from = cut + 1;
            let at = At {
                position,
                frame: solve_at,
                rate: self.rate,
            };
            let target = rig.params.read(io, at);
            if self
                .applied
                .as_ref()
                .is_none_or(|applied| rig.params.moved(applied, &target))
            {
                self.render(rig, io, held..cut);
                self.apply(rig, target);
                held = cut;
            }
        }
        self.render(rig, io, held..size);
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
    /// The ports and cells always; latency, tail and cost from the prepared rate. Unprepared,
    /// a node declares no latency and a tail that is unknown (or none, for a family that
    /// does not ring), and the graph reads the rest after `prepare`.
    fn shape(&self) -> Shape {
        let rig = &self.rig;
        let addrs = match rig.mode {
            Mode::Live => rig.params.addrs(),
            Mode::Fixed(_) => &[],
        };
        let shape = ports(rig.channels).with_param_addrs(addrs);
        let Some(t) = &self.tuned else {
            return shape.with_tail(match rig.family.rings() {
                Rings::Silent => Tail::None,
                _ => Tail::Unknown,
            });
        };
        shape
            .with_cost(cost(t.realization.effort()))
            .with_latency(Latency::new(t.latency))
            .with_tail(match rig.family.rings() {
                Rings::Tracks => tail_of(t.ceiling),
                Rings::Unknown => Tail::Unknown,
                Rings::Silent => Tail::None,
                Rings::Ceiling(c) => Tail::Finite(c.get().to_samples_ceil(t.rate)),
            })
    }

    fn prepare(&mut self, p: &Prepare) {
        self.tuned = Some(Tuned::new(&mut self.rig, p.sample_rate(), p.max_block()));
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let Self { rig, tuned } = self;
        let Some(t) = tuned else {
            return Status::Silent;
        };
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let position = t.position;
        t.position += size as u64;

        // The controls at the block's last frame, and whether the design moves
        // inside the block: a driven port, or a cell that moved past the dead-band
        // (it glides across the block, one design per step of the grid).
        let target = match rig.mode {
            Mode::Fixed(c) => c,
            Mode::Live => {
                rig.params.begin(position);
                rig.params.read(
                    &io,
                    At {
                        position,
                        frame: size - 1,
                        rate: t.rate,
                    },
                )
            }
        };
        let moved = |applied: &F::Controls| rig.params.moved(applied, &target);
        let stepped = F::SAMPLING != Cadence::BLOCK
            && matches!(rig.mode, Mode::Live)
            && (rig.params.driven(&io) || t.applied.as_ref().is_some_and(moved));
        if !stepped && t.applied.as_ref().is_none_or(moved) {
            t.apply(rig, target);
        }

        let cut = if rig.family.rings() == Rings::Tracks {
            t.cut(rig, &mut io)
        } else {
            size
        };

        // Past its tail the room is cut off and parked: its state is below the
        // floor, and the input has been silent for longer than the realization's
        // latency, so the output is silent.
        if cut == 0 {
            if !t.silent {
                t.realization.reset();
                t.silent = true;
            }
            return Status::Idle;
        }

        if stepped {
            t.render_steps(rig, &mut io, position);
        } else {
            t.render(rig, &mut io, 0..cut);
        }
        if cut < size {
            let (_, mut outs) = io.split();
            for ch in outs.bus(rig.out).into_channels() {
                ch[cut..size].fill(0.0);
            }
            if !t.silent {
                t.realization.reset();
                t.silent = true;
            }
        }

        Status::Modified
    }

    fn reset(&mut self) {
        if let Some(t) = &mut self.tuned {
            t.clear(&mut self.rig);
        }
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match self.rig.mode {
            Mode::Live => self.rig.params.base(k),
            Mode::Fixed(_) => None,
        }
    }
}

/// A clone shares the cells and starts silent: a copy of the realization with its
/// state empty ([`Realized::fresh`]).
impl<F: NodeFamily> Clone for FamilyNode<F> {
    fn clone(&self) -> Self {
        Self {
            rig: Rig {
                family: self.rig.family.clone(),
                params: self.rig.params.clone(),
                mode: self.rig.mode,
                channels: self.rig.channels,
                out: self.rig.out,
                key: self.rig.key,
            },
            tuned: self.tuned.as_ref().map(|t| Tuned {
                realization: t.realization.fresh(),
                rate: t.rate,
                network: t.network.clone(),
                longest: t.longest.clone(),
                applied: t.applied,
                current: t.current,
                ceiling: t.ceiling,
                latency: t.latency,
                ring: 0,
                since: UNBOUNDED,
                silent: true,
                position: 0,
            }),
        }
    }
}

impl<F: NodeFamily> ParamNode for FamilyNode<F> {
    /// In a live node the algorithm's cells; a fixed one has none.
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder();
        match self.rig.mode {
            Mode::Live => self.rig.params.bind(set).build(),
            Mode::Fixed(_) => set.build(),
        }
    }

    /// A clone with every cell detached (at its value now) and the room empty.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.rig.params.detach();
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
