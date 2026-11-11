//! [`Io`]: one node call's buffers, reached through methods.
//!
//! # Why methods and not public fields
//!
//! The buffers are planar `f32`. Exposing them as
//! `&[&[f32]]` fields would make that layout part of every node's source; as
//! methods (`input`, `output`, `split`, `channel`) a later `input_f64` — or a
//! port format the compiler converts at a mismatched edge — is an addition,
//! not a breaking change. See [`PortKind`].
//!
//! # The length guarantee
//!
//! [`Io`] can only be built by the executor (or the reference interpreter),
//! through a constructor that asserts every slice is exactly
//! [`frames`](Io::frames) long and that `frames` is at most the
//! [`MaxBlock`] the node was prepared with. A node therefore never clamps.

use tutti_types::{Samples, Tail};

use crate::bus::{InBus, OutBus, PortSpan};
use crate::event::{EventWriter, SortedEvents, SubBlocks};
use crate::node::{ConstantMask, InPlaceMask, MainPorts, SilenceMask};
use crate::param::ParamInput;
use crate::time::Offset;
use tutti_types::MaxBlock;

/// What a port carries.
///
/// `Audio` is planar `f32` blocks. A sample *format* is deliberately not a
/// variant yet: when one is needed (an `f64` path for a plugin that wants it,
/// say), it would ride on `Audio`, and the compiler would insert a conversion
/// op on an edge whose two ends disagree — the same way it would coerce a
/// channel-count mismatch — so nodes on either side never see the other
/// format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PortKind {
    /// Planar `f32` audio, one channel per port.
    Audio,
    /// Sorted [`Event`](crate::Event) streams.
    Event,
}

/// One node call's buffers: audio in and out, events in and out, and the
/// block's param values, handed to [`Node::process`](crate::Node::process).
///
/// Only the executor (or the `Reference` interpreter)
/// builds one, and it guarantees every audio slice is exactly
/// [`frames`](Self::frames) long, at most the [`MaxBlock`] the node was
/// prepared with — so a node never needs to clamp. Buffers are planar `f32`
/// and reached through methods rather than fields: [`split`](Self::split)
/// borrows inputs and outputs together, [`channel`](Self::channel) gives
/// one channel as a [`Channel`] (separate or in place), and
/// [`sub_blocks`](Self::sub_blocks) splits the block at an event port's
/// offsets for sample-accurate event handling.
///
/// The silence, constant and in-place masks say what the executor knows
/// about the inputs; a node may use them to skip work but must render the
/// same result without them.
///
/// # The main bus, and the others by handle
///
/// [`input`](Self::input), [`output`](Self::output), [`channel`](Self::channel),
/// [`split`](Self::split) and the masks cover the node's **main bus** on each
/// side ([`Shape::buses`](crate::Shape::buses)), or every port of a side that
/// declares no bus: channel `c` is the main bus's channel `c`. Any other bus
/// (a key, a crossover's further bands, a plugin's aux buses, the ports past
/// the buses) is reached only through its handle ([`InBus`], [`OutBus`], from
/// the node's shape) as a view bounded to it: [`bus_in`](Self::bus_in),
/// [`bus_out`](Self::bus_out), [`Inputs::bus`], [`Outputs::bus`]. So `c + width`
/// on the main view is out of range in every build, as a slice is, and never
/// another bus's port.
pub struct Io<'a> {
    frames: usize,
    inputs: &'a [&'a [f32]],
    outputs: &'a mut [&'a mut [f32]],
    /// The ports the main views cover.
    main: MainPorts,
    silent: SilenceMask,
    constant: ConstantMask,
    in_place: InPlaceMask,
    events_in: &'a [SortedEvents<'a>],
    events_out: &'a mut [EventWriter<'a>],
    /// Per declared param port, what it reads this block; a port past the
    /// end reads its base.
    params: &'a [ParamInput<'a>],
    /// Where [`count_lost`](Self::count_lost) adds; `None` where nothing
    /// counts (a hand-built `Io`).
    lost: Option<&'a std::cell::Cell<u32>>,
    /// Where [`report_tail`](Self::report_tail) stores; `None` where nothing
    /// reads it (a hand-built `Io`).
    tail: Option<&'a std::cell::Cell<Option<Tail>>>,
}

