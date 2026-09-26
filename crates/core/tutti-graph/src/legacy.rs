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
//! - **Time read out of band: the renderer renders chunk-major.** See below.
//! - **Modulated params.** A unit with a `tutti_node::ParamFeed`
//!   (`AudioUnit::param_feed`) declares the feed's params as the node's
//!   param ports ([`Shape::params`]), answers [`Node::param_base`] from
//!   `AudioUnit::param_base`, and before each 64-frame call the adapter
//!   feeds it the chunk of every param the graph modulates this block and
//!   clears the rest, which the unit then reads from its own controls. The
//!   bridge that replaced the `with_param_inputs` ports and their base /
//!   shaper / sum chain (design doc 013 item 6).
//!
//! # A timeline polled per call: [`Shape::legacy`]
//!
//! An `AudioUnit` receives no `Env`. One that follows the transport (a
//! sampler voice, and so every clip reader; a MIDI clip source feeding a
//! synth; the in-process VST2 plugin's transport) polls a shared timeline, an
//! `Arc<dyn Timeline>` in tutti-core, on every `process` call, and takes what
//! it reads as the position of that call's first frame. `Net` rendered every
//! node 64 frames at a time and moved its clock between chunks, so the poll
//! was right to the chunk, and every reader of one timeline saw it move
//! forward only. The graph moves its clock once per block; over a longer
//! block every chunk would read the same beat, and a voice would replay its
//! first 64 frames `block / 64` times (a dry 440 Hz voice measured 768 Hz at
//! 1024).
//!
//! So every `Legacy` node declares [`Shape::legacy`], a plan holding one
//! reports [`Plan::has_legacy`](crate::Plan::has_legacy), and a renderer that
//! sees it (tutti-core's `Engine` and `RenderClock::render_graph`) hands the
//! executor blocks of at most [`LEGACY_CHUNK`], across **all** nodes, moving
//! its clock between them, exactly as `Net` did. Doc 013 has the decision
//! ("chunk-major `Legacy` compatibility mode") and why per-node seating of a
//! shared timeline was rejected; it goes away as nodes port natively and
//! read `Env::transport_at` (Phase 4).
//!
//! # Never skipped, unless [`pure`](Legacy::pure)
//!
//! The executor skips an event-free node whose inputs are silent once its
//! last call was silent and its tail has elapsed (see `Executor`). That is
//! right for a node whose output depends only on its audio inputs, and wrong
//! for one fed **out of band** — a SoundFont or a PolySynth steered through a
//! channel, a plugin instrument driven through its own MIDI queue, a mic
//! monitor, a constant source whose value is 0 until someone writes its cell.
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
//! - a **shadow**: an isolated deep copy of the unit (`clone()` then
//!   `AudioUnit::isolate`) taken at construction, behind an `Arc<Mutex<_>>`
//!   on the control side, never processed and never locked by the audio
//!   thread. Every [`LegacyControls::set`] is applied to it as well, so it
//!   holds the unit's by-value params as the caller last set them — what a
//!   fork ([`Editor::fork`]) clones from. It is a snapshot, not a window:
//!   `isolate` severs the `Arc` cells a plain clone would share with the live
//!   unit, so writing the shadow never moves live state ahead of the ring.
//!   Live handles come from a node's captured controls (Phase 3 PR 9).
//!
//! **A full ring never blocks and never drops.** The setting is *held* on
//! the control side, coalesced per parameter with anything already held for
//! it: the earlier value (same parameter kind and address) is removed and the
//! new one appended, so what reaches the unit is always a subsequence of what
//! was sent — each held parameter at its last send's position. Held settings
//! go out ahead of anything newer, on the next [`set`](LegacyControls::set),
//! [`flush`](LegacyControls::flush), or [`Editor::collect`] (and so every
//! commit) of the editor the node was built for, which holds each node's queue
//! weakly — a burst that fills the ring and then goes quiet is still delivered.
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
//!
//! # Forking
//!
//! [`Legacy`] is a builder, not the node: inserting it
//! ([`IntoNode::into_parts`]) hands the editor the node *and* a
//! [`ForkSource`], so an `AudioUnit` is forkable ([`Editor::fork`]) from day
//! one **if it says so**: `AudioUnit::forkable()`, default `true`, is the
//! unit's promise that its `isolate` severs all its shared mutable state.
//! The fork trusts that promise and nothing else. A unit that cannot keep it
//! answers `false` — `MicMonitorNode` (its clone shares the ring consumer),
//! `InProcessVst2Client` (clones share the plugin; an out-of-process
//! `PluginClient` is a native node, not an `AudioUnit`) — and
//! so does any unit holding one (`Net` asks its vertices); the node is then
//! inserted without a fork source, and a fork that needs it is
//! [`ForkError::NotForkable`](crate::ForkError::NotForkable).
//! [`Legacy::unforkable`] opts a node out explicitly. A fork follows fundsp's sequence (`PendingClone::isolate_for_offline`
//! then `Net::reset`): clone, `AudioUnit::isolate`, `AudioUnit::rebind_offline`
//! for an offline fork, `AudioUnit::reset` — with a host's
//! [`with_fork_hook`](Legacy::with_fork_hook) step, if it added one, between
//! the rebind and the reset. What it clones depends on how the node was built:
//!
//! - **[`Legacy::controlled`]: the shadow.** It has every setting sent
//!   through [`LegacyControls`] applied, in order, which is the only way a
//!   `Legacy`'s by-value state changes after insert — the sampler voice's
//!   `play.gain` included. Its limit is the shadow's: it was isolated when it
//!   was built, so a value the unit shares through an `Arc` cell *and* that
//!   something other than these controls writes is at whatever the shadow
//!   last had (its value at construction, or the last `set` that wrote it).
//!   Settings are how a `Legacy` is steered today; a handle captured from the
//!   unit (Phase 3 PR 9) that writes such a cell directly is not seen.
//! - **Any other `Legacy`: a clone taken at insert**, never processed, and
//!   deliberately **not** isolated. A plain `Legacy` has no settings path at
//!   all, so nothing can change its by-value state after insert except a
//!   cell it shares; a shared cell stays shared with this copy, and a fork
//!   reads its value at fork time, when `isolate` copies it out — what
//!   `Net::clone_isolated` then `isolate` saw. Everything else the live unit
//!   accumulates is running state, which the fork's `reset` clears anyway.
//!   Holding an un-isolated copy is safe because it never runs: it keeps an
//!   inbox or a command channel alive, and never reads from one. (Requiring
//!   `controlled` for every forkable node was the alternative; it would make
//!   export refuse every node the adapter builds with [`Legacy::new`] until
//!   each was rebuilt, for no state that a plain node can have.)
//!
//! **The memory cost.** Either way a forkable `Legacy` keeps a second deep
//! copy of its unit for as long as it is in the graph (the shadow, or the
//! insert-time clone). For most units that is a few hundred bytes of
//! coefficients and state; for one that owns large buffers by value it is
//! all of them again — a `ConvolverNode` copies its IR spectra, megabytes
//! per long reverb, and a delay line its ring. Units that share such data
//! read-only through an `Arc` (a sampler's `Wave`) cost nothing extra.
//! Doc 013 Phase 4 moves the convolver's IR to `Arc` spectra; until then a
//! host short on memory can build such a node [`unforkable`](Legacy::unforkable).
//!
//! A node built with [`IntoNode::into_node`] (a bare `Box<dyn Node>`) has no
//! fork source, and is inserted as [`Unforkable`](crate::Unforkable): not
//! forkable. A fork's own nodes have none either: a
//! fork is not forked again.

