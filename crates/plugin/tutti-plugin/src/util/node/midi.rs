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
//! **Out:** an optional sink ([`Midi::set_out`]) the plugin's own MIDI-out is
//! collected into. Delivery is **not** immediate: the post-block phase
//! ([`tutti_midi_runtime::MidiPostBlock`]) fans the whole block's emission out
//! once the graph has rendered, which is what makes it independent of the order
//! the nodes happened to be scheduled in. See [`Midi::emit`].

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::protocol::MidiEventVec;
use tutti_midi_runtime::{MidiInPort, MidiOutSink, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;
use tutti_midi_types::MidiUnitIn;

const POLL_BUFFER_SIZE: usize = 256;

// The queue and routing table belong to `MidiPostBlock`, which owns the delivery
// phase. A node needs neither — only somewhere to put its events.

fn empty_poll_scratch() -> Vec<MidiEvent> {
    vec![MidiEvent::noop(); POLL_BUFFER_SIZE]
}

/// A hosted plugin's MIDI endpoint: the inbound port and the outbound slot.
pub struct Midi {
    /// The inbound half: this plugin's mailbox plus the swappable source
    /// installed over it. Identical in duty to a built-in synth's port, so it
    /// *is* one — including the shared-cell clone semantics that survive
    /// fundsp's clone-on-commit (see [[plugin-source-install-shared-cell]]).
    port: MidiInPort,
    drain: MidiEventVec,
    poll_scratch: Vec<MidiEvent>,
    /// Optional post-block sink for a plugin that emits MIDI. `None` (the
    /// default) means the plugin's MIDI-out is dropped. Set via
    /// [`Self::set_out`]; written per block by [`Self::emit`] — which the node
    /// only calls for a plugin that declared `Features::MIDI_OUT`, so an
    /// emission is gated on the self-reported capability (the mirror of the
    /// input feeds).
    ///
    /// Wrapped in a **shared** `Arc<ArcSwapOption<…>>` for the same reason the
    /// port shares its input cell: fundsp's frontend/backend split runs a
    /// *different clone* than the one `PluginClient::set_midi_out` mutates, so
    /// a per-clone `Option` would silently never fire. Sharing the slot makes
    /// an install on any clone visible to the running box, lock-free.
    /// See [[plugin-source-install-shared-cell]].
    ///
    /// Note the sink *inside* is shared too, by `Arc` — which is what lets a
    /// cloned node write into the one buffer `MidiPostBlock` drains, rather
    /// than into a per-clone buffer nothing reads.
    out: Arc<ArcSwapOption<MidiOutSink>>,
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
    /// Creates an endpoint with a fresh routing address and no source installed.
    pub fn new() -> Self {
        Self {
            port: MidiInPort::new(),
            drain: MidiEventVec::new(),
            poll_scratch: empty_poll_scratch(),
            out: Arc::new(ArcSwapOption::empty()),
        }
    }

    /// This plugin's MIDI input endpoint — routing address, push mailbox, and
    /// the source-install slot, in one borrow.
    ///
    /// The whole-port accessor exists so a host can reach all three through a
    /// single downcast, the same shape `SoundFontUnit` and `PolySynth` expose.
    pub fn port(&self) -> &MidiInPort {
        &self.port
    }

    /// This endpoint's routing address, which MIDI is addressed to.
    pub fn unit_id(&self) -> MidiUnitId {
        self.port.unit_id()
    }

    /// Producer handle for this plugin's MIDI inbox.
    pub fn sender(&self) -> MidiSender {
        self.port.sender()
    }

    /// Layer an `Arc`-backed [`MidiUnitIn`](tutti_midi_types::MidiUnitIn) over
    /// the live receiver. Both are polled per block in `drain_for_process`, so a
    /// clip-driven plugin synth still answers live events. Used by clip players
    /// (`tutti_midi_runtime::MidiClipSource`).
    pub fn set_source(&mut self, source: Arc<dyn MidiUnitIn>) {
        self.port.install(source);
    }

    /// Drop a previously-layered source. Subsequent ticks poll only the
    /// live receiver.
    pub fn clear_source(&mut self) {
        self.port.clear();
    }

    /// Install the post-block sink this plugin's MIDI-out is collected into, so
    /// it re-enters routing after the graph renders. Off-RT (call once at
    /// wiring time).
    ///
    /// Take the sink from
    /// [`MidiPostBlock::sink`](tutti_midi_runtime::MidiPostBlock::sink). The
    /// phase owns the routing snapshot and the fan-out bus; a node needs
    /// neither, only somewhere to put its events.
    pub fn set_out(&self, sink: Arc<MidiOutSink>) {
        self.out.store(Some(sink));
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    pub fn clear_out(&self) {
        self.out.store(None);
    }

    /// Hand this block's plugin MIDI-out to the post-block phase. No-op when no
    /// sink is installed (the plugin's MIDI-out is then dropped).
    ///
    /// **This only collects — it does not deliver.** The fan-out happens in
    /// [`MidiPostBlock::run`](tutti_midi_runtime::MidiPostBlock::run), after the
    /// whole graph has rendered.
    ///
    /// That split is the point. Fanning out from here — inside `process` —
    /// interleaves delivery with consumption, so whether a downstream unit sees
    /// an event this block or next depends on **graph traversal order**, which
    /// nothing in the MIDI layer controls (fundsp orders by *audio* edges, and
    /// two units in a MIDI relationship may share no audio edge). Collecting
    /// here and delivering once, later, makes that order unobservable: every
    /// consumer has already polled by the time anything is delivered.
    ///
    /// The cost is a uniform one-block delay
    /// ([`MIDI_OUT_LATENCY_BLOCKS`](tutti_midi_runtime::MIDI_OUT_LATENCY_BLOCKS)).
    /// Each event keeps its own `frame_offset` for the destination to time it.
    ///
    /// Returns how many events were accepted — `< events.len()` means the sink
    /// was full and the rest were **dropped**. A dropped note-off whose note-on
    /// landed is a stuck note, so this reports rather than hides it.
    #[inline]
    pub fn emit(&self, events: &[MidiEvent]) -> usize {
        let sink = self.out.load();
        let Some(sink) = sink.as_ref() else {
            return 0;
        };
        sink.extend(events)
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
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    /// A source that reports it wrote `n` no-op events — enough to prove it was
    /// the thing polled (vs. the empty live receiver, which writes 0).
    struct CountingSource {
        n: usize,
    }
    impl MidiUnitIn for CountingSource {
        fn poll_unit(&self, _unit: MidiUnitId, _block: usize, buffer: &mut [MidiEvent]) -> usize {
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

    /// The outbound analogue of [`source_install_propagates_across_clones`]:
    /// installing an out-sink on ONE clone must be visible to ANOTHER, since
    /// fundsp runs a different clone than the one `set_midi_out` mutates.
    ///
    /// Two layers of sharing have to hold for a plugin's MIDI-out to survive a
    /// `commit()`, and this covers both: the `ArcSwapOption` **slot** is shared
    /// (so the install is seen), and the `MidiOutSink` **inside** it is shared
    /// (so every clone writes to the one buffer the post-block phase drains,
    /// not to a per-clone buffer nothing reads).
    #[test]
    fn out_install_propagates_across_clones() {
        let sink = Arc::new(MidiOutSink::new());

        let original = Midi::new();
        let clone_a = original.clone();
        let clone_b = original.clone();

        // Install on clone_a; the running box could be any clone.
        clone_a.set_out(Arc::clone(&sink));

        let ev = [MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000,
        )];
        assert_eq!(
            clone_b.emit(&ev),
            1,
            "emit on a sibling clone reaches the sink"
        );
        assert_eq!(
            original.emit(&ev),
            1,
            "emit on the original reaches the sink"
        );
        assert_eq!(
            clone_a.emit(&ev),
            1,
            "emit on the installing clone reaches it"
        );

        assert_eq!(
            sink.len(),
            3,
            "all three clones must write into the ONE buffer the phase drains"
        );

        // Clearing on one clone clears for all.
        clone_b.clear_out();
        assert_eq!(clone_a.emit(&ev), 0, "no sink installed after clear_out");
        assert_eq!(sink.len(), 3, "no new events after clear_out");
    }

    /// `emit` reports what it accepted, so a full sink cannot swallow a
    /// plugin's notes silently.
    #[test]
    fn emit_reports_a_partial_accept() {
        let sink = Arc::new(MidiOutSink::new());
        let midi = Midi::new();
        midi.set_out(Arc::clone(&sink));

        let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
        // Fill the sink, then offer more than it can take.
        while sink.push(ev) {}
        assert_eq!(
            midi.emit(&[ev, ev]),
            0,
            "a full sink must report zero accepted, not silently discard"
        );
    }

    /// With no sink installed the plugin's MIDI-out is dropped — reported as
    /// zero accepted rather than looking like a successful emit.
    #[test]
    fn emit_without_a_sink_reports_nothing_accepted() {
        let midi = Midi::new();
        let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
        assert_eq!(midi.emit(&[ev]), 0);
    }
}
