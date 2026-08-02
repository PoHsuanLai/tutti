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
//! *is* a [`MidiIn`]. So there is no "inbox vs. override" duality to switch on;
//! both are polled through one trait.
//!
//! ## Layering, not replacement
//!
//! [`poll`](MidiInPort::poll) drains the mailbox **and** any installed source,
//! in that order. A synth driven by a clip still answers its keyboard: live
//! preview, musical typing and a scheduled sequence coexist, which is what a
//! player expects when they touch the keys during playback.
//!
//! It used to replace instead — one `Arc<dyn MidiIn>` cell defaulting to the
//! receiver, swapped by `install`. That silenced preview for as long as a clip
//! was installed, and worse, the mailbox kept accepting pushes the whole time:
//! the events did not vanish, they *queued*, and popped out stale on the next
//! [`clear`](MidiInPort::clear). Layering is what the source-installing callers
//! wanted in the first place — all three of them (`SoundFontUnit`, `PolySynth`,
//! the plugin MIDI node) install clip/export sources onto ports that also have
//! a live inbox.
//!
//! ## fundsp clone semantics (why the cells are shared)
//!
//! fundsp clones a whole audio unit on every graph `commit()` and runs a
//! *different* clone than the one app code holds. So both the sender and the
//! installed-source pointer live behind shared cells:
//!
//! - **Clone** shares the mailbox and the source cell, so a `MidiSender` handed
//!   out earlier keeps reaching the running box, and an [`install`](Self::install)
//!   on any clone is seen by the box the audio thread runs (the
//!   `[[plugin-source-install-shared-cell]]` bug: a per-clone `Option` made
//!   installs invisible to the audio thread — clip playback silently died).
//! - **[`isolate`](Self::isolate)** deliberately does the opposite: a fresh
//!   private mailbox *and* source cell, severing both the shared inbox (so an
//!   offline-export clone can't *steal* the live synth's events) and the shared
//!   source (so clearing on the export clone can't sever the live clip).

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiUnitId};

use crate::registry::{MidiMailbox, MidiReceiver, MidiSender};

/// A MIDI-receiving unit's input endpoint: a routing address, a push mailbox,
/// and an optional pull source layered over it.
///
/// Clone shares the mailbox + source cell (fundsp clone-on-commit safe);
/// [`isolate`](Self::isolate) severs both.
pub struct MidiInPort {
    unit_id: MidiUnitId,
    /// The push half — the [`MidiOut`](tutti_midi_types::MidiOut) other code
    /// registers on a bus to route events to this unit by id.
    sender: MidiSender,
    /// The pull half of the mailbox, polled on every block.
    receiver: MidiReceiver,
    /// An additional source layered *over* the mailbox, shared across clones.
    /// `None` until an [`install`](Self::install); [`clear`](Self::clear) puts
    /// it back. Optional rather than defaulting to the receiver because the
    /// receiver is now polled unconditionally — a default of "the receiver"
    /// would drain the mailbox twice.
    source: Arc<ArcSwapOption<Arc<dyn MidiIn>>>,
}

impl std::fmt::Debug for MidiInPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The installed source is a `dyn MidiIn` (no Debug bound), so report
        // the routing address; the source's guts aren't Debug-inspectable.
        f.debug_struct("MidiInPort")
            .field("unit_id", &self.unit_id)
            .finish_non_exhaustive()
    }
}

impl MidiInPort {
    /// Build a fresh port with a new unique [`MidiUnitId`] and an empty mailbox,
    /// no source layered over it yet.
    pub fn new() -> Self {
        Self::with_unit_id(MidiUnitId::next())
    }

