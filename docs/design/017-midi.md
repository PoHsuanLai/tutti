# 017: MIDI, modelled on the specification

The MIDI stack is correct at the packet layer and careful inside the graph. What
sits above the packet is not MIDI's own model: devices are OS ports, a message
has no type a consumer has to go through, translation happens while reading, and
crates mix the specification's layers. This design gives the stack MIDI's
hierarchy, one typed message, and translation in one place, and closes the data
loss and stuck notes found on the way.

Citations are to the MIDI Association documents M2-100 v1.1, M2-101 v1.2.1,
M2-102 v1.1, M2-103 v1.2, M2-104 v1.1.2, M2-115 v1.0.1 and M2-116 v1.0, which
are not vendored (`crates/tutti-midi-types/docs/midi2-spec/README.md`).

## Summary

- **MIDI's hierarchy.** Transport, UMP Endpoint, Function Block, Group, Channel,
  Note. Devices are modelled as endpoints with function blocks, and every message
  has the scope the specification gives it (§2).
- **One word type, one message type.** `tutti-ump` holds `Ump`, the `Copy` storage,
  and `Message`, a `Copy` enum, with `decode` and `encode`: pure, and the only code
  that knows a bit position. It has no tutti dependency, and `tutti-graph` depends
  on it. MIDI vocabulary leaves `tutti-types` (§3).
- **Translation at the edge.** MIDI 1.0 to MIDI 2.0 and MPE happen once, in a
  stateful `Translator` per group and channel at the input edge. Reading never
  changes a message's meaning (§4).
- **Connections as a state machine.** M2-100's connection steps are a no-I/O
  `Connection`, driven by tests with no device (§5).
- **Crates follow the layers** (§6).
- **`midi2` is removed** (§7).
- **Events are not silently lost.** Capacity follows the block size, note-offs
  have priority and are retried, drops are counted exactly, and an export reports
  them (§8).
- **Stages**, each landing on its own (§10).

## 1. What is wrong today

Findings were verified on `main` with probes through the public API unless
marked as read.

### 1.1 The model

| The specification | Today | Consequence |
|---|---|---|
| Endpoint, Function Block, Group (M2-100 §7, M2-104 App. H) | `PortId` and a name; `EndpointId` is ALSA's client and port | Identity changes across restarts; `UmpCapability::function_blocks` is never filled |
| Connection steps 2 to 5 (M2-100 Table 4) | Never run on connect | Protocol is guessed, and JR timestamps are a host boolean, against M2-104 §7.1.6.2 |
| `FunctionBlock` is a device concept | Defined in `tutti-midi-runtime`'s `negotiate` | `tutti-midi-hardware` depends on the runtime, and so on `tutti-graph`, for one struct |
| A translator bridges MIDI 1.0 connections (M2-104 §6.2) | One per input node, keyed by channel | RPN state crosses groups; output is forced to group 0 |
| Decoding and translating are separate | `MidiEvent::message()` calls `normalize` | A MIDI 2.0 Note On at velocity 0 becomes a Note Off (M2-104 §7.4.2 says it does not), while `clip_node` counts it as a note-on |
| Messages have a scope | One flat `MidiEvent` | An endpoint message can be routed like a note |
| One packet format | `tutti_graph::Ump`, `MidiEvent`, `MidiMessage` | Note-off decoding is written by hand in four places |

Three ways of reading a message compete. `MidiMessage` has 22 variants and almost
no production consumer outside its crate; it has no group, mints a `NoteId` on
decode, and repeats `frame_offset` in each variant. The polysynth, SoundFont,
the CLAP host and the VST3 host now read `tutti_ump::Message` (stage 2); the VST3
host still reaches `midi2` through `tutti-midi-types`' `PerNoteBendRanges::observe`
and `MidiEvent::from_midi1_bytes`.
`tutti-graph` reads through
`Ump::is_note_off` (one test kernel takes a data byte by hand); `clip_node`,
`clip_file` and `tutti-midi-types`' `wire.rs` shift bits by hand. `MidiEvent::frame_offset`
has three meanings: a block offset, an absolute frame, and meaningless in storage.

