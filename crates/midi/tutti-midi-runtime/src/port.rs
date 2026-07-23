//! [`MidiInPort`] — a MIDI-receiving audio unit's complete input endpoint.
//!
//! Every internal synth needs the same three things to receive MIDI, and used to
//! open-code them as three fields plus a pairing ritual: a routing address
//! ([`MidiUnitId`]), a push mailbox other code routes to (the [`MidiSender`] /
//! [`MidiReceiver`] pair), and an optional pull override (a clip player, an
//! export snapshot). This bundles them into one owned endpoint.
//!
//! The key simplification: the two roles are already the two
//! [`tutti_midi_types`] traits — [`MidiSender`] *is* a [`MidiOut`], [`MidiReceiver`]
//! *is* a [`MidiIn`]. So there is no "inbox vs. override" duality to switch on.
//! The port holds **one** current input as an `Arc<dyn MidiIn>`, defaulting to
//! the receiver; installing a clip source is just swapping that pointer, and
//! clearing it swaps the receiver back. `poll()` is a single trait call — no
//! match, no `Option`.
//!
//! ## fundsp clone semantics (why the cells are shared)
//!
//! fundsp clones a whole audio unit on every graph `commit()` and runs a
//! *different* clone than the one app code holds. So both the sender and the
//! installed-source pointer live behind shared cells:
//!
//! - **Clone** shares the mailbox and the input cell, so a `MidiSender` handed
//!   out earlier keeps reaching the running box, and an [`install`](Self::install)
//!   on any clone is seen by the box the audio thread runs (the
//!   `[[plugin-source-install-shared-cell]]` bug: a per-clone `Option` made
//!   installs invisible to the audio thread — clip playback silently died).
//! - **[`isolate`](Self::isolate)** deliberately does the opposite: a fresh
//!   private mailbox *and* input cell, severing both the shared inbox (so an
//!   offline-export clone can't *steal* the live synth's events) and the shared
//!   source (so clearing on the export clone can't sever the live clip).

use std::sync::Arc;

use arc_swap::ArcSwap;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiUnitId};

use crate::registry::{MidiMailbox, MidiReceiver, MidiSender};

/// A MIDI-receiving unit's input endpoint: a routing address, a push mailbox,
/// and the currently-plugged-in pull source (the receiver by default).
///
/// Clone shares the mailbox + source cell (fundsp clone-on-commit safe);
/// [`isolate`](Self::isolate) severs both.
pub struct MidiInPort {
    unit_id: MidiUnitId,
    /// The push half — the [`MidiOut`](tutti_midi_types::MidiOut) other code
    /// registers on a bus to route events to this unit by id.
    sender: MidiSender,
    /// The receiver half, kept so [`clear`](Self::clear) can swap it back as the
    /// default input.
    receiver: MidiReceiver,
    /// The current input, shared across clones. Defaults to `receiver`; an
    /// [`install`](Self::install) swaps in a clip/export source. Never empty —
    /// there is always *some* `MidiIn` to poll, so the poll path has no branch.
    input: Arc<ArcSwap<Arc<dyn MidiIn>>>,
}

impl std::fmt::Debug for MidiInPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The installed `input` is a `dyn MidiIn` (no Debug bound), so report
        // the routing address; the source's guts aren't Debug-inspectable.
        f.debug_struct("MidiInPort")
            .field("unit_id", &self.unit_id)
            .finish_non_exhaustive()
    }
}

impl MidiInPort {
    /// Build a fresh port with a new unique [`MidiUnitId`] and an empty mailbox,
    /// its input defaulting to the live receiver.
    pub fn new() -> Self {
        Self::with_unit_id(MidiUnitId::next())
    }

    /// Build a port bound to a specific [`MidiUnitId`] (used by
    /// [`isolate`](Self::isolate) to keep the address stable across the re-pair).
    fn with_unit_id(unit_id: MidiUnitId) -> Self {
        let (sender, receiver) = MidiMailbox::pair(unit_id);
        let input: Arc<dyn MidiIn> = Arc::new(receiver.clone());
        Self {
            unit_id,
            sender,
            receiver,
            input: Arc::new(ArcSwap::from_pointee(input)),
        }
    }

