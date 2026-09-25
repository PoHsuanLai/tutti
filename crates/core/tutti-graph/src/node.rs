//! The node contract: [`Node`], and everything its four methods name.
//!
//! Doc 013 §2. This is deliberately *smaller* than `tutti_node::AudioUnit`,
//! and it sits beside it rather than replacing it — the [`Legacy`](crate::Legacy)
//! adapter runs every existing `AudioUnit` through this trait unmodified.
//!
//! What it drops, and why each drop is safe:
//!
//! - **`tick`, `route`, `Signal`.** Latency is *declared* in [`Shape`], as a
//!   [`Latency`] — a type a delay time cannot be passed as. `route` used one
//!   `Signal::delay` for both musical delay and processing latency, which is
//!   the D1–D3 defect class in doc 013.
//! - **`as_any` / downcasts.** A node's control surface comes back *typed* from
//!   [`IntoNode::into_node`], at insertion. Nothing needs to find a node again.
//! - **`DynClone`.** Nothing clones a unit: the executor owns it, and a
//!   recompile keeps it by key.
//! - **The `S: Sample` generic.** Owner decision 2: `f32` only inside the
//!   graph (see the crate docs on precision).
//! - **The 64-frame block.** A node gets whatever block the executor was
//!   prepared for, up to [`Prepare::max_block`], whole — sub-chunking at event
//!   offsets is the node's job (doc 013 §4: an out-of-process plugin's declared
//!   pipeline latency is only constant if it sees whole blocks).

use tutti_types::{Beat, Bpm, ChannelLayout, Frame, Latency, SampleRate, Samples, Tail};

use crate::io::Io;
use crate::time::Offset;

/// Most audio channels, or event ports, on one side of one node.
///
/// Bounded because the per-channel masks are `u64` bitsets and because the
/// executor builds each node's port table on the stack — no allocation per
/// node call. A node wider than this is a [`CompileError`](crate::CompileError),
/// not a silent truncation.
pub const MAX_PORTS: usize = 64;

/// What a node looks like from the outside: its ports, and the two figures the
/// compiler folds over the graph.
///
/// Returned by `&self` so asking costs nothing — fundsp's latency probe cloned
/// the whole node to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Audio input channels. In a [`Topology`](tutti_types::Topology) each
    /// channel is one port.
    pub audio_in: ChannelLayout,
    /// Audio output channels.
    pub audio_out: ChannelLayout,
    /// Event input ports.
    pub event_in: u16,
    /// Event output ports.
    pub event_out: u16,
    /// **Processing latency only** — frames the node buffers as a side effect
    /// (lookahead, FFT block, plugin pipeline). Never a musical delay: a
    /// 500 ms echo has latency zero, or PDC drags every parallel path 500 ms
    /// late (defect D1 in doc 013). The type makes the mistake a compile
    /// error: a delay time is a `Samples` or a `Seconds`, not a [`Latency`].
    pub latency: Latency,
    /// Ring-out after the input stops. Also what the executor's silence skip
    /// trusts: a node reporting [`Tail::None`] is not called on a silent block.
    pub tail: Tail,
    /// Whether this node accepts **in-place** channels (see
    /// [`Io::in_place`](crate::Io::in_place)).
    ///
    /// Opt-in because it changes what the node is handed: on an aliased
    /// channel the input arrives *already in the output buffer*.
    pub in_place: bool,
    /// How finely the node honours the offsets of the events it is handed
    /// (doc 013 §6 item 5). A declaration, not a request: the executor still
    /// hands every node its sorted events and whole block, and the compiler
    /// refuses an event edge that requires a finer resolution than its sink
    /// declares (see [`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution)).
    pub event_resolution: Resolution,
}

