//! [`MidiInPort`] — a MIDI-receiving audio unit's complete input endpoint.
//!
//! Every internal synth needs the same three things to receive MIDI: a routing
//! address ([`MidiUnitId`]), a push mailbox other code routes to (the
//! [`MidiSender`] / [`MidiReceiver`] pair), and an optional pull override (a
//! clip player, an export snapshot). This bundles them into one owned endpoint,
//! so a unit holds one field rather than three plus a pairing ritual.
//!
//! The key simplification: the push half is already a [`tutti_midi_types`]
//! trait — [`MidiSender`] *is* a [`MidiOut`](tutti_midi_types::MidiOut). The
//! pull half is this port's own [`poll`](MidiInPort::poll), which reads the
//! mailbox through [`MidiReceiver`]'s inherent method and layers any installed
//! [`MidiUnitIn`] over it, supplying its own id.
//!
//! ## Layering, not replacement
//!
//! [`poll`](MidiInPort::poll) drains the mailbox **and** any installed source,
//! in that order. A synth driven by a clip still answers its keyboard: live
//! preview, musical typing and a scheduled sequence coexist, which is what a
//! player expects when they touch the keys during playback.
//!
//! Replacing rather than layering is the trap: it silences preview for as long as
//! a clip is installed, and the mailbox keeps accepting pushes the whole time —
//! the events do not vanish, they *queue*, and pop out stale on the next
//! [`clear`](MidiInPort::clear). Every source-installing caller (`SoundFontUnit`,
//! `PolySynth`, the plugin MIDI node) installs onto a port that also has a live
//! inbox, so layering is the only behaviour that serves all three.
//!
//! ## fundsp clone semantics (why the cells are shared)
//!
//! fundsp clones a whole audio unit on every graph `commit()` and runs a
//! *different* clone than the one app code holds. So both the sender and the
//! installed-source pointer live behind shared cells:
//!
//! - **Clone** shares the mailbox and the source cell, so a [`MidiSender`]
//!   handed out earlier keeps reaching the running box, and an
//!   [`install`](MidiInPort::install) on any clone is seen by the box the audio
//!   thread runs. A per-clone `Option` here makes installs invisible to the
//!   audio thread, and clip playback dies silently.
//! - **[`isolate`](MidiInPort::isolate)** deliberately does the opposite: a
//!   fresh private mailbox *and* source cell, severing both the shared inbox (so
//!   an offline-export clone cannot *steal* the live synth's events) and the
//!   shared source (so clearing on the export clone cannot sever the live clip).

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiUnitId, MidiUnitIn};

use crate::block::registry::{MidiMailbox, MidiReceiver, MidiSender};

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
    /// it back. Optional rather than defaulting to the receiver: the receiver is
    /// polled unconditionally, so a default of "the receiver" would drain the
    /// mailbox twice.
    source: Arc<ArcSwapOption<Arc<dyn MidiUnitIn>>>,
}

impl std::fmt::Debug for MidiInPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The installed source is a `dyn MidiUnitIn` (no Debug bound), so report
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
    ///
    /// The port supplies its own [`unit_id`](Self::unit_id) when it polls, so a
    /// source can only ever be asked for the events belonging to *this* unit.
    /// That is why the parameter is a [`MidiUnitIn`] and not a
    /// [`MidiIn`](tutti_midi_types::MidiIn): a pre-routing edge hands back the
    /// whole undifferentiated stream, which is not this port's to take.
    pub fn install(&self, source: Arc<dyn MidiUnitIn>) {
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
        let n = self.receiver.poll_into(buffer);
        match self.source.load().as_deref() {
            Some(source) if n < buffer.len() => {
                // This port is the only thing that mints the selector: the id it
                // passes is the id it owns, so an installed source cannot be
                // asked for another unit's events.
                n + source.poll_unit(self.unit_id, block_size, &mut buffer[n..])
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

    /// Install on `fork` — a forked unit's own port — an offline copy of the
    /// source installed here, reading the render's timeline out of `ctx`
    /// ([`MidiUnitIn::rebind_offline`]). Control thread; reads this port's
    /// source cell, never its mailbox, so the live unit keeps every event.
    ///
    /// Returns whether a source was installed: `false` when none is
    /// installed here, or the one that is cannot be rebound (it is not a
    /// function of a timeline, or `ctx` is not an `OfflineTransport`). The
    /// fork's port is then left as it was, with no source.
    pub fn rebind_offline_into(&self, fork: &MidiInPort, ctx: &dyn std::any::Any) -> bool {
        let Some(rebound) = self
            .source
            .load()
            .as_deref()
            .and_then(|source| source.rebind_offline(fork.unit_id, ctx))
        else {
            return false;
        };
        fork.install(rebound);
        true
    }

    /// Tell the installed source, if any, the rate its unit now runs at
    /// ([`MidiUnitIn::set_sample_rate`]). Called from a unit's
    /// `set_sample_rate`; lock-free.
    pub fn set_source_sample_rate(&self, sample_rate: tutti_core::SampleRate) {
        if let Some(source) = self.source.load().as_deref() {
            source.set_sample_rate(sample_rate);
        }
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
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn note_on(note: u8) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0x8000)
    }

    /// A source that emits one note-on on its first poll — proves it was polled.
    struct OneNote(u8);
    impl MidiUnitIn for OneNote {
        fn poll_unit(&self, _u: MidiUnitId, _b: usize, out: &mut [MidiEvent]) -> usize {
            if out.is_empty() {
                return 0;
            }
            out[0] = note_on(self.0);
            1
        }
    }

    /// Records the unit id it was polled with, so a test can assert the port
    /// supplied its own rather than a sentinel.
    #[derive(Default)]
    struct RecordsUnit(std::sync::Mutex<Vec<MidiUnitId>>);
    impl MidiUnitIn for RecordsUnit {
        fn poll_unit(&self, u: MidiUnitId, _b: usize, _out: &mut [MidiEvent]) -> usize {
            self.0.lock().unwrap().push(u);
            0
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
    /// An `install` that *replaced* rather than layered would leave the pushed
    /// note unpolled — sitting in the mailbox, popping out stale on the next
    /// `clear()`.
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

    /// The port polls an installed source with **its own** unit id.
    ///
    /// The hazard this guards: a `MidiUnitId::new(0)` sentinel is not a
    /// distinguishable "no unit" — `0` is a real id, so a per-unit source polled
    /// with a sentinel is handed the whole hardware stream instead of its own
    /// events. The type system separates the port seam from the pre-block's
    /// hardware seam; this pins the half that passes an id.
    #[test]
    fn the_port_supplies_the_unit_id_the_source_is_polled_with() {
        let port = MidiInPort::new();
        let recorder = Arc::new(RecordsUnit::default());
        port.install(recorder.clone());

        let mut buf = [MidiEvent::noop(); 4];
        port.poll(64, &mut buf);

        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            &[port.unit_id()],
            "the source must be polled with this port's id, not a sentinel"
        );
        assert_ne!(
            port.unit_id(),
            MidiUnitId::new(0),
            "a fresh port must not be issued the id the old sentinel used"
        );
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
