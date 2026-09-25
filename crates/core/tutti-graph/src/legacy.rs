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
//! # Settings, and the shadow: [`Legacy::controlled`]
//!
//! `Net::set(Setting)` reached a unit's [`AudioUnit::set`] on the audio
//! thread: the frontend enqueued the setting and the backend drained its
//! queue at the start of each `process` (`fundsp-tutti/src/realnet.rs`,
//! `handle_messages`). Some units depend on exactly that — the sampler
//! voice's `set` writes `play.gain`, a plain field, so a write anywhere but on
//! the copy that renders is lost. The graph has no `Net` to carry it, so
//! [`Legacy::controlled`] builds the same path per node:
//!
//! - a preallocated SPSC **settings ring** ([`LEGACY_SETTINGS_CAPACITY`]
//!   deep) that the node drains at the start of each call, before the first
//!   chunk, applying each setting through `AudioUnit::set` in the order sent;
//! - a **shadow**: a clone of the unit taken at construction, behind an
//!   `Arc<Mutex<_>>` on the control side, never processed and never locked by
//!   the audio thread. Every [`LegacyControls::set`] is applied to it as well,
//!   so it holds the unit's by-value params as the caller last set them — what
//!   a fork (doc 013, Phase 3 PR 2) clones from — and it shares whatever the
//!   unit shares through `Arc`s (a voice's command channel, a meter cell), so
//!   node-specific handles can be read off it, as `bevy-tutti` reads them off
//!   its frontend `Net` today.
//!
//! **A full ring never blocks and never drops.** The setting is *held* on
//! the control side, coalesced per parameter with anything already held for
//! it (a later value for the same parameter and address replaces the earlier
//! one, in place), and sent by the next [`set`](LegacyControls::set) or
//! [`flush`](LegacyControls::flush) that finds room, ahead of anything newer.
//! The answer says which happened ([`Delivery`], after doc 011's `Delivered`
//! minus its "dropped": nothing here is). 64 settings per block per node is
//! far above what a UI or an automation lane sends; `Net` had 256 for the
//! whole graph.
//!
//! **Timing.** A setting sent between blocks lands on the next block, as it
//! did through `Net`. `Net`'s backend drained per `process` call, which the
//! engine made at most 64 frames long; the graph hands a node the whole
//! block, so a setting that races a block lands at the block's start rather
//! than at the next 64-frame chunk. A pure node that is parked does not drain
//! until it is next called — nothing it renders can differ, since it renders
//! only silence meanwhile; its ring may fill, and holding covers that.
//!
//! (Two shapes never reached the park even before this: the executor never
//! skips a node with no audio inputs, and never one with no outputs at all.
//! The first covers a 0-input source; the second a sink that exists for its
//! side effects. The hazard was a unit *with* audio inputs fed out of band —
//! a plugin instrument with a sidechain, a vocoder carrier. `Modified` closes
//! it for every shape at once, without leaning on either rule.)

use std::mem::Discriminant;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_node::buffer::BufferVec;
use tutti_node::{Address, AudioUnit, Parameter, Setting, MAX_BUFFER_SIZE};
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
    /// The audio-thread end of [`Legacy::controlled`]'s settings ring.
    settings: Option<HeapCons<Setting>>,
}

/// Settings one [`Legacy::controlled`] node's ring holds between two of its
/// calls. Past that, [`LegacyControls::set`] holds and coalesces on the
/// control side (see the `legacy` module docs, `src/legacy.rs`).
pub const LEGACY_SETTINGS_CAPACITY: usize = 64;

/// What became of a [`LegacyControls::set`] or
/// [`flush`](LegacyControls::flush). Never "dropped": a setting that did not
/// fit is held, not lost.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Everything sent so far is in the ring: the unit applies it at the
    /// start of its next call.
    Queued,
    /// The ring was full. What did not fit is held on the control side,
    /// coalesced per parameter, and goes out on the next `set` or `flush`
    /// that finds room. Transient: call [`flush`](LegacyControls::flush)
    /// after the executor has run a block. The shadow already has it.
    Held,
}

/// Which parameter a [`Setting`] sets: its kind and its address. Two
/// settings with the same key set the same thing, so the later one wins
/// when both are held.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SettingKey {
    kind: Discriminant<Parameter>,
    address: [(u8, u64); 4],
}

