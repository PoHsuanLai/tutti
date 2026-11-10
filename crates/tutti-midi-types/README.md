# tutti-midi-types

Pure MIDI types for the Tutti audio engine — MIDI 2.0 / UMP native, plus the
MPE and sync vocabulary. The `tutti` facade re-exports this crate as
`tutti::midi` (feature `midi`).

## What is here

The wire type is `tutti_ump::Ump`: four UMP words, so any MIDI 1.0 / 2.0 /
SysEx / utility message fits one type. An event in a block is the pair
`(Offset, Ump)`, the offset kept beside the packet, so no type here carries
timing. Build a single message with `tutti_ump::Message` and `Ump::from`; build
a multi-packet one (SysEx, Flex Data, UMP Stream) with the free functions here
(`tutti_ump::sysex7_packets`, [`flex_set_tempo`], …); decode through
[`MidiMessage::from_ump`] (a [`MidiMessage`]) or `midi2::UmpMessage::try_from`.

Around it, six public modules:

- [`ump`] — the UMP surface: SysEx7 and SysEx8 packets, endpoint capabilities,
  Flex Data, JR Timestamps.
- [`mpe`] — MIDI Polyphonic Expression (RP-053): zones, modes, note rotation.
- [`sync`] — MIDI clock and MTC decoders.
- [`cc`] — CC numbers and CC→target mapping ([`cc::mapping`]).
- [`translation`] — the MIDI 1↔2 boundary (the widths are `tutti_midi_protocol`'s).

The MIDI 2.0 Clip File codec ([`ParsedClipFile::from_bytes`] / [`write_clip_file`],
M2-116), and [`MidiMessage`] are at the crate root. `midi2` and `midly` are re-exported whole, so a
consumer matching this crate's version needs no dependency entry of its own.

```rust
use tutti_midi_types::prelude::*;
use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};

// Build a note, decode it back — no midi2 imports, no width juggling.
let ump = Ump::from(Message::Midi2(
    Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST },
    Voice2::NoteOn { note: NoteNumber::new(60).unwrap(), velocity: 0x8000, attribute: Attribute::None },
));
let msg = MidiMessage::from_ump(ump);
assert!(msg.is_note_on());
assert_eq!(msg.note(), Some(60));
```

## What it does not own

It is **pure types**: no OS API, no runtime state machine, no audio graph. That
is what lets it sit under everything MIDI-shaped in the engine — the runtime, the
hardware layer, the file codecs, the format hosts and both synths all name it,
and none pulls the others in by doing so.

- **No ports.** Enumerating and opening OS endpoints is
  `tutti-midi-hardware`'s.
- **No mailboxes, nodes or schedulers.** The graph's MIDI nodes, the rings
  MIDI crosses threads on and clip playback are `tutti-midi-runtime`'s. This
  crate defines no queue trait: `MidiIn` / `MidiOut` are `tutti-midi-runtime`'s.
- **No `.mid` files.** SMF is `tutti-midi-file`'s. The Clip File codec here is
  the **byte-level** half; the path-level half is that crate's.

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

It pulls in what building and decoding MIDI needs: the decoded view, per-note identity, the unit id, the `normalize` seam, and the Clip File
codec. The advanced surfaces — UMP-Stream endpoint negotiation, Flex Data,
RPN/NRPN translation state, MPE zone config, the sync decoders — stay explicit imports so a glob does not flood scope.

## Where it sits

Depends on `tutti-rt` (for `ArcLend`), `tutti-types` and `tutti-ump` (the `MidiGroup`,
`MidiChannel` and `CCNumber` newtypes, which callers import from there) plus
`midi2` and `midly`. Everything
MIDI-shaped in the engine builds on it: `tutti-engine`, the other `tutti-midi-*`
crates, the synths and the plugin hosts.

## Features

`default = []`.

- `serde` — `Serialize` / `Deserialize` on the **authored-configuration** types
  only, MPE zone setup today. Forwards to `tutti-types/serde`, because
  `MpeZoneConfig` is built from those newtypes.

The wire types are deliberately not serializable. A MIDI type carries these
derives only when it is authored configuration, carries no audio-thread-stamped
field, and has the same shape at rest as on the wire. `MpeMode` passes all
three; an event in a block (`(Offset, Ump)`) fails the last two — its offset is
stamped by the audio thread against a block size a document never sees, and
MIDI has no durational note record.

## License

MIT OR Apache-2.0
