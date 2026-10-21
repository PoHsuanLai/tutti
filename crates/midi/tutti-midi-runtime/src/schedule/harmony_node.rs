//! [`HarmonyNode`]: a sequencer's chord and scale lanes as a graph node,
//! sending [`Harmony`] events out of an event port,
//! for a node that follows them (a hosted VST3 plugin's chord and scale
//! events).
//!
//! It walks each block's transport as a MIDI clip does (the shared walk,
//! `walk.rs`): every change on its frame, loop wraps and mid-block starts
//! included, with no play cursor. Chords and scales are **context**, not
//! notes — each holds until the next — so where playback jumps (a start, a
//! seek, a loop wrap) or the lanes are replaced, the node re-states the chord
//! and the scale in force there, on the jump's frame. A consumer joining
//! mid-lane therefore always has the context, which the timeline-polling
//! source this replaces left out ("priming-on-join").
//!
//! The lanes are published to the node with [`ArcLend`]
//! ([`HarmonyControls::set`]); the node reads them once per block.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_core::{ArcLend, Beat, ChannelLayout, Reader};
use tutti_graph::{
    Cx, Event, EventWriter, ForkCause, ForkMode, ForkSource, Forked, Harmony, HarmonyKind,
    IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status,
};

use super::walk::{Beated, Visit, Walk};

/// The most events a [`HarmonyNode`] writes in one block, its declared
/// event capacity: past it the rest are refused and counted
/// (`Executor::dropped_events`).
pub const HARMONY_EVENT_CAPACITY: u32 = 64;

/// A chord or scale taking effect at a beat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedHarmony {
    /// Where it takes effect.
    pub beat: Beat,
    /// The chord or scale.
    pub harmony: Harmony,
}

impl TimedHarmony {
    /// `harmony` from `beat`.
    pub const fn new(beat: Beat, harmony: Harmony) -> Self {
        Self { beat, harmony }
    }
}

impl Beated for TimedHarmony {
    fn beat(&self) -> f64 {
        self.beat.get()
    }
}

/// Published lanes: the changes sorted by beat (non-finite beats dropped),
/// and a generation that tells the node they were replaced.
struct Lanes {
    generation: u64,
    changes: Box<[TimedHarmony]>,
}

fn lanes(generation: u64, changes: impl IntoIterator<Item = TimedHarmony>) -> Lanes {
    let mut v: Vec<TimedHarmony> = changes
        .into_iter()
        .filter(|c| c.beat.get().is_finite())
        .collect();
    // Stable: two changes at one beat keep their order, the later winning.
    v.sort_by(|a, b| a.beat.get().total_cmp(&b.beat.get()));
    Lanes {
        generation,
        changes: v.into_boxed_slice(),
    }
}

/// What the node and its controls share.
struct Shared {
    /// What the audio thread reads, once per block, through the node's
    /// reader.
    cell: Arc<ArcLend<Lanes>>,
    /// The lanes last published, for the control side (a fork's snapshot, a
    /// count, the unchanged-lanes check). The node never takes this lock.
    current: Mutex<Arc<Lanes>>,
    next_generation: AtomicU64,
}

impl Shared {
    fn new(changes: impl IntoIterator<Item = TimedHarmony>) -> Arc<Self> {
        let first = Arc::new(lanes(0, changes));
        Arc::new(Self {
            cell: Arc::new(ArcLend::from_arc(Arc::clone(&first))),
            current: Mutex::new(first),
            next_generation: AtomicU64::new(1),
        })
    }

    fn current(&self) -> Arc<Lanes> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A sequencer's chord and scale lanes as a graph node with one event output.
/// See the module docs.
///
/// ```
/// use tutti_core::{Beat, NodeKey, SampleRate, Samples};
/// use tutti_graph::{Editor, Harmony, Prepare};
/// use tutti_midi_runtime::{HarmonyNode, TimedHarmony};
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let c_major = Harmony::chord(60, 60, 0b1001_0001);
/// let node = HarmonyNode::new([TimedHarmony::new(Beat(0.0), c_major)]);
/// let controls = editor.insert(NodeKey(1), "harmony", node);
/// assert_eq!(controls.len(), 1);
/// ```
pub struct HarmonyNode {
    shared: Arc<Shared>,
    /// A reader of `shared.cell`, registered in [`over`](Self::over): a
    /// fork's node is built over its own cell and registers there.
    lanes: Reader<Lanes>,
    /// The generation of the lanes this node last walked.
    generation: u64,
    walk: Walk,
}

impl HarmonyNode {
    /// A node sending `changes` (in any order: they are sorted by beat).
    pub fn new(changes: impl IntoIterator<Item = TimedHarmony>) -> Self {
        Self::over(Shared::new(changes))
    }