impl std::fmt::Debug for Io<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Io")
            .field("frames", &self.frames)
            .field("inputs", &self.inputs.len())
            .field("outputs", &self.outputs.len())
            .field("silent", &self.silent)
            .field("constant", &self.constant)
            .field("in_place", &self.in_place)
            .finish_non_exhaustive()
    }
}

/// The audio inputs, while the outputs are borrowed too: the main bus by
/// channel ([`get`](Self::get)), any other bus by its handle
/// ([`bus`](Self::bus)).
pub struct Inputs<'s> {
    slices: &'s [&'s [f32]],
    main: PortSpan,
    in_place: InPlaceMask,
}

/// One input bus, read by channel: channel `c` of the bus, bounded to it.
pub struct BusIn<'s> {
    slices: &'s [&'s [f32]],
    first: usize,
    in_place: InPlaceMask,
}

/// One output bus, written by channel: channel `c` of the bus, bounded to it.
pub struct BusOut<'s, 'a> {
    slices: &'s mut [&'a mut [f32]],
}

impl std::fmt::Debug for Inputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs")
            .field("channels", &self.main.width)
            .field("in_place", &self.in_place)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BusIn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusIn")
            .field("channels", &self.slices.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BusOut<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusOut")
            .field("channels", &self.slices.len())
            .finish_non_exhaustive()
    }
}

/// The audio outputs, while the inputs are borrowed too: the main bus by
/// channel ([`get`](Self::get)), any other bus by its handle
/// ([`bus`](Self::bus)).
pub struct Outputs<'s, 'a> {
    slices: &'s mut [&'a mut [f32]],
    main: PortSpan,
}

impl std::fmt::Debug for Outputs<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outputs")
            .field("channels", &self.main.width)
            .finish_non_exhaustive()
    }
}

/// One channel's input and output together.
pub enum Channel<'s> {
    /// Separate buffers: read `input`, write `output`.
    Split {
        /// The input samples.
        input: &'s [f32],
        /// Where the output goes.
        output: &'s mut [f32],
    },
    /// One buffer: it holds the input now and must hold the output after.
    InPlace(&'s mut [f32]),
}

impl std::fmt::Debug for Channel<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Channel::Split { input, .. } => f
                .debug_struct("Channel::Split")
                .field("len", &input.len())
                .finish_non_exhaustive(),
            Channel::InPlace(buf) => f
                .debug_struct("Channel::InPlace")
                .field("len", &buf.len())
                .finish_non_exhaustive(),
        }
    }
}

impl Channel<'_> {
    /// Apply `f` sample by sample, whichever form this is.
    // `#[inline]` is load-bearing: without it the loop is compiled apart from
    // the node's closure, whose captured state (a filter's two state
    // variables, say) then lives behind pointers, and every sample pays a
    // store and a reload on the recurrence — measured 1.5× slower on a chain
    // of SVFs.
    #[inline]
    pub fn map(self, mut f: impl FnMut(f32) -> f32) {
        match self {
            Channel::Split { input, output } => {
                for (o, &i) in output.iter_mut().zip(input) {
                    *o = f(i);
                }
            }
            Channel::InPlace(buf) => {
                for x in buf.iter_mut() {
                    *x = f(*x);
                }
            }
        }
    }
}