use std::collections::VecDeque;
use std::mem::Discriminant;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_node::buffer::BufferVec;
use tutti_node::{Address, AudioUnit, Parameter, Setting, MAX_BUFFER_SIZE};
use tutti_types::{ChannelLayout, Latency, Samples};

use crate::editor::Editor;
use crate::fork::{ForkCause, ForkFaultKind, ForkHealth, ForkMode, ForkSource, Forked};
use crate::io::Io;
use crate::node::{
    ConstantMask, Cx, IntoNode, Node, NodeParts, Prepare, Resolution, Shape, SilenceMask, Status,
};
use crate::param::ParamInput;

/// An `AudioUnit`, ready to run as a [`Node`]: insert it
/// ([`Editor::insert`], through [`IntoNode`]).
///
/// It is a builder, not the node itself, so that insertion can hand the
/// editor a [`ForkSource`] beside the node (see "Forking" in the module
/// docs). [`IntoNode::into_node`] yields the node alone, for a caller that
/// wants a `Box<dyn Node>`; a node inserted that way cannot be forked.
pub struct Legacy {
    node: Adapter,
    /// Where forks come from when not from a clone taken at insert: the
    /// shadow of a [`Legacy::controlled`] node.
    fork_from: Option<Box<dyn Snapshot>>,
    /// Opted out of forking ([`Legacy::unforkable`]).
    unforkable: bool,
    /// A host's last step on each fork ([`Legacy::with_fork_hook`]).
    fork_hook: Option<LegacyForkHook>,
}