    /// Build a port bound to a specific [`MidiUnitId`] (used by
    /// [`isolate`](Self::isolate) to keep the address stable across the re-pair).
    fn with_unit_id(unit_id: MidiUnitId) -> Self {
        let (sender, receiver) = MidiMailbox::pair(unit_id);
        Self {
            unit_id,
            sender,
            receiver,
            source: Arc::new(ArcSwapOption::empty()),
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

    /// Layer a pull source (a [`MidiClipSource`](crate::MidiClipSource) or a
    /// [`MidiSnapshotReader`](crate::MidiSnapshotReader)) over the live mailbox
    /// until [`clear`](Self::clear). Lock-free; visible to every clone sharing
    /// this port.
    ///
    /// Both are polled: a clip plays *and* the keyboard still sounds. Installing
    /// a second source replaces the first — the port layers the mailbox with one
    /// source, not a stack of them.
    pub fn install(&self, source: Arc<dyn MidiIn>) {
        self.source.store(Some(Arc::new(source)));
    }

    /// Drop the installed source, leaving the mailbox alone as the input.
    /// Lock-free; visible to every clone.
    pub fn clear(&self) {
        self.source.store(None);
    }

    /// Poll this block's events: the mailbox first, then any installed source,
    /// appended into the remainder of `buffer`.
    ///
    /// Returns the total written. **Events are not offset-ordered** — each half
    /// is internally ordered but the two interleave, so a caller that times
    /// events within the block must sort by `frame_offset` (as `PolySynth` and
    /// `SoundFontUnit` already do).
    ///
    /// A full `buffer` truncates the *source*, since the mailbox fills first.
    /// Sizing it past the 256-slot mailbox capacity is what keeps that
    /// unreachable.
    #[inline]
    pub fn poll(&self, block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        // The receiver's *inherent* `poll_into` — its `MidiIn` impl only adds a
        // unit-id check against the id this port already owns.
        let n = self.receiver.poll_into(buffer);
        match self.source.load().as_deref() {
            Some(source) if n < buffer.len() => {
                n + source.poll_into(self.unit_id, block_size, &mut buffer[n..])
            }
            _ => n,
        }
    }

    /// Sever all sharing with sibling clones: a fresh private mailbox (so this
    /// clone can't drain events destined for the live unit) and an empty source
    /// cell (so installing or clearing here can't disturb the live unit's).
    /// Keeps the same [`unit_id`](Self::unit_id).
    ///
    /// Note it also drops whatever source *was* installed, so an offline render
    /// installs its own — which it does, being the caller that has the snapshot.
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
    /// Shares the mailbox and the source cell (fundsp clone-on-commit): an
    /// outstanding sender keeps reaching whichever clone runs, and an install on
    /// any clone is seen by all. Use [`isolate`](Self::isolate) to break this.
    fn clone(&self) -> Self {
        Self {
            unit_id: self.unit_id,
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
            source: self.source.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    fn note_on(note: u8) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0x8000)
    }

    /// A source that emits one note-on on its first poll — proves it was polled.
    struct OneNote(u8);
    impl MidiIn for OneNote {
        fn poll_into(&self, _u: MidiUnitId, _b: usize, out: &mut [MidiEvent]) -> usize {
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
        let n = port.poll(64, &mut buf);
        assert_eq!(n, 1);
        assert_eq!(buf[0].note(), Some(60));
    }

    #[test]
    fn an_installed_source_is_polled_then_cleared() {
        let port = MidiInPort::new();
        port.install(Arc::new(OneNote(72)));
        // Even with the mailbox empty, the installed source yields a note.
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(port.poll(64, &mut buf), 1);
        assert_eq!(buf[0].note(), Some(72));

        // Clearing drops it; empty mailbox → nothing polled.
        port.clear();
        assert_eq!(port.poll(64, &mut buf), 0);
    }

    /// The point of layering: a clip plays and the keyboard still sounds.
    ///
    /// Under the old replacing `install` the pushed note was not polled at all —
    /// it sat in the mailbox and popped out stale on the next `clear()`.
    #[test]
    fn the_mailbox_still_sounds_under_an_installed_source() {
        let port = MidiInPort::new();
        port.install(Arc::new(OneNote(72)));
        port.sender().queue(&[note_on(60)]);

        let mut buf = [MidiEvent::noop(); 8];
        let n = port.poll(64, &mut buf);
        assert_eq!(n, 2, "both halves must be polled");

        let mut notes: Vec<Option<u8>> = buf[..n].iter().map(|e| e.note()).collect();
        notes.sort();
        assert_eq!(notes, vec![Some(60), Some(72)]);
    }

    /// A full buffer truncates the source rather than overrunning it. The
    /// mailbox fills first, so it is the layered source that loses events —
    /// which is why the real poll buffers are sized past the mailbox capacity.
    #[test]
    fn a_full_buffer_truncates_the_source_not_the_mailbox() {
        let port = MidiInPort::new();
        port.install(Arc::new(OneNote(72)));
        port.sender().queue(&[note_on(60)]);

        // Room for exactly the mailbox event.
        let mut buf = [MidiEvent::noop(); 1];
        assert_eq!(port.poll(64, &mut buf), 1);
        assert_eq!(buf[0].note(), Some(60), "the mailbox event survives");
    }

    #[test]
    fn install_propagates_across_clones() {
        // The shared-cell contract: install on one clone, visible on another.
        let live = MidiInPort::new();
        let clone_a = live.clone();
        let audio_clone = live.clone();

        clone_a.install(Arc::new(OneNote(60)));
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(audio_clone.poll(64, &mut buf), 1, "install must be shared");

        clone_a.clear();
        let fresh = live.clone();
        assert_eq!(fresh.poll(64, &mut buf), 0, "clear must be shared");
    }

    #[test]
    fn isolate_severs_inbox_and_source() {
        let live = MidiInPort::new();
        let mut render = live.clone();
        render.isolate();

        // A note queued to the live sender must NOT reach the isolated clone.
        live.sender().queue(&[note_on(60)]);
        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(render.poll(64, &mut buf), 0, "isolated clone sees nothing");
        // The live port still has its note (not stolen by the clone's poll).
        assert_eq!(live.poll(64, &mut buf), 1, "live keeps its event");

        // An install on the live port must NOT leak into the isolated clone.
        live.install(Arc::new(OneNote(64)));
        assert_eq!(
            render.poll(64, &mut buf),
            0,
            "install doesn't reach isolate"
        );
    }

    /// `isolate` drops the installed source too, so an offline render starts
    /// from a bare mailbox and installs its own snapshot reader.
    #[test]
    fn isolate_drops_the_installed_source() {
        let live = MidiInPort::new();
        live.install(Arc::new(OneNote(72)));

        let mut render = live.clone();
        render.isolate();

        let mut buf = [MidiEvent::noop(); 8];
        assert_eq!(
            render.poll(64, &mut buf),
            0,
            "the isolated clone keeps no source"
        );
        assert_eq!(live.poll(64, &mut buf), 1, "the live port keeps its own");
    }
}