impl<'a> Io<'a> {
    /// Build a call's buffers, enforcing the length guarantee.
    ///
    /// # Panics
    ///
    /// Always, if `frames` is zero or exceeds `max` — the bound a node's
    /// scratch was sized from. In debug builds also if an output is not
    /// exactly `frames` long, or an input is not (an aliased input is empty):
    /// both executors build every slice from `frames` themselves, so that
    /// check is a guard on this crate's code, not on the caller's, and is not
    /// worth a per-channel loop on every node call in release.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max: MaxBlock,
        frames: usize,
        inputs: &'a [&'a [f32]],
        outputs: &'a mut [&'a mut [f32]],
        silent: SilenceMask,
        constant: ConstantMask,
        in_place: InPlaceMask,
        events_in: &'a [SortedEvents<'a>],
        events_out: &'a mut [EventWriter<'a>],
    ) -> Self {
        assert!(
            frames > 0 && frames <= max.get(),
            "a block of {frames} frames against a prepared maximum of {}",
            max.get()
        );
        debug_assert!(outputs.iter().all(|o| o.len() == frames));
        debug_assert!(
            inputs
                .iter()
                .enumerate()
                .all(|(c, i)| i.len() == frames || (in_place.get(c) && i.is_empty()))
        );
        let main = MainPorts {
            inputs: PortSpan::new(0, inputs.len()),
            outputs: PortSpan::new(0, outputs.len()),
        };
        Self {
            frames,
            inputs,
            outputs,
            main,
            silent,
            constant,
            in_place,
            events_in,
            events_out,
            params: &[],
            lost: None,
            tail: None,
        }
    }

    /// These buffers, with the main views covering `main`: the node's
    /// [`Shape::main_ports`](crate::Shape). Without it they cover every port.
    ///
    /// # Panics
    ///
    /// If `main` reaches past the ports.
    #[inline]
    pub(crate) fn with_main(mut self, main: MainPorts) -> Self {
        assert!(
            main.inputs.range().end <= self.inputs.len()
                && main.outputs.range().end <= self.outputs.len(),
            "a main bus past the node's ports"
        );
        self.main = main;
        self
    }

    /// These buffers, with [`report_tail`](Self::report_tail) storing into
    /// `tail`.
    #[inline]
    pub(crate) fn with_tail(mut self, tail: &'a std::cell::Cell<Option<Tail>>) -> Self {
        self.tail = Some(tail);
        self
    }

    /// Narrows this node's tail for the silence skip: the frames it keeps
    /// sounding after its input stops, counted once its latency is
    /// compensated, **measured from the last non-quiet input frame, not from
    /// now**. The executor parks the node once its input has been quiet for
    /// its latency plus this figure, so a reverb with a one-second RT60
    /// reports one second on every call, however long its input has been
    /// quiet; reporting the half second that remains after half a second of
    /// quiet would park it at half its ring.
    ///
    /// **Report on every call or not at all.** A report holds until the next
    /// call, and a call that reports nothing restores the ceiling
    /// ([`Shape::tail`](crate::Shape::tail)), the safe default. A report above
    /// the ceiling is clamped to it, [`Tail::Unknown`] says nothing (the
    /// ceiling stands), and [`Tail::Unbounded`] under an `Unbounded` ceiling
    /// is never skipped. A node whose shape does not declare a tail with
    /// [`with_tail`](crate::Shape::with_tail) has the default ceiling and is
    /// never narrowed below it; one that forgets to declare any tail at all
    /// (`Tail::Unknown`) is never skipped, whatever it reports, because an
    /// `Unknown` ceiling bounds nothing for a report to clamp to.
    ///
    /// Only a node **without event inputs** is parked on its tail; one with
    /// event inputs parks only on its own [`Status::Idle`](crate::Status::Idle),
    /// and its report changes nothing.
    ///
    /// The report must cover the node's ring-out (see [`Node`](crate::Node),
    /// "A tail that follows the params"); `contract::assert_honest_tail`
    /// (feature `contract`) checks it. Audio thread: one store into the
    /// unit's own record, no allocation; the plan is untouched, and nothing
    /// but the task that runs the node reads it, so a pooled render parks the
    /// same blocks a serial one does.
    #[inline]
    pub fn report_tail(&self, tail: Tail) {
        if let Some(cell) = self.tail {
            cell.set(Some(tail));
        }
    }

    /// These buffers, with `params` as the declared params' inputs (every
    /// port past `params`' end reads its base).
    #[inline]
    pub(crate) fn with_params(mut self, params: &'a [ParamInput<'a>]) -> Self {
        self.params = params;
        self
    }

    /// These buffers, counting [`count_lost`](Self::count_lost) into `lost`.
    #[inline]
    pub(crate) fn with_lost(mut self, lost: &'a std::cell::Cell<u32>) -> Self {
        self.lost = Some(lost);
        self
    }

    /// Counts `n` events this node took from its inputs and did not deliver
    /// (a plugin's intake past its cap): the executor adds them to
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count) and
    /// reports them for the node ([`EventLoss`](crate::EventLoss), `port: None`),
    /// so an export that watches the count sees them. Audio thread: one add.
    #[inline]
    pub fn count_lost(&self, n: u64) {
        if let Some(c) = self.lost {
            c.set(c.get().saturating_add(u32::try_from(n).unwrap_or(u32::MAX)));
        }
    }

    /// The block length in frames — at most the prepared
    /// [`MaxBlock`]. A slice length, hence `usize`.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Channels of the main input bus.
    pub fn input_count(&self) -> usize {
        usize::from(self.main.inputs.width)
    }

    /// Channels of the main output bus.
    pub fn output_count(&self) -> usize {
        usize::from(self.main.outputs.width)
    }

    /// The port of main input channel `c`.
    fn in_port(&self, c: usize) -> usize {
        assert!(
            c < self.input_count(),
            "input {c} is past the main bus ({} channels); another bus is read by its handle",
            self.input_count()
        );
        usize::from(self.main.inputs.first) + c
    }

    /// The port of main output channel `c`.
    fn out_port(&self, c: usize) -> usize {
        assert!(
            c < self.output_count(),
            "output {c} is past the main bus ({} channels); another bus is written by its handle",
            self.output_count()
        );
        usize::from(self.main.outputs.first) + c
    }

    /// Main input channel `c`, wherever it is: its own buffer, or (when the
    /// executor aliased it in place) the output buffer that holds it.
    ///
    /// # Panics
    ///
    /// If `c` is not below [`input_count`](Self::input_count).
    pub fn input(&self, c: usize) -> &[f32] {
        let p = self.in_port(c);
        if self.in_place.get(p) {
            &self.outputs[p][..]
        } else {
            self.inputs[p]
        }
    }

    /// Main output channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is not below [`output_count`](Self::output_count).
    pub fn output(&mut self, c: usize) -> &mut [f32] {
        let p = self.out_port(c);
        &mut self.outputs[p][..]
    }

    /// Main input and output channel `c` together.
    ///
    /// # Panics
    ///
    /// If `c` is out of range for the main input or output bus.
    pub fn channel(&mut self, c: usize) -> Channel<'_> {
        let (i, o) = (self.in_port(c), self.out_port(c));
        if self.in_place.get(i) {
            debug_assert_eq!(i, o, "a channel in place is one port");
            Channel::InPlace(&mut self.outputs[o][..])
        } else {
            Channel::Split {
                input: self.inputs[i],
                output: &mut self.outputs[o][..],
            }
        }
    }

    /// Every input and every output at once, for a node that mixes across
    /// channels: the main buses by channel, any other bus by its handle. An
    /// aliased channel is reachable only through the outputs.
    pub fn split(&mut self) -> (Inputs<'_>, Outputs<'_, 'a>) {
        (
            Inputs {
                slices: self.inputs,
                main: self.main.inputs,
                in_place: self.in_place,
            },
            Outputs {
                slices: &mut *self.outputs,
                main: self.main.outputs,
            },
        )
    }

    /// Input bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's input ports: one taken from
    /// another shape.
    pub fn bus_in(&self, bus: InBus) -> BusIn<'_> {
        bus_in(self.inputs, bus, self.in_place)
    }

    /// Output bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's output ports.
    pub fn bus_out(&mut self, bus: OutBus) -> BusOut<'_, 'a> {
        BusOut {
            slices: &mut self.outputs[bus.0.range()],
        }
    }

    /// Main input channels that are exact silence this block.
    pub fn silent(&self) -> SilenceMask {
        SilenceMask(shift(self.silent.0, self.main.inputs))
    }

    /// Main input channels that hold one value throughout this block.
    pub fn constant(&self) -> ConstantMask {
        ConstantMask(shift(self.constant.0, self.main.inputs))
    }

    /// Main channels aliased in place: for each set `c`, output `c` **already
    /// holds input `c`**. Never set unless the node's
    /// [`Shape::in_place`](crate::Shape::in_place) is `true`.
    pub fn in_place(&self) -> InPlaceMask {
        InPlaceMask(shift(self.in_place.0, self.main.inputs))
    }

    /// Number of event input ports.
    pub fn event_input_count(&self) -> usize {
        self.events_in.len()
    }

    /// Event input port `p`, sorted by offset, every offset inside the block.
    ///
    /// # Panics
    ///
    /// If `p` is not below [`event_input_count`](Self::event_input_count).
    pub fn events(&self, p: usize) -> SortedEvents<'a> {
        self.events_in[p]
    }

    /// Number of event output ports.
    pub fn event_output_count(&self) -> usize {
        self.events_out.len()
    }

    /// The writer for event output port `p`.
    ///
    /// # Panics
    ///
    /// If `p` is not below [`event_output_count`](Self::event_output_count).
    pub fn event_out(&mut self, p: usize) -> &mut EventWriter<'a> {
        &mut self.events_out[p]
    }

    /// The block split at event input port `p`'s offsets: `(range, events)`
    /// chunks that tile the block, each carrying exactly the events at its
    /// first frame (see [`SubBlocks`]).
    ///
    /// The shape a sample-accurate node is written in — apply the chunk's
    /// events, then render its range — so honouring offsets is the loop's
    /// structure rather than a check the node has to remember. Borrows the
    /// events, not the `Io`, so the loop body can write the outputs:
    ///
    /// ```
    /// use tutti_graph::{Io, EventKind};
    /// fn render(mut io: Io<'_>, level: &mut f32) {
    ///     for (range, events) in io.sub_blocks(0) {
    ///         for e in events {
    ///             if let EventKind::Midi(_) = e.kind {
    ///                 *level = 1.0 - *level; // toggle on every event
    ///             }
    ///         }
    ///         io.output(0)[range].fill(*level);
    ///     }
    /// }
    /// ```
    pub fn sub_blocks(&self, p: usize) -> SubBlocks<'a> {
        self.events_in[p].sub_blocks(Samples(self.frames))
    }

    /// What declared param `k` ([`Shape::params`](crate::Shape::params))
    /// reads this block: [`ParamInput::Base`] when nothing modulates it — the
    /// node reads its own control, the fast path — or its value at every
    /// frame, base and modulation fused and clamped by the graph. A param
    /// nothing is connected to is always `Base`, never 0.
    #[inline]
    pub fn param(&self, k: usize) -> ParamInput<'a> {
        self.params.get(k).copied().unwrap_or(ParamInput::Base)
    }

    /// Frame `index` of this block as an [`Offset`], when it is inside it.
    pub fn offset(&self, index: usize) -> Option<Offset> {
        Offset::new(index, Samples(self.frames))
    }
}