### 1.2 What is solid and stays

Packet encoding, bit scaling (M2-115), SysEx packetization and clip file framing
match the specifications. The event path is UMP-native across the graph, plugin
IPC and hosts. PDC covers event ports. `MidiClipNode`, `HarmonyNode` and
`ClockMaster` place events by closed form. The audio-thread side of the hardware
input (SPSC rings, `ArcLend` port set) and the no-allocation gates are careful.
About 600k hostile inputs caused no panic in the parsers.

### 1.3 Cost is not the problem

No MIDI crate has a benchmark, but measured in release, the receive path takes
about 0.3% of the budget of a 64-frame block at 48 kHz, at 40 events per block,
with no allocation. The stack's own code compiles in about 2 s. Neither justifies
a change on its own. This design is for correctness and for what a caller can
express.

## 2. The hierarchy

```
Transport         an OS API or a test double: moves words, names devices
└ Endpoint        stream messages; one active protocol; JR timestamps on or off
  └ FunctionBlock groups it spans, direction, MIDI 1.0 bandwidth; MIDI-CI lives here
    └ Group       system messages, SysEx, flex data
      └ Channel   channel voice
        └ Note    per-note messages
```

Three choices differ from the specification on purpose:

- **Frames inside the engine.** JR timestamps and Delta Clockstamps belong on the
  wire and in files. An event in the graph has an `Offset`, and an OS timestamp or
  JR value is converted once, at the edge.
- **One protocol inside the graph.** MIDI allows either protocol per endpoint.
  Every graph node sees MIDI 2.0 channel voice, so a node has one case to write.
- **Graph ports for routing.** Group and channel select a route at the edge. The
  graph does not reproduce the hierarchy.

## 3. `Ump` and `Message`

```rust
// tutti-ump. No tutti dependency, so tutti-graph can use it.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable)]
pub struct Ump([u32; 4]);

impl Ump {
    pub const fn words(&self) -> &[u32];     // 1, 2, 3 or 4 by message type
    pub const fn is_note_off(self) -> bool;  // for the graph's FIFOs, the only copy
}
```

Message types `0xB` and `0xC` are 96 bits (Table 4), so a packet is 1, 2, 3 or 4
words. The graph's `EventKind::Midi` holds this `Ump`. Its doc says the graph never
inspects the words, and `Event` does: `is_note_off` makes that the one place.
`Pod` lets plugin IPC copy a block of events as one memory copy.

`tutti-ump`'s only normal dependency is `bytemuck`, because the graph and everything
above it compile it. `tutti-types` has no MIDI vocabulary: `MidiGroup`, `MidiChannel`
and `CCNumber` move here. `tutti_types::Note` stays, because it is a musical pitch
(class and octave) used by `tutti-dsp`'s analyzers, not a wire value.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    Utility(Utility),                  // MT 0x0: no group
    System(Group, System),             // MT 0x1
    Midi1(Address, Voice1),            // MT 0x2
    Sysex7(Group, Sysex7Packet),       // MT 0x3
    Midi2(Address, Voice2),            // MT 0x4
    Data128(Group, Data128),           // MT 0x5: SysEx8, Mixed Data Set
    Flex(Group, Flex),                 // MT 0xD
    Stream(Stream),                    // MT 0xF: no group
}

