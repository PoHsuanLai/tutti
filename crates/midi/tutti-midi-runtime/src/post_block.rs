//! [`MidiPostBlock`] — the once-per-block MIDI *consumer* that runs **after**
//! the graph renders. The mirror of [`MidiPreBlock`](crate::pre_block).
//!
//! # Why outbound needs a phase of its own
//!
//! Inbound MIDI is order-independent because its producer is *outside* the
//! graph: `MidiPreBlock::run` completes before the first `process()`, so every
//! inbox is full before any node executes and what a node finds cannot depend
//! on when it ran.
//!
//! Outbound cannot copy that. A node's MIDI-out **is a product of rendering** —
//! it does not exist until `process()` has run, so there is no "before" to emit
//! from. Emitting from inside `process` instead makes delivery interleave with
//! consumption:
//!
//! | traversal order | does B see A's event this block? |
//! | --- | --- |
//! | A before B | yes |
//! | B before A | no — B's inbox was empty; it arrives next block |
//!
//! Same graph, same events, different timing. And the order is not something
//! this layer can correct: fundsp computes it from **audio** edges, while MIDI
//! flow is a different graph — an arpeggiator feeding a synth may share no
//! audio edge at all. The two cannot simply be merged either, because MIDI
//! targets resolve *per event* through a channel-keyed
//! [`MidiRoutingSnapshot`](tutti_midi_types::MidiRoutingSnapshot) that a control
//! thread can swap, and MIDI routing may legitimately contain cycles that an
//! audio schedule has no way to order.
//!
//! # What this phase changes
//!
//! Collection still runs by the graph — unavoidable. **Only the fan-out moves.**
//! Nodes push their emissions into this sink during `process`; the fan-out
//! happens once, here, after the graph is done:
//!
//! ```text
//! pre_block.run   →   engine.process   →   post_block.run
//!   (deliver)          (render; nodes        (fan out once;
//!                       push to the sink)     nothing is polling)
//! ```
//!
//! Because every consumer's poll for this block has already happened, delivery
//! always lands after it, whatever order the nodes ran in. The order still
//! varies; it stops being *observable*.
//!
//! # The cost, stated plainly
//!
//! **This makes the latency uniform and declarable. It does not make it zero** —
//! see [`MIDI_OUT_LATENCY_BLOCKS`]. Every emitted event is delivered exactly one
//! block late. For the batched (out-of-process) plugin path that was already
//! true. For the in-process path it is **new**: it used to emit mid-block with
//! intact offsets and no floor at all. That regression is deliberate — it buys
//! order-independence, and a delivery time that can be stated rather than
//! discovered.

use std::sync::Arc;

use tutti_midi_types::tutti_types::RtPublish;

use tutti_core::RtEventBuf;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiRouter, MidiRoutingSnapshot};

/// Delivery delay this phase imposes, in blocks.
///
/// **The declared value that replaces an emergent one.** Before the phase
/// existed, a plugin's MIDI-out reached a consumer this block or next depending
/// on graph traversal order; now it is always the next block.
///
/// One block is the floor, not an implementation compromise: a node's output
/// exists only after it has rendered, and every consumer already polled this
/// block. Going below it would mean ordering the graph by MIDI edges — which is
/// the cyclic, runtime-routed thing the module docs rule out.
pub const MIDI_OUT_LATENCY_BLOCKS: usize = 1;

/// Events collected per block before fan-out. Past this, pushes are dropped
/// (never allocated) on the audio thread and [`MidiOutSink::overflowed`]
/// reports it.
const MIDI_OUT_BUFFER_CAPACITY: usize = 512;

