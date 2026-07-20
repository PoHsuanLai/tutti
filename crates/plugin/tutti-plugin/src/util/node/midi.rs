//! MIDI event collection for `PluginClient`. Merges direct [`Midi::queue`]
//! events with events polled from this client's own
//! [`tutti_midi_runtime::MidiReceiver`] (or an installed
//! [`tutti_midi_types::MidiSource`] override — typically a
//! [`tutti_midi_runtime::MidiClipSource`]) into a single buffer for
//! the audio path. Callers route MIDI to the plugin via either:
//!
//! - [`Midi::sender`] for live producers (hardware drivers, panel
//!   previews) that push events as they arrive.
//! - [`Midi::set_source`] for clip-driven playback that polls the
//!   transport-aware source per block.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::protocol::MidiEventVec;
use tutti_midi_types::MidiUnitId;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiSource;
use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};

const POLL_BUFFER_SIZE: usize = 256;

/// `Sized` wrapper so a `dyn MidiSource` trait object can live in an
/// [`ArcSwapOption`] (arc-swap needs the stored `Arc`'s pointee to be `Sized`).
/// One extra `Arc` hop on install/read — negligible next to the poll itself.
struct MidiSourceHandle(Arc<dyn MidiSource>);

fn empty_poll_scratch() -> Vec<MidiEvent> {
    vec![MidiEvent::noop(); POLL_BUFFER_SIZE]
}

pub struct Midi {
    unit_id: MidiUnitId,
    pending: Vec<MidiEvent>,
    drain: MidiEventVec,
    sender: MidiSender,
    receiver: MidiReceiver,
    poll_scratch: Vec<MidiEvent>,
    /// Optional override polled per block instead of `receiver`. Set
    /// via [`Self::set_source`]; mirrors PolySynth's
    /// `midi_source_override` so clip-driven playback feeds plugins
    /// the same way it feeds built-in synths.
    ///
    /// Wrapped in a **shared** `Arc<ArcSwapOption<…>>` (not a plain per-clone
    /// `Option`): fundsp's frontend/backend split means the box the audio
    /// thread runs is a *different clone* than the one `PluginClient::set_*`
    /// mutates, and `node_mut`/clone edits are discarded by `Net::migrate` on
    /// commit. Sharing the slot itself makes an install on any clone visible to
    /// the running box, lock-free, with no commit needed. See
    /// [[plugin-source-install-shared-cell]].
    source_override: Arc<ArcSwapOption<MidiSourceHandle>>,
    /// Running sample position. Bumped by 1 per `drain_for_tick` and
    /// by `block_size` per `drain_for_process`. Passed to the
    /// `MidiSource::poll_into` call so clip players know what beat
    /// range to emit events for.
    sample_pos: u64,
}

impl Clone for Midi {
    fn clone(&self) -> Self {
        Self {
            unit_id: self.unit_id,
            pending: Vec::new(),
            drain: MidiEventVec::new(),
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
            poll_scratch: empty_poll_scratch(),
            // Share the override SLOT (Arc clone) across fundsp's graph-commit
            // clones — a later install on any clone is then seen by the box the
            // audio thread runs. Cloning the `Option` instead (the old bug)
            // gave each clone a private slot that never propagated.
            source_override: Arc::clone(&self.source_override),
            sample_pos: self.sample_pos,
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
        let unit_id = MidiUnitId::next();
        let (sender, receiver) = MidiEventSlot::pair(unit_id);
        Self {
            unit_id,
            pending: Vec::new(),
            drain: MidiEventVec::new(),
            sender,
            receiver,
            poll_scratch: empty_poll_scratch(),
            source_override: Arc::new(ArcSwapOption::empty()),
            sample_pos: 0,
        }
    }

    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }

    /// Producer handle for this plugin's MIDI inbox.
    pub fn sender(&self) -> MidiSender {
        self.sender.clone()
    }

    /// Replace the pending queue. Sent on next process.
    pub fn queue(&mut self, events: &[MidiEvent]) {
        self.pending.clear();
        self.pending.extend_from_slice(events);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Install an `Arc`-backed [`MidiSource`] override. Polled per
    /// block in `drain_for_process` instead of the live `MidiReceiver`.
    /// Used by clip players (`tutti_midi_runtime::MidiClipSource`) to
    /// drive plugin synths from MIDI clips.
    pub fn set_source(&mut self, source: Arc<dyn MidiSource>) {
        self.source_override
            .store(Some(Arc::new(MidiSourceHandle(source))));
    }

    /// Drop a previously-installed source override. Subsequent ticks
    /// poll the live receiver again.
    pub fn clear_source(&mut self) {
        self.source_override.store(None);
    }

    /// Reset the running sample position. Called on `reset()` /
    /// `set_sample_rate()` so the source override sees a clean
    /// playhead at transport restarts.
    pub fn reset_sample_pos(&mut self) {
        self.sample_pos = 0;
    }

    /// Merge pending + override-or-receiver events into one buffer and
    /// return it. Bumps `sample_pos` by `block_size` so the next call
    /// sees the next block's window.
    pub fn drain_for_process(&mut self, block_size: usize) -> &MidiEventVec {
        self.drain.clear();
        self.drain.extend(self.pending.drain(..));
        // `load()` is lock-free; the guard holds the current source (if any)
        // for the duration of the poll.
        let source = self.source_override.load();
        let count = match source.as_ref() {
            Some(handle) => handle.0.poll_into(
                self.unit_id,
                self.sample_pos,
                block_size,
                &mut self.poll_scratch,
            ),
            None => self.receiver.poll_into(&mut self.poll_scratch),
        };
        // Clamp to scratch capacity so `drain` never spills its SmallVec
        // inline storage and allocates on the audio thread.
        let count = count.min(self.poll_scratch.len());
        self.drain
            .extend(self.poll_scratch[..count].iter().copied());
        self.sample_pos = self.sample_pos.wrapping_add(block_size as u64);
        &self.drain
    }

    /// Sample-by-sample variant for the `tick` path. Bumps the
    /// sample_pos by 1 each call.
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
    impl MidiSource for CountingSource {
        fn poll_into(
            &self,
            _unit: MidiUnitId,
            _start: u64,
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

        assert_eq!(clone_b.drain_for_process(64).len(), 3, "clone_b sees install");
        assert_eq!(original.drain_for_process(64).len(), 3, "original sees install");
        assert_eq!(clone_a.drain_for_process(64).len(), 3, "clone_a sees install");

        // Clearing on one clone clears for all.
        clone_b.clear_source();
        assert_eq!(clone_a.drain_for_process(64).len(), 0, "clear propagates");
        assert_eq!(original.drain_for_process(64).len(), 0, "clear propagates");
    }
}