/// A step a host adds to every fork of a [`Legacy`] node
/// ([`Legacy::with_fork_hook`]): handed the forked unit after `isolate` and
/// (offline) `rebind_offline`, before `reset`.
pub type LegacyForkHook =
    Box<dyn Fn(&mut dyn AudioUnit, ForkMode<'_>) -> Result<(), ForkCause> + Send>;

/// The node a [`Legacy`] runs as.
struct Adapter {
    unit: Box<dyn AudioUnit>,
    shape: Shape,
    input: BufferVec,
    output: BufferVec,
    /// Whether the unit's output depends only on its audio inputs, so its
    /// silence may be reported (and the node skipped). See the module docs.
    pure: bool,
    /// The audio-thread end of [`Legacy::controlled`]'s settings ring.
    settings: Option<HeapCons<Setting>>,
    /// Whether the last block fed the unit's feed any param, so a block that
    /// feeds none clears it once and then leaves it alone.
    fed: bool,
}

/// Settings one [`Legacy::controlled`] node's ring holds between two of its
/// calls. Past that, [`LegacyControls::set`] holds and coalesces on the
/// control side (see the `legacy` module docs, `src/legacy.rs`).
pub const LEGACY_SETTINGS_CAPACITY: usize = 64;

/// The frames a [`Legacy`] hands its unit per `AudioUnit::process` call,
/// walking each block from its first frame: fundsp's `MAX_BUFFER_SIZE`, the
/// most a call can take. The longest block a renderer hands a plan that
/// [`has_legacy`](crate::Plan::has_legacy) (see the module docs).
pub const LEGACY_CHUNK: usize = MAX_BUFFER_SIZE;

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

/// The control-side end of one controlled node's ring, and what did not fit
/// in it. Shared between its [`LegacyControls`] and, weakly, the [`Editor`]
/// that flushes it on every [`collect`](Editor::collect), so a burst that
/// filled the ring is delivered even if no `set` ever follows it.
pub(crate) struct Outbox {
    tx: HeapProd<Setting>,
    /// Settings that did not fit, one per parameter, in the order of each
    /// parameter's **last** send.
    held: VecDeque<(SettingKey, Setting)>,
}

impl Outbox {
    fn answer(&self) -> Delivery {
        if self.held.is_empty() {
            Delivery::Queued
        } else {
            Delivery::Held
        }
    }

    /// Send what is held, oldest first, as far as the ring has room.
    pub(crate) fn flush(&mut self) -> Delivery {
        while let Some((key, setting)) = self.held.pop_front() {
            if let Err(setting) = self.tx.try_push(setting) {
                self.held.push_front((key, setting));
                break;
            }
        }
        self.answer()
    }

    /// Send `setting` after everything held, or hold it.
    ///
    /// Holding coalesces by **moving to the back**: an earlier held value for
    /// the same parameter is removed and this one appended. So what reaches
    /// the unit is always a subsequence of what was sent, with each held
    /// parameter at its last send's position. Replacing in place would
    /// reorder: with the ring full, `Center(1000)`, `CenterQ(500, 0.7)`,
    /// `Center(2000)` would deliver `Center(2000)` *before* the `CenterQ`,
    /// leaving the unit at 500 Hz while the shadow — which applied all three
    /// in order — says 2000, for good.
    fn send(&mut self, setting: Setting) -> Delivery {
        if self.flush() == Delivery::Queued {
            match self.tx.try_push(setting) {
                Ok(()) => return Delivery::Queued,
                Err(setting) => return self.hold(setting),
            }
        }
        self.hold(setting)
    }

    fn hold(&mut self, setting: Setting) -> Delivery {
        let key = SettingKey::of(&setting);
        self.held.retain(|(k, _)| *k != key);
        self.held.push_back((key, setting));
        Delivery::Held
    }
}

/// Lock `m`, recovering from poison: nothing behind these locks is ever left
/// torn in a way audio could hear (see [`LegacyControls::shadow`]).
fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The control side of a [`Legacy::controlled`] node: its settings ring, and
/// the shadow copy of its unit. Control thread only.
pub struct LegacyControls<T> {
    outbox: Arc<Mutex<Outbox>>,
    shadow: Arc<Mutex<T>>,
}

impl<T: AudioUnit> LegacyControls<T> {
    /// Send `setting` to the unit, and apply it to the shadow now.
    ///
    /// Anything held from before goes first, so settings reach the unit in
    /// the order they were sent, less any earlier value a later one for the
    /// same parameter replaced while held. Never blocks, never drops.
    pub fn set(&mut self, setting: Setting) -> Delivery {
        self.shadow().set(setting.clone());
        lock(&self.outbox).send(setting)
    }

    /// Send what is held, as far as the ring has room. The editor the node
    /// was built for does this on every [`collect`](Editor::collect) (and so
    /// on every commit); calling it here is only for a host that wants the
    /// answer.
    pub fn flush(&mut self) -> Delivery {
        lock(&self.outbox).flush()
    }

    /// Settings held because the ring was full, one per parameter.
    pub fn held(&self) -> usize {
        lock(&self.outbox).held.len()
    }

    /// The shadow: an **isolated deep copy** of the unit — `clone()`, then
    /// [`AudioUnit::isolate`], taken at construction — with every setting
    /// sent since applied to it. Never processed, never prepared (its sample
    /// rate is whatever the unit had when it was wrapped).
    ///
    /// It is a **by-value snapshot**: what a fork ([`Editor::fork`])
    /// clones, and where a plain field the unit's `set` writes (a sampler
    /// voice's `play.gain`) can be read back. It is *not* a window onto the
    /// live unit: `isolate` severs the `Arc` cells a clone would share, so a
    /// `set` here never writes live state ahead of the ring (a clone of an
    /// `SvfFilterNode` shares its param atomics, and would move the live
    /// cutoff at once, only for the ring to write older values back). Live
    /// handles come from a node's captured controls (Phase 3 PR 9), not from
    /// here.
    ///
    /// A poisoned lock is recovered: the shadow renders nothing, so a panic
    /// mid-`set` leaves nothing torn that audio could hear.
    pub fn shadow(&self) -> MutexGuard<'_, T> {
        lock(&self.shadow)
    }
}