impl<'s> Inputs<'s> {
    /// Channels of the main input bus.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.main.width)
    }

    /// Whether the main input bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.main.width == 0
    }

    /// Main input channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is aliased in place (read it through the outputs) or past the
    /// main bus.
    pub fn get(&self, c: usize) -> &[f32] {
        assert!(
            c < self.len(),
            "input {c} is past the main bus ({} channels); another bus is read by its handle",
            self.len()
        );
        let p = usize::from(self.main.first) + c;
        assert!(
            !self.in_place.get(p),
            "input {c} is in place; it is in the output buffer"
        );
        self.slices[p]
    }

    /// Input bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's input ports.
    pub fn bus(&self, bus: InBus) -> BusIn<'s> {
        bus_in(self.slices, bus, self.in_place)
    }
}

impl<'s, 'a> Outputs<'s, 'a> {
    /// Channels of the main output bus.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.main.width)
    }

    /// Whether the main output bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.main.width == 0
    }

    /// Main output channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is past the main bus.
    pub fn get(&mut self, c: usize) -> &mut [f32] {
        assert!(
            c < self.len(),
            "output {c} is past the main bus ({} channels); another bus is written by its handle",
            self.len()
        );
        &mut self.slices[usize::from(self.main.first) + c][..]
    }

    /// Every main output channel.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [f32]> + use<'_, 's, 'a> {
        self.slices[self.main.range()].iter_mut().map(|s| &mut **s)
    }

    /// Output bus `bus`, by its handle, while the inputs stay borrowed.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's output ports.
    pub fn bus(&mut self, bus: OutBus) -> BusOut<'_, 'a> {
        BusOut {
            slices: &mut self.slices[bus.0.range()],
        }
    }
}

