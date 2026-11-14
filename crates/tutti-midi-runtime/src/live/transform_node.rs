//! The MPE transform as a graph node.
//!
//! [`MpeTransformNode`] runs one [`MpeIngest`] over one event stream: MIDI 1.0
//! in is widened by a [`Translator`], classic-MPE channel-spread comes out as
//! native per-note messages, and a Manager Channel's zone-wide messages and bend
//! come out on each member channel of its zone, as the ingest's docs say. It is
//! the input edge's transform for a stream that does not come off a wire (a
//! clip's), so a graph holds the MPE zones in two kinds of place alone, this
//! node and [`MidiIngestNode`](crate::MidiIngestNode), and no instrument holds
//! any.
//!
//! One event input and one event output, every Group. Events that are not MIDI
//! are not forwarded. The zones are [`MpeMode`], set through
//! [`MpeTransformControls::set_mode`] and adopted at the next block. A mode
//! starts each Group at its zones' default bend ranges and a change of it stops
//! and resets the channels that enter or leave MPE control, which is what a
//! receiver's zone change sends ([`MpeIngest::zone_defaults`]); the transform
//! sends it for a Group the moment the mode changes when the Group has carried
//! events, and ahead of the Group's first event when it has not, so a stream
//! uses one Group and the other fifteen cost nothing. The output can hold more events than the input
//! (a zone-wide message is one per channel of the zone): a block's translation
//! goes in whole or is dropped whole, and the drops are counted
//! ([`MpeTransformControls::losses`]).
//!
//! Its fork is a transform in the mode the controls hold when it is made, with
//! no state of the notes: an offline render starts its clip from the top.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_graph::{
    Cost, Cx, Event, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Losses,
    Node, NodeParts, Offset, Prepare, Shape, Status, UmpAt, note_off_room,
};
use tutti_midi_protocol::{MpeMode, Translator};
use tutti_rt::{ArcLend, Reader};
use tutti_ump::MidiGroup;

use super::ingest_node::{MIDI_INPUT_EVENT_CAPACITY, Staging};
use super::mpe_ingest::MpeIngest;
use super::order::BLANK;
use crate::schedule::CLIP_EVENT_DENSITY;

