//! The controllers a clip leaves off their normal position: the hold pedals
//! and the pitch bend, per (protocol, Group, Channel).
//!
//! MIDI 1.0 Detailed Specification 4.2.1, p. 31 (Stop): "In addition to note
//! events, any controllers not in their initialized position (pitch wheels,
//! sustain pedal, etc.) should be returned to their normal positions." A
//! pedal left down also defeats the note-offs the ledger has just sent: "A
//! Hold or Sustain pedal 'On' message takes priority over Note Off and All
//! Notes Off until it is released" (Appendix A-5). So the pedals are lifted
//! after the notes are ended, and only then does the receiver let go of them.
//!
//! The table is fixed: one `u16` per lane, 512 lanes (MIDI 1.0 and MIDI 2.0
//! channel voice, 16 Groups, 16 Channels), allocated in [`Controllers::new`].
//! Per-note pitch bend (MIDI 2.0) is not tracked: M2-104 resets a per-note
//! controller when its note ends, and the ledger ends every note first.

use tutti_graph::{Event, EventWriter, Offset};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, U7, U14, Ump, Voice1, Voice2};

const LANES: usize = 512;
/// The hold controllers: sustain, sostenuto, soft pedal, hold 2.
const PEDALS: [u8; 4] = [64, 66, 67, 69];
/// The bit of the pitch bend in a lane's state; the pedals are bits 0 to 3.
const BEND: u8 = 4;
/// A 14-bit bend at rest (M1) and its MIDI 2.0 32-bit image.
const CENTRE_14: u16 = 0x2000;
const CENTRE_32: u32 = 0x8000_0000;

/// Which controllers are off their normal position, low byte, and which of
/// those a refused reset still owes, high byte (the same bits).
pub(super) struct Controllers {
    lanes: Box<[u16; LANES]>,
    /// Some lane owes a reset: a block with no debt pays in one test.
    owing: bool,
}

impl Controllers {
    /// Control thread: allocates.
    pub(super) fn new() -> Self {
        Self {
            lanes: Box::new([0; LANES]),
            owing: false,
        }
    }

    pub(super) fn forget(&mut self) {
        self.lanes.fill(0);
        self.owing = false;
    }

    fn lane(m2: bool, address: Address) -> usize {
        usize::from(m2) * 256 + address.index()
    }

    /// Folds in a message the port took. A controller back at its normal
    /// position is forgotten, and a new value supersedes a reset still owed.
    pub(super) fn observe(&mut self, ump: Ump) {
        let (m2, address, bit, off_normal) = match ump.decode() {
            Ok(Message::Midi1(a, Voice1::ControlChange { controller, value })) => {
                let Some(bit) = pedal_bit(controller) else {
                    return;
                };
                (false, a, bit, value.get() >= 64)
            }
            Ok(Message::Midi1(a, Voice1::PitchBend(b))) => (false, a, BEND, b.get() != CENTRE_14),
            Ok(Message::Midi2(a, Voice2::ControlChange { controller, value })) => {
                let Some(bit) = pedal_bit(controller) else {
                    return;
                };
                (true, a, bit, value >= CENTRE_32)
            }
            Ok(Message::Midi2(a, Voice2::PitchBend(v))) => (true, a, BEND, v != CENTRE_32),
            _ => return,
        };
        let lane = &mut self.lanes[Self::lane(m2, address)];
        *lane &= !(0x101 << bit);
        if off_normal {
            *lane |= 1 << bit;
        }
    }

    /// The message that returns control `bit` of `lane` to its normal position.
    fn reset(lane: usize, bit: u8) -> Ump {
        let m2 = lane >= 256;
        let address = Address {
            group: MidiGroup::masked((lane >> 4) as u8),
            channel: MidiChannel::masked(lane as u8),
        };
        let message = if m2 {
            Message::Midi2(
                address,
                match PEDALS.get(usize::from(bit)) {
                    Some(&cc) => Voice2::ControlChange {
                        controller: CCNumber::new(cc).expect("a controller"),
                        value: 0,
                    },
                    None => Voice2::PitchBend(CENTRE_32),
                },
            )
        } else {
            Message::Midi1(
                address,
                match PEDALS.get(usize::from(bit)) {
                    Some(&cc) => Voice1::ControlChange {
                        controller: CCNumber::new(cc).expect("a controller"),
                        value: U7::of(0),
                    },
                    None => Voice1::PitchBend(U14::of(CENTRE_14)),
                },
            )
        };
        Ump::from(message)
    }

    /// Returns to normal every controller that is off, at `at`, in lane and
    /// bit order, after the notes. A message the port refuses is owed and goes
    /// out first at a later block ([`pay`](Self::pay)); once the port refuses
    /// one the rest are not tried (it is full) but each is owed and counted.
    pub(super) fn lift_all(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        let mut full = false;
        for (lane, state) in self.lanes.iter_mut().enumerate() {
            for bit in 0..=BEND {
                let (held, owed) = (*state & (1 << bit) != 0, *state & (0x100 << bit) != 0);
                if !held {
                    continue;
                }
                let attempted = !full;
                if attempted && out.push(Event::midi(at, Self::reset(lane, bit))).is_ok() {
                    if owed {
                        out.recovered();
                    }
                    *state &= !(0x101 << bit);
                    continue;
                }
                match (attempted, owed) {
                    // The refused push counted itself.
                    (true, true) => out.recovered(),
                    (false, false) => out.withhold(),
                    _ => {}
                }
                full = true;
                *state |= 0x100 << bit;
                self.owing = true;
            }
        }
    }

    /// Writes the resets the port refused earlier, stopping at the first it
    /// refuses again. Each attempt gives its count back, so a delayed reset
    /// nets zero.
    pub(super) fn pay(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        if !self.owing {
            return;
        }
        let mut full = false;
        let mut left = false;
        for (lane, state) in self.lanes.iter_mut().enumerate() {
            for bit in 0..=BEND {
                if *state & (0x100 << bit) == 0 {
                    continue;
                }
                if !full {
                    let taken = out.push(Event::midi(at, Self::reset(lane, bit))).is_ok();
                    out.recovered();
                    full = !taken;
                    if taken {
                        *state &= !(0x101 << bit);
                        continue;
                    }
                }
                left = true;
            }
        }
        self.owing = left;
    }
}

fn pedal_bit(controller: CCNumber) -> Option<u8> {
    PEDALS
        .iter()
        .position(|&p| p == controller.get())
        .map(|i| i as u8)
}
