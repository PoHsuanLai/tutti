//! The reverb: any [`Design`] as a graph node (design 022 section 5).
//!
//! [`DesignNode<D>`] is the one node over every reverb algorithm. It is generic
//! rather than boxed: a [`Design`] has a `Controls` type and a [`Realization`] of its
//! own, and erasing them would bring back a settings bag. The node holds the
//! design, its realization and the algorithm's [`Param`] cells ([`ControlParams`]),
//! and per block it reads the cells once, designs a [`Network`] when the
//! controls changed, moves the realization to it ([`Realization::retarget`]), renders, and
//! tells the graph how long the room will still ring ([`Io::report_tail`]).
//!
//! The node is wet only unless its **mix** ([`UnitParam::Wet`], default 1) is
//! lowered: then it blends the dry input (aligned to the realization's latency, never
//! to its pre-delay) against the wet one, for use as an insert.
//! [`FreeverbNode`] is the General MIDI send of the SoundFont player.

use tutti_dsp::{
    Ceiling, Channels, Design, Effect, Effort, FreeverbControls, FreeverbDesign, MAX_CHANNELS,
    Ramp, Range, Realization, Ring, Setup, Sizes, System,
};
use tutti_graph::{
    Cost, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, ParamSetBuilder, Prepare, Shape,
    Status,
};
use tutti_types::{
    Db, Latency, MaxBlock, Mix, Param, SampleRate, Samples, Tail, UnitAddr, UnitParam,
};

