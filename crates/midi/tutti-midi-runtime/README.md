# tutti-midi-runtime

MIDI **runtime state machines** for the Tutti audio engine.

## What this is

Pure MIDI types live in [`tutti-midi-types`](../tutti-midi-types); OS ports live
in [`tutti-midi-hardware`](../tutti-midi-hardware). This crate owns the runtime
state that connects them:

- MIDI as graph nodes (doc 013, rewrite item 5), sending on `tutti-graph`
  event ports: `MidiInputNode` (a wire, one port per channel), `MidiQueueNode`
  (what a control thread sends, a keyboard's notes), `MidiClipNode` and
  `HarmonyNode` (a timeline), `ClockNode` (Beat Clock and MTC), and
  `MidiOutNode`, the sink handing MIDI back to a control thread (a hardware
  pump).
- `MidiMailbox` / `MidiSender` / `MidiReceiver` — the lock-free ring MIDI
  crosses a thread boundary on (into a `MidiQueueNode`, out of a `MidiOutNode`).
- `ClockMaster` and the `outbound` machinery — engine-produced MIDI *out* (Beat
  Clock, MTC, JR timestamps).
- `MpeIngest` — the input-edge transform that rewrites classic-MPE channel
  spread into native MIDI-2 per-note messages.
- `negotiate` / `sysex` — UMP-Stream endpoint discovery, MIDI-CI, and SysEx7/8
  packet reassembly.

The MPE mode/zone types are re-exported from `tutti-midi-types`, so a consumer
of the runtime needs one import rather than two.

## What it does not own

It is the middle of a three-way split that keeps two costs off consumers who do
not pay them. `tutti-midi-types` is pure values with no state; this crate is
state with **no OS API**; `tutti-midi-hardware` is the OS edge. So a synth that
plays a clip, or an offline export, gets its MIDI without linking CoreMIDI or
ALSA.

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

## Routing is wiring

A `MidiInputNode` sends each event out of its channel's port, and everything
with no channel (system, SysEx, Flex) out of `CHANNELLESS_PORT`. Which node
hears which channel is which ports it takes event edges from: `input_ports`
names them.

```rust
use tutti_midi_runtime::{input_ports, CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_midi_types::MidiChannel;

// A synth on channel 3 hears port 3 and the channelless port...
let on_three: Vec<usize> = input_ports(Some(MidiChannel::new(3))).collect();
assert_eq!(on_three, vec![3, CHANNELLESS_PORT]);
// ...one on every channel hears all of them.
assert_eq!(input_ports(None).count(), MIDI_INPUT_PORTS);
```

## MIDI across a thread boundary

A `MidiQueueNode`'s controls are the push half of its ring: a control thread
sends, and the node drains the ring at the top of its block. Nothing here
allocates or locks, which is what lets either half sit on the audio thread.

```rust
use tutti_midi_runtime::MidiMailbox;
use tutti_midi_types::{MidiChannel, MidiEvent};

let (tx, rx) = MidiMailbox::pair();
assert!(tx.note_on(MidiChannel::FIRST, 60, 100));

let mut block = [MidiEvent::noop(); 8];
assert_eq!(rx.poll_into(&mut block), 1);
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

Depends on `tutti-midi-types`, `tutti-core` (with `midi`) and `tutti-graph`.
`tutti-polysynth`, `tutti-soundfont`, `tutti-plugin`, `tutti-midi-hardware` and
`bevy-tutti` depend on it.

## Features

`default = []`, and nothing else — the crate is the runtime.

## License

MIT OR Apache-2.0
