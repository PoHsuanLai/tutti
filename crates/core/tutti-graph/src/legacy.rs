//! [`Legacy`]: any `tutti_node::AudioUnit` as a [`Node`], unmodified.
//!
//! Doc 013 Phase 1: "a `Legacy<U: AudioUnit>` adapter implements `Node` so all
//! 43 existing nodes run unmodified". One box per node is acceptable here
//! because nothing downcasts through the graph any more. Deleted in Phase 4,
//! once every node is ported.
//!
//! What the adapter has to bridge:
//!
//! - **Block size.** `AudioUnit::process` takes at most `MAX_BUFFER_SIZE` (64)
//!   frames in fundsp's SIMD layout; a [`Node`] takes any block. The adapter
//!   walks the block in 64-frame chunks through two preallocated `BufferVec`s.
//!   This is the *node's* sub-chunking, not the executor's — the executor still
//!   hands every node the whole block.
//! - **Latency and tail.** Read from `latency()` (which fundsp derives from
//!   `route`) and `tail()` at construction and again in `prepare` — both can
//!   depend on the rate — and cached in the [`Shape`]: both take `&mut self`
//!   on `AudioUnit`, and [`Node::shape`] takes `&self`.
//!   The rounding is `Net`'s own (`fundsp-tutti/src/latency/mod.rs:51`), so a
//!   compiled plan and a `Net` agree about the same unit.
//! - **Bit-identity with `Net` holds only for per-sample units.** The adapter
//!   chunks at 64 frames from the start of *each* block, so its chunk
//!   boundaries drift against the ones `Net` would use. A unit whose output
//!   depends only on its per-sample state (filters, oscillators, gains — the
//!   `tests/legacy.rs` comparison) renders bit-identically; one that does
//!   block-rate work at chunk boundaries (a coefficient update per call, a
//!   block FFT) can differ from `Net` by where those boundaries fall.
//! - **In place.** The adapter copies each chunk of input into its own buffer
//!   before the unit runs, so an output that already holds its input is no
//!   hazard: it opts in, and reads aliased channels through [`Io::input`].
//! - **Silence: no claim unless the caller makes it.** See below.
//!
//! # Never skipped, unless [`pure`](Legacy::pure)
//!
//! The executor skips an event-free node whose inputs are silent once its
//! last call was silent and its tail has elapsed (see `Executor`). That is
//! right for a node whose output depends only on its audio inputs, and wrong
//! for one fed **out of band** — a SoundFont or a PolySynth steered through a
//! channel, a plugin instrument driven through its own MIDI queue, a mic
//! monitor, an `AtomicSourceNode` whose base is 0 until someone writes it.
//! Skipped once, such a unit is never called again to notice that it has
//! something to say: parked for good.
//!
//! An `AudioUnit` receives no events, so the adapter cannot tell the two
//! kinds apart, and `Net` never skipped anything. So a `Legacy` makes **no
//! silence claim** by default: it returns [`Status::Modified`], which leaves
//! its outputs unflagged and the node running every block, as `Net` ran it.
//! [`Legacy::pure`] is the opt-in for a unit that *is* a function of its audio
//! inputs (a filter, a gain, a mixer): it scans what the unit wrote, reports
//! the silent channels as [`Status::Masked`], and so becomes skippable.
//!
//! **Downstream masks come with the skip, not without it.** A default
//! `Legacy` could scan its output and report silence purely so the nodes
//! *after* it may skip — but a node with no event inputs that reports silent
//! outputs is exactly the node the executor parks, and the `Node` contract has
//! no status for "silent, but keep calling me". So a default `Legacy` does not
//! scan at all (it saves the scan, too), and a pure filter after it is not
//! skipped on the silence it cannot see. Losing that skip costs CPU on a quiet
//! graph; parking an instrument costs its audio. If the lost skip shows up in
//! a profile, the fix is a contract flag the executor reads, not a scan here.
//!
//! (Two shapes never reached the park even before this: the executor never
//! skips a node with no audio inputs, and never one with no outputs at all.
//! The first covers a 0-input source; the second a sink that exists for its
//! side effects. The hazard was a unit *with* audio inputs fed out of band —
//! a plugin instrument with a sidechain, a vocoder carrier. `Modified` closes
//! it for every shape at once, without leaning on either rule.)

use tutti_node::buffer::BufferVec;
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::{ChannelLayout, Latency, Samples};

use crate::io::Io;
use crate::node::{ConstantMask, Cx, Node, Prepare, Resolution, Shape, SilenceMask, Status};

/// An `AudioUnit` running as a [`Node`].
pub struct Legacy {
    unit: Box<dyn AudioUnit>,
    shape: Shape,
    input: BufferVec,
    output: BufferVec,
    /// Whether the unit's output depends only on its audio inputs, so its
    /// silence may be reported (and the node skipped). See the module docs.
    pure: bool,
}

