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

use tutti_types::{Beat, Bpm, ChannelLayout, Latency, SampleRate, Samples, Tail};

use crate::io::Io;

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
}

impl Shape {
    /// A shape with these audio widths, no event ports, no latency, no tail,
    /// and no in-place support.
    pub const fn audio(audio_in: ChannelLayout, audio_out: ChannelLayout) -> Self {
        Self {
            audio_in,
            audio_out,
            event_in: 0,
            event_out: 0,
            latency: Latency::ZERO,
            tail: Tail::None,
            in_place: false,
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
    /// The node wrote nothing; every output is silence. The executor zeroes.
    Silent,
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

/// The per-block environment — read once per block by the executor and passed
/// by reference to every node (doc 013 §2, the `Env` of
/// `tutti-core/src/lib.rs:175-181` given a seam).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Env {
    /// Frames the executor has rendered before this block.
    ///
    /// `u64` and not `Samples`: `Samples` is `usize`, which on a 32-bit
    /// target wraps after about a day at 48 kHz, and `SamplePosition` is an
    /// `f64` playhead rather than a count.
    pub frame: u64,
    /// The rate the graph runs at.
    pub sample_rate: SampleRate,
    /// This block's length.
    pub block_len: Samples,
    /// The transport at the block's first frame.
    pub transport: Transport,
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