/// The shared collection point every MIDI-emitting node pushes into during
/// `process`, drained once by [`MidiPostBlock::run`].
///
/// # Why a shared sink rather than a registry of emitters
///
/// fundsp clones every unit on `commit()` and runs a *different clone* than the
/// one app code holds. A registry of emitter handles captured at wiring time
/// would therefore point at stale clones and silently never fire — the
/// `[[plugin-source-install-shared-cell]]` bug, which is why
/// [`MidiInPort`](crate::MidiInPort) shares its mailbox behind a cell rather
/// than owning it per clone.
///
/// Pushing inverts the problem out of existence: a node holds an `Arc` to this
/// sink, `Arc::clone` on unit-clone shares it, and every clone therefore writes
/// to the same place. There is nothing to keep in sync.
///
/// # Thread contract
///
/// Written by nodes during the graph render, drained by the phase after it —
/// both on the audio thread, never concurrently. That is exactly the "one
/// borrow at a time" contract the underlying cell documents.
pub struct MidiOutSink {
    events: RtEventBuf<MidiEvent, MIDI_OUT_BUFFER_CAPACITY>,
}

impl MidiOutSink {
    pub fn new() -> Self {
        Self {
            events: RtEventBuf::new(),
        }
    }

    /// Append one event for fan-out at the end of this block.
    ///
    /// Returns `false` when the sink is full and the event was **dropped**. A
    /// dropped note-off while its note-on landed is what produces a stuck note,
    /// so a caller that can act on this should — silently discarding accepted
    /// work is the pattern this engine reports rather than hides.
    #[inline]
    #[must_use = "a dropped event is a lost note; check or explicitly ignore"]
    pub fn push(&self, event: MidiEvent) -> bool {
        self.events.push(event)
    }

    /// Append a whole block's emission. Returns how many were accepted —
    /// `< events.len()` means the sink filled and the rest were dropped.
    ///
    /// Mirrors [`MidiSender::queue`](crate::MidiSender::queue), which returns a
    /// count for the same reason.
    #[inline]
    pub fn extend(&self, events: &[MidiEvent]) -> usize {
        let mut accepted = 0;
        for &event in events {
            if !self.events.push(event) {
                break;
            }
            accepted += 1;
        }
        accepted
    }

    /// Whether anything has been dropped since the last drain.
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.events.overflowed()
    }

    /// Number of events waiting for fan-out.
    #[inline]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Drop everything pending without delivering it.
    #[inline]
    pub fn clear(&self) {
        self.events.clear();
    }

    /// Take each pending event in order, leaving the sink empty.
    ///
    /// The borrow is released around every call, so `f` may reach back into
    /// whatever owns this sink — see [`RtEventBuf::drain_each`].
    #[inline]
    fn drain_each(&self, f: impl FnMut(MidiEvent)) {
        self.events.drain_each(f);
    }

    /// Reset the interior-mutable RT owner (device switch).
    pub fn reset_owner(&self) {
        self.events.reset_owner();
    }
}

impl Default for MidiOutSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The once-per-block MIDI consumer: fans out everything the graph emitted,
/// after the graph has finished.
///
/// Held by the audio-callback assembly alongside the engine and
/// [`MidiPreBlock`](crate::pre_block::MidiPreBlock); the callback calls
/// `pre_block.run` → `engine.process` → [`run`](Self::run).
pub struct MidiPostBlock {
    /// The collection point nodes push into. Handed to each emitting node at
    /// wiring time as an `Arc`.
    sink: Arc<MidiOutSink>,
    /// The fan-out that delivers a routed event to a destination unit's inbox.
    /// `None` before wiring — events are then dropped, since there is nowhere
    /// to put them.
    queue: Option<Arc<dyn MidiRouter>>,
    /// The live routing snapshot, shared with the inbound phase: a node's
    /// MIDI-out is routed by exactly the rules a hardware input is.
    routing: Arc<RtPublish<MidiRoutingSnapshot>>,
}

impl MidiPostBlock {
    /// Build a consumer reading the given routing snapshot. Pass the *same*
    /// `Arc` the inbound phase uses, so both directions route by one table.
    pub fn new(routing: Arc<RtPublish<MidiRoutingSnapshot>>) -> Self {
        Self {
            sink: Arc::new(MidiOutSink::new()),
            queue: None,
            routing,
        }
    }

