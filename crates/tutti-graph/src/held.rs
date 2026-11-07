//! Keys held on an event output port, so an edge that ends can close them.
//!
//! The executor watches what a node writes to an event output port that
//! carries notes and keeps the set of notes started and not ended, and the
//! channels whose sustain pedal is down. When a commit removes an edge the port
//! still holds keys on, the sink that loses the edge is sent the offs through
//! the injection path that flushes a vanished delay. Nodes know nothing of
//! this.
//!
//! The set is a bitset over the 16-bit [`Key`], fixed at 8 KiB, allocated when
//! a plan is applied and never grown. A node's op writes only its own unit's
//! sets, so a parallel block needs no new claim.
//!
//! A sink fed by several sources cannot tell whose note a key is: one source's
//! close ends another's note on the same key.

use tutti_ump::{Key, KeyEvent, Ump};

use crate::event::{Event, EventKind};
use crate::time::Offset;

const WORDS: usize = 1 << 10;
const PEDALS: usize = 512;

#[derive(Clone, Debug)]
pub(crate) struct Held {
    keys: Box<[u64; WORDS]>,
    live: u32,
    /// Sustain pedals down, by `m2 * 256 + lane`.
    pedals: [u64; PEDALS / 64],
}

impl Held {
    pub(crate) fn new() -> Self {
        Self {
            keys: Box::new([0; WORDS]),
            live: 0,
            pedals: [0; PEDALS / 64],
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.live == 0 && self.pedals == [0; PEDALS / 64]
    }

    #[inline]
    fn set(&mut self, k: Key, on: bool) {
        let (w, b) = (usize::from(k.index() >> 6), 1u64 << (k.index() & 63));
        let was = self.keys[w] & b != 0;
        if on && !was {
            self.keys[w] |= b;
            self.live += 1;
        } else if !on && was {
            self.keys[w] &= !b;
            self.live -= 1;
        }
    }

    fn pedal(&mut self, m2: bool, lane: u8, down: bool) {
        let i = usize::from(m2) * 256 + usize::from(lane);
        let (w, b) = (i >> 6, 1u64 << (i & 63));
        if down {
            self.pedals[w] |= b;
        } else {
            self.pedals[w] &= !b;
        }
    }

    /// Fold in what a block wrote.
    #[inline]
    pub(crate) fn observe(&mut self, events: &[Event]) {
        for e in events {
            let EventKind::Midi(u) = e.kind else { continue };
            match u.key_event() {
                Some(KeyEvent::On(k)) => self.set(k, true),
                Some(KeyEvent::Off(k)) => self.set(k, false),
                Some(KeyEvent::Sustain { m2, lane, down }) => self.pedal(m2, lane, down),
                None => {}
            }
        }
    }

    /// Whether any of `events` is a held-key event.
    pub(crate) fn any_key(events: &[Event]) -> bool {
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Midi(u) if u.key_event().is_some()))
    }

    /// The messages that close everything held: pedals up, then every note
    /// off, at offset 0 (ties keep this order).
    pub(crate) fn close_into(&self, out: &mut Vec<Event>) {
        let event = |ump| Event {
            offset: Offset::ZERO,
            kind: EventKind::Midi(ump),
        };
        for i in (0..PEDALS).filter(|&i| self.pedals[i >> 6] & (1u64 << (i & 63)) != 0) {
            out.push(event(Ump::sustain_up(i >= 256, (i & 255) as u8)));
        }
        for (w, &word) in self.keys.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                out.push(event(Ump::note_off(Key::from_index((w * 64 + b) as u16))));
            }
        }
    }
}