/// A unit a fork can be cloned from: the one place a [`LegacyFork`] reads.
trait Snapshot: Send {
    /// A clone of the unit, as it stands now. Not yet isolated.
    fn snapshot(&self) -> Box<dyn AudioUnit>;
}

/// A plain [`Legacy`]'s clone, taken at insert. Never processed.
impl Snapshot for Box<dyn AudioUnit> {
    fn snapshot(&self) -> Box<dyn AudioUnit> {
        self.clone()
    }
}

/// A [`Legacy::controlled`] node's shadow, shared with its controls.
impl<T: AudioUnit + Clone + 'static> Snapshot for Arc<Mutex<T>> {
    fn snapshot(&self) -> Box<dyn AudioUnit> {
        Box::new(lock(self).clone())
    }
}

/// A [`Legacy`] node's [`ForkSource`]: see "Forking" in the module docs.
struct LegacyFork {
    from: Box<dyn Snapshot>,
    pure: bool,
    hook: Option<LegacyForkHook>,
}

impl ForkSource for LegacyFork {
    /// fundsp's sequence, in its order (`PendingClone::isolate_for_offline`,
    /// then `Net::reset`): `rebind_offline` after `isolate`, because
    /// `isolate` severs the very handles a rebind installs — the other way
    /// round, a transport-aware unit would be rebound and then cut loose, and
    /// render against nothing.
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let mut unit = self.from.snapshot();
        unit.isolate();
        if let ForkMode::Offline(ctx) = mode {
            unit.rebind_offline(ctx);
        }
        // The host's step (`Legacy::with_fork_hook`): after the rebind, so
        // what it installs is not severed; before the reset, as the rebind is.
        if let Some(hook) = &self.hook {
            hook(unit.as_mut(), mode)?;
        }
        unit.reset();
        // Asked of the copy itself, once it is rebound: a unit that can fail
        // while it renders offline hands its probe over here, and the forked
        // editor keeps it (`Editor::fork_health`).
        let health = unit.render_fault();
        let mut node = Adapter::new(unit);
        node.pure = self.pure;
        // A clone cannot fail; only a hook can.
        let forked = Forked::new(Box::new(node));
        Ok(match health {
            Some(probe) => forked.with_health(Arc::new(UnitHealth(probe))),
            None => forked,
        })
    }
}