impl Legacy {
    /// Wrap `unit`. It is called every block, silent or not — see "Never
    /// skipped, unless pure" in the module docs.
    pub fn new(unit: impl AudioUnit + 'static) -> Self {
        Self::from_box(Box::new(unit))
    }

    /// Wrap a unit whose output is a function of its **audio inputs alone**
    /// (and its own state, which its declared tail bounds): a filter, a gain,
    /// a mixer. Its silent outputs are reported, so the executor may skip it
    /// once its inputs are silent and its tail has elapsed, and nodes after it
    /// see the silence too.
    ///
    /// A claim the caller makes about the unit, and the executor trusts: wrap
    /// a unit fed any other way — a channel, an atomic, a MIDI queue of its
    /// own — with [`new`](Self::new), or it falls silent for good the first
    /// time it is quiet.
    pub fn pure(unit: impl AudioUnit + 'static) -> Self {
        Self::new(unit).assume_pure()
    }

    /// Mark this node [`pure`](Self::pure): for a node built some other way
    /// ([`from_box`](Self::from_box)). The same claim, with the same
    /// consequence if it is false.
    pub fn assume_pure(mut self) -> Self {
        self.pure = true;
        self
    }

    /// Whether this node was declared [`pure`](Self::pure).
    pub fn is_pure(&self) -> bool {
        self.pure
    }

    /// Wrap an already boxed unit.
    pub fn from_box(mut unit: Box<dyn AudioUnit>) -> Self {
        let (ins, outs) = (unit.inputs(), unit.outputs());
        let shape = Self::probe(unit.as_mut());
        Self {
            unit,
            shape,
            input: BufferVec::new(ins),
            output: BufferVec::new(outs),
            pure: false,
        }
    }

    /// Ask the unit for its shape. Both questions take `&mut self` on
    /// `AudioUnit`, and both answers can depend on the sample rate — a
    /// lookahead limiter's latency is a time — so this runs again in
    /// [`prepare`](Node::prepare), after the rate is set.
    fn probe(unit: &mut dyn AudioUnit) -> Shape {
        // `route` conflates musical delay with processing latency (doc 013
        // D1–D3). Whatever it reports is what `Net` compensates today, so the
        // adapter declares the same figure: the fix belongs in each node's
        // `route`, not in a second opinion here.
        let latency = Latency::new(Samples(
            unit.latency().unwrap_or(0.0).round().max(0.0) as usize
        ));
        Shape::audio(
            ChannelLayout::from_count(unit.inputs() as u16),
            ChannelLayout::from_count(unit.outputs() as u16),
        )
        .with_latency(latency)
        .with_tail(unit.tail())
        .with_in_place()
        // An `AudioUnit` receives no events at all, so it promises nothing
        // about their timing — and says so, rather than inheriting `Sample`.
        .with_event_resolution(Resolution::Block)
    }

    /// The wrapped unit.
    pub fn unit(&self) -> &dyn AudioUnit {
        self.unit.as_ref()
    }
}

impl Node for Legacy {
    fn shape(&self) -> Shape {
        self.shape
    }

    fn prepare(&mut self, p: &Prepare) {
        self.unit.set_sample_rate(p.sample_rate());
        self.unit.allocate();
        self.shape = Self::probe(self.unit.as_mut());
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let ins = self.shape.audio_in.count() as usize;
        let outs = self.shape.audio_out.count() as usize;
        let frames = io.frames();
        let mut start = 0;
        while start < frames {
            let len = (frames - start).min(MAX_BUFFER_SIZE);
            for c in 0..ins {
                self.input.channel_f32_mut(c)[..len]
                    .copy_from_slice(&io.input(c)[start..start + len]);
            }
            self.unit
                .process(len, &self.input.buffer_ref(), &mut self.output.buffer_mut());
            for c in 0..outs {
                io.output(c)[start..start + len]
                    .copy_from_slice(&self.output.channel_f32_mut(c)[..len]);
            }
            start += len;
        }
        // No claim unless the caller made one for us (see the module docs):
        // any silence reported here would let the executor park the unit,
        // and a unit fed out of band would never be called again.
        if !self.pure {
            return Status::Modified;
        }
        // Pure: report the silence the unit produced, so the executor can
        // skip it (it has no event inputs, so its tail decides — see
        // `Executor`). One scan of what was just written; cheap next to the
        // unit.
        let mut silent = SilenceMask::NONE;
        for c in 0..outs {
            if io
                .output(c)
                .iter()
                .all(|&x| x == 0.0 && x.is_sign_positive())
            {
                silent = silent.with(c);
            }
        }
        Status::Masked {
            silent,
            constant: ConstantMask(silent.0),
        }
    }

    fn reset(&mut self) {
        self.unit.reset();
    }
}