/// How finely a node honours event offsets: the timing it promises for what
/// arrives on its event inputs.
///
/// Ordered from finest to coarsest. A node written against
/// [`Io::sub_blocks`](crate::Io::sub_blocks) is [`Sample`](Self::Sample) by
/// construction; one bound by an engine that renders in fixed chunks declares
/// the chunk (a rustysynth-backed SoundFont is `Frames(8)`); one that reads its
/// events once per call is [`Block`](Self::Block).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// Every event takes effect on its exact frame.
    Sample,
    /// Events take effect on the first frame of the `n`-frame chunk they fall
    /// in, counted from the block's start. `n` is at least 1; `Frames(1)`
    /// promises what `Sample` does.
    Frames(u32),
    /// Events take effect somewhere in the block they arrive in — whatever
    /// the offset. Also the resolution of a node with no event inputs, whose
    /// promise is vacuous.
    Block,
}

impl Resolution {
    /// Whether a node at this resolution honours an edge that requires
    /// `required`: it is at least as fine.
    ///
    /// `Frames(n)` honours `Frames(m)` when `n <= m` — a finer chunk is still
    /// inside the coarser one's promise — but never `Sample` unless `n == 1`.
    pub const fn honours(self, required: Resolution) -> bool {
        match (self, required) {
            (Resolution::Sample, _) | (_, Resolution::Block) => true,
            (Resolution::Frames(n), Resolution::Sample) => n <= 1,
            (Resolution::Frames(n), Resolution::Frames(m)) => n <= m,
            (Resolution::Block, _) => false,
        }
    }
}

impl Shape {
    /// A shape with these audio widths, no event ports, no latency, no tail,
    /// no in-place support, and [`Resolution::Sample`] — a new node is
    /// written to honour its event offsets, and says otherwise explicitly.
    pub const fn audio(audio_in: ChannelLayout, audio_out: ChannelLayout) -> Self {
        Self {
            audio_in,
            audio_out,
            event_in: 0,
            event_out: 0,
            latency: Latency::ZERO,
            tail: Tail::None,
            in_place: false,
            event_resolution: Resolution::Sample,
        }
    }

    /// This shape with `event_in` / `event_out` event ports.
    #[must_use]
    pub const fn with_events(mut self, event_in: u16, event_out: u16) -> Self {
        self.event_in = event_in;
        self.event_out = event_out;
        self
    }

    /// This shape with processing latency.
    ///
    /// Takes a [`Latency`], never a frame count: a delay time is not a
    /// processing latency, and passing one is a type error —
    ///
    /// ```compile_fail
    /// use tutti_graph::Shape;
    /// use tutti_types::{ChannelLayout, Samples};
    /// let echo_time = Samples(24_000);
    /// let _ = Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_latency(echo_time);
    /// ```
    ///
    /// — and there is no conversion that would make it one silently:
    ///
    /// ```compile_fail
    /// use tutti_types::{Latency, Samples};
    /// let _: Latency = Samples(24_000).into();
    /// ```
    ///
    /// The one spelling is [`Latency::new`], a visible decision:
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use tutti_types::{ChannelLayout, Latency, Samples};
    /// let lookahead = Latency::new(Samples(512));
    /// let s = Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_latency(lookahead);
    /// assert_eq!(s.latency.samples(), Samples(512));
    /// ```
    #[must_use]
    pub const fn with_latency(mut self, latency: Latency) -> Self {
        self.latency = latency;
        self
    }

    /// This shape with the given ring-out.
    #[must_use]
    pub const fn with_tail(mut self, tail: Tail) -> Self {
        self.tail = tail;
        self
    }

    /// This shape, accepting in-place channels.
    #[must_use]
    pub const fn with_in_place(mut self) -> Self {
        self.in_place = true;
        self
    }

    /// This shape, honouring event offsets only as finely as `resolution`.
    #[must_use]
    pub const fn with_event_resolution(mut self, resolution: Resolution) -> Self {
        self.event_resolution = resolution;
        self
    }
}