/// A forked unit's [`RenderFault`](tutti_node::RenderFault) probe, as the
/// fork's [`ForkHealth`]: a failure it reports is [`ForkFaultKind::Failed`].
struct UnitHealth(Arc<dyn tutti_node::RenderFault>);

impl ForkHealth for UnitHealth {
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        self.0
            .fault()
            .map(|cause| (ForkFaultKind::Failed, ForkCause::from_arc(cause)))
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
        self.node.pure = true;
        self
    }

    /// Insert this node without a fork source, whatever its unit's
    /// `AudioUnit::forkable` says: a fork that needs it is
    /// [`ForkError::NotForkable`](crate::ForkError::NotForkable). For a unit
    /// whose `isolate` the caller does not trust, or one too large to keep
    /// a second copy of (see "Forking" in the module docs).
    pub fn unforkable(mut self) -> Self {
        self.unforkable = true;
        self
    }

    /// Add a last step to every fork of this node: `hook` is handed the
    /// forked unit after `isolate` and, offline, `rebind_offline`, and before
    /// `reset` (see "Forking" in the module docs), with the fork's mode.
    ///
    /// For what a host knows about the unit that the unit's own
    /// `rebind_offline` cannot: a MIDI clip installed on the live unit's port
    /// through a handle the host keeps, which `isolate` severed from the
    /// clone and the host re-installs, rebound, on the fork's own port. An
    /// `Err` fails the whole fork as
    /// [`ForkError::Source`](crate::ForkError::Source), naming the key —
    /// for a step that would otherwise render the fork wrong (its notes
    /// dropped) rather than not at all.
    ///
    /// No effect on a node with no fork source (an unforkable unit).
    pub fn with_fork_hook(
        mut self,
        hook: impl Fn(&mut dyn AudioUnit, ForkMode<'_>) -> Result<(), ForkCause> + Send + 'static,
    ) -> Self {
        self.fork_hook = Some(Box::new(hook));
        self
    }

    /// Whether this node was declared [`pure`](Self::pure).
    pub fn is_pure(&self) -> bool {
        self.node.pure
    }

    /// Wrap `unit` with a settings path: the `Net::set` replacement. Returns
    /// the node and its [`LegacyControls`] — a ring the node drains into
    /// `AudioUnit::set` at the start of each call, and a never-processed,
    /// isolated shadow copy of `unit` that every setting is also applied to.
    /// See "Settings, and the shadow" in the module docs (`src/legacy.rs`).
    ///
    /// `editor` is the editor the node will be inserted into: it flushes
    /// what the ring could not take on every [`collect`](Editor::collect),
    /// so held settings go out without another `set`. It holds the queue
    /// weakly; dropping the controls unregisters it.
    ///
    /// Forks of the node are cloned from the shadow, so they carry every
    /// setting sent (see "Forking" in the module docs).
    ///
    /// Not [`pure`](Self::pure) unless marked with
    /// [`assume_pure`](Self::assume_pure).
    pub fn controlled<T: AudioUnit + Clone + 'static>(
        editor: &mut Editor,
        unit: T,
    ) -> (Self, LegacyControls<T>) {
        let mut shadow = unit.clone();
        shadow.isolate();
        let (tx, rx) = HeapRb::<Setting>::new(LEGACY_SETTINGS_CAPACITY).split();
        let outbox = Arc::new(Mutex::new(Outbox {
            tx,
            held: VecDeque::new(),
        }));
        editor.register_outbox(Arc::downgrade(&outbox));
        let shadow = Arc::new(Mutex::new(shadow));
        let mut node = Self::new(unit);
        node.node.settings = Some(rx);
        node.fork_from = Some(Box::new(Arc::clone(&shadow)));
        let controls = LegacyControls { outbox, shadow };
        (node, controls)
    }

    /// Wrap an already boxed unit.
    pub fn from_box(unit: Box<dyn AudioUnit>) -> Self {
        Self {
            node: Adapter::new(unit),
            fork_from: None,
            unforkable: false,
            fork_hook: None,
        }
    }

    /// The wrapped unit.
    pub fn unit(&self) -> &dyn AudioUnit {
        self.node.unit.as_ref()
    }

    /// The shape the unit declares now: probed at construction, so at
    /// whatever rate the unit had then. Inserting prepares it, and probes
    /// again.
    pub fn shape(&self) -> Shape {
        self.node.shape
    }
}