pub fn decode(ump: Ump) -> Result<Message, Unknown>;  // pure
impl From<Message> for Ump { .. }                     // pure
```

Rules:

- **No lifetimes.** A message is a value. It can be matched, stored or queued on
  any thread with no allocation. This is checked against M2-104 App. G: every
  defined message is one packet of at most 16 bytes, so every field, including
  text and SysEx chunks, fits in a small array. Strings across packets are
  reassembled above the codec, off the audio thread.
- **`Midi1` and `Midi2` stay distinct.** They are different message types on the
  wire, and decode does not turn one into the other.
- **Unknown stays intact.** `Err(Unknown(ump))` keeps the words, so a message from
  a later revision is forwarded unchanged. Reserved message types and a defined
  message type with an unknown status both land here.
- **Only the codec shifts bits.** The helpers are private. A consumer matches on
  `Message`, and the graph calls `Ump::is_note_off`.
- **Hand-written match arms, not a macro or a proc macro.** There are few messages
  and each arm is short. A reader checks them against the specification's table,
  and a macro would hide what they are checking.
- **Fields are typed.** Note, group, channel and the 4-, 7- and 14-bit values
  (`NoteNumber`, `U4`, `U7`, `U14`) have a public constructor that refuses
  out-of-range values and a private one masked off the wire. A note of 200 cannot
  be built, where today it silently becomes 72 in release. There is no `Velocity`
  type: MIDI 2.0 velocities stay `u16`, and their scaling is M2-115's. The refusal
  holds for every public `new`, the three moved types (`MidiGroup`,
  `MidiChannel`, `CCNumber`) included. `of` builds one from a literal or a
  constant and panics out of range, and `masked` is the one constructor that
  masks, for bits a decoder has just framed. Types are added under the rules in
  CLAUDE.md: a distinct range or a distinct algebra. `Pitch` (Q7.25, widened from
  the Note On attribute's Q7.9 by zero extension) is one: the two pitch fields
  share it, and `Attribute::pitch` reads it.
- **A per-note controller number is typed where Appendix A defines it.**
  `RegisteredPerNoteController` names each defined number and keeps any other as
  `Reserved(u8)`, so every raw value re-encodes; build one from a wire number with
  `From<u8>`, because a hand-built `Reserved` of a named number does not match its
  named variant. `AssignablePerNote.controller`
  stays a byte: there are 256 assignable controllers, so it is not a `CCNumber`
  (7 bits), and masking it would alias controllers 128 to 255 onto 0 to 127.
- **Flex addressing follows the specification.** Set Tempo, Set Time Signature and
  Set Metronome are Group messages: §7.5.3 to §7.5.5 fix Format 0 and Address 1,
  so they carry no `FlexAddress`. Key signature, chord name and text do, and carry
  it in their variant.
- **Reserved bits** are ignored on decode and zero on encode. A reserved value in
  an enumerated field is kept raw (`Letter::Reserved`, `FlexAddress::Reserved`,
  `Attribute::Reserved`, `RegisteredPerNoteController::Reserved`, the field types'
  out-of-range `wire`), so the message still decodes and re-encodes exactly.
  `Unknown` is for a packet that cannot be framed: a reserved message type, or a
  defined type with an undefined status. A consumer that forwards forwards
  `Unknown` unchanged and never drops it; one that is a sink says so in its arm.
- **Name limits are not in the codec.** They apply to a reassembled string, so
  they are checked where strings are reassembled.

`NoteId` is an engine concept: `MidiMessage` mints one on decode and the wire has
none. It moves to whichever node assigns voices, and leaves decoding.

`MidiEvent`'s three meanings of `frame_offset` become separate types: `Offset` in
a block (the graph's `Event` already has it) and an absolute frame in storage.

## 4. Translation

```rust
// tutti-midi-protocol
pub struct Translator { /* per (group, channel): RPN state, MPE zones */ }

impl Translator {
    pub fn feed(&mut self, msg: Message, out: impl FnMut(Message));
}
```

- It runs once, at the input edge. Nothing else calls it, and no read translates.
  Its stateless part exists first as `widen(Voice1) -> Voice2` (§10, stage 1.5);
  the `Translator` calls it for the cases that need no state and adds the rest.
- State is keyed by group and channel, and the group is kept.
- It follows M2-104 App. D, which fixes several current defects: a MIDI 1.0 Note
  On at velocity 0 becomes a Note Off with velocity `0x8000` (D.3.1 in v1.1.2); a
  MIDI 2.0 Note On at velocity 0 stays a Note On; RPN data is zero-extended for
  index 0 to 31 (M2-115 §4.1), which `rpn.rs` scales with Min-Center-Max.
- Translating back is D.2: an RPN or NRPN becomes its four Control Changes
  (D.2.3). Today it has no MIDI 1.0 form, so a keyboard's pitch bend range never
  reaches a CLAP, VST2, AU or SoundFont consumer.
- MPE is a dialect on MIDI 1.0, handled here. Channels outside a zone pass through
  ("remain available for conventional use", RP-053), and bend, pressure and CC74
  sent before a note-on bind to it instead of being dropped. The group is kept.

A plugin host that wants MIDI 2.0 data takes `Midi2` directly. One that speaks
MIDI 1.0 asks the translator for D.2, so the rule lives in one place. Hosts then
honour a plugin's declared dialect, including `CLAP_EVENT_MIDI2`, which is never
sent today.

## 5. Connections

```rust
// tutti-midi-device
pub struct Endpoint { identity: Option<ProductInstance>, protocol: Protocol,
                      blocks: ArrayVec<FunctionBlock, 32>, /* .. */ }