    /// Install the fan-out that delivers routed events to unit inboxes.
    pub fn set_queue(&mut self, queue: Arc<dyn MidiRouter>) {
        self.queue = Some(queue);
    }

    /// The sink every emitting node pushes into. Hand this to each node at
    /// wiring time; `Arc::clone` is what makes it survive fundsp's
    /// clone-on-commit.
    pub fn sink(&self) -> Arc<MidiOutSink> {
        Arc::clone(&self.sink)
    }

    /// Fan out everything the graph emitted this block.
    ///
    /// Runs **after** `engine.process`, so no consumer is polling: delivery
    /// lands in destination inboxes to be drained on their next poll, one block
    /// from now, regardless of the order the emitting nodes ran in.
    ///
    /// Each event keeps its own `frame_offset` for the destination to time it.
    /// RT-safe: lock-free, alloc-free.
    #[inline]
    pub fn run(&self) {
        if self.sink.is_empty() {
            return;
        }

        let Some(queue) = &self.queue else {
            // Nowhere to deliver. Drain anyway — leaving events pending would
            // fan them out later against a block they did not belong to.
            self.sink.clear();
            return;
        };

        // ONE routing read for the whole block: every event this block is
        // delivered by one set of rules, the same discipline the inbound phase
        // follows.
        let routing = self.routing.read();
        self.sink.drain_each(|event| {
            for target in routing.route(&event) {
                queue.queue(target, std::slice::from_ref(&event));
            }
        });
    }

    /// Reset the interior-mutable RT owner (device switch).
    pub fn reset_owners(&self) {
        self.sink.reset_owner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tutti_midi_types::convert::midi1_velocity_to_midi2;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::{MidiRoutingTable, MidiUnitId};

    /// Captures every routed event so a test can assert what reached the router.
    #[derive(Default)]
    struct CapturingRouter {
        routed: Mutex<Vec<(MidiUnitId, MidiEvent)>>,
    }
    impl MidiRouter for CapturingRouter {
        fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
            let mut routed = self.routed.lock().unwrap();
            for &e in events {
                routed.push((unit_id, e));
            }
        }
    }

