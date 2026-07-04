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

use crate::protocol::MidiEventVec;
use tutti_midi_types::MidiUnitId;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiSource;
use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};

const POLL_BUFFER_SIZE: usize = 256;

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
    source_override: Option<Arc<dyn MidiSource>>,
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
            // The source override travels with clones so fundsp's
            // graph-commit clones don't lose the clip source — same
            // contract as PolySynth's `Arc`-backed override.
            source_override: self.source_override.clone(),
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
            source_override: None,
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
        self.source_override = Some(source);
    }

    /// Drop a previously-installed source override. Subsequent ticks
    /// poll the live receiver again.
    pub fn clear_source(&mut self) {
        self.source_override = None;
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
        let count = match &self.source_override {
            Some(src) => src.poll_into(
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
