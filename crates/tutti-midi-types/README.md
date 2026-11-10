# tutti-midi-types

Pure MIDI types for the Tutti audio engine — MIDI 2.0 / UMP native, plus the
MPE vocabulary. The `tutti` facade re-exports this crate as
`tutti::midi` (feature `midi`).

## What is here

The wire type is `tutti_ump::Ump`: four UMP words, so any MIDI 1.0 / 2.0 /
SysEx / utility message fits one type. An event in a block is the pair
`(Offset, Ump)`, the offset kept beside the packet, so no type here carries
timing. Build a message with `tutti_ump::Message` and `Ump::from`, decode it
with `tutti_ump::decode` or, for the "just tell me what it is" view,
[`MidiMessage::from_ump`].

This crate is being emptied (design 017, stage 5b). What is left is
[`MidiMessage`] and its companions (`ControllerNamespace`, `NoteAttribute`,
`PerNoteController`, `UnencodableMessage`) at the root, and [`translation`], the
MIDI 1.0 to 2.0 boundary of the old API. Both read through `midi2`, and the
next part deletes them, the crate and that dependency. Everything else moved to
its owner: the MPE zone types are `tutti_midi_protocol`'s, the CC numbers are
`tutti_ump::CCNumber`'s constants, Flex Data, SysEx, the JR messages and
`Protocol` are `tutti_ump`'s, the Clip File and its tempo are `tutti-midi-file`'s.
There is no re-export of `midi2`, `midly`, `ArcLend` or the unit types here: a
consumer names the crate.

```rust
use tutti_midi_types::prelude::*;
use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};

// Build a note, decode it back.
let ump = Ump::from(Message::Midi2(
    Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST },
    Voice2::NoteOn { note: NoteNumber::new(60).unwrap(), velocity: 0x8000, attribute: Attribute::None },
));
let msg = MidiMessage::from_ump(ump);
assert!(msg.is_note_on());
assert_eq!(msg.note(), Some(60));
```

## What it does not own

It is **pure types**: no OS API, no runtime state machine, no audio graph.

- **No ports.** Enumerating and opening OS endpoints is
  `tutti-midi-hardware`'s.
- **No mailboxes, nodes or schedulers.** The graph's MIDI nodes, the rings
  MIDI crosses threads on and clip playback are `tutti-midi-runtime`'s.
- **No `.mid` files.** SMF is `tutti-midi-file`'s. The MIDI 2.0 Clip File codec is
  that crate's too.

## One vocabulary, whatever the source protocol

Two notes enter — one off a MIDI 1.0 DIN cable, one authored natively at MIDI 2.0
width. `normalize` promotes the first to Channel Voice 2, so a consumer
downstream matches a single protocol and never needs a Channel-Voice-1 arm.

```rust
use tutti_midi_types::prelude::*;
use tutti_midi_types::from_midi1_bytes;
use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};

// From the wire: a MIDI 1.0 note-on, 7-bit velocity 100.
let from_din = from_midi1_bytes(&[0x90, 60, 100])
    .expect("0x90 is a well-formed note-on");
assert_eq!(from_din.message_type(), 0x2); // Channel Voice 1

let promoted = normalize(&from_din);
assert_eq!(promoted.message_type(), 0x4); // Channel Voice 2

// Authored natively: 16 bits of velocity, no 7-bit original to be faithful to.
let authored = Ump::from(Message::Midi2(
    Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST },
    Voice2::NoteOn { note: NoteNumber::new(60).unwrap(), velocity: 0xABCD, attribute: Attribute::None },
));
assert!(MidiMessage::from_ump(authored).is_note_on());
```

## Constraint: the resolution boundary, and why the loss hides

Widening is the spec's Min-Center-Max scaler (`tutti_midi_protocol::widen`),
not a shift: `127` must reach full scale, and `127 << 9` is `65024`, which is not.
Read a promoted note through `MidiMessage::velocity` and the 7-bit original is
recovered exactly, in both directions:

```rust
use tutti_midi_protocol::{narrow_velocity, widen};
use tutti_ump::{NoteNumber, U7, Voice1, Voice2};

// A 7-bit release velocity as the 16-bit value `widen` gives it.
let wide = |v: u8| {
    let off = Voice1::NoteOff { note: NoteNumber::of(60), velocity: U7::of(v) };
    let Voice2::NoteOff { velocity, .. } = widen(off) else { unreachable!() };
    velocity
};
assert_eq!(wide(127), 65535);
assert_eq!(wide(64), 0x8000);

// Every 7-bit value round-trips exactly.
for v in 0u8..128 {
    assert_eq!(narrow_velocity(wide(v)).get(), v);
}
```

**That exactness is the trap.** A path that narrows to 7 bits is
*self-consistently* lossy — it round-trips every value it can emit, so a
round-trip test passes and the loss never shows. It is only visible on a value
that did not start at 7 bits:

```rust
use tutti_midi_protocol::{narrow_velocity, widen};
use tutti_ump::{NoteNumber, U7, Voice1, Voice2};

let wide = |v: u8| {
    let off = Voice1::NoteOff { note: NoteNumber::of(60), velocity: U7::of(v) };
    let Voice2::NoteOff { velocity, .. } = widen(off) else { unreachable!() };
    velocity
};
let authored = 0xABCD_u16;
let via_7bit = wide(narrow_velocity(authored).get());
assert_ne!(via_7bit, authored); // 43981 in, 43690 out — 128 codes survive of 65536
```

So `tutti_midi_protocol::narrow_velocity` is for a MIDI 1.0 *destination* only.
Reading a velocity for any other purpose goes through `MidiMessage::velocity`,
which is lossless from either protocol.

## The prelude is deliberately narrow

The decoded view, its companions and the `normalize` seam.

## Where it sits

Depends on `tutti-ump` and `tutti-midi-protocol` (the widths) plus `midi2`,
until the stage that deletes this crate.

## Features

None.

## License

MIT OR Apache-2.0