pub struct Connection { /* M2-100 Table 4, steps 2 to 5 */ }

impl Connection {
    pub fn on_message(&mut self, msg: Message, now: Instant) -> Action;
    pub fn on_timeout(&mut self, now: Instant) -> Action;
}
```

A MIDI 1.0 device has no steps 2 to 4 and appears as an `Endpoint` with one
inferred block. An endpoint with many groups is one device, not many.

**Stable identity.** A device is identified by what survives a restart: CoreMIDI's
`MIDIUniqueID`, or the UMP Product Instance Id, falling back to the name. An ALSA
client number is only a handle to the current connection. `DeviceId` is the
Product Instance Id when there is one, and otherwise the client and port name plus
an ordinal on ALSA.

**Device loss is an event.** Backends watch for removal (the ALSA announce port, a
CoreMIDI notify proc). Today a vanished device stays listed as connected, verified
on ALSA, and `Disconnected` never fires. On loss the session retires the port,
emits `Disconnected`, and the input edge flushes (§8.3).

### Device detection

```rust
// tutti-midi-hardware
let devices = Devices::open()?;            // starts watching; drop stops
let all: Vec<DeviceInfo> = devices.list();
for event in devices.poll() {              // DeviceEvent::{Added, Removed, Changed}
    /* .. */
}
let events = devices.wait(timeout);        // blocks until an event or the timeout
let input = devices.open_input(id)?;

