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
//! The lanes are published to the node with [`RtPublish`]
//! ([`HarmonyControls::set`]); the node reads them once per block.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{Beat, ChannelLayout, RtPublish};
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
    cell: RtPublish<Lanes>,
    next_generation: AtomicU64,
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
    /// The generation of the lanes this node last walked.
    generation: u64,
    walk: Walk,
}

impl HarmonyNode {
    /// A node sending `changes` (in any order: they are sorted by beat).
    pub fn new(changes: impl IntoIterator<Item = TimedHarmony>) -> Self {
        Self::over(Arc::new(Shared {
            cell: RtPublish::new(lanes(0, changes)),
            next_generation: AtomicU64::new(1),
        }))
    }

    fn over(shared: Arc<Shared>) -> Self {
        let generation = shared.cell.read().generation;
        Self {
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
        if self.shared.cell.read().changes == next.changes {
            return;
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        self.shared.cell.publish(Arc::new(Lanes {
            generation,
            changes: next.changes,
        }));
    }

    /// Sends nothing from the next block on. Control thread.
    pub fn clear(&self) {
        self.set([]);
    }

    /// How many changes the lanes hold.
    pub fn len(&self) -> usize {
        self.shared.cell.read().changes.len()
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
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let lanes = self.shared.cell.read();
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
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let changes = self.shared.cell.read().changes.clone();
        let node = HarmonyNode::new(changes.iter().copied());
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
