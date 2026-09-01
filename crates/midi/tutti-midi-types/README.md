# tutti-midi-types

Pure MIDI types for the Tutti audio engine — MIDI 2.0 / UMP native, plus the
tutti-domain routing, MPE and sync vocabulary.

## What this is

The canonical event type is `MidiEvent`: a packed 20-byte UMP event carrying a
sample-accurate frame offset plus up to four UMP words, so any MIDI 1.0 / 2.0 /
SysEx / utility message fits one type. Build with the inherent constructors
(`MidiEvent::note_on`, `MidiEvent::cc`, …) and decode through `.message()` or
`midi2::UmpMessage::try_from`.

Around it, six public modules:

- `ump` — the UMP surface: `MidiEvent` itself, endpoint capabilities, Flex Data,
  JR Timestamps.
- `mpe` — MIDI Polyphonic Expression (RP-053): zones, modes, note rotation.
- `sync` — clock and MTC decoders.
- `cc` — CC numbers and CC→target mapping (`cc::mapping`).
- `ci` — Capability Inquiry.
- `translation` — the MIDI 1↔2 boundary and its bit-scaling, reachable at the
  root as `convert` because that is the path most consumers import.

Routing (`MidiRoutingTable`, `MidiRoute`) and the MIDI 2.0 Clip File codec
(`read_clip_file` / `write_clip_file`, M2-116) are **not** modules you import
through — their modules are private and everything public in them is re-exported
at the crate root, so there is one path per type rather than two. The same is
true of `MidiMessage`, `NoteId` and `MidiUnitId`.

```rust
use tutti_midi_types::prelude::*;

// Build a note, decode it back — no midi2 imports, no width juggling.
let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
let msg = ev.message();
assert!(msg.is_note_on());
assert_eq!(msg.note(), Some(60));
```

## What it does not own

It is **pure types**: no OS API, no runtime state machine, no audio graph. That
is what lets it sit under everything MIDI-shaped in the engine — the runtime, the
hardware layer, the file codecs, the format hosts and both synths all name it,
and none pulls the others in by doing so.

- **No ports.** Enumerating and opening OS endpoints is
  [`tutti-midi-hardware`](../tutti-midi-hardware)'s.
- **No mailboxes, buses or schedulers.** Per-unit inboxes, fan-out and snapshot
  playback are [`tutti-midi-runtime`](../tutti-midi-runtime)'s. This crate
  defines the `MidiIn` / `MidiOut` / `MidiRouter` *traits*; it implements none of
  them over real state.
- **No `.mid` files.** SMF is [`tutti-midi-file`](../tutti-midi-file)'s. The
  Clip File codec here is the **byte-level** half; the path-level half is that
  crate's.

## One vocabulary, whatever the source protocol

Two notes enter — one off a MIDI 1.0 DIN cable, one authored natively at MIDI 2.0
width. `normalize` promotes the first to Channel Voice 2, so a consumer
downstream matches a single protocol and never needs a Channel-Voice-1 arm.

```rust
use tutti_midi_types::prelude::*;
use tutti_midi_types::{convert, UmpMessageType};

// From the wire: a MIDI 1.0 note-on, 7-bit velocity 100.
let from_din = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 100])
    .expect("0x90 is a well-formed note-on");
assert_eq!(from_din.message_type(), UmpMessageType::ChannelVoice1);

let promoted = normalize(&from_din);
assert_eq!(promoted.message_type(), UmpMessageType::ChannelVoice2);

// Authored natively: 16 bits of velocity, no 7-bit original to be faithful to.
let authored = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD);
assert!(authored.message().is_note_on());
```

## Constraint: the resolution boundary, and why the loss hides

Widening is the spec's Min-Center-Max scaler (`convert::midi1_velocity_to_midi2`),
not a shift: `127` must reach full scale, and `127 << 9` is `65024`, which is not.
Read a promoted note through `MidiEvent::velocity_u16` and the 7-bit original is
recovered exactly, in both directions:

```rust
use tutti_midi_types::prelude::*;
use tutti_midi_types::convert;

assert_eq!(convert::midi1_velocity_to_midi2(127), 65535);
assert_eq!(convert::midi1_velocity_to_midi2(64), 0x8000);

// Every 7-bit value round-trips exactly.
for v in 0u8..128 {
    let wide = convert::midi1_velocity_to_midi2(v);
    assert_eq!(convert::midi2_velocity_to_midi1(wide), v);
}
```

**That exactness is the trap.** A path that narrows to 7 bits is
*self-consistently* lossy — it round-trips every value it can emit, so a
round-trip test passes and the loss never shows. It is only visible on a value
that did not start at 7 bits:

```rust
use tutti_midi_types::convert;

let authored = 0xABCD_u16;
let via_7bit = convert::midi1_velocity_to_midi2(convert::midi2_velocity_to_midi1(authored));
assert_ne!(via_7bit, authored); // 43981 in, 43690 out — 128 codes survive of 65536
```

So `MidiEvent::velocity_u7` is for a MIDI 1.0 *destination* only. Reading a
velocity for any other purpose goes through `MidiEvent::velocity_u16`, which is
lossless from either protocol.

## The prelude is deliberately narrow

It pulls in what building and decoding MIDI needs: the wire event, its decoded
view, per-note identity, the unit id, the `normalize` seam, and the Clip File
codec. The advanced surfaces — UMP-Stream endpoint negotiation, Flex Data,
RPN/NRPN translation state, MPE zone config, the sync decoders, the raw `convert`
scaling module — stay explicit imports so a glob does not flood scope.

## Where it sits

Depends only on `tutti-types` (for `RtPublish`, and the `MidiGroup` /
`MidiChannel` newtypes) plus `midi2` and `midly`, both re-exported whole so a
consumer matching our version needs no dependency entry of its own. Almost
everything MIDI-adjacent depends on it: `tutti-core`, `tutti-midi-runtime`,
`tutti-midi-hardware`, `tutti-midi-file`, `tutti-polysynth`, `tutti-soundfont`,
`tutti-plugin-types`, all four format hosts, and `tutti-plugin-server`.

## Features

`default = []`.

- `serde` — `Serialize` / `Deserialize` on the **authored-configuration** types
  only, MPE zone setup today. Forwards to `tutti-types/serde`, because
  `MpeZoneConfig` is built from those newtypes.

**This is not a licence to derive serde on the wire types.** The criterion, so it
is not re-litigated: a MIDI type may carry these derives only when it is authored
configuration, carries no audio-thread-stamped field, and has the same shape at
rest as on the wire. `MpeMode` passes all three; `MidiEvent` fails the last two —
its `frame_offset` is stamped by the audio thread against a block size the
document never sees, and MIDI has no durational note record — so it must never
get this treatment.

## License

MIT OR Apache-2.0
