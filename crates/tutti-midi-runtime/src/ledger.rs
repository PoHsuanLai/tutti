//! [`NoteLedger`]: the notes a node has started and not ended, and the only
//! way such a node puts a MIDI event on its port.
//!
//! A node that starts notes owes each one a note-off. The debt cannot be a
//! `Drop`: a port can refuse the off, and a refusal is not an error the node
//! may lose. So the debt is a row in a fixed table, and the node reaches its
//! port only through a [`Sending`], which [`NoteLedger::block`] returns after
//! it has paid what is owed.
//!
//! A key is a note's Group, Channel and Note Number (M2-104 section 7.4,
//! Appendix C): the wire has no other note identity, so there is one held
//! note per key. A note-on of a key that sounds ends the sounding note first,
//! on the same frame, and takes the key. The optional owner tag `T` lets a
//! source that pairs its own on and off messages (the clip) have an off end
//! the key only when the note that owns it sent the off.
//!
//! A held key is in one of two states, `Sounding` or `Owing` (the port
//! refused the off, so it goes out at the first frame of a later block).
//! Independently, a key counts the note-ons the port refused: each such
//! note's off is withheld and counted rather than written.
//!
//! The table holds at most its capacity in keys, held or refused. A note-on
//! that finds it full is withheld and counted ([`EventWriter::withhold`])
//! like one the port refused: the note does not start, so no off is owed. A
//! note already started always has its row, so a full table never strands one.
//!
//! The node cannot write to the port around the handle:
//!
//! ```compile_fail,E0499
//! use tutti_graph::{Cx, Event, Io, Node, Offset, Prepare, Shape, Status};
//! use tutti_midi_runtime::NoteLedger;
//!
//! struct Bad(NoteLedger<()>);
//!
//! impl Node for Bad {
//!     fn shape(&self) -> Shape {
//!         Shape::EMPTY
//!             .with_events(0, 1)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let first = cx.env.offset(0).unwrap();
//!         let mut sending = self.0.block(io.event_out(0), first);
//!         let _ = io.event_out(0).push(Event::midi(Offset::ZERO, Ump([0x4090_3c00, 0xffff_0000, 0, 0])));
//!         sending.release_all(first);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//! ```

use tutti_graph::{Event, EventWriter, Offset};
use tutti_ump::{Address, Key, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};

/// A note's place on the wire: its Group, Channel and Note Number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NoteKey {
    group: MidiGroup,
    channel: MidiChannel,
    note: NoteNumber,
}

impl NoteKey {
    fn off(self, at: Offset) -> Event {
        let address = Address {
            group: self.group,
            channel: self.channel,
        };
        let off = Voice2::note_off(self.note, 0);
        Event::midi(at, Ump::from(Message::Midi2(address, off)))
    }
}

/// A note message, read once. A MIDI 1.0 Note On at velocity 0 ends a note; a
/// MIDI 2.0 one starts it (M2-104 section 7.4.2).
pub(crate) enum Class {
    On(NoteKey),
    Off(NoteKey),
    Other,
}

impl Class {
    pub(crate) fn of(ump: Ump) -> Self {
        let (Some(address), Some(event)) = (ump.address(), ump.key_event()) else {
            return Self::Other;
        };
        let key = |key: Key| NoteKey {
            group: address.group,
            channel: address.channel,
            note: NoteNumber::masked(key.note()),
        };
        match event {
            KeyEvent::On(k) => Self::On(key(k)),
            KeyEvent::Off(k) => Self::Off(key(k)),
            KeyEvent::Sustain { .. } => Self::Other,
        }
    }
}

/// What the port has been told about a held key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// The port took the note-on; no off has gone out.
    Sounding,
    /// The port refused the off; it goes out at a later block's first frame.
    Owing,
}

#[derive(Clone, Copy, Debug)]
struct Entry<T> {
    key: NoteKey,
    held: Option<Held>,
    /// How many note-ons of this key the port refused and whose offs have not
    /// come by: each such off is withheld and counted. Saturates, so more than
    /// 255 refused ons on one key with no off between them undercount.
    refused: u8,
    /// The note that owns the held key; `None` for an untagged source.
    owner: Option<T>,
}

impl<T> Entry<T> {
    fn owing(&self) -> bool {
        self.held == Some(Held::Owing)
    }
}