/// The MPE transform of one event stream as a graph node. See the module docs.
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_types::{SampleRate, Samples};
/// use tutti_graph::{Editor, NodeKey, Prepare};
/// use tutti_midi_protocol::MpeMode;
/// use tutti_midi_runtime::MpeTransformNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
/// let controls = editor.insert(NodeKey(1), "mpe", MpeTransformNode::new(MpeMode::Disabled));
/// controls.set_mode(MpeMode::Disabled);
/// assert_eq!(controls.mode(), MpeMode::Disabled);
/// ```
pub struct MpeTransformNode {
    translator: Translator,
    mpe: MpeIngest,
    /// The mode in force.
    mode: MpeMode,
    /// What the host asked for, read once per block. The reader owns its cell
    /// (`Reader::cell`), which the controls publish to.
    request: Reader<MpeMode>,
    /// What the block sends, which may outnumber what it read.
    out: Box<[UmpAt]>,
    /// The events a block translates and the port holds.
    capacity: usize,
    /// The Groups that have carried an event, one bit each.
    active: u16,
    /// The Groups that were sent the mode in force, one bit each.
    told: u16,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for MpeTransformNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpeTransformNode")
            .field("mode", &self.mode)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

/// `group`'s bit in a mask of Groups.
const fn bit(group: MidiGroup) -> u16 {
    1 << group.get()
}

/// Sends `group` the mode's stops and default ranges at `at`; the Group is
/// `told` if they all fit, and is otherwise sent them again ahead of its next
/// event.
fn tell(mpe: &MpeIngest, told: &mut u16, group: MidiGroup, at: Offset, staging: &mut Staging<'_>) {
    if staging.whole(false, |send| mpe.zone_defaults(group, at, send)) {
        *told |= bit(group);
    }
}

impl MpeTransformNode {
    /// A transform that starts in `mode`.
    #[must_use]
    pub fn new(mode: MpeMode) -> Self {
        let cell = Arc::new(ArcLend::new(mode));
        Self {
            translator: Translator::default(),
            mpe: MpeIngest::new(mode),
            mode,
            request: cell.reader(),
            out: vec![BLANK; note_off_room(MIDI_INPUT_EVENT_CAPACITY)].into_boxed_slice(),
            capacity: MIDI_INPUT_EVENT_CAPACITY,
            active: 0,
            told: 0,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// The host's handle on a [`MpeTransformNode`]: its MPE mode. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MpeTransformControls {
    /// What the node reads; the controls publish to it and read it back with
    /// `ArcLend::current`.
    request: Arc<ArcLend<MpeMode>>,
    dropped: Arc<AtomicU64>,
}

impl MpeTransformControls {
    /// Transforms in `mode` from the next block. Control thread.
    ///
    /// A new mode stops and resets the channels that enter or leave MPE control
    /// (M1-100 section 2.2.3: All Sound Off and Reset All Controllers, sent
    /// ahead of the new zones' default bend ranges); the others keep their
    /// notes. Setting the mode in force changes nothing.
    pub fn set_mode(&self, mode: MpeMode) {
        self.request.publish(mode);
    }

    /// The mode last set (or the node's first), which the node adopts at its
    /// next block.
    #[must_use]
    pub fn mode(&self) -> MpeMode {
        *self.request.current()
    }

    /// The translated events a block had no room for, which a translation
    /// drops whole rather than cut partway.
    #[must_use]
    pub fn losses(&self) -> Losses {
        Losses::refused(self.dropped.load(Ordering::Relaxed))
    }
}

/// Frees what the control side retired and no block holds, without publishing;
/// control thread, from the owner's tick.
impl tutti_rt::Reclaim for MpeTransformControls {
    fn reclaim(&self) -> usize {
        self.request.reclaim()
    }
}

impl Node for MpeTransformNode {
    /// No audio; one event input and one event output of the prepared capacity:
    /// [`CLIP_EVENT_DENSITY`] per frame, which is what a clip's port holds, so
    /// a clip's events pass whole (a fan-out may reach past it, and is counted).
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, 1)
            .with_event_capacity(self.capacity as u32)
            // Event-only: a translation per event, nothing per frame.
            .with_cost(Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        let capacity = CLIP_EVENT_DENSITY.capacity(p) as usize;
        if capacity != self.capacity {
            self.capacity = capacity;
            self.out = vec![BLANK; note_off_room(capacity)].into_boxed_slice();
        }
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let requested = *self.request.lend();
        let mut staging = Staging::new(&mut self.out, self.capacity);
        // A new mode's stops and ranges, at frame 0, ahead of the block's
        // events, for the Groups that have carried some.
        if requested != self.mode {
            self.mode = requested;
            self.mpe.set_mode(requested);
            self.told = 0;
            for group in MidiGroup::all() {
                if self.active & bit(group) != 0 {
                    tell(&self.mpe, &mut self.told, group, Offset::ZERO, &mut staging);
                }
            }
        }
        for event in io.events(0) {
            let EventKind::Midi(raw) = event.kind else {
                continue;
            };
            // A Group's first event since the mode was set starts it at the
            // mode's ranges.
            if let Some(group) = raw.group() {
                self.active |= bit(group);
                if self.told & bit(group) == 0 {
                    tell(&self.mpe, &mut self.told, group, event.offset, &mut staging);
                }
            }
            if let Some(ev) = self.translator.translate(raw) {
                let mpe = &mut self.mpe;
                staging.whole(ev.is_note_off(), |send| {
                    mpe.translate(&(event.offset, ev), send);
                });
            }
        }
        let Staging { kept, dropped, .. } = staging;
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
            // The executor's too, so an export watching its count sees it.
            io.count_lost(dropped);
        }
        for ev in &self.out[..kept] {
            // Refused past the capacity: counted by the executor.
            let _ = io.event_out(0).push(Event::midi(ev.0, ev.1));
        }
        Status::Modified
    }

    /// Forgets the notes and what each channel was told, and sends each Group
    /// the mode's ranges again ahead of its next event: a reset receiver is
    /// back at its own defaults.
    fn reset(&mut self) {
        self.translator.reset();
        self.mpe.reset();
        self.told = 0;
    }
}

/// A [`MpeTransformNode`]'s fork: a transform in the mode the controls hold.
struct ModeFork {
    mode: Arc<ArcLend<MpeMode>>,
}

impl ForkSource for ModeFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(MpeTransformNode::new(
            *self.mode.current(),
        ))))
    }
}

impl IntoNode for MpeTransformNode {
    type Controls = MpeTransformControls;

    fn into_parts(self) -> NodeParts<MpeTransformControls> {
        let request = Arc::clone(self.request.cell());
        let controls = MpeTransformControls {
            request: Arc::clone(&request),
            dropped: Arc::clone(&self.dropped),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(ModeFork { mode: request })),
        }
    }
}
