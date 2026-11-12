# tutti-midi-runtime

MIDI **runtime state** for the Tutti audio engine: the graph nodes that carry
MIDI in, through and out of an audio graph, the lock-free rings MIDI crosses
threads on, and the engine's outbound clocks. Re-exported by `tutti` as
`tutti::midi_runtime` (feature `midi`).

## What this is

Pure MIDI values live in `tutti-ump` and `tutti-midi-protocol`; OS ports live in
`tutti-midi-hardware`. This crate owns the runtime state that connects them:

- **MIDI as graph nodes**, sending on `tutti-graph` event ports:
  [`MidiIngestNode`] (the device inlets in, one port per Group and channel), [`MidiClipNode`] and
  [`HarmonyNode`] (a timeline), [`ClockNode`] (Beat Clock and MTC). MIDI leaves the graph
  through an event tap (`tutti-graph`'s `EventTapReader`).
- [`ClockMap`]: the pure OS-time to graph-frame map (and back), built from
  readings of the OS `Instant` and the graph's `Frame`; it holds no clock.
- [`DeviceInlets`] and [`DeviceInput`]: one event inlet per hardware MIDI
  device. The device's thread places each arrival on a frame with a `ClockMap`
  fed the engine's block clock and pushes it into its inlet, so the audio thread
  converts no time; the ingest node has one event input per device slot.
- [`NoteLedger`]: the notes a node has started and not ended, and the only way
  it writes to its port; [`MidiClipNode`] uses it.
- [`MidiKeys`]: the key helpers (`note_on`, `note_off`) over `tutti-graph`'s
  event inlet, which is how a control thread sends MIDI into the graph.
- [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`]: the lock-free ring MIDI
  a host's own outbound MIDI crosses to the session's drain on (it goes when
  the host's outbound path is its own).
- [`ClockMaster`], [`JrClock`] and [`JrStamper`]: engine-produced MIDI *out*
  (Beat Clock, MTC, JR timestamps).
- [`MidiClockDecoder`], [`MtcDecoder`], [`SmpteTimecode`] and
  [`SmpteFrameRate`]: the inbound half of sync (Beat Clock and MTC decoders),
  beside the master that sends them.
- [`MpeIngest`]: the input-edge transform that rewrites classic-MPE channel
  spread into native MIDI 2.0 per-note messages.

The MPE zone types (`MpeMode`, `MpeZoneConfig`, ...) are
`tutti-midi-protocol`'s; `MidiIngestNode::with_mpe` takes a `MpeMode` from there.

## What it does not own

`tutti-ump` and `tutti-midi-protocol` are pure values with no state; this crate is state with **no
OS API**; `tutti-midi-hardware` is the OS edge. So a synth that plays a clip,
or an offline export, gets its MIDI without linking CoreMIDI or ALSA.

- **No ports, and no `cfg(target_os)`.** Enumerating, opening and sending on a
  real endpoint belongs to `tutti-midi-hardware`.
- **No wire vocabulary.** `Ump`, `NoteId` and the MIDI 1↔2 scaling belong to
  `tutti-ump` and `tutti-midi-protocol` (MPE zone values included).
- **No files.** SMF and the path-level Clip File codec belong to
  `tutti-midi-file`.
- **No audio, and no ECS.** Nothing here touches a sample buffer; the Bevy
  systems that drive this are in `bevy-tutti`.

MPE lives here as an *ingestion* transform, not as a per-synth state machine:
per M2-104, MPE is an input-edge concern, and synth voices track per-note
expression themselves.

## Routing is wiring

A `MidiIngestNode` sends each event out of the port of its Group and channel,
and a Group's System, SysEx and Flex messages out of that Group's channelless
port (`CHANNELLESS_PORT` for the first). A Group is a channel space of its own
(M2-104 section 2.1.2, Appendix B.1), so each Group the node hears has seventeen
ports; the node hears the first Group unless built with
`MidiIngestNode::hearing(MidiInputPorts::through(last))`, so a one-Group device is
`MIDI_INPUT_PORTS` ports and costs nothing more. A message with no Group
(Utility, UMP Stream) leaves the first Group's channelless port. Which node hears
which channel is which ports it takes event edges from: `MidiInputPorts::listening`
names them.

```rust
use tutti_midi_runtime::{CHANNELLESS_PORT, MIDI_INPUT_PORTS, MidiInputPorts};
use tutti_ump::{MidiChannel, MidiGroup};

let ports = MidiInputPorts::FIRST_GROUP;
// A synth on channel 3 hears port 3 and the channelless port...
let on_three: Vec<usize> = ports
    .listening(Some(MidiGroup::FIRST), Some(MidiChannel::of(3)))
    .collect();
assert_eq!(on_three, vec![3, CHANNELLESS_PORT]);
// ...one on every channel hears all of them.
assert_eq!(ports.listening(None, None).count(), MIDI_INPUT_PORTS);
// Group 2's channel 3 is another port, on a node that hears two Groups.
let two = MidiInputPorts::through(MidiGroup::of(1));
let group2: Vec<usize> = two
    .listening(Some(MidiGroup::of(1)), Some(MidiChannel::of(3)))
    .collect();
assert_eq!(group2, vec![MIDI_INPUT_PORTS + 3, MIDI_INPUT_PORTS + CHANNELLESS_PORT]);
```

A set of routing rules compiles to those port sets with `compile_routes`: each
rule's targets take its Group and channel's ports (several rules, the union), and an
optional fallback takes every port no rule covers.

## MIDI across a thread boundary

Into the graph, a control thread pushes into an event inlet's writer
(`Graph::open_event_inlet`), held on the graph's clock until its frame, and
`MidiKeys` adds the key helpers. Nothing here allocates or locks on the audio
thread.

```rust
use tutti_graph::EventInletNode;
use tutti_midi_runtime::{LIVE_INLET_CAPACITY, MidiKeys};
use tutti_types::At;
use tutti_ump::{MidiChannel, NoteNumber};

let (_node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
let mut keys = MidiKeys::new(writer);
keys.note_on(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60), 0xC800)
    .unwrap();
```

Out of the graph, a mailbox is a lock-free ring a control thread drains:

```rust
use tutti_graph::Offset;
use tutti_midi_runtime::MidiMailbox;
use tutti_ump::{KeyEvent, MidiChannel, NoteNumber, Ump, Voice2, Address, Message};

let (tx, rx) = MidiMailbox::pair();
let on = Ump::from(Message::Midi2(
    Address::first(MidiChannel::FIRST),
    Voice2::note_on(NoteNumber::of(60), 0xC800),
));
assert_eq!(tx.queue(&[(Offset::ZERO, on)]), 1);

let mut block = [(0, Ump([0; 4])); 8];
assert_eq!(rx.poll_into(&mut block), 1);
let Some(KeyEvent::On(key)) = block[0].1.key_event() else { panic!("a note-on") };
assert_eq!(key.note(), NoteNumber::of(60));
```

## MPE folded to native per-note messages

`MpeIngest` is the input edge: a member channel's bend is rewritten as a
*per-note* bend addressed at the note that channel holds, so no voice downstream
has to know what MPE is.

```rust
use tutti_midi_runtime::MpeIngest;
use tutti_fixtures::midi::{lower_zone, note_on, pitch_bend, wide_velocity};
use tutti_graph::Offset;
use tutti_ump::{Message, MidiChannel, MidiGroup, Voice2};

let mut ingest = MpeIngest::new(lower_zone(7));

// A note-on claims member channel 1; the map now resolves that channel.
// Each event becomes none, one, or (a bend range) one per member channel,
// each handed to the closure.
let member = MidiChannel::of(1);
let mut sent = Vec::new();
let on = note_on(MidiGroup::FIRST, member, 60, wide_velocity(100));
ingest.translate(&(Offset::ZERO, on), |e| sent.push(e));
assert_eq!(sent.len(), 1, "a note-on passes through");

// A channel-wide bend on that member becomes a per-note bend on note 60.
let bend = pitch_bend(MidiGroup::FIRST, member, 0xC000_0000);
sent.clear();
ingest.translate(&(Offset::ZERO, bend), |e| sent.push(e));
let [(_, folded)] = sent[..] else { panic!("the channel holds a note") };
let Ok(Message::Midi2(_, Voice2::PerNotePitchBend { note, .. })) = folded.decode() else {
    panic!("a per-note bend")
};
assert_eq!(note.get(), 60);
```

## Constraint: [`MpeIngest::set_mode`] drops all held-note state

The channel→note maps are keyed by a channel range the new mode may not share, so
they cannot carry over. A note held across the call is forgotten: its note-off
arrives on a channel with no mapping and passes through *unfolded*. Silence the
sounding voices separately — reconfiguring is not a panic.

## Constraint: refusal is a value here, not an error

This crate has no `Error` type. Everything here runs on or feeds the audio
thread, where refusal is shaped as a value: a full mailbox drops and reports a
count, and [`MpeIngest::translate`] yields nothing for an event it drops. The file codec's error
is `tutti-midi-file`'s `ClipFileError`; `Midi1Parser` refuses a message with `None`.

## Where it sits

Builds on `tutti-ump`, `tutti-midi-protocol`, `tutti-transport` and `tutti-graph`. Used by the
synth, SoundFont and plugin crates and by `tutti-midi-hardware`; re-exported by
`tutti` and used by `bevy-tutti`.

## Features

None.

## License

MIT OR Apache-2.0