impl<'s, 'a> Outputs<'s, 'a> {
    /// Several output buses at once, by their handles in port order, so a node
    /// that writes more than one bus in a pass (a split's bands, a plugin's
    /// buses) holds them together.
    ///
    /// # Panics
    ///
    /// When it reaches a handle out of port order or sharing a port with the
    /// one before, or one past this node's output ports.
    pub fn buses<'h>(
        &mut self,
        buses: &'h [OutBus],
    ) -> impl Iterator<Item = BusOut<'_, 'a>> + use<'_, 'h, 's, 'a> {
        let mut rest: &mut [&'a mut [f32]] = &mut self.slices[..];
        let mut at = 0;
        buses.iter().map(move |bus| {
            let span = bus.0.range();
            assert!(
                span.start >= at,
                "output buses are taken in port order, each once"
            );
            let (_, tail) = core::mem::take(&mut rest).split_at_mut(span.start - at);
            let (view, tail) = tail.split_at_mut(span.len());
            rest = tail;
            at = span.end;
            BusOut { slices: view }
        })
    }
}

impl<'s> BusIn<'s> {
    /// The bus's channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether the bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Every channel of the bus, in order.
    ///
    /// # Panics
    ///
    /// When it reaches a channel aliased in place.
    pub fn channels(self) -> impl Iterator<Item = &'s [f32]> {
        (0..self.len()).map(move |c| self.get(c))
    }

    /// Channel `c` of the bus.
    ///
    /// # Panics
    ///
    /// If `c` is past the bus, or aliased in place.
    pub fn get(&self, c: usize) -> &'s [f32] {
        assert!(
            !self.in_place.get(self.first + c),
            "input {c} of the bus is in place; it is in the output buffer"
        );
        self.slices[c]
    }
}

