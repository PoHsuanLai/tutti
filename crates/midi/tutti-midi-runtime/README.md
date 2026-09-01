# tutti-midi-runtime

MIDI **runtime state machines** for the Tutti audio engine.

## What this is

Pure MIDI types live in [`tutti-midi-types`](../tutti-midi-types); OS ports live
in [`tutti-midi-hardware`](../tutti-midi-hardware). This crate owns the runtime
state that connects them:

- `MidiMailbox` / `MidiSender` / `MidiReceiver` — lock-free per-unit MIDI
  inboxes. A node owns a receiver; callers push through senders.
- `MidiInPort` — a unit's whole MIDI endpoint in one borrow: routing address,
  push mailbox, and the source-install slot.
- `MidiBus` — fan-out from `MidiUnitId` to the right inbox; the engine installs
  one as its audio-thread dispatch target.
- `MidiSnapshot` / `MidiSnapshotReader` / `MidiClipSource` — non-destructive
  event storage and the readers that play it back, on a live or an offline
  timeline.
- `ClockMaster` and the `outbound` machinery — engine-produced MIDI *out* (Beat
  Clock, MTC, JR timestamps), riding the **same** mailbox as MIDI in: the
  producer holds a `MidiSender` (lock-free `&self` push), and an off-RT pump
  drains the paired `MidiReceiver` to a hardware-out port.
- `MpeIngest` — the input-edge transform that rewrites classic-MPE channel
  spread into native MIDI-2 per-note messages.
- `negotiate` / `sysex` — UMP-Stream endpoint discovery, MIDI-CI, and SysEx7/8
  packet reassembly.

`MidiRoutingTable` and the MPE mode/zone types are re-exported from
`tutti-midi-types`, so a consumer of the runtime needs one import rather than
two.

## What it does not own

It is the middle of a three-way split that keeps two costs off consumers who do
not pay them. `tutti-midi-types` is pure values with no state; this crate is
state with **no OS API**; `tutti-midi-hardware` is the OS edge. So a synth that
needs a MIDI inbox, or an offline export that needs a snapshot reader, gets both
without linking CoreMIDI or ALSA.

- **No ports, and no `cfg(target_os)`.** Enumerating, opening and sending on a
  real endpoint is `tutti-midi-hardware`'s.
- **No wire vocabulary.** `MidiEvent`, `MidiMessage`, `NoteId` and the MIDI 1↔2
  scaling are `tutti-midi-types`'.
- **No files.** SMF and the path-level Clip File codec are
  [`tutti-midi-file`](../tutti-midi-file)'s.
- **No audio, and no ECS.** Nothing here touches a sample buffer; the Bevy
  systems that drive this are `bevy_tutti::midi`'s.

One placement worth noting: MPE lives here as an *ingestion* transform, not as a
per-synth state machine. Per M2-104, MPE is an input-edge concern — synth voices
track per-note expression themselves.

## An event reaching a unit's inbox

A `MidiBus` routes by `MidiUnitId`; the unit owns the paired `MidiReceiver` and
drains it at the top of its block. Nothing here allocates or locks, which is what
lets the poll side sit on the audio thread.

```rust
use tutti_midi_runtime::{MidiBus, MidiMailbox};
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_midi_types::{MidiEvent, MidiUnitId};

let synth = MidiUnitId::new(7);
let (tx, rx) = MidiMailbox::pair(synth);

let bus = MidiBus::new();
bus.insert(tx);

let note = MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);
assert_eq!(bus.queue(synth, &[note]), 1);

// The unit's side of the block boundary.
let mut block = [MidiEvent::noop(); 8];
assert_eq!(rx.poll_into(&mut block), 1);
assert_eq!(block[0].note(), Some(60));
```

## MIDI on the engine's timeline

The seam with `tutti-core` is the transport. A `MidiSnapshot` stores events at
absolute `Beat`s; a `MidiSnapshotReader` emits the ones the block just crossed,
stamped with a sample-accurate `frame_offset`. A poll that advanced no beats
yields nothing — the window is half-open, so no event is emitted twice.

```rust
use std::sync::Arc;
use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{Beat, Bpm, SampleRate};
use tutti_midi_runtime::{MidiSnapshot, MidiSnapshotReader};
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_midi_types::{MidiEvent, MidiUnitId, MidiUnitIn};

let synth = MidiUnitId::new(7);
let mut snapshot = MidiSnapshot::new();
snapshot.add_event(
    synth,
    Beat(0.0),
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
);

let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
    start_beat: Beat(0.0),
    tempo: Bpm(120.0),
    sample_rate: SampleRate(48_000.0),
    loop_range: None,
}));
let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));

let mut block = [MidiEvent::noop(); 8];
assert_eq!(reader.poll_unit(synth, 512, &mut block), 0); // no beats crossed yet

timeline.advance(24_000); // half a beat at 120 BPM / 48 kHz
assert_eq!(reader.poll_unit(synth, 512, &mut block), 1);
assert_eq!(block[0].note(), Some(60));
```

## MPE folded to native per-note messages

`MpeIngest` is the input edge: a member channel's bend is rewritten as a
*per-note* bend addressed at the note that channel holds, so no voice downstream
has to know what MPE is.

```rust
use tutti_midi_runtime::MpeIngest;
use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
use tutti_midi_types::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));

// A note-on claims member channel 1; the map now resolves that channel.
let member = MidiChannel::new(1);
ingest
    .translate(&MidiEvent::note_on_7bit(MidiGroup::FIRST, member, 60, 100))
    .expect("a note-on passes through");

// A channel-wide bend on that member becomes a per-note bend on note 60.
let bend = MidiEvent::pitch_bend(MidiGroup::FIRST, member, 0xC000_0000);
let folded = ingest.translate(&bend).expect("the channel holds a note");
assert_eq!(folded.note(), Some(60));
```

## Constraint: `MpeIngest::set_mode` drops all held-note state

The channel→note maps are keyed by a channel range the new mode may not share, so
they cannot carry over. A note held across the call is forgotten: its note-off
arrives on a channel with no mapping and passes through *unfolded*. Silence the
sounding voices separately — reconfiguring is not a panic.

## Constraint: refusal is a value here, not an error

This crate has no `Error` type at all. Everything here runs on or feeds the audio
thread, where refusal is shaped as a value: a full mailbox drops and reports a
count, and `MpeIngest::translate` returns `Option`. The MIDI parse errors live one
crate down (`tutti_midi_types::ClipFileError`, `MidiParseError`).

## Where it sits

Depends on `tutti-midi-types` and `tutti-core` (with `midi`). `tutti-polysynth`,
`tutti-soundfont`, `tutti-plugin`, `tutti-midi-hardware`, `tutti-cpal` (behind
its `midi` feature) and `bevy-tutti` all depend on it.

## Features

`default = []`, and nothing else — the crate is the runtime.

## License

MIT OR Apache-2.0