/// The longest block a node will ever be handed.
///
/// Obtainable **only** from [`Prepare`], so a node's scratch is always sized
/// from the same number the executor enforces on every [`Io`] — a node never
/// needs a truncating clamp, and never has one to get wrong (doc 013 defect
/// D4 is a release build silently truncating past a hard-coded 64).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaxBlock(usize);

impl MaxBlock {
    /// The bound, in frames.
    pub const fn get(self) -> usize {
        self.0
    }

    /// The bound as a frame count.
    pub const fn samples(self) -> Samples {
        Samples(self.0)
    }
}

/// What a node is prepared for. Handed to [`Node::prepare`] on the control
/// thread, before the node first processes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prepare {
    sample_rate: SampleRate,
    max_block: MaxBlock,
}

impl Prepare {
    /// Prepare for `sample_rate`, with blocks of up to `max_block` frames.
    ///
    /// # Panics
    ///
    /// If `max_block` is zero.
    pub fn new(sample_rate: SampleRate, max_block: Samples) -> Self {
        assert!(!max_block.is_zero(), "a block holds at least one frame");
        Self {
            sample_rate,
            max_block: MaxBlock(max_block.get()),
        }
    }

    /// The rate every block will run at.
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The longest block [`Node::process`] will be handed. Blocks may be any
    /// length from 1 up to this, and need not repeat.
    pub const fn max_block(&self) -> MaxBlock {
        self.max_block
    }
}

/// Planar per-channel scratch a node sizes in [`Node::prepare`].
///
/// Built from a [`MaxBlock`], so every block the executor can hand the node
/// fits by construction: [`channel`](Self::channel) takes the block's own
/// length and never needs to be clamped.
#[derive(Clone, Debug, Default)]
pub struct Scratch {
    data: Vec<f32>,
    max: usize,
}

impl Scratch {
    /// `channels` zeroed channels of `max` frames. Control thread: allocates.
    pub fn new(max: MaxBlock, channels: usize) -> Self {
        Self {
            data: vec![0.0; max.get() * channels],
            max: max.get(),
        }
    }

    /// Channel `c`'s first `frames` samples.
    ///
    /// # Panics
    ///
    /// If `frames` exceeds the `MaxBlock` this was built from — which an
    /// executor-supplied block length never does.
    pub fn channel(&mut self, c: usize, frames: usize) -> &mut [f32] {
        assert!(
            frames <= self.max,
            "{frames} frames past the prepared maximum {}",
            self.max
        );
        &mut self.data[c * self.max..c * self.max + frames]
    }
}

/// A 64-channel bitset: bit `c` is channel `c`. Channels past 63 are never set.
macro_rules! mask {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $name(pub u64);

        impl $name {
            /// No channel set.
            pub const NONE: Self = Self(0);

            /// Whether channel `c` is set.
            #[inline]
            pub const fn get(self, c: usize) -> bool {
                c < 64 && (self.0 >> c) & 1 == 1
            }

            /// This mask with channel `c` set. A no-op past 63.
            #[inline]
            #[must_use]
            pub const fn with(self, c: usize) -> Self {
                if c < 64 {
                    Self(self.0 | (1 << c))
                } else {
                    self
                }
            }

            /// Every channel below `n` set.
            #[inline]
            pub const fn all(n: usize) -> Self {
                if n >= 64 {
                    Self(u64::MAX)
                } else {
                    Self((1u64 << n) - 1)
                }
            }

            /// Whether every channel below `n` is set.
            #[inline]
            pub const fn covers(self, n: usize) -> bool {
                let want = Self::all(n).0;
                self.0 & want == want
            }
        }
    };
}

mask!(
    /// Channels known to be **exact** silence (every sample `0.0`) for the
    /// whole block. Unset means unknown, not "not silent".
    SilenceMask
);
mask!(
    /// Channels known to hold one value for the whole block. Unset means
    /// unknown.
    ConstantMask
);
mask!(
    /// Channels the executor aliased in place: output `c` already holds input
    /// `c`. Only ever set for a node whose [`Shape::in_place`] is `true`.
    InPlaceMask
);

