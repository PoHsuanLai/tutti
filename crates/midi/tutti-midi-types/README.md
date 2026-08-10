# tutti-midi-types

Pure MIDI types for Tutti — MIDI 2.0 / UMP native, plus the tutti-domain routing,
MPE and sync vocabulary.

## What this is

The canonical event type is `MidiEvent`: a packed 20-byte UMP event carrying a
sample-accurate frame offset plus up to four UMP words, so any MIDI 1.0 / 2.0 /
SysEx / utility message fits one type. Build with the inherent constructors
(`MidiEvent::note_on`, `MidiEvent::cc`, …) and decode through `.message()` or
`midi2::UmpMessage::try_from`.

Around it, the tutti-domain modules: `routing` (the DAW routing table),
`mpe` (MIDI Polyphonic Expression, RP-053), `sync` (clock and MTC decoders),
`cc` (CC→target mapping), `ci` (Capability Inquiry), `translation` (the
MIDI 1↔2 boundary and its bit-scaling), and `clip_file` (the MIDI 2.0 Clip File
codec, M2-116).

```rust
use tutti_midi_types::prelude::*;

// Build a note, decode it back — no midi2 imports, no width juggling.
let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
let msg = ev.message();
assert!(msg.is_note_on());
assert_eq!(msg.note(), Some(60));
```

## Why it is its own crate

It is **pure types**: no OS API, no runtime state machine, no audio graph. That
is what lets it sit under everything MIDI-shaped in the engine — the runtime,
the hardware layer, the file codecs, the format hosts and both synths all name
it, and none of them pulls the others in by doing so.

The `prelude` is deliberately **narrow**. The advanced surfaces — UMP-Stream
endpoint negotiation, Flex Data, RPN/NRPN translation state, MPE zone config,
the sync decoders, the raw `convert` scaling module — stay explicit imports so a
glob does not flood scope.

## Where it sits

Depends only on `tutti-types` (for `RtPublish`, and the `MidiGroup`/`MidiChannel`
newtypes) plus `midi2` and `midly`, both re-exported. Almost everything MIDI-
adjacent depends on it: `tutti-core`, `tutti-midi-runtime`,
`tutti-midi-hardware`, `tutti-midi-file`, `tutti-polysynth`, `tutti-soundfont`,
`tutti-plugin-types`, all four format hosts, and `tutti-plugin-server`.

## Features

`default = []`.

- `serde` — `Serialize`/`Deserialize` on the **authored-configuration** types
  only, MPE zone setup today. Forwards to `tutti-types/serde`, because
  `MpeZoneConfig` is built from those newtypes.

**This is not a licence to derive serde on the wire types.** The criterion, so
it is not re-litigated: a MIDI type may carry these derives only when it is
authored configuration, carries no audio-thread-stamped field, and has the same
shape at rest as on the wire. `MpeMode` passes all three; `MidiEvent` fails the
last two — its `frame_offset` is stamped by the audio thread against a block
size the document never sees — so it must never get this treatment.

## License

MIT OR Apache-2.0