    fn note(n: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            n,
            midi1_velocity_to_midi2(100),
        )
    }

    fn routed_to(unit: MidiUnitId) -> (MidiPostBlock, Arc<CapturingRouter>) {
        let mut table = MidiRoutingTable::new();
        table.set_routes(Vec::new(), Some(unit));
        table.commit();

        let router = Arc::new(CapturingRouter::default());
        let mut post = MidiPostBlock::new(table.snapshot_arc());
        post.set_queue(router.clone());
        (post, router)
    }

    /// **The property this phase exists for.**
    ///
    /// Two nodes push in opposite orders; delivery must be identical. Before the
    /// phase, a node emitted from inside its own `process`, so whether a
    /// consumer saw an event this block or next depended on traversal order —
    /// the one thing nothing in the MIDI layer controls.
    ///
    /// Pushing in a different order is exactly what a different traversal order
    /// *is*, as far as this layer can observe.
    #[test]
    fn delivery_is_independent_of_the_order_nodes_emitted_in() {
        let unit = MidiUnitId::new(7);

        let (post_ab, router_ab) = routed_to(unit);
        let sink_ab = post_ab.sink();
        assert!(sink_ab.push(note(60)));
        assert!(sink_ab.push(note(64)));
        post_ab.run();

        let (post_ba, router_ba) = routed_to(unit);
        let sink_ba = post_ba.sink();
        assert!(sink_ba.push(note(64)));
        assert!(sink_ba.push(note(60)));
        post_ba.run();

        let ab = router_ab.routed.lock().unwrap();
        let ba = router_ba.routed.lock().unwrap();
        assert_eq!(ab.len(), 2, "both events delivered");

        // Compare the delivered *set*: same destinations, same events. Sorting
        // by note is what makes this a statement about delivery rather than
        // about push order — the events are expected to arrive in the order
        // they were pushed, which differs between the two runs by construction.
        let mut ab_set: Vec<_> = ab.iter().map(|(u, e)| (*u, e.data_words())).collect();
        let mut ba_set: Vec<_> = ba.iter().map(|(u, e)| (*u, e.data_words())).collect();
        ab_set.sort();
        ba_set.sort();
        assert_eq!(
            ab_set, ba_set,
            "the same events must reach the same destinations whichever order \
             the nodes ran in"
        );
    }

    /// The sink must be empty after a fan-out, or the next block would re-deliver
    /// this block's events.
    #[test]
    fn running_drains_the_sink() {
        let (post, router) = routed_to(MidiUnitId::new(3));
        assert!(post.sink().push(note(60)));

        post.run();
        assert!(post.sink.is_empty(), "sink must be empty after fan-out");

        // A second run with nothing pushed delivers nothing more.
        post.run();
        assert_eq!(
            router.routed.lock().unwrap().len(),
            1,
            "a drained event must not be delivered twice"
        );
    }

    /// A node's emission is routed by the same rules a hardware input is, so an
    /// empty table means no targets — not an error, and not a stuck sink.
    #[test]
    fn an_unrouted_event_is_consumed_not_left_pending() {
        let mut table = MidiRoutingTable::new();
        table.commit();
        let router = Arc::new(CapturingRouter::default());
        let mut post = MidiPostBlock::new(table.snapshot_arc());
        post.set_queue(router.clone());

        assert!(post.sink().push(note(60)));
        post.run();

        assert!(router.routed.lock().unwrap().is_empty());
        assert!(
            post.sink.is_empty(),
            "an unroutable event must still be consumed, or it would fan out \
             next block against a table it never belonged to"
        );
    }

    /// With no router wired there is nowhere to deliver, but the sink must still
    /// be drained — otherwise events accumulate and fan out against a later
    /// block once one appears.
    #[test]
    fn no_queue_still_drains() {
        let table = MidiRoutingTable::new();
        let post = MidiPostBlock::new(table.snapshot_arc());
        assert!(post.sink().push(note(60)));

        post.run();

        assert!(post.sink.is_empty());
    }

    /// Overflow is reported rather than silent: a dropped note-off whose note-on
    /// landed is a stuck note.
    #[test]
    fn a_full_sink_reports_the_drop() {
        let sink = MidiOutSink::new();
        for i in 0..MIDI_OUT_BUFFER_CAPACITY {
            assert!(sink.push(note((i % 128) as u8)), "within capacity");
        }
        assert!(!sink.overflowed(), "nothing dropped yet");

        assert!(
            !sink.push(note(60)),
            "push past capacity must report failure"
        );
        assert!(sink.overflowed(), "and the sink must remember it dropped");
    }

    /// `extend` reports how many landed, so a batch cannot vanish silently.
    #[test]
    fn extend_reports_a_partial_accept() {
        let sink = MidiOutSink::new();
        let batch: Vec<_> = (0..MIDI_OUT_BUFFER_CAPACITY + 10)
            .map(|i| note((i % 128) as u8))
            .collect();

        let accepted = sink.extend(&batch);

        assert_eq!(
            accepted, MIDI_OUT_BUFFER_CAPACITY,
            "extend must report the count that landed, not the count offered"
        );
        assert!(accepted < batch.len());
    }

    /// The sink is shared by `Arc`, which is what survives fundsp's
    /// clone-on-commit: a node's clone pushes to the same place the phase
    /// drains. A per-clone buffer is the bug this design avoids.
    #[test]
    fn clones_of_the_sink_handle_share_one_buffer() {
        let (post, router) = routed_to(MidiUnitId::new(5));

        let handle_a = post.sink();
        let handle_b = post.sink();
        assert!(handle_a.push(note(60)));
        assert!(handle_b.push(note(64)));

        post.run();

        assert_eq!(
            router.routed.lock().unwrap().len(),
            2,
            "both handles must write to the one buffer the phase drains"
        );
    }
}
