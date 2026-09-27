# tutti-midi-runtime

MIDI **runtime state** for the Tutti audio engine: the graph nodes that carry
MIDI in, through and out of an audio graph, the lock-free rings MIDI crosses
threads on, and the engine's outbound clocks. Re-exported by `tutti` as
`tutti::midi_runtime` (feature `midi`).

## What this is

Pure MIDI values live in `tutti-midi-types`; OS ports live in
`tutti-midi-hardware`. This crate owns the runtime state that connects them:

- **MIDI as graph nodes**, sending on `tutti-graph` event ports:
  [`MidiInputNode`] (a wire, one port per channel), [`MidiQueueNode`] (what a
  control thread sends, a keyboard's notes), [`MidiClipNode`] and
  [`HarmonyNode`] (a timeline), [`ClockNode`] (Beat Clock and MTC), and
  [`MidiOutNode`], the sink handing MIDI back to a control thread (a hardware
  pump).
- [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`]: the lock-free ring MIDI
  crosses a thread boundary on (into a [`MidiQueueNode`], out of a
  [`MidiOutNode`]).
- [`ClockMaster`], [`JrClock`] and [`JrStamper`]: engine-produced MIDI *out*
  (Beat Clock, MTC, JR timestamps).
- [`MpeIngest`]: the input-edge transform that rewrites classic-MPE channel
  spread into native MIDI 2.0 per-note messages.
- [`EndpointNegotiator`], [`CiInitiator`] and [`CiResponder`]: UMP Stream
  endpoint discovery and MIDI-CI; [`Sysex7PacketReassembler`] and
  [`Sysex8PacketReassembler`]: SysEx7/8 packet reassembly.

The MPE mode/zone types ([`MpeMode`], [`MpeZone`], [`MpeZoneConfig`]) are
re-exported from `tutti-midi-types`, so a consumer of the runtime needs one
import rather than two.

## What it does not own

`tutti-midi-types` is pure values with no state; this crate is state with **no
OS API**; `tutti-midi-hardware` is the OS edge. So a synth that plays a clip,
or an offline export, gets its MIDI without linking CoreMIDI or ALSA.

- **No ports, and no `cfg(target_os)`.** Enumerating, opening and sending on a
  real endpoint belongs to `tutti-midi-hardware`.
- **No wire vocabulary.** `MidiEvent`, `MidiMessage`, `NoteId` and the MIDI 1↔2
  scaling belong to `tutti-midi-types`.
- **No files.** SMF and the path-level Clip File codec belong to
  `tutti-midi-file`.
- **No audio, and no ECS.** Nothing here touches a sample buffer; the Bevy
  systems that drive this are in `bevy-tutti`.

MPE lives here as an *ingestion* transform, not as a per-synth state machine:
per M2-104, MPE is an input-edge concern, and synth voices track per-note
expression themselves.

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

## Constraint: [`MpeIngest::set_mode`] drops all held-note state

The channel→note maps are keyed by a channel range the new mode may not share, so
they cannot carry over. A note held across the call is forgotten: its note-off
arrives on a channel with no mapping and passes through *unfolded*. Silence the
sounding voices separately — reconfiguring is not a panic.

## Constraint: refusal is a value here, not an error

This crate has no `Error` type. Everything here runs on or feeds the audio
thread, where refusal is shaped as a value: a full mailbox drops and reports a
count, and [`MpeIngest::translate`] returns `Option`. The MIDI parse errors are
in `tutti-midi-types` (`ClipFileError`, `MidiParseError`).

## Where it sits

Builds on `tutti-midi-types`, `tutti-core` and `tutti-graph`. Used by the
synth, SoundFont and plugin crates and by `tutti-midi-hardware`; re-exported by
`tutti` and used by `bevy-tutti`.

## Features

None.

## License

MIT OR Apache-2.0