    /// This unit's routing address.
    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }

    /// A clone of the push handle — register it on a `MidiBus` (or hand to any
    /// producer) so events routed to [`unit_id`](Self::unit_id) reach this unit.
    pub fn sender(&self) -> MidiSender {
        self.sender.clone()
    }

    /// Install a pull source (a [`MidiClipSource`](crate::MidiClipSource), a
    /// [`MidiSnapshotReader`](crate::MidiSnapshotReader), a
    /// [`CompositeMidiSource`](crate::CompositeMidiSource) merging live+clip) as
    /// the current input, replacing the live receiver until [`clear`](Self::clear).
    /// Lock-free; visible to every clone sharing this port.
    pub fn install(&self, source: Arc<dyn MidiIn>) {
        self.input.store(Arc::new(source));
    }

    /// Swap the live receiver back in as the current input, dropping any
    /// installed source. Lock-free; visible to every clone.
    pub fn clear(&self) {
        let receiver: Arc<dyn MidiIn> = Arc::new(self.receiver.clone());
        self.input.store(Arc::new(receiver));
    }

    /// Poll the current input for this block. One trait call — the receiver and
    /// any installed source are both [`MidiIn`], so there is nothing to branch on.
    #[inline]
    pub fn poll(
        &self,
        block_start_sample: u64,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize {
        self.input
            .load()
            .poll_into(self.unit_id, block_start_sample, block_size, buffer)
    }

    /// Sever all sharing with sibling clones: a fresh private mailbox (so this
    /// clone can't drain events destined for the live unit) and a fresh input
    /// cell defaulting to the new receiver (so clearing here can't disturb the
    /// live unit's installed source). Keeps the same [`unit_id`](Self::unit_id).
    ///
    /// Used by offline export, which clones the live graph and ticks the clone on
    /// a worker thread.
    pub fn isolate(&mut self) {
        *self = Self::with_unit_id(self.unit_id);
    }
}

impl Default for MidiInPort {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for MidiInPort {
    /// Shares the mailbox and the input cell (fundsp clone-on-commit): an
    /// outstanding sender keeps reaching whichever clone runs, and an install on
    /// any clone is seen by all. Use [`isolate`](Self::isolate) to break this.
    fn clone(&self) -> Self {
        Self {
            unit_id: self.unit_id,
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
            input: self.input.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note: u8) -> MidiEvent {
        MidiEvent::note_on(0, 0, note, 0x8000)
    }

    /// A source that emits one note-on on its first poll — proves it was polled.
    struct OneNote(u8);
    impl MidiIn for OneNote {
        fn poll_into(&self, _u: MidiUnitId, _s: u64, _b: usize, out: &mut [MidiEvent]) -> usize {
            if out.is_empty() {
                return 0;
            }
            out[0] = note_on(self.0);
            1
        }
    }

    #[test]
    fn defaults_to_receiver_input() {
        let port = MidiInPort::new();
        // Push via the sender; poll drains it (the receiver is the default input).
        port.sender().queue(&[note_on(60)]);
        let mut buf = [MidiEvent::noop(); 8];
        let n = port.poll(0, 64, &mut buf);
        assert_eq!(n, 1);
        assert_eq!(buf[0].note(), Some(60));
    }

    #[test]
    fn install_overrides_then_clear_restores_receiver() {
        let port = MidiInPort::new();
        port.install(Arc::new(OneNote(72)));
        // Even with the mailbox empty, the installed source yields a note.
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(port.poll(0, 64, &mut buf), 1);
        assert_eq!(buf[0].note(), Some(72));

        // Clearing swaps the receiver back; empty mailbox → nothing polled.
        port.clear();
        assert_eq!(port.poll(0, 64, &mut buf), 0);
    }

    #[test]
    fn install_propagates_across_clones() {
        // The shared-cell contract: install on one clone, visible on another.
        let live = MidiInPort::new();
        let clone_a = live.clone();
        let audio_clone = live.clone();

        clone_a.install(Arc::new(OneNote(60)));
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(
            audio_clone.poll(0, 64, &mut buf),
            1,
            "install must be shared"
        );

        clone_a.clear();
        let fresh = live.clone();
        assert_eq!(fresh.poll(0, 64, &mut buf), 0, "clear must be shared");
    }

    #[test]
    fn isolate_severs_inbox_and_source() {
        let live = MidiInPort::new();
        let mut render = live.clone();
        render.isolate();

        // A note queued to the live sender must NOT reach the isolated clone.
        live.sender().queue(&[note_on(60)]);
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(
            render.poll(0, 64, &mut buf),
            0,
            "isolated clone sees nothing"
        );
        // The live port still has its note (not stolen by the clone's poll).
        assert_eq!(live.poll(0, 64, &mut buf), 1, "live keeps its event");

        // An install on the live port must NOT leak into the isolated clone.
        live.install(Arc::new(OneNote(64)));
        assert_eq!(
            render.poll(0, 64, &mut buf),
            0,
            "install doesn't reach isolate"
        );
    }
}