impl<'s, 'a> BusOut<'s, 'a> {
    /// The bus's channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether the bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Channel `c` of the bus.
    ///
    /// # Panics
    ///
    /// If `c` is past the bus.
    pub fn get(&mut self, c: usize) -> &mut [f32] {
        &mut self.slices[c][..]
    }

    /// Every channel of the bus.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [f32]> + use<'_, 's, 'a> {
        self.slices.iter_mut().map(|s| &mut **s)
    }

    /// Every channel of the bus, for as long as the view was lent.
    pub fn into_channels(self) -> impl Iterator<Item = &'s mut [f32]> + use<'s, 'a> {
        self.slices.iter_mut().map(|s| &mut **s)
    }
}

/// The view of input bus `bus` over `slices`.
fn bus_in<'s>(slices: &'s [&'s [f32]], bus: InBus, in_place: InPlaceMask) -> BusIn<'s> {
    BusIn {
        slices: &slices[bus.0.range()],
        first: usize::from(bus.0.first),
        in_place,
    }
}

/// `mask`'s bits for the ports of `span`, from bit 0.
fn shift(mask: u64, span: PortSpan) -> u64 {
    let first = u32::from(span.first);
    let bits = mask.checked_shr(first).unwrap_or(0);
    let width = usize::from(span.width);
    if width >= 64 {
        bits
    } else {
        bits & ((1u64 << width) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bus, BusRole, Buses, Shape};
    use tutti_types::{BusTopology, ChannelLayout};

    const FRAMES: usize = 4;

    /// A stereo main bus and a mono key in, a stereo main bus out, and one
    /// port past the buses on each side (a plugin's channels a bus cannot
    /// describe).
    fn shape() -> Shape {
        let main = Bus {
            topology: BusTopology::STEREO,
            role: BusRole::Main,
        };
        let key = Bus {
            topology: BusTopology::unassigned(ChannelLayout::MONO).expect("mono fits"),
            role: BusRole::Sidechain,
        };
        let ins = Buses::new(&[main, key]).expect("two buses");
        Shape::buses(ins, Buses::main(BusTopology::STEREO))
            .with_ports(ChannelLayout::from_count(4), ChannelLayout::from_count(3))
    }

    /// Runs `f` over an `Io` of [`shape`]: input port `p` holds `p + 1`, every
    /// output 0; returns the outputs afterwards.
    fn with_io(f: impl FnOnce(&Shape, Io<'_>)) -> Vec<Vec<f32>> {
        let shape = shape();
        let ins: Vec<Vec<f32>> = (0..4).map(|p| vec![(p + 1) as f32; FRAMES]).collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![0.0; FRAMES]; 3];
        {
            let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            let io = Io::new(
                MaxBlock::of(Samples(FRAMES)),
                FRAMES,
                &in_refs,
                &mut out_refs,
                SilenceMask::NONE,
                ConstantMask::NONE,
                InPlaceMask::NONE,
                &[],
                &mut [],
            )
            .with_main(shape.main_ports());
            f(&shape, io);
        }
        outs
    }

    /// Input and output `c` are the main bus's: two channels each side.
    #[test]
    fn the_flat_view_is_the_main_bus() {
        with_io(|_, io| {
            assert_eq!((io.input_count(), io.output_count()), (2, 2));
            assert_eq!(io.input(1)[0], 2.0);
        });
    }

    /// The port after the main bus is the key's, and it is not reachable as
    /// input 2: the flat view ends with the main bus.
    #[test]
    #[should_panic(expected = "input 2 is past the main bus")]
    fn a_port_past_the_main_bus_is_not_an_input() {
        with_io(|_, io| {
            let _ = io.input(2);
        });
    }

    /// Every input port in port order, a bus at a time: the main bus, the key,
    /// then the port past the buses.
    #[test]
    fn the_handles_cover_every_input_in_port_order() {
        with_io(|shape, io| {
            let read: Vec<f32> = shape
                .input_handles()
                .flat_map(|bus| io.bus_in(bus).channels())
                .map(|ch| ch[0])
                .collect();
            assert_eq!(read, [1.0, 2.0, 3.0, 4.0]);
        });
    }

    /// The output buses held together write every output port, each once, in
    /// port order.
    #[test]
    fn the_output_buses_held_together_reach_every_port_in_order() {
        let outs = with_io(|shape, mut io| {
            let handles: Vec<OutBus> = shape.output_handles().collect();
            let (_, mut outputs) = io.split();
            for (k, ch) in outputs
                .buses(&handles)
                .flat_map(BusOut::into_channels)
                .enumerate()
            {
                ch.fill(k as f32 + 10.0);
            }
        });
        let first: Vec<f32> = outs.iter().map(|o| o[0]).collect();
        assert_eq!(first, [10.0, 11.0, 12.0]);
    }

    /// Two views of one port cannot be held at once: handles out of port
    /// order are refused.
    #[test]
    #[should_panic(expected = "in port order")]
    fn output_buses_out_of_order_are_refused() {
        with_io(|shape, mut io| {
            let handles = [
                shape.spare_out().expect("a port past the buses"),
                shape.output_bus(0).expect("the main bus"),
            ];
            let (_, mut outputs) = io.split();
            for _ in outputs.buses(&handles) {}
        });
    }
}