/// The notes a node has started and not ended. Fixed capacity: nothing here
/// allocates after [`new`](Self::new).
pub struct NoteLedger<T> {
    /// Sorted by key; never longer than `capacity`, so it never reallocates.
    entries: Vec<Entry<T>>,
    capacity: usize,
    /// How many entries are `Owing`, so a block with no debt pays in one test.
    owing: usize,
}

impl<T> std::fmt::Debug for NoteLedger<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteLedger")
            .field("keys", &self.entries.len())
            .field("owing", &self.owing)
            .finish_non_exhaustive()
    }
}

impl<T: Copy + PartialEq> NoteLedger<T> {
    /// A ledger for up to `capacity` keys at once. Control thread: allocates.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            owing: 0,
        }
    }

    /// Bytes of heap the table holds.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.capacity * size_of::<Entry<T>>()
    }

    /// Forgets every note: the node was reset, and whatever it fed is reset
    /// with it.
    pub fn forget(&mut self) {
        self.entries.clear();
        self.owing = 0;
    }

    /// The handle a block sends through. Everything owed goes out first, at
    /// `first`, the block's first frame: a node cannot write a note before it
    /// has paid what it owes, because this is how it gets the handle.
    pub fn block<'l, 'w>(
        &'l mut self,
        out: &'l mut EventWriter<'w>,
        first: Offset,
    ) -> Sending<'l, 'w, T> {
        let mut sending = Sending { ledger: self, out };
        sending.pay(first);
        sending
    }

    /// The index of `key`'s entry, made if absent; `None` when the table is
    /// full.
    fn entry(&mut self, key: NoteKey) -> Option<usize> {
        match self.entries.binary_search_by_key(&key, |e| e.key) {
            Ok(i) => Some(i),
            Err(_) if self.entries.len() >= self.capacity => None,
            Err(at) => {
                self.entries.insert(
                    at,
                    Entry {
                        key,
                        held: None,
                        refused: 0,
                        owner: None,
                    },
                );
                Some(at)
            }
        }
    }

    fn refuse(&mut self, i: usize) {
        let e = &mut self.entries[i];
        e.refused = e.refused.saturating_add(1);
    }

    fn set_held(&mut self, i: usize, held: Option<Held>) {
        let e = &mut self.entries[i];
        let (was, now) = (e.owing(), held == Some(Held::Owing));
        e.held = held;
        self.owing = self.owing + usize::from(now) - usize::from(was);
    }

    fn recount(&mut self) {
        self.owing = self.entries.iter().filter(|e| e.owing()).count();
    }
}

/// One block's sending through a [`NoteLedger`]. It holds the port's writer,
/// so nothing else can write to the port while it lives.
pub struct Sending<'l, 'w, T> {
    ledger: &'l mut NoteLedger<T>,
    out: &'l mut EventWriter<'w>,
}