/// What a node's [`process`](Node::process) did to its outputs.
///
/// Firewheel's set (doc 013 §2), plus [`Masked`](Self::Masked) so a
/// multichannel node can flag individual channels. The executor acts on it:
/// masks are what downstream silence-skipping reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Every output was written. No claim about its content.
    Modified,
    /// Every output was written, and these channels are silent / constant.
    /// A claim the node must be able to back — the executor trusts it.
    Masked {
        /// Channels that are exact silence.
        silent: SilenceMask,
        /// Channels that hold one value throughout.
        constant: ConstantMask,
    },
    /// The node wrote nothing; this block's output is silence. The executor
    /// zeroes. Says nothing about the *next* block: a synth in a delayed
    /// attack, or a sample with leading silence under a held note, is
    /// `Silent` and still busy.
    Silent,
    /// As [`Silent`](Self::Silent), and the node has **no pending internal
    /// activity**: park it until an input arrives. The only status that lets
    /// the executor skip a node with event inputs (see `Executor`'s silence
    /// skip); a node that returns it must produce silence for as long as its
    /// inputs stay quiet.
    Idle,
    /// The node wrote only **sample 0** of each output; each is that value
    /// throughout. The executor fills the rest.
    Constant,
    /// The node wrote nothing; output `c` is input `c` (zero past the input
    /// width). The executor copies.
    Bypass,
}

/// Loop points of the transport, when looping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopRange {
    /// Where the loop starts.
    pub start: Beat,
    /// Where it wraps back to `start`.
    pub end: Beat,
}

/// The transport as the executor saw it at the start of a block.
///
/// Minimal on purpose (doc 013 §4, "Env and PDC"): the fields `TransportClock`
/// sends down its two f32 ports today, plus play state. A node reads it at its
/// own **compensated** time through [`Cx::arrival`]. Position is a [`Beat`],
/// which is `f64`: time is one of the places the graph stays wide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    /// Whether the transport is rolling.
    pub playing: bool,
    /// Tempo.
    pub tempo: Bpm,
    /// The beat at the first frame of the block.
    pub beat: Beat,
    /// Loop points, when looping.
    pub looping: Option<LoopRange>,
}

impl Default for Transport {
    /// Stopped at beat zero, 120 BPM, not looping.
    fn default() -> Self {
        Self {
            playing: false,
            tempo: Bpm(120.0),
            beat: Beat(0.0),
            looping: None,
        }
    }
}

/// Most transport changes one block can carry; see [`TransportChanges`].
///
/// A transport command is a user or arrangement action (play, stop, a seek, a
/// tempo or loop edit), so more than a handful inside one block (a few
/// milliseconds) is not music. Bounded so an [`Env`] stays `Copy` and the
/// audio thread never allocates one. Whoever fills the list decides what
/// happens to a change past the bound (the engine lands it at the start of
/// the next block and counts it late).
pub const MAX_TRANSPORT_CHANGES: usize = 8;

/// A change of the transport inside a block: from frame [`at`](Self::at) on,
/// the transport is [`to`](Self::to).
///
/// Doc 013 §6: a transport command lands on its exact frame, and the
/// executor never splits a block for it, not even for a start or a seek. The
/// block's [`Env`] carries the change instead, the way it carries a loop
/// wrap, and a node that cares reads the transport at a frame with
/// [`Env::transport_at`] (or walks [`Env::segments`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportChange {
    /// The first frame the new transport applies to. Never
    /// [`Offset::ZERO`]: a change at the first frame is the block's own
    /// [`Env::transport`].
    pub at: Offset,
    /// The transport at frame `at`. Its `beat` is the position *at* that
    /// frame, after the change (the target of a seek, the held position of a
    /// stop).
    pub to: Transport,
}