// tutti-midi-device: the pure core
impl DeviceTable {
    pub fn apply(&mut self, change: OsChange) -> Option<DeviceEvent>;
}
```

- `Devices::open()` starts the watch and dropping it stops it. `poll` and `wait`
  give the changes since the last call.
- `DeviceTable` is pure. It takes an `OsChange` from the backend and returns the
  event, if the change alters the table. A repeat of a known state returns `None`.
- `DeviceId` is stable across a restart (above).
- `FakeBackend` has `plug()` and `unplug()`, so a test drives hot-plug with no
  device.

### Timing at the edge

- **The clock map.** `ClockMap` converts an OS time to a frame. It is pure: it
  takes the two clocks' readings and holds no clock itself.
- **Input.** Take the OS's timestamp where one exists (CoreMIDI packets carry
  host time, ALSA has queue timestamps). Replace ALSA's 1 ms sleep with a blocking
  `poll` on the sequencer's descriptor, plus a descriptor to wake it for shutdown. Today's read-time stamp measured 1 ms of
  jitter, about 48 samples at 48 kHz, and collapses a burst's spacing.
- **Output.** Schedule from the event's frame, on the OS's timestamp or queue.
  Today output is flushed once per Bevy frame with no timestamp, so MIDI clock
  leaves in bursts, and a headless `Session` does no better. This is the largest
  timing defect that users can hear from external gear.

### MIDI-CI

`ci/*` and `negotiate` are about 2,400 production lines with no production path:
no inbound SysEx reaches them. They also miss current requirements: no ACK, no
Invalidate MUID response (M2-101 App. F), profile bodies without Message Version 2
fields (18 bytes against 20), and Property Exchange built for v1.1.

Keep the header, MUID, Discovery, NAK and Invalidate codec and the Stream
constructors, about 400 lines, moved to `tutti-midi-device`. Delete the state
machines and profile and property bodies. Rebuild the App. F minimum with a real
inbound path, in one change, when a stage needs it (an estimated 650 to 750
lines). Property Exchange waits for a named peer and for M2-105, which is not in
the core set.

## Layers

Every MIDI feature has four layers:

1. A pure core with no OS, no thread and no clock. Time arrives as a value.
2. A thin OS or graph layer that feeds the core and carries out what it returns.
3. Policy in `tutti-session`: when to poll, what to open, what to do on loss.
4. `bevy-tutti`, which translates only. `scripts/check-adapter.sh` enforces it.

| Feature | Core |
|---|---|
| Device detection | `DeviceTable` |
| Connection steps | `Connection` |
| OS time to frame | `ClockMap` |
| Output scheduling | `OutputPlan` |
| Held notes and controllers | `HeldState` |
| Recording into clips | `ClipBuilder` |
| Chase on seek | `ChaseIndex` |
| Clock, MTC and MMC slave | `ClockFollower` |
| CC modulation and MIDI learn | `Binding`, `LearnState` |

The same pattern is already in use: the plugin hosts' pure `step` (design 016),
`Phase::Poll`, `PhaseCx { now }`, and `Session::update() -> Vec<SessionEvent>`.
Audio devices (design 018) follow the pattern and share `Phase::Poll`. They share
no code with MIDI.

## 6. Crates

| Crate | Layer | Holds | Depends on |
|---|---|---|---|
| `tutti-ump` | Packet | `Ump`, `Message`, codec, `MidiGroup`, `MidiChannel`, `CCNumber`, field types | No tutti crate |
| `tutti-midi-protocol` | Translation | `Translator`, App. D, M2-115 scaling, MPE | `tutti-ump` |
| `tutti-midi-device` | Endpoint, Function Block | `Endpoint`, `Connection`, CI codec, JR | `tutti-ump` |
| `tutti-midi-hardware` | Transport | OS backends, no graph, `#![forbid(unsafe_code)]` | `tutti-midi-device`, the two `-ffi` crates |
| `tutti-alsa-ffi` | FFI | All ALSA `unsafe`, safe wrappers (from `tutti-alsa-ump-sys`) | Linux only |
| `tutti-coremidi-ffi` | FFI | The raw CoreMIDI calls the `coremidi` crate lacks | macOS only |
| `tutti-midi-runtime` | Engine | Graph nodes | `tutti-graph`, `tutti-midi-protocol` |
| `tutti-graph` | Graph | Event ports hold `Ump` | `tutti-types`, `tutti-ump` |
| `tutti-midi-file` | Files | SMF, Clip File | `tutti-ump`, `tutti-tempo` |
| `tutti-tempo` | Time | `TempoMap` (beat to sample); not MIDI | `tutti-types` |

`tutti-midi-types` goes away. This is not a split for compile time, which is
already small. Dependencies enforce the layering: `tutti-ump` cannot call the
translator, so reading cannot translate, and `tutti-midi-hardware` cannot reach
the graph. The cycle that keeps `ump` and `translation` together today is mostly
`message()` calling `normalize`, and it ends with that call.

`tutti-midi-runtime` keeps only graph nodes and what they need: the input, queue,
out, clip, harmony and clock nodes with `ClockMaster`, plus `MidiMailbox`, routes,
`gather_midi` and `BlockSplitter`, about 3,100 of its 5,700 production lines. Its
layout flattens to match. `negotiate` and `jr_timestamp` move to `tutti-midi-device`,
`mpe_ingest` to `tutti-midi-protocol`, and the SysEx reassemblers go with the layer
that uses them (`Sysex8PacketReassembler` has no production caller and is deleted
unless one appears). `outbound` goes away, along with the re-exports from
`tutti-midi-types`, and `pub mod` inside a private parent becomes `mod`.

`tutti-midi-hardware` is a transport and nothing else. Its re-exports go: `midi2`,
`midly`, `ci`, the translator, the Clip File codec, MPE, `CCMapping`, the sync
decoders, the graph nodes, and a prelude that hands out `MidiInputNode`. A consumer
names the owner. So does its vocabulary: one crate says port, endpoint, connection
and device for overlapping things, and `MidiSession` reads as if it were
`tutti_session::Session`. It becomes `Endpoint` from `tutti-midi-device`, with `Port`
only for an OS's handle to a connection, and `async_port` (an SPSC ring, not async)
is named for what it is. A byte-stream source is read by `tutti_ump::Midi1Parser`;
`Sysex7ByteAssembler` had no product caller and is deleted.

MIDI follows the plugin hosts' FFI rule (design 016 §2.9): `unsafe` lives only in the
`-ffi` crates, and `scripts/check-plugin-ffi.sh` or a sibling gate enforces it.

`sync`'s clock and MTC decoders have no consumer today. They move to the runtime
when a clock slave uses them (§9), and are deleted otherwise. `cc::mapping` is
replaced by a CC modulation source (§9), and `jr_timestamp` is rewritten in
`tutti-midi-device`, where it can be negotiated and driven by elapsed time.

## 7. `midi2` and `midly`

`pub use midi2` and `pub use midly` make third-party, pre-1.0 versions part of our
API, with 55 uses of that path across crates. Both re-exports go.

`midi2` gives correct layouts, mostly valuable for Flex, Stream and SysEx, and a
typed view over a buffer (`NoteOn<&[u32]>`). That is typed packets, not this
design's model, and it does not enforce the name limits in M2-104 §7.1.

- `tutti-graph` depends on `tutti-ump`, so `midi2` and its build tree (`fixed`,
  `typenum`, `half`, `zerocopy`, `derive_more`) cannot be a normal dependency. Every
  message, including Flex and Stream, is written directly.
- It is removed from the workspace at stage 5, with the old types. Until then, while
  the old types still depend on it anyway, it serves as a test-only oracle: random
  words go through both decoders, and each disagreement is settled by reading the
  specification and pinned as an analytic test. What survives its removal is what
  those tests name: a swap of two neighbouring numbers in a number-to-name table, or
  of two fields or flags, fails a spec-literal test in `tests/spec_words.rs` or
  `tests/spec_tables.rs` with `tests/midi2_oracle.rs` deleted. The round-trip
  properties and the fuzz target check the codec only against itself (decode,
  encode, decode), so a mistake made the same way in both directions passes them;
  they do not replace the oracle. What stage 5 does lose is independent agreement
  with a second decoder on words no table or figure pins, and the name-only
  comparison of the Flex and Stream families. The fuzz target is not run in CI
  yet; nothing here leans on it.

`midly` stays in `tutti-midi-file`. `tutti-midi-types` uses it only for about 12
lines of live MIDI 1.0 parsing, which `tutti-midi-protocol` does itself.

## 8. Not losing events

### 8.1 What happens today

A dense clip into a slow reader loses events at a fixed block capacity
(`CLIP_EVENT_CAPACITY`, 256). Measured at 32,000 events per second: 25% lost at
512 frames, 62% at 1,024. A real `render_to_buffers` export (1,024-frame blocks)
returned `Ok` while losing 62%, and a chord clip left 144 of 200 notes sounding to
the end of the file. The executor's drop counter reports about half of the loss,
because a refused note-on's later note-off is swallowed uncounted. A refused
note-off is not retried on ordinary playback (`owed` is set only in `release`).

### 8.2 The change

- **Capacity follows the block.** Port capacity is a function of `Prepare::max_block`
  and a configured density, sized on the control side.
- **Note-offs have priority.** A port at capacity refuses a new note-on before a
  note-off. A refused note-off stays owed and is retried in the next block. The
  clip node stops forgetting one.
- **Drops are counted exactly,** per port, and surface as a session event, not only
  as a load-meter figure.
- **An export reports loss.** A render that dropped an event returns that in its
  result, and a caller can make it an error.

### 8.3 Flush

The input edge and queue node track held notes and controllers per group and
channel. On a route change, device loss, node removal or replacement they send
Note Offs, then reset what they changed (sustain, bend). Today a held key plus
`retarget` or `unroute` leaves a stuck note on the old target and an orphan off on
the new one.

## 9. Missing capabilities, in the order a DAW needs them

Each is its own lane after its dependency stage.

1. **Chase on seek** in `MidiClipNode`: controllers, program and bend, and notes
   spanning the seek point. Starting at beat 2 of a clip emits nothing today.
2. **MIDI recording** into clips, timestamped against the transport.
3. **CC modulation source and MIDI learn for native nodes.**
4. **Windows backend** (Windows MIDI Services), **Linux virtual ports** and more
   than one output.
5. **Clock, MTC and MMC slave** with tempo follow, and a `ClockMaster` that handles
   a loop wrap inside a block.
6. **Files** (below).

### Files

`tutti-midi-file` adds real work on top of `midly`: musical time, note
pairing, and one reader for SMF and Clip File. The rest is thin or awkward. There
are two read models from one parse, `pub use midly` and `SmfMessage` expose
`midly`'s types, and `iter()` writes each parsed message to wire bytes in order to
parse it again. Reading drops SysEx, time and key signatures and markers, merges
Format 2 as if parallel, rejects SMPTE division, and returns `Ok` on a truncated
file. Writing has no tempo map, track names or SysEx.

```rust
pub struct MidiFile { kind: MidiFileKind, tracks: Vec<Track>, tempo_map: TempoMap }  // tutti_tempo
pub struct Track { name: Option<String>, events: Vec<(Tick, FileEvent)> }

pub enum FileEvent {
    Message(Message),      // SMF: Midi1 on group 0. Clip File: as stored.
    SysEx(Box<[u8]>),      // any length, unlike a packet
    Meta(Meta),            // text, markers, key signature, Other with raw bytes
}

impl MidiFile {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error>;  // reports truncation
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error>;
    pub fn events(&self) -> impl Iterator<Item = (Beat, &FileEvent)>;  // merged
}
impl Track { pub fn notes(&self) -> impl Iterator<Item = PairedNote> + '_; }
```

- **Our types in the API, `midly` private.** Its events borrow the input bytes, and
  exposing them would make it a public dependency. It stays as the SMF byte parser,
  and swapping it would touch one file.
- **The file layer does not translate.** An SMF reads as `Midi1`, and a MIDI 1.0 Note
  On at velocity 0 stays one. `Translator` runs when a clip is loaded for playback.
- **`FileEvent` is not a mirror of `Message`.** It adds what a packet cannot hold.
  It allocates, and files do not reach the audio thread: the clip node holds a
  compiled form.
- **`TempoMap` is not a MIDI type.** It maps beats to samples and lives in
  `tutti-tempo`, a small pure crate above `tutti-types` with
  `#![forbid(unsafe_code)]`, because the transport, the export plan, plugin
  transport info and SMF import all want it. The file layer builds one from tempo
  meta events. Adopting it in the transport is separate work.
- **Integer ticks.** Events keep their tick and the track its division, so a write is
  exact with no rounding, and `events()` gives callers `Beat`.
- **A lossless round trip** is the test: a file read and written holds the same
  events, including SysEx and meta. Running status and event spelling may differ.
- **Clip File moves here** from `tutti-midi-types`, its only production consumer.
- Measure `midly`'s `parallel` feature, which pulls in `rayon`, and check its `strict`
  feature for truncated files.

## 10. Stages

Each stage lands on its own, with renders bit-identical except where a stage names
a behaviour change.

0. **Stop losing events** (§8). No model change needed, and no dependency on the
   others. Behaviour change: fewer drops, a new report.
1. **`tutti-ump`.** First `Ump` and the moved vocabulary, with every caller updated
   and no behaviour change. Then `Message`, the codec, tests and benchmarks,
   alongside the old types.
1.5. **Stateless widening,** before SoundFont and the hosts move:
   `tutti_midi_protocol::widen` turns a `Voice1` into a `Voice2` (M2-115 scaling,
   App. D.3, a MIDI 1.0 Note On at velocity 0 as a Note Off at `0x8000`), so a
   consumer matches `Voice2` once instead of writing each message twice. A
   Control Change that needs per-channel state (RPN and NRPN data, Bank Select)
   stays a plain Control Change there. A consumer's call is transitional: stage 3
   moves the call to the input edge and consumers drop it with their `Voice1`
   arm. Behaviour change: a MIDI 1.0 Note On at velocity 0 reaches the consumer
   as a Note Off with release velocity `0x8000`, not `0`.
2. **Consumers move to `Message`,** one per PR: polysynth, SoundFont, then each
   host (the CLAP host is done; it cannot forward `Unknown`, because it sends no
   `CLAP_EVENT_MIDI2`).
   A consumer with MIDI 1.0 resolution only (SoundFont, the hosts) narrows with
   `tutti_midi_protocol::{narrow_velocity, narrow_value, narrow_bend}` (M2-115
   section 3.4, a right shift).
3. **`Translator` in `tutti-midi-protocol`.** Translation to the edge, with the
   state: RPN and NRPN assembly, Bank Select, groups, MPE. Behaviour change: §4's
   remaining spec fixes.
4. **`tutti-midi-device` and the `-ffi` crates.** Device detection comes first
   (`Devices`, `DeviceTable`, `FakeBackend::plug`; §5), directly after stage 1.
   Then model, connection, identity, device loss, timing (§5), and the backends
   with no `unsafe` of their own.
   Behaviour change: real hot-plug, timestamps, output scheduling. CI codec moves
   here and the rest of `ci`/`negotiate` is deleted.
5. **Remove the old types:** `tutti-midi-types`, `MidiEvent`, `MidiMessage`, the
   re-exports, and `midi2` from the workspace and `Cargo.lock`.
6. **Capabilities** (§9).

## 11. Testing

- **Spec tables.** Each message's words from M2-104 App. F and the tables in M2-115
  are analytic tests.
- **`midi2` as an oracle** until stage 5, a dev-dependency. What it finds becomes
  spec-pinned tests.
- **Properties.** `decode` never panics on any words, and `encode(decode(x)) == x`
  for a canonical `x`.
- **Fuzzing.** `cargo-fuzz` targets for UMP, SysEx, MIDI-CI and clip files, none of
  which exist in the repo today.
- **Scheduler-free connections.** `Connection` tests take a clock value and never
  sleep.
- **A real transport in CI.** ALSA user-space virtual clients on Linux. The ALSA
  backend has no tests today and CI runs neither loopback example.
- **Benchmarks before migration.** Decode, translate, input node, clip node, host
  conversion, recorded before stage 2 so the gain or cost is measured.
- **No-allocation gates** on every new path, and mutation records on new tests.

## 12. Optimization

No layout change is justified by measured CPU. These come at no cost with the
design, or remove a measured user-visible problem:

1. Decode once per consumer, instead of the 3 to 6 decodes per event today.
2. Fewer conversions: a note takes up to 9 steps from key to CLAP synth, and its
   value changes in two. One `Ump` removes most of them, and `Pod` makes plugin
   IPC one memory copy (gain unmeasured).
3. `HeldNotes` allocates about 132 KB per clip node, and can shrink.
4. Clips can store variable-length word streams as M2-116 does, a memory saving
   for very large clips only.

`bytes::Bytes` does not fit `Ump`: its handle alone is twice the size of a packet,
it reference-counts atomically, and its last drop can free on the audio thread,
which is against the publishing rule. It may suit large off-audio-thread payloads
such as reassembled SysEx; that is decided when that code is written.

## 13. Open questions

- **Device-side MIDI 2.0 on ALSA.** Group-to-port mapping is read from kernel docs,
  and no UMP hardware has been tested. Needs a real device before stage 4 is
  declared done.
- **MPE details.** RP-053 v1.0 was read from a public copy, and M1-100 v1.1 was not
  available. Check before stage 3 pins MPE tests.
- **The MPE Profile, GM2 and other profiles** are out of the core set and unread.