use crate::dry_align::DryAlign;
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
    /// the node's mix (port 0), which is the node's and no algorithm's. A method,
    /// not a constant: a design that can freeze has a freeze param and one that
    /// cannot has none.
    fn addrs(&self) -> &'static [UnitAddr];

    /// Adds each cell to `set` under its address.
    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder;

    /// The base of the `k`th address ([`Node::param_base`]): its cell's value.
    fn base(&self, k: usize) -> Option<f32>;

    /// The controls for this block: each cell, or the value its port carries
    /// when the graph modulates it (the block's last frame), read through
    /// `LastGood` so a non-finite write reads as unchanged. `first` is the
    /// port of the first address.
    fn read(&mut self, io: &Io<'_>, first: usize) -> Self::Controls;

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
/// controls.
pub trait NodeDesign: Design<Realization: Effect<<Self as Design>::Model>> + 'static {
    /// The cells.
    type Params: ControlParams<Controls = Self::Controls>;

    /// Cells that hold `initial`. Control side; allocates.
    fn params(&self, initial: Self::Controls) -> Self::Params;
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

    fn read(&mut self, io: &Io<'_>, first: usize) -> FreeverbControls {
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
/// the controls differ from the last block's it designs the [`Network`] **in
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
    mix: Param<Mix>,
    good_mix: LastGood,
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
    dry: Vec<DryAlign>,
    dry_buf: Vec<Vec<f32>>,
    /// The mix the previous block ended on.
    last_mix: Option<f32>,
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
    pub fn new(design: D, controls: D::Controls) -> Self {
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
            mix: Param::new(Mix::WET),
            good_mix: LastGood::fixed(Mix::WET.get()),
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
            dry: Vec::new(),
            dry_buf: Vec::new(),
            last_mix: None,
        };
        node.retune(rate, MaxBlock::of(Samples(1)));
        node
    }

    /// The shared cell of the mix: `0` is the dry input alone, `1` (the
    /// default) the room alone.
    #[must_use]
    pub fn mix(&self) -> Param<Mix> {
        self.mix.handle()
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
                input: self.channels.input,
                output: self.channels.output,
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
        let latency = self.realization.latency().get();
        self.dry = (0..inputs).map(|_| DryAlign::new(latency)).collect();
        self.dry_buf = vec![vec![0.0; max_block.get()]; inputs];
        self.clear();
    }

    /// Back to silence: no ring-out owed, no input heard.
    fn clear(&mut self) {
        self.realization.reset();
        for d in &mut self.dry {
            d.clear();
        }
        self.silent = true;
        self.ring = 0;
        self.since = UNBOUNDED;
        self.last_mix = None;
    }

    /// Designs `controls` into the node's network and retargets the realization.
    fn apply(&mut self, controls: D::Controls) {
        self.design
            .design_into(&controls, self.rate, &mut self.network);
        self.realization.retarget(&self.network);
        self.current = frames(self.network.tail(TAIL_FLOOR), self.rate);
        self.applied = Some(controls);
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
        let mut addrs = vec![UnitAddr::first(UnitParam::Wet)];
        if matches!(self.mode, Mode::Live) {
            addrs.extend_from_slice(self.params.addrs());
        }
        Shape::audio(self.channels.input, self.channels.output)
            .with_cost(cost(self.realization.effort()))
            .with_latency(Latency::new(self.realization.latency()))
            .with_tail(tail_of(self.ceiling))
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
        let inputs = usize::from(self.channels.input.count());

        // The controls, and the network when they changed.
        let target = match self.mode {
            Mode::Fixed(c) => c,
            Mode::Live => self.params.read(&io, 1),
        };
        if self.applied != Some(target) {
            self.apply(target);
        }
        let mut current = self.current;
        if matches!(self.mode, Mode::Live)
            && let Some(longest) = self.params.longest(&io, 1)
            && Some(longest) != self.applied
        {
            self.design
                .design_into(&longest, self.rate, &mut self.longest);
            current = current.max(frames(self.longest.tail(TAIL_FLOOR), self.rate));
        }

        // The tail owed: measured from the last non-silent input frame, the
        // longer figure held until a shorter decay has been heard out.
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

        // How many frames of the block may still sound: the tail after the last
        // input frame, and the realization's latency before it starts (a tail
        // excludes latency).
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
        let cut = live.min(size as u64) as usize;
        let pass_through = self.dry.iter().all(DryAlign::is_pass_through);

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

        // The mix this block ends on, ramped from where the last one ended.
        let to = Mix::new_clamped(
            self.good_mix
                .read(block_driven(&io, 0).unwrap_or_else(|| self.mix.load().get())),
        )
        .get();
        let from = self.last_mix.unwrap_or(to);
        self.last_mix = Some(to);
        let wet_only = from == 1.0 && to == 1.0;

        let (ins, mut outs) = io.split();
        if cut > 0 {
            let mut input: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (c, slot) in input.iter_mut().enumerate().take(inputs) {
                *slot = &ins.get(c)[..cut];
            }
            let mut output: [&mut [f32]; MAX_CHANNELS] =
                std::array::from_fn(|_| Default::default());
            for (slot, ch) in output.iter_mut().zip(outs.iter_mut()) {
                *slot = &mut ch[..cut];
            }
            let width = usize::from(self.channels.output.count());
            self.realization
                .process(&input[..inputs], &mut output[..width]);
        }
        if cut < size {
            for ch in outs.iter_mut() {
                ch[cut..size].fill(0.0);
            }
            if !self.silent {
                self.realization.reset();
                self.silent = true;
            }
        }

        // The dry path, only when the mix is not all wet (or the dry delay holds
        // history a later mix would need).
        if !(wet_only && pass_through) {
            for (c, dry) in self.dry.iter_mut().enumerate() {
                for (b, &x) in self.dry_buf[c][..size].iter_mut().zip(&ins.get(c)[..size]) {
                    *b = dry.step(x);
                }
            }
        }
        if !wet_only {
            let ramp = Ramp::new(from, to, size);
            for (c, ch) in outs.iter_mut().enumerate() {
                let dry = &self.dry_buf[c % inputs.max(1)];
                for (i, (o, &d)) in ch[..size].iter_mut().zip(dry).enumerate() {
                    *o = Mix(ramp.at(i)).blend(d, *o);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match (k, self.mode) {
            (0, _) => Some(self.mix.load().get()),
            (k, Mode::Live) => self.params.base(k - 1),
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
            mix: self.mix.handle(),
            good_mix: self.good_mix,
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
            dry: self.dry.clone(),
            dry_buf: self.dry_buf.clone(),
            last_mix: None,
        };
        for d in &mut copy.dry {
            d.clear();
        }
        copy
    }
}

impl<D: NodeDesign> ParamNode for DesignNode<D> {
    /// The mix, and in a live node the algorithm's cells.
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(UnitParam::Wet, self.mix.as_atomic());
        match self.mode {
            Mode::Live => self.params.bind(set).build(),
            Mode::Fixed(_) => set.build(),
        }
    }

    /// A clone with every cell detached (at its value now) and the room empty.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.mix.detach();
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