impl IntoNode for Legacy {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(self.node), ())
    }

    /// The node, and a [`ForkSource`]: from the shadow for a
    /// [`controlled`](Legacy::controlled) node, from a clone of the unit
    /// taken now for any other (see "Forking" in the module docs).
    fn into_parts(self) -> NodeParts<()> {
        // The unit's promise, or the caller's opt-out: without it there is
        // no source, and a fork that needs this node is refused.
        if self.unforkable || !self.node.unit.forkable() {
            return NodeParts {
                node: Box::new(self.node),
                controls: (),
                fork: None,
            };
        }
        let from = match self.fork_from {
            Some(shadow) => shadow,
            None => Box::new(self.node.unit.clone()) as Box<dyn Snapshot>,
        };
        let fork = LegacyFork {
            from,
            pure: self.node.pure,
            hook: self.fork_hook,
        };
        NodeParts {
            node: Box::new(self.node),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

impl Adapter {
    fn new(mut unit: Box<dyn AudioUnit>) -> Self {
        let (ins, outs) = (unit.inputs(), unit.outputs());
        let shape = Self::probe(unit.as_mut());
        Self {
            unit,
            shape,
            input: BufferVec::new(ins),
            output: BufferVec::new(outs),
            pure: false,
            settings: None,
            fed: false,
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
        let params = unit.param_feed().map_or(&[][..], |f| f.params());
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
        // It may poll a timeline out of band: rendered chunk-major (see "A
        // timeline polled per call").
        .with_legacy()
        // Its feed's params, the graph may modulate (see the module docs).
        .with_params(params)
    }
}

impl Node for Adapter {
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
        // Bounded by what was there on entry (at most the ring's capacity),
        // so a producer racing this loop cannot keep the callback in it.
        // (No test covers the bound: single-threaded, nothing can push while
        // the loop runs, so a bounded and an unbounded drain look alike.)
        if let Some(rx) = &mut self.settings {
            for _ in 0..rx.occupied_len() {
                match rx.try_pop() {
                    Some(setting) => self.unit.set(setting),
                    None => break,
                }
            }
        }
        // Whether the graph modulates any param this block: when it does
        // not, the feed is cleared once (if the last block fed it) and each
        // chunk skips it.
        let n_params = self.shape.params.len();
        let modulated = n_params != 0 && (0..n_params).any(|k| io.param(k) != ParamInput::Base);
        if !modulated && self.fed {
            if let Some(feed) = self.unit.param_feed() {
                feed.clear_all();
            }
        }
        self.fed = modulated;
        let mut start = 0;
        while start < frames {
            let len = (frames - start).min(LEGACY_CHUNK);
            for c in 0..ins {
                self.input.channel_f32_mut(c)[..len]
                    .copy_from_slice(&io.input(c)[start..start + len]);
            }
            if modulated {
                // The chunk of each modulated param; the rest read their
                // own controls (see the module docs).
                if let Some(feed) = self.unit.param_feed() {
                    for k in 0..n_params {
                        match io.param(k) {
                            ParamInput::Base => feed.clear(k),
                            ParamInput::Frames(v) => feed.feed(k, &v[start..start + len]),
                        }
                    }
                }
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

    fn param_base(&self, port: usize) -> Option<f32> {
        self.unit.param_base(port)
    }
}
