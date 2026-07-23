//! MIDI for `PluginClient` — the plugin node's two halves.
//!
//! **In:** a [`tutti_midi_runtime::MidiInPort`], the same port a built-in synth
//! owns. Each block it is polled into one buffer for the audio path; the port
//! resolves live-mailbox vs installed source itself. Callers route MIDI to the
//! plugin via either:
//!
//! - [`Midi::sender`] for live producers (hardware drivers, panel
//!   previews) that push events as they arrive.
//! - [`Midi::set_source`] for clip-driven playback that polls the
//!   transport-aware source per block.
//!
//! **Out:** an optional routing target ([`Midi::set_out`]) through which the
//! plugin's own MIDI-out re-enters the graph like any other source.

use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};

use crate::protocol::MidiEventVec;
use tutti_midi_runtime::{MidiInPort, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiIn;
use tutti_midi_types::{MidiOut, MidiRoutingSnapshot, MidiUnitId};

const POLL_BUFFER_SIZE: usize = 256;

/// The outbound routing target for a plugin that emits MIDI. Installed once at
/// wiring time; read per block by [`Midi::emit`].
///
/// The plugin's MIDI-out re-enters routing exactly like a hardware input: each
/// emitted event is fanned out through the shared [`MidiRoutingSnapshot`] (keyed
/// on the event's channel) to whatever destination units the route resolves, and
/// delivered via the same lock-free [`MidiOut`]. A plugin's output is just
/// another source.
struct OutHandle {
    queue: Arc<dyn MidiOut>,
    routing: Arc<ArcSwap<MidiRoutingSnapshot>>,
}

fn empty_poll_scratch() -> Vec<MidiEvent> {
    vec![MidiEvent::noop(); POLL_BUFFER_SIZE]
}

pub struct Midi {
    /// The inbound half: this plugin's mailbox plus the swappable source
    /// installed over it. Identical in duty to a built-in synth's port, so it
    /// *is* one — including the shared-cell clone semantics that survive
    /// fundsp's clone-on-commit (see [[plugin-source-install-shared-cell]]).
    port: MidiInPort,
    drain: MidiEventVec,
    poll_scratch: Vec<MidiEvent>,
    /// Optional outbound routing target for a plugin that emits MIDI. `None`
    /// (the default) means the plugin's MIDI-out is dropped. Set via
    /// [`Self::set_out`]; read per block by [`Self::emit`].
    ///
    /// Wrapped in a **shared** `Arc<ArcSwapOption<…>>` for the same reason the
    /// port shares its input cell: fundsp's frontend/backend split runs a
    /// *different clone* than the one `PluginClient::set_midi_out` mutates, so
    /// a per-clone `Option` would silently never fire. Sharing the slot makes
    /// an install on any clone visible to the running box, lock-free.
    /// See [[plugin-source-install-shared-cell]].
    out: Arc<ArcSwapOption<OutHandle>>,
}

impl Clone for Midi {
    fn clone(&self) -> Self {
        Self {
            // `MidiInPort::clone` shares the mailbox + input cell, which is the
            // clone-on-commit contract this node depends on.
            port: self.port.clone(),
            drain: MidiEventVec::new(),
            poll_scratch: empty_poll_scratch(),
            // Share the outbound SLOT (Arc clone) too — same shared-cell
            // rationale as the port's input cell.
            out: Arc::clone(&self.out),
        }
    }
}

impl Default for Midi {
    fn default() -> Self {
        Self::new()
    }
}

impl Midi {
    pub fn new() -> Self {
        Self {
            port: MidiInPort::new(),
            drain: MidiEventVec::new(),
            poll_scratch: empty_poll_scratch(),
            out: Arc::new(ArcSwapOption::empty()),
        }
    }

    pub fn unit_id(&self) -> MidiUnitId {
        self.port.unit_id()
    }

    /// Producer handle for this plugin's MIDI inbox.
    pub fn sender(&self) -> MidiSender {
        self.port.sender()
    }

    /// Install an `Arc`-backed [`MidiIn`] override. Polled per
    /// block in `drain_for_process` instead of the live receiver.
    /// Used by clip players (`tutti_midi_runtime::MidiClipSource`) to
    /// drive plugin synths from MIDI clips.
    pub fn set_source(&mut self, source: Arc<dyn MidiIn>) {
        self.port.install(source);
    }

    /// Drop a previously-installed source override. Subsequent ticks
    /// poll the live receiver again.
    pub fn clear_source(&mut self) {
        self.port.clear();
    }

    /// Install the outbound routing target so this plugin's MIDI-out re-enters
    /// routing. `routing` is the shared snapshot the engine already uses for
    /// hardware input, and `queue` the fan-out bus. Off-RT (call once at wiring
    /// time).
    pub fn set_out(&self, queue: Arc<dyn MidiOut>, routing: Arc<ArcSwap<MidiRoutingSnapshot>>) {
        self.out.store(Some(Arc::new(OutHandle { queue, routing })));
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    pub fn clear_out(&self) {
        self.out.store(None);
    }

    /// Route this block's plugin MIDI-out back into the graph. No-op when no
    /// outbound target is installed. For each event, fan out through the shared
    /// routing snapshot (keyed on the event's channel, like any source) to every
    /// destination unit and deliver via the lock-free queue — byte-for-byte the
    /// path `MidiProcessor::route_events_in_range` runs for hardware input, so
    /// it's RT-safe. Each event keeps its own `frame_offset`; the destination
    /// unit sub-buffer-splits on it next block. Non-recursive: delivery lands in
    /// the destination's inbox, drained on *its* next poll — `emit` never
    /// re-enters any `process()`.
    #[inline]
    pub fn emit(&self, events: &[MidiEvent]) {
        let handle = self.out.load();
        let Some(handle) = handle.as_ref() else {
            return;
        };
        let routing = handle.routing.load();
        for event in events {
            for target in routing.route(event) {
                handle.queue.queue(target, std::slice::from_ref(event));
            }
        }
    }

    /// Drain the override-or-receiver events for this block into one buffer and
    /// return it.
    pub fn drain_for_process(&mut self, block_size: usize) -> &MidiEventVec {
        self.drain.clear();
        // One lock-free poll: the port resolves receiver-or-installed-source
        // itself, so there is no branch (and no second code path) here.
        let count = self.port.poll(block_size, &mut self.poll_scratch);
        // Clamp to scratch capacity so `drain` never spills its SmallVec
        // inline storage and allocates on the audio thread.
        let count = count.min(self.poll_scratch.len());
        self.drain
            .extend(self.poll_scratch[..count].iter().copied());
        &self.drain
    }

    /// Sample-by-sample variant for the `tick` path.
    pub fn drain_for_tick(&mut self) -> &MidiEventVec {
        self.drain_for_process(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that reports it wrote `n` no-op events — enough to prove it was
    /// the thing polled (vs. the empty live receiver, which writes 0).
    struct CountingSource {
        n: usize,
    }
    impl MidiIn for CountingSource {
        fn poll_into(
            &self,
            _unit: MidiUnitId,
            _block: usize,
            buffer: &mut [MidiEvent],
        ) -> usize {
            let n = self.n.min(buffer.len());
            for slot in buffer.iter_mut().take(n) {
                *slot = MidiEvent::noop();
            }
            n
        }
    }

    /// The regression guard for [[plugin-source-install-shared-cell]]: installing
    /// a source on ONE clone must be visible to ANOTHER clone, because fundsp
    /// runs a different clone than the one the install call mutates. With the old
    /// per-clone `Option<Arc<…>>` this failed silently (clone_b saw `None`).
    #[test]
    fn source_install_propagates_across_clones() {
        let mut original = Midi::new();
        let mut clone_a = original.clone();
        let mut clone_b = original.clone();

        // Install on clone_a; clone_b and the original must all see it, since the
        // slot itself is shared (Arc<ArcSwapOption>).
        clone_a.set_source(Arc::new(CountingSource { n: 3 }));

        assert_eq!(
            clone_b.drain_for_process(64).len(),
            3,
            "clone_b sees install"
        );
        assert_eq!(
            original.drain_for_process(64).len(),
            3,
            "original sees install"
        );
        assert_eq!(
            clone_a.drain_for_process(64).len(),
            3,
            "clone_a sees install"
        );

        // Clearing on one clone clears for all.
        clone_b.clear_source();
        assert_eq!(clone_a.drain_for_process(64).len(), 0, "clear propagates");
        assert_eq!(original.drain_for_process(64).len(), 0, "clear propagates");
    }

    use std::sync::Mutex;
    use tutti_midi_types::{MidiRoute, MidiRoutingSnapshot};

    /// Records every `(unit, event-count)` queued, to prove `emit` routed.
    #[derive(Default)]
    struct RecordingQueue {
        queued: Mutex<Vec<(MidiUnitId, usize)>>,
    }
    impl MidiOut for RecordingQueue {
        fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
            self.queued.lock().unwrap().push((unit_id, events.len()));
        }
    }

    /// The outbound analogue of [`source_install_propagates_across_clones`]:
    /// installing an out-target on ONE clone must be visible to ANOTHER, since
    /// fundsp runs a different clone than the one `set_midi_out` mutates.
    #[test]
    fn out_install_propagates_across_clones() {
        let dest = MidiUnitId::new(77);
        let queue = Arc::new(RecordingQueue::default());
        let routing = Arc::new(ArcSwap::from_pointee(MidiRoutingSnapshot::from_routes(
            vec![MidiRoute::new().with_target(dest)],
            None,
        )));

        let original = Midi::new();
        let clone_a = original.clone();
        let clone_b = original.clone();

        // Install on clone_a; the running box could be any clone.
        clone_a.set_out(queue.clone(), routing);

        let ev = [MidiEvent::note_on(0, 0, 60, 0x8000)];
        clone_b.emit(&ev);
        original.emit(&ev);
        clone_a.emit(&ev);

        let queued = queue.queued.lock().unwrap();
        assert_eq!(
            queued.as_slice(),
            &[(dest, 1), (dest, 1), (dest, 1)],
            "emit on any clone routes to the destination unit"
        );
        drop(queued);

        // Clearing on one clone clears for all.
        clone_b.clear_out();
        clone_a.emit(&ev);
        assert_eq!(
            queue.queued.lock().unwrap().len(),
            3,
            "no new events after clear_out"
        );
    }
}