/// Why [`TransportChanges::push`] refused a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportChangeRejected {
    /// At [`Offset::ZERO`]. A change at the block's first frame is the
    /// block's transport, not a change inside it.
    AtBlockStart,
    /// Before the last change pushed. Changes are pushed in time order.
    OutOfOrder,
    /// [`MAX_TRANSPORT_CHANGES`] are already held.
    Full,
}

impl std::fmt::Display for TransportChangeRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AtBlockStart => "a change at the first frame is the block's transport",
            Self::OutOfOrder => "transport changes must be pushed in time order",
            Self::Full => "too many transport changes in one block",
        })
    }
}

impl std::error::Error for TransportChangeRejected {}

/// The transport changes inside one block: in time order, at distinct
/// offsets, none at the first frame. A fixed-capacity list, so an [`Env`]
/// that holds one is still `Copy` and building it never allocates.
///
/// Every offset is checked against the block when the list is handed to
/// [`Executor::process_with_changes`](crate::Executor::process_with_changes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportChanges {
    len: u8,
    items: [TransportChange; MAX_TRANSPORT_CHANGES],
}

impl TransportChanges {
    /// No change: the transport of the block's first frame holds throughout
    /// (moving, if it rolls).
    pub const NONE: Self = Self {
        len: 0,
        items: [TransportChange {
            at: Offset::ZERO,
            to: Transport {
                playing: false,
                tempo: Bpm(120.0),
                beat: Beat(0.0),
                looping: None,
            },
        }; MAX_TRANSPORT_CHANGES],
    };

    /// Add a change after the ones already held. A change at the same offset
    /// as the last one **replaces** it: two commands landing on one frame
    /// leave the transport where the later one put it, and a frame has one
    /// transport.
    pub fn push(&mut self, at: Offset, to: Transport) -> Result<(), TransportChangeRejected> {
        if at == Offset::ZERO {
            return Err(TransportChangeRejected::AtBlockStart);
        }
        let len = self.len as usize;
        if let Some(last) = self.items[..len].last_mut() {
            if at < last.at {
                return Err(TransportChangeRejected::OutOfOrder);
            }
            if at == last.at {
                last.to = to;
                return Ok(());
            }
        }
        if len == MAX_TRANSPORT_CHANGES {
            return Err(TransportChangeRejected::Full);
        }
        self.items[len] = TransportChange { at, to };
        self.len += 1;
        Ok(())
    }

    /// The changes, in time order.
    pub fn as_slice(&self) -> &[TransportChange] {
        &self.items[..self.len as usize]
    }

    /// Whether the block has no change.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many changes the block has.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether a change at a new offset would be refused as
    /// [`Full`](TransportChangeRejected::Full).
    pub fn is_full(&self) -> bool {
        self.len as usize == MAX_TRANSPORT_CHANGES
    }
}

impl Default for TransportChanges {
    fn default() -> Self {
        Self::NONE
    }
}

/// The per-block environment — read once per block by the executor and passed
/// by reference to every node (doc 013 §2, the `Env` of
/// `tutti-core/src/lib.rs:175-181` given a seam).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Env {
    /// The block's first frame on the executor's clock: frames rendered
    /// before it.
    ///
    /// A [`Frame`] — an absolute position — and never an
    /// [`Offset`](crate::Offset): the two convert only through this `Env`
    /// ([`offset_of`](Self::offset_of), [`frame_at`](Self::frame_at)), so an
    /// event cannot be stamped with one where the other was meant.
    pub frame: Frame,
    /// The rate the graph runs at.
    pub sample_rate: SampleRate,
    /// This block's length.
    pub block_len: Samples,
    /// The transport at the block's first frame.
    pub transport: Transport,
    /// Where the transport changes inside the block: a start, a stop, a
    /// seek, a tempo or loop edit landing on its frame (doc 013 §6). Empty
    /// for most blocks. The executor does not split the block at them; read
    /// the transport at a frame with [`transport_at`](Self::transport_at), or
    /// walk [`segments`](Self::segments).
    pub changes: TransportChanges,
}

