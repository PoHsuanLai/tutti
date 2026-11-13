//! [`HeldState`]: what a source has started and not ended, as a pure value.
//!
//! A source that vanishes, falls silent or resets leaves its receiver holding
//! what it started: sounding notes, a pedal down, a bent wheel. The receiver
//! cannot know the source is gone, so whoever sits on the source's edge keeps
//! this table, folds every message the source sent into it
//! ([`observe`](HeldState::observe)) and, when the source ends,
//! [`release`](HeldState::release)s what it held as the messages that end it.
//!
//! **What a release sends, and why not All Notes Off.** Per held key a Note
//! Off, in the protocol the key was started in; then, for each hold pedal
//! (CC 64, 66, 67, 69) that is down a pedal-up and for each pitch bend off
//! centre a centred bend, in the protocol it was sent in. Nothing channelwide
//! (CC 120, 121, 123): a channel is shared, because several sources merge into
//! one listener's fan-in, so a channelwide message from one source would end
//! another's notes and reset another's wheel. MIDI 1.0 Detailed Specification
//! 4.2.1 also says that recognising All Notes Off is not required of a
//! receiver and that notes should first be turned off by individual Note-Off
//! messages (p. 24), so the individual offs are what every
//! receiver understands. The notes come first and the controllers after,
//! because a pedal that is down holds a note through a Note Off or All Notes
//! Off until it is lifted (Appendix A-5), so the pedal-up lets go of the
//! notes just ended; p. 31 (Stop) and RP-015 (Reset All Controllers) name the
//! pedals and the pitch wheel as what returns to its normal position. A
//! per-note controller (MIDI 2.0) ends with its note (M2-104 section 7.4), so
//! none is tracked.
//!
//! A release may be asked for one Group: a Group is one cable of a device
//! (M2-104 section 2.1.2, Appendix B.1), and what Active Sensing or a System
//! Reset says of a cable is about that cable's Group.
//!
//! The table is fixed: 8 KiB of keys and 1 KiB of controllers, allocated in
//! [`new`](HeldState::new). Nothing else allocates.

mod controllers;

pub(crate) use controllers::Controllers;
use tutti_ump::{KeyEvent, MidiGroup, Ump};

use tutti_ump::Key;

/// The packed key is 16 bits.
const WORDS: usize = 1 << 10;

/// The notes and controllers a source has started and not ended.
pub struct HeldState {
    /// One bit per [`Key`] (protocol, Group, Channel, Note).
    keys: Box<[u64; WORDS]>,
    /// How many bits are set.
    notes: u32,
    controllers: Controllers,
}

impl Default for HeldState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for HeldState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldState")
            .field("notes", &self.notes)
            .finish_non_exhaustive()
    }
}

impl HeldState {
    /// An empty table. Control thread: allocates.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keys: Box::new([0; WORDS]),
            notes: 0,
            controllers: Controllers::new(),
        }
    }

    /// Whether nothing is held: no note, no pedal down, no bend off centre.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notes == 0 && !self.controllers.any()
    }

    /// Forgets everything held, sending nothing.
    pub fn forget(&mut self) {
        self.keys.fill(0);
        self.notes = 0;
        self.controllers.forget();
    }

    /// Folds in a message the source sent. A note on starts its key (and a
    /// note off ends it, a MIDI 1.0 Note On at velocity 0 included); a pedal
    /// or bend off its normal position is held, and back at it is let go.
    /// Anything else changes nothing.
    pub fn observe(&mut self, ump: impl Into<Ump>) {
        let ump = ump.into();
        match ump.key_event() {
            Some(KeyEvent::On(key)) => self.set(key, true),
            Some(KeyEvent::Off(key)) => self.set(key, false),
            Some(KeyEvent::Sustain { .. }) | None => {}
        }
        self.controllers.observe(ump);
    }

    fn set(&mut self, key: Key, on: bool) {
        let i = usize::from(key.index());
        let bit = 1u64 << (i & 63);
        let word = &mut self.keys[i >> 6];
        let was = *word & bit != 0;
        match (on, was) {
            (true, false) => {
                *word |= bit;
                self.notes += 1;
            }
            (false, true) => {
                *word &= !bit;
                self.notes -= 1;
            }
            _ => {}
        }
    }

    /// Hands `accept` the message that ends each thing held (all of `group`'s
    /// Group, or every Group's for `None`): each note's Note Off in key order,
    /// then each controller's return (see the module docs), and forgets what
    /// `accept` takes (`true`). Stops at the first it refuses (`false`), which
    /// stays held with everything after it, so a receiver with no room now is
    /// asked again. Returns whether everything in scope was taken.
    /// Allocates nothing.
    pub fn release(
        &mut self,
        group: Option<MidiGroup>,
        mut accept: impl FnMut(Ump) -> bool,
    ) -> bool {
        for w in 0..WORDS {
            let mut bits = self.keys[w];
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let key = Key::from_index((w * 64 + b) as u16);
                if group.is_some_and(|g| key.address().group != g) {
                    continue;
                }
                if !accept(Ump::note_off(key)) {
                    return false;
                }
                self.keys[w] &= !(1u64 << b);
                self.notes -= 1;
            }
        }
        self.controllers.lift_while(group, accept)
    }
}

#[cfg(test)]
mod tests;