impl SettingKey {
    fn of(setting: &Setting) -> Self {
        // `Setting`'s address is private; walk it the way a structural unit
        // does, one level per `peel`. Four levels is `Setting`'s own bound.
        let mut rest = setting.clone();
        let mut address = [(0u8, 0u64); 4];
        for level in &mut address {
            *level = match rest.direction() {
                Address::Null => (0, 0),
                Address::Left => (1, 0),
                Address::Right => (2, 0),
                Address::Index(i) => (3, i as u64),
                Address::Node(n) => (4, n.get()),
            };
            rest = rest.peel();
        }
        Self {
            kind: std::mem::discriminant(setting.parameter()),
            address,
        }
    }
}

/// The control side of a [`Legacy::controlled`] node: its settings ring, and
/// the shadow copy of its unit. Control thread only.
pub struct LegacyControls<T> {
    tx: HeapProd<Setting>,
    /// Settings that did not fit, oldest first, one per parameter.
    held: Vec<(SettingKey, Setting)>,
    shadow: Arc<Mutex<T>>,
}

impl<T: AudioUnit> LegacyControls<T> {
    /// Send `setting` to the unit, and apply it to the shadow now.
    ///
    /// Anything held from before goes first, so settings reach the unit in
    /// the order they were sent (less the ones a later value for the same
    /// parameter replaced). Never blocks, never drops.
    pub fn set(&mut self, setting: Setting) -> Delivery {
        self.shadow().set(setting.clone());
        if self.flush() == Delivery::Queued && self.tx.try_push(setting.clone()).is_ok() {
            return Delivery::Queued;
        }
        let key = SettingKey::of(&setting);
        match self.held.iter_mut().find(|(k, _)| *k == key) {
            Some((_, old)) => *old = setting,
            None => self.held.push((key, setting)),
        }
        Delivery::Held
    }

    /// Send what is held, as far as the ring has room.
    pub fn flush(&mut self) -> Delivery {
        let sent = self
            .held
            .iter()
            .take_while(|(_, s)| self.tx.try_push(s.clone()).is_ok())
            .count();
        self.held.drain(..sent);
        if self.held.is_empty() {
            Delivery::Queued
        } else {
            Delivery::Held
        }
    }

    /// Settings held because the ring was full, one per parameter.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The shadow: a clone of the unit taken at construction, with every
    /// setting sent since applied to it. Never processed, never prepared
    /// (its sample rate is whatever the unit had when it was wrapped). Read
    /// by-value params and `Arc`-shared handles from it; do not mistake it
    /// for the unit that renders. A poisoned lock is recovered: the shadow
    /// renders nothing, so a panic mid-`set` leaves nothing torn that audio
    /// could hear.
    pub fn shadow(&self) -> MutexGuard<'_, T> {
        self.shadow.lock().unwrap_or_else(PoisonError::into_inner)
    }
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

    /// Wrap `unit` with a settings path: the `Net::set` replacement. Returns
    /// the node and its [`LegacyControls`] — a ring the node drains into
    /// `AudioUnit::set` at the start of each call, and a never-processed
    /// shadow clone of `unit` that every setting is also applied to. See
    /// "Settings, and the shadow" in the module docs (`src/legacy.rs`).
    ///
    /// Not [`pure`](Self::pure) unless marked with
    /// [`assume_pure`](Self::assume_pure).
    pub fn controlled<T: AudioUnit + Clone + 'static>(unit: T) -> (Self, LegacyControls<T>) {
        let shadow = Arc::new(Mutex::new(unit.clone()));
        let (tx, rx) = HeapRb::<Setting>::new(LEGACY_SETTINGS_CAPACITY).split();
        let mut node = Self::new(unit);
        node.settings = Some(rx);
        let controls = LegacyControls {
            tx,
            held: Vec::new(),
            shadow,
        };
        (node, controls)
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
            settings: None,
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
        // Settings first, as `Net`'s backend applied its queue at the start
        // of `process`. `Setting` owns no heap memory, so neither the pop nor
        // the drop inside `set` allocates or frees.
        if let Some(rx) = &mut self.settings {
            while let Some(setting) = rx.try_pop() {
                self.unit.set(setting);
            }
        }
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