/// What a node is told about *this* call besides its buffers.
#[derive(Clone, Copy, Debug)]
pub struct Cx<'a> {
    /// The block's environment.
    pub env: &'a Env,
    /// The compiled **arrival latency** at this node's inputs: how far behind
    /// the graph's own time its input signal is. A node reading the transport
    /// reads it this many frames earlier, so a latent path and a direct path
    /// agree about which beat a sample belongs to.
    pub arrival: Latency,
}

/// A processor in the graph.
///
/// Object-safe: the executor stores `Box<dyn Node>`. `Send + 'static` so a
/// unit can be built on the control thread and moved to the audio thread in a
/// commit (phase 2); not `Sync`, because only the thread that runs a node
/// ever touches it.
pub trait Node: Send + 'static {
    /// Ports, latency, tail. Must not change after [`prepare`](Self::prepare)
    /// without the node being re-inserted — a shape change is a recompile.
    fn shape(&self) -> Shape;

    /// Size buffers for `p`. Control thread; may allocate. Called before the
    /// first [`process`](Self::process), and again whenever the rate or the
    /// maximum block changes.
    fn prepare(&mut self, p: &Prepare);

    /// Render one block. Audio thread: must not allocate, lock or block.
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status;

    /// Return to the state of a freshly prepared node.
    fn reset(&mut self);
}

/// How a value becomes a node, and what the caller gets back to control it.
///
/// The replacement for `node_as::<T>` (doc 013 §2): the *type* of the control
/// surface is fixed at insertion, so nothing downcasts to find a node's
/// parameters later. A plain node has no controls; a node with live
/// parameters implements this on a builder type and returns its `Param<U>`
/// handles.
pub trait IntoNode {
    /// What the caller keeps: `Param<U>` handles, `RtPublish` cells, or `()`.
    type Controls;

    /// Split into the unit the executor will own and the handles the caller
    /// keeps.
    fn into_node(self) -> (Box<dyn Node>, Self::Controls);
}

impl<N: Node> IntoNode for N {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(self), ())
    }
}

impl IntoNode for Box<dyn Node> {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        (self, ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `all` and `covers` agree at the edges, including the 64-channel cap.
    ///
    /// Mutation: `n >= 64` → `n > 64` in `all` → `1u64 << 64` overflows →
    /// panics in debug → fails.
    #[test]
    fn masks_cover_their_width() {
        assert_eq!(SilenceMask::all(0), SilenceMask(0));
        assert_eq!(SilenceMask::all(3), SilenceMask(0b111));
        assert_eq!(SilenceMask::all(64), SilenceMask(u64::MAX));
        assert!(SilenceMask(0b111).covers(3));
        assert!(!SilenceMask(0b101).covers(3));
        assert!(SilenceMask::NONE.covers(0));
        assert!(!SilenceMask::NONE.with(70).get(70));
        assert!(SilenceMask::NONE.with(5).get(5));
    }

    /// Scratch built from a `MaxBlock` serves every block up to it, and
    /// refuses past it rather than truncating.
    ///
    /// Mutation: in `Scratch::channel`, clamp `frames` to `max` instead of
    /// asserting → the over-long request returns a short slice and no panic →
    /// the `should_panic` half fails.
    #[test]
    fn scratch_fits_every_prepared_block() {
        let p = Prepare::new(SampleRate(48_000.0), Samples(100));
        let mut s = Scratch::new(p.max_block(), 2);
        assert_eq!(s.channel(1, 100).len(), 100);
        assert_eq!(s.channel(0, 7).len(), 7);
        let over = std::panic::catch_unwind(move || {
            let _ = s.channel(0, 101);
        });
        assert!(over.is_err(), "a block past MaxBlock is refused");
    }
}