impl<T> std::fmt::Debug for Sending<'_, '_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sending")
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl<T: Copy + PartialEq> Sending<'_, '_, T> {
    /// Writes the offs the port refused earlier, in key order, stopping at the
    /// first it refuses again (the port is full).
    ///
    /// An owed off is a loss the writer already counted: each attempt gives
    /// its count back, so a delayed off nets zero.
    fn pay(&mut self, at: Offset) {
        if self.ledger.owing == 0 {
            return;
        }
        let mut full = false;
        self.ledger.entries.retain_mut(|e| {
            if full || !e.owing() {
                return true;
            }
            let taken = self.out.push(e.key.off(at)).is_ok();
            self.out.recovered();
            full = !taken;
            if taken {
                e.held = None;
                e.owner = None;
            }
            !taken || e.refused > 0
        });
        self.ledger.recount();
    }

    /// Writes `ump` at `at`. A note message is kept track of: a note-on the
    /// port took is sounding, one it refused marks its key so that note's off
    /// is withheld, an off it refused is owed. `owner` says which note a
    /// message belongs to; `None` is an untagged source's message.
    ///
    /// A note-on of a key that sounds ends the sounding note first, on the
    /// same frame, and takes the key. A tagged off ends the key only when its
    /// owner owns it, and is swallowed otherwise.
    pub fn send(&mut self, at: Offset, ump: Ump, owner: Option<T>) {
        match Class::of(ump) {
            Class::Other => {
                let _ = self.out.push(Event::midi(at, ump));
            }
            Class::On(key) => self.on(at, ump, key, owner),
            Class::Off(key) => self.off(at, ump, key, owner),
        }
    }

    fn on(&mut self, at: Offset, ump: Ump, key: NoteKey, owner: Option<T>) {
        let Some(i) = self.ledger.entry(key) else {
            self.out.withhold();
            return;
        };
        if let Some(held) = self.ledger.entries[i].held {
            if self.out.push(key.off(at)).is_err() {
                // The old note stays, its off owed, and the new one does not
                // start.
                self.owe(i);
                self.ledger.refuse(i);
                self.out.withhold();
                return;
            }
            if held == Held::Owing {
                self.out.recovered();
            }
            self.ledger.set_held(i, None);
        }
        if self.write(at, ump) {
            self.ledger.set_held(i, Some(Held::Sounding));
            self.ledger.entries[i].owner = owner;
        } else {
            self.ledger.refuse(i);
        }
    }

    fn off(&mut self, at: Offset, ump: Ump, key: NoteKey, owner: Option<T>) {
        let found = self
            .ledger
            .entries
            .binary_search_by_key(&key, |e| e.key)
            .ok();
        if owner.is_none() {
            self.untagged_off(at, ump, found);
            return;
        }
        let Some(i) = found else { return };
        let e = self.ledger.entries[i];
        if e.held.is_some() && e.owner == owner {
            let taken = self.write(at, ump);
            self.finish(i, taken);
        } else if e.refused > 0 {
            // The off of a note whose on the port refused.
            self.settle(i);
        }
    }

    fn write(&mut self, at: Offset, ump: Ump) -> bool {
        self.out.push(Event::midi(at, ump)).is_ok()
    }

    /// An untagged source has one note per key, so its off ends whatever its
    /// key holds and settles a refused on. A key whose on the port refused is
    /// swallowed and counted, as a tagged off is; any other off goes through.
    fn untagged_off(&mut self, at: Offset, ump: Ump, found: Option<usize>) {
        let Some(i) = found else {
            self.write(at, ump);
            return;
        };
        let e = self.ledger.entries[i];
        if e.held.is_some() {
            let taken = self.write(at, ump);
            self.ledger.entries[i].refused = 0;
            self.finish(i, taken);
        } else if e.refused > 0 {
            self.settle(i);
        } else {
            self.write(at, ump);
        }
    }

    /// The off of a note whose on the port refused: swallowed and counted.
    fn settle(&mut self, i: usize) {
        self.ledger.entries[i].refused -= 1;
        self.forget_if_idle(i);
        self.out.withhold();
    }

    /// The off of held entry `i` was written (`taken`) or refused.
    fn finish(&mut self, i: usize, taken: bool) {
        if !taken {
            self.owe(i);
            return;
        }
        if self.ledger.entries[i].owing() {
            self.out.recovered();
        }
        self.ledger.set_held(i, None);
        self.ledger.entries[i].owner = None;
        self.forget_if_idle(i);
    }

    /// Drops entry `i` when it holds nothing.
    fn forget_if_idle(&mut self, i: usize) {
        let e = &self.ledger.entries[i];
        if e.held.is_none() && e.refused == 0 {
            self.ledger.entries.remove(i);
        }
    }

    /// The port refused the off of entry `i`: it is owed. The writer counted
    /// the refusal; one already owed is the same loss, taken back.
    fn owe(&mut self, i: usize) {
        if self.ledger.entries[i].owing() {
            self.out.recovered();
        } else {
            self.ledger.set_held(i, Some(Held::Owing));
        }
    }

    /// Ends every note at `at` (playback stopped or jumped, or the clip was
    /// replaced). An off the port refuses stays owed. Once the port refuses
    /// one the rest are not tried (it is full), but each is owed and counted:
    /// a note that gets no off is a lost event whether or not its push was
    /// attempted.
    pub fn release_all(&mut self, at: Offset) {
        let mut full = false;
        self.ledger.entries.retain_mut(|e| {
            // What was refused belongs to the pass that is ending.
            e.refused = 0;
            let Some(held) = e.held else { return false };
            if !full && self.out.push(e.key.off(at)).is_ok() {
                if held == Held::Owing {
                    self.out.recovered();
                }
                return false;
            }
            match (full, held) {
                // The refused push counted itself.
                (false, Held::Owing) => self.out.recovered(),
                (true, Held::Sounding) => self.out.withhold(),
                _ => {}
            }
            full = true;
            e.held = Some(Held::Owing);
            true
        });
        self.ledger.recount();
    }
}

#[cfg(test)]
mod tests;