    /// A node over `shared`, reading it through a reader registered here.
    /// Control thread: registering may allocate.
    fn over(shared: Arc<Shared>) -> Self {
        let generation = shared.current().generation;
        Self {
            lanes: shared.cell.reader(),
            shared,
            generation,
            walk: Walk::default(),
        }
    }
}

/// The host's handle on a [`HarmonyNode`] in a graph. Cheap to clone; every
/// clone reaches the same node.
#[derive(Clone)]
pub struct HarmonyControls {
    shared: Arc<Shared>,
}

impl HarmonyControls {
    /// Sends `changes` from the next block on, the node re-stating the context
    /// in force on that block's first frame. Changes equal to the lanes' (in
    /// beat order) change nothing. Control thread.
    pub fn set(&self, changes: impl IntoIterator<Item = TimedHarmony>) {
        let next = lanes(0, changes);
        let mut current = self
            .shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if current.changes == next.changes {
            return;
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        let published = Arc::new(Lanes {
            generation,
            changes: next.changes,
        });
        *current = Arc::clone(&published);
        self.shared.cell.publish(published);
    }

    /// Sends nothing from the next block on. Control thread.
    pub fn clear(&self) {
        self.set([]);
    }

    /// Frees the lanes the node has let go of since the last
    /// [`set`](Self::set) (ones a block still held when they were replaced),
    /// without publishing; returns how many are still held. Control thread,
    /// from the owner's tick; with nothing retired it is a lock and a length
    /// check.
    pub fn reclaim(&self) -> usize {
        self.shared.cell.reclaim()
    }

    /// How many changes the lanes hold.
    pub fn len(&self) -> usize {
        self.shared.current().changes.len()
    }

    /// Whether the lanes hold no change.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The harmony side of the walk: each change on its frame, and at a jump the
/// chord and the scale in force there.
struct Sending<'a, 'w> {
    out: &'a mut EventWriter<'w>,
}

impl Sending<'_, '_> {
    /// The latest change of `kind` strictly before `beat`.
    fn in_force(changes: &[TimedHarmony], beat: f64, kind: HarmonyKind) -> Option<Harmony> {
        let before = changes.partition_point(|c| c.beat.get() < beat);
        changes[..before]
            .iter()
            .rev()
            .find(|c| c.harmony.kind() == kind)
            .map(|c| c.harmony)
    }
}

impl Visit<TimedHarmony> for Sending<'_, '_> {
    fn stop(&mut self, _at: Option<Offset>) {}

    /// Re-state the context in force a frame before the jump's beat (a
    /// change within that frame is sent by the walk itself, on the same
    /// frame, after this).
    fn jump(&mut self, at: Option<Offset>, beat: f64, changes: &[TimedHarmony]) {
        let Some(at) = at else { return };
        for kind in [HarmonyKind::Chord, HarmonyKind::Scale] {
            if let Some(h) = Self::in_force(changes, beat, kind) {
                // Refused past the capacity: counted by the executor.
                let _ = self.out.push(Event::harmony(at, h));
            }
        }
    }

    fn event(&mut self, at: Offset, c: &TimedHarmony) {
        let _ = self.out.push(Event::harmony(at, c.harmony));
    }
}

impl Node for HarmonyNode {
    /// No audio, one event output of [`HARMONY_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(HARMONY_EVENT_CAPACITY)
            // Event-only: the clip node's walk over its changes, nothing per
            // frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let lanes = self.lanes.lend();
        if lanes.generation != self.generation {
            self.generation = lanes.generation;
            self.walk.forget();
        }
        let mut sending = Sending {
            out: io.event_out(0),
        };
        for (start, seg) in cx.env.segments() {
            self.walk
                .segment(&lanes.changes, cx.env, start, &seg, &mut sending);
        }
        Status::Modified
    }

    /// Start over: the next block re-states the context.
    fn reset(&mut self) {
        self.walk.forget();
    }
}

/// A fork of a [`HarmonyNode`]: its lanes as they stand, its own cell.
struct HarmonyFork {
    shared: Arc<Shared>,
}

impl ForkSource for HarmonyFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        let lanes = self.shared.current();
        let node = HarmonyNode::new(lanes.changes.iter().copied());
        Ok(Forked::new(Box::new(node)))
    }
}

impl IntoNode for HarmonyNode {
    type Controls = HarmonyControls;

    fn into_parts(self) -> NodeParts<HarmonyControls> {
        let controls = HarmonyControls {
            shared: Arc::clone(&self.shared),
        };
        let fork = HarmonyFork {
            shared: Arc::clone(&self.shared),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::Harmony;

    /// **Lanes a block still held when `set` replaced them are freed by the
    /// next `reclaim`**, with no further publish.
    ///
    /// Mutation (run): `HarmonyControls::reclaim` returning 0 without
    /// reclaiming → the first lanes are still alive, fails.
    #[test]
    fn reclaim_frees_lanes_a_block_held_across_their_replacement() {
        let c_major = Harmony::chord(60, 60, 0b1001_0001);
        let mut node = HarmonyNode::new([TimedHarmony::new(Beat(0.0), c_major)]);
        let controls = HarmonyControls {
            shared: Arc::clone(&node.shared),
        };
        let gone = Arc::downgrade(&node.shared.current());
        {
            // A block in progress across the replacement.
            let _held = node.lanes.lend();
            controls.clear();
        }
        assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
        assert_eq!(controls.reclaim(), 0);
        assert!(gone.upgrade().is_none(), "freed by the reclaim");
    }
}
