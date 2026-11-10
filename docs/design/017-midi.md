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
- **One event shape, then the shared steps.** Where MIDI stands today is in §1.4,
  the spec's interfaces as traits (mostly not) in §4.1, and the order the remaining
  stages run in is §10: `(Offset, Ump)` first, then the `Translator`, `dispatch`
  and the dedup.
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
host still reaches `midi2` through `tutti-midi-types`' `PerNoteBendRanges::observe`.
`tutti-graph` reads through
`Ump::is_note_off` (one test kernel takes a data byte by hand); `clip_node`,
`clip_file` and `tutti-midi-types`' `wire.rs` (`to_midi1_bytes` and `to_midi1_messages`,
which the VST2 host still calls) shift bits by hand. `MidiEvent::frame_offset`
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

### 1.4 Where MIDI stands

A survey of `main` after stages 1, 1.5 and 2 (the CLAP host, the VST3 host, the
polysynth and the SoundFont read `Message`). What is left is mostly duplication
and a vocabulary that is half old.

**Three event shapes coexist.**

- `tutti_graph::Event { offset, kind: EventKind::Midi(Ump) }`
  (`tutti-graph/src/event.rs`): the target.
- The legacy `tutti_midi_types::MidiEvent { frame_offset, data: [u32; 4] }`
  (`tutti-midi-types/src/ump/mod.rs:37`). It is still the currency of the
  `MidiIn` and `MidiOut` traits, the plugin hosts (`tutti-plugin-types`
  re-exports it, and its `ProcessContext` and `emit_midi` use it),
  `MidiSession::send`, the SoundFont's `apply_event(&MidiEvent)`,
  `BlockSplitter` and `gather_midi`. It appears in about 40 files of
  `tutti-midi-types`, 30 of `tutti-midi-runtime` and 18 of `tutti-plugin`.
- Bare `Ump` or `Message` (`PolySynth::process_midi_event`, `Voicing::step`).

**Stage 3's `Translator` is not built.** The old `Midi1ToMidi2Translator`
(`tutti-midi-types/src/translation/rpn.rs:81`) is what `MidiInputNode` runs, and it
speaks `MidiEvent`.

**Duplication.**

- *Widening.* `fn widened` is copied at `tutti-clap-host/src/events.rs:123` and
  `tutti-vst3-host/src/events/midi.rs:356`. The same widen is written in
  `PolySynth::voice1` (`tutti-polysynth/src/polysynth.rs:373`) and
  `SoundFontUnit::voice1` (`tutti-soundfont/src/lib.rs:366`). Three more calls to
  `widen` sit in the VST3 host (`legacy_cc.rs`, `midi_mapping.rs`) and the CLAP
  host's `from_voice2` path.
- *Ignoring non-voice messages.* A match with seven arms (the six non-voice
  `Message` variants and `Err(Unknown)`) is the same in
  `PolySynth::process_midi_event` (`polysynth.rs:~351-369`) and
  `SoundFontUnit::apply_event` (`tutti-soundfont/src/lib.rs:~344-364`).
- *RPN and NRPN, four places.* The old translator; the SoundFont's
  `Channel::{set_rpn_*, data_entry_*}` (`tutti-soundfont/src/channel.rs:149-190`),
  which ignores NRPN (it notes that the last data type was NRPN and drops the
  data); `RpnRun` in `tutti-midi-runtime`'s `outbound/mpe_ingest.rs:90`; and
  `Voicing`'s consumption of the `Voice2` registered-controller form, which is the
  target.
- *MIDI 1.0 byte parsers.* `tutti_ump::Midi1Parser` against `tutti-plugin-types`'
  `Midi1Splitter` and `Midi1Decoder` (about 700 lines), whose one production caller
  is `tutti-au-ffi/src/midi_out.rs`.
- *SysEx.* `tutti-plugin-types`' `SysExAssembler` (about 790 lines, packets to
  whole messages for a plugin) and `tutti-midi-runtime`'s `Sysex7PacketReassembler`
  (packets to the complete run) do overlapping work. `Sysex8PacketReassembler` and
  the FlexText reassembly sit beside them.

**Held-note structures answer different questions and stay separate.**
`tutti-voicing`'s `Holds` is why one voice is still open (key, sustain,
sostenuto). `NoteLedger` (§8.4) is which notes a node has started and not ended on
its own port, and what the port owes. `tutti-graph`'s `held.rs` is which keys an
event output port carries, so an edge that ends can close them. They have
different owners, keys and failure modes; unifying them would give one of them a
question it does not ask.

**`tutti-midi-types` is still about 12,000 lines of pre-017 vocabulary:** `ci/`,
`mpe/`, `translation/`, `clip_file`, `cc/mapping`, `sync/`, and the `MidiIn` and
`MidiOut` traits, beside the `MidiEvent` above.

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

MIDI 1.0 on the wire is `tutti-ump`'s too, so every status byte and bit position is
in one crate:

```rust
// tutti-ump
impl Voice1 { pub fn bytes(self, channel: MidiChannel) -> Midi1Bytes }  // also System, Bank
pub struct Midi1Bytes;                       // padded() for a fixed 3-byte field, len(), as_slice()
pub fn frame<B: Copy + Into<u8>>(payload: &[B]) -> Framed<'_, B>;      // F0 .. F7, ExactSizeIterator
impl Sysex7Packet { pub fn framed(&self) -> Option<Framed<'_, U7>> }    // Complete only

pub struct Midi1Parser;                      // 9 bytes of state, no allocation
impl Midi1Parser {
    pub const fn new(group: MidiGroup) -> Self;   // MIDI 1.0 has no group: the port's
    pub fn feed(&mut self, byte: u8) -> Option<Message>;
    pub fn feed_all<'a>(&'a mut self, bytes: &'a [u8]) -> impl Iterator<Item = Message> + 'a;
    pub fn end_of_message(&mut self);        // packet transports: ends a short message, keeps SysEx
    pub fn message(group: MidiGroup, bytes: &[u8]) -> Option<Message>;  // one message at the start of a fixed field
}
```

The parser is per byte, so a state that survives a buffer boundary is the same
struct. Running status survives data and real-time bytes and ends at any other status
byte; real-time bytes come out at once wherever they sit; SysEx comes out as
`Sysex7Packet`s as its bytes arrive, and a status byte other than real-time or `0xF7`
ends it with no `End` packet (M2-104 §7.7.1), so the receiver of the packets drops
the open run. A Note On at velocity 0 stays a Note On; reading it as a Note Off is
translation's. `message` exists because a fixed field cannot be fed to a parser:
`[0xC0, 5, 0]` would read the padding as running-status data. SMF does not share the
parser: it has delta times, a running status that meta and SysEx events cancel, no
real-time bytes and length-prefixed SysEx.

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
  they are checked where strings are reassembled. Nothing checks them today: the
  `endpoint_name`, `function_block_name` and `product_instance_id` constructors
  in `tutti-midi-types` and `EndpointNegotiator::with_names` (and
  `FunctionBlock::name`) in `tutti-midi-runtime` take any `&str` or `String`, and
  the `midi2` crate adds no limit. The limits, from M2-104 v1.1.2 §7.1:
  - **Endpoint Name:** at most 98 bytes of UTF-8 (seven packets).
  - **Function Block Name:** at most 91 bytes (seven packets). Version 1.1.2
    (§7.1.9) lowered it from 98, so a name built to the old figure is now too long.
  - **Product Instance ID:** at most 42 bytes, every byte ASCII 32 to 126
    (§7.1.5), so a non-ASCII or control byte is refused, not passed through.

  A name is a checked value built on the control side (`new -> Result`, never a
  truncation, as in the range-checked table of CLAUDE.md), so a sender cannot
  emit an over-long name. A receiver caps what it accumulates at the same limit
  and drops a name that exceeds it (`EndpointInquiry` has no cap today). Each
  limit gets three boundary tests, at the limit, one byte over and a long
  input, each asserting the packet count (7, 7, 3) and the refusal; the
  Product Instance ID also gets a byte below 32, 127 and a multi-byte
  character. Mutation: raise a limit by one and the one-over test must fail.

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

### 4.1 The spec's interfaces as traits

The question was whether MIDI should get traits that follow the specification's
interfaces. Most should not, because the pure-step shape already used here (a value
in, a value out, no I/O) covers them with less surface. The decisions:

- **Channel-voice receiver: a function first.** `tutti-midi-protocol` gets
  `dispatch(ump, handler)`: it decodes, widens a MIDI 1.0 message, and routes the
  `Voice2` to a channel-voice handler. The default handling ignores every
  non-voice message and `Unknown`, which is the match two crates copy today (§1.4).
  It becomes a `VoiceReceiver` trait only when a third crate implements it; two
  implementors do not justify a trait the function already serves. Boundary: a
  `VoiceReceiver` is what turns one channel voice message into a change in an
  instrument, and nothing else. Oracle: M2-104 App. D.3 (a MIDI 1.0 Note On at
  velocity 0 arrives as a Note Off; a MIDI 2.0 Note On at velocity 0 stays a Note
  On).
- **Controller state (RPN, NRPN, bank, data entry) is a value, not a trait.** It is
  the stage 3 `Translator` (§4), one value per input edge. Oracle: M2-115 for the
  scaling and M2-104 App. D for the assembly.
- **The wire edge consolidates onto `Midi1Parser`.** No trait: once the AU copy is
  gone one implementation remains.
- **Reassembly: a `Reassemble` trait is revisited after stage 5a and the dedup,**
  when every reassembler takes `Ump` and none allocates. Before that the shapes
  differ (`MidiEvent` against `Ump`, allocating against not) and a trait would
  cover the difference instead of ending it.

Rejected:

- **An Endpoint trait.** `MidiEndpoints` and `MidiIn`/`MidiOut` already are it, and
  Function Blocks are data (§5).
- **Profile.** Nothing implements it, and the unwired profile code is slated for
  deletion (§5, "MIDI-CI").
- **Property Exchange.** Deferred until a named peer and M2-105 (§5).
- **MIDI-CI.** A pure step, `Connection::on_message` (§5), not an object to
  implement.
- **Timing.** Values converted at the edges (§2, "Timing at the edge").
- **SMF and Clip.** They differ in kind (ticks and meta events against a word
  stream); both convert to `(Beat, Ump)` (§9, "Files").
- **A note-logic trait per instrument.** `Voicing::step` with `Rules` and `Patch`
  is already the values form (`tutti-voicing`).

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
let devices = Devices::open()?;            // starts watching
let all: Vec<DeviceInfo> = devices.list();
let events = devices.subscribe();          // its own queue; events.initial() is the list then
for event in events.try_iter() {           // DeviceEvent::{Added, Removed, Changed}
    /* .. */
}
let next = events.recv_timeout(timeout);   // an event, Timeout, or Ended(WatchFailure)
let input = devices.open_input(&id, &ports)?;   // OpenInput: closes its port on drop

// tutti-midi-device: the pure core
impl DeviceTable {
    pub fn apply(&mut self, change: OsChange) -> Option<DeviceEvent>;
    pub fn resolve(&self, saved: &DeviceId) -> Resolved<'_>;
}
```

- `Devices::open()` starts the watch; it stops when the last `Devices` and
  `DeviceEvents` over it are dropped. Each `subscribe()` has a queue of its own,
  dropped with it, so an app that wants no events keeps none and one reader never
  takes another's. `recv_timeout` returns `Ended` once a dead watch's queued
  events are taken.
- `DeviceTable` is pure. It takes an `OsChange` from the backend and returns the
  event, if the change alters the table. A repeat of a known state returns `None`.
- `DeviceId` is stable across a restart where the OS gives something to hold; an
  ordinal tells alike devices apart within a run ("Built", below). With the
  `serde` feature of `tutti-midi-device` it serializes, and `DeviceTable::resolve`
  restores it from the saved `DeviceInfo` (`Devices::resolve`, reached through
  `MidiSession::devices`). Only `Exact` is safe to bind unasked, and what an
  identity can promise differs by backend: a CoreMIDI unique id names one
  endpoint and a Product Instance Id one unit (its ports share the id, so the
  saved name and direction pick the port): both `Exact`. An ALSA identity is the
  client and port name, so another keyboard of the same model answers to it: a
  lone present device with the saved name and a saved ordinal of 0 is
  `SameName`. A saved ordinal above 0, or several alike devices, is `Ambiguous`
  with the candidates; no device with the identity is `Missing`. A saved `#0`
  cannot tell a device that was once one of two from a one-of-a-kind one, which
  is why ALSA never answers `Exact`.

  ```rust
  // Restoring a remembered keyboard.
  let saved: DeviceInfo = serde_json::from_str(&settings.keyboard)?;
  match session.devices().resolve(&saved) {
      Resolved::Exact(info) => open(info),
      Resolved::SameName(info) => open_or_confirm(info),
      Resolved::Ambiguous(candidates) => ask_the_user(candidates),
      Resolved::Missing => wait_for_added(&saved),   // an Added event, then resolve again
  }
  ```
- `FakeBackend` has `plug()` and `unplug()`, so a test drives hot-plug with no
  device.

**Built** (stage 4, first part). `tutti-midi-device` is a new crate that holds
`DeviceTable`, `OsChange`, `OsDevice`, `DeviceEvent`, `DeviceInfo`, `DeviceId` and
`EndpointId` and nothing else; it has no dependency yet (the stage's `Endpoint`
and `Connection` will add `tutti-ump`). `Devices` is in `tutti-midi-hardware`,
over a `Watch` the backend returns from `MidiEndpoints::watch`.

- **Linux.** A thread subscribes to `System:Announce` and blocks in `poll` on the
  sequencer's descriptors and a stop descriptor. On an announcement it
  enumerates again, on its own client, and reports the difference. It reads from
  an event only which client or port exited. Enumerating on a client of its own
  matters: a client opened to enumerate announces its start, port and exit to the
  watch, which then wakes itself without end (measured before the fix: about
  20,000 wakes a second, and the same flood to every other subscriber).
  `MidiEndpoints::inputs` and `outputs` use one enumerating client held for the
  backend's life, for the same reason. The engine's own pump and output ports are
  not subscribable by others (no `SUBS_*` capability), so no process lists them as
  devices. Dropping `Devices` closes the stop descriptor and joins the thread.
  Tested against the real sequencer with user-space clients as devices (suite
  `sequencer-tests`). The watch client is a UMP client like the enumerating one,
  so it needs the same kernel.
- **macOS.** One process-wide CoreMIDI client with a notification block, on one
  thread that runs its run loop for the life of the process; each `Devices` is a
  registration with it, removed on drop. (coremidi 0.9.2's `Client` never
  disposes on drop, so a client per `Devices` would leak one.) Measured on
  GitHub's macOS runner: CoreMIDI sends notifications to the run loop that was
  current when the process created its *first* client, whichever thread creates
  later ones. With the hub's client first, its callback fired on the hub thread
  for every endpoint added or removed in the process; with the engine's shared
  client created first on another thread, it never fired in four seconds and the
  hub's loop had no source (`run_current` returned at once), and the callback did
  fire on that other thread when it ran its own loop. So both places this backend
  creates a client (`shared_client` and the raw client of `UmpVirtualSource`) call
  `hub_first`, which starts the hub, before taking their own locks. Nothing the
  hub thread runs may reach those two (its enumeration builds no client), or
  startup would wait on itself. A host that creates a CoreMIDI client
  earlier still takes the notifications away, and a hub whose loop has nothing to
  wait for also looks again once a second (a hot-plug is then found in about a
  second); the same look backs up a missed notification. A run loop that
  returns at once is rested four times a second, not spun. The hub never holds
  its lock across a CoreMIDI call (with one held across a call, the macOS tests
  hung); a watch's own lock is held across its enumeration, so a `Devices` drop
  can wait for one. A panic in a watch's enumeration is caught, fails that watch
  (`DeviceEvents::failure`, as on Linux) and leaves the others; a refused client fails
  the watch the same way and is retried by the next. Compiled and linted for
  `aarch64-apple-darwin` from Linux; its tests run only on macOS CI (one asserts
  the notification callback fired, one that `Devices` sees a virtual endpoint
  come and go), and the author has not run any of this on a Mac.
- **Windows,** and Linux without alsa-lib 1.2.10: the stub lists nothing and
  watches nothing, as it enumerates nothing. Opening reports `Unsupported`.
- **`DeviceId`** is an `Identity` and an ordinal. `Identity` is the Product
  Instance Id, CoreMIDI's `MIDIUniqueID`, or the ALSA client and port name. No
  backend produces a Product Instance Id yet: alsa-lib 1.2.16 has no getter for
  one and CoreMIDI offers its unique id. The ordinal is the lowest one no listed
  device with that identity holds, so two identical keyboards are `#0` and `#1`
  and a freed ordinal comes back.
  **An ordinal names a device within a run, not a physical unit.** Unplug A (`#0`)
  and B (`#1`) and plug B back first, and B is `#0`; seeding follows the OS's
  order, which can change between boots. An id whose identity no other device
  shares (`#0` of a one-of-a-kind name, a CoreMIDI unique id) names the same
  device again; an ordinal above 0 does not, and a saved one can reach another
  unit.
- **Rename.** Where the identity is the name (ALSA), a renamed device is a
  different device: `Removed`, then `Added` under the new name, and a port held
  on the old one is closed. A handle that stays under another name is not
  taken for the same device, because the kernel gives a number to a new client
  as readily as it keeps one for a renamed client, and a dropped announcement
  would otherwise leave a dead subscription listed as live under a stale name.
  `Changed` is for a backend whose identity survives a rename (CoreMIDI's unique
  id) and for a direction change. A client that makes its port before it names
  itself is listed once under `Client-N` and then removed and added under its
  name.
- **A reused handle is a new device.** ALSA gives a new client the number of one
  that just exited, so two enumerations can see one handle for two devices. A
  different identity on a handle is enough to tell. For an identical device the
  announcements are the only evidence: they say which client or port exited,
  and the `Reporter` sends `Gone` for such a handle before the `Present` of the
  new one, so a held input is closed. Limits: an exit announcement the kernel
  dropped (an overrun is read past, and the enumeration after it still runs)
  goes unseen when an identical device then takes the number, which leaves a dead
  subscription listed as connected. The reverse also happens: the kernel can
  announce the exit of a number's previous owner after a new client has taken it,
  and the new device is then reported `Removed` and `Added` with the same id
  (a held input on it is closed). Both need a number reused within microseconds;
  the second showed up only when tests started and stopped clients in parallel.
  Opening reads the handle after looking it up, not under the list's lock, so a
  handle reused in between opens the new device; the `Removed` that follows is
  how a holder finds out.
- **What CI runs.** None of the ALSA watch runs in CI: the runner has no
  `snd-seq` (see CLAUDE.md), so the real-sequencer suite, and with it the
  announcement parsing, `Departed` and the `catch_unwind` around the watch
  thread, run only on a developer's machine. CI runs the `Reporter` and
  `Devices` tests over the fake backend and compiles and lints the rest.
- **A watch that dies says so.** An error or a panic in the watch thread sets
  the subscribers' `DeviceEvents::failure` and `recv_timeout` (and is logged); the list is then the last it saw.
  The session reports `SessionEvent::MidiWatchFailed` once, and `bevy-tutti`
  logs it.
- **Session.** `Phase::Poll` reads the session's `DeviceEvents`, reports each event as
  `SessionEvent::MidiDevice`, and closes a held input or output whose device was
  removed, reporting `MidiInputLost` or `MidiOutputLost` after it
  (`SessionBuilder::midi_hardware_over` takes a `FakeBackend`). `bevy-tutti` writes
  `MidiHotplug` for each device event and clears its poll timer for a lost port.

Deviations from the sketch above:

- `open_input` takes the ring manager: `open_input(&id, &ports)`. Input events
  reach the graph through a ring of `ports`; the returned `OpenInput` holds the
  connection and the ring's port and closes both on drop. A removed device does
  not close it: the holder does (the session, in `Phase::Poll`).
- `MidiSession` keys its connections by `DeviceId`, and its `inputs` and `outputs`
  return `DeviceInfo`; `Devices::capability(&id)` answers what `EndpointInfo`
  carried.
- The stage-1 note says `tutti-midi-device` depends on `tutti-ump`; this PR's
  crate does not.

Not built: the flush of held notes when a device is lost (§8.3). Nothing at the
input edge tracks held notes yet. The retired port discards the UMPs queued in it
at removal, and releases nothing: a device removed while a note is held leaves it
sounding until the node that tracks notes exists.

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
no inbound SysEx reaches them (`bevy-tutti`'s `negotiation.rs` says its inbound
message types have no producer, and `CiRes` is inserted by no plugin). They also
miss current requirements: no ACK, no Invalidate MUID response (M2-101 App. F),
profile bodies without Message Version 2 fields (18 bytes against 20), and
Property Exchange built for v1.1.

Keep the header, MUID, Discovery, NAK and Invalidate codec and the Stream
constructors, about 400 lines, moved to `tutti-midi-device`. Delete the state
machines and profile and property bodies. Rebuild the App. F minimum with a real
inbound path, in one change, when a stage needs it (an estimated 650 to 750
lines). Property Exchange waits for a named peer and for M2-105, which is not in
the core set.

The rebuild is done when each requirement below holds and its test fails with the
requirement broken. Section numbers are M2-101 v1.2.1 unless named. Every test
runs on the pure core with time as a value, and the inbound path gets one more
that sends SysEx7 bytes through the whole route.

1. **MUID generation and regeneration** (§3.3). A device draws a new random MUID
   from an OS random source each time it starts, never from a caller's seed
   (`Muid::from_seed` and `CiRes::new`'s `muid_seed` go). The reserved range
   `0x0FFFFF00` to `0x0FFFFFFE` and the broadcast value are never drawn. One MUID
   serves a Function Block in every role, so the responder and the initiator
   share it (`CiRes` builds two with different seeds today). On Invalidate MUID
   addressed to its own MUID, a device draws a different one and announces the
   change. Tests: a draw loop with an injected source that returns reserved
   values, asserting none is kept; two starts give different MUIDs; the
   Invalidate response differs from the old MUID; the responder and the
   initiator report one MUID.
2. **Collisions and echo detection** (§5.9, §5.9.1, App. A.1.1). A Discovery whose
   source MUID equals ours is a collision, resolved by one of the two options of
   §5.9.1; a message that is our own transmission looped back is not, and the
   two are told apart by the Product Instance ID in the Discovery body. The
   responder examines the source of every inbound Discovery (it reads only the
   source for the reply today), and the initiator no longer treats any reply
   carrying its MUID as a collision. Tests: each option; a looped-back
   Discovery with our own ID causes no Invalidate; a foreign device with the
   same MUID does. Initiator collision (two replies with one MUID, §5.9.2) too.
3. **The destination-MUID filter** (§3.3.3). A device acts on a message whose
   destination is its MUID, or the broadcast MUID for the message types that
   allow a broadcast, and ignores every other (`respond_to` answers a Discovery
   addressed to another device today). Tests: a Discovery, Profile Inquiry and
   Property request addressed elsewhere yield no reply; a non-broadcast message
   sent to broadcast yields none.
4. **Discovery collects for at least 3 s** (§5.5.5). The initiator takes a clock
   as a value, keeps every Discovery Reply that arrives in the window, and
   reports the set when the window closes (`CiInitiator` keeps only the last
   reply). Tests: two devices reply at 0.5 s and 2.9 s, both kept; the set is
   not reported at 2.9 s and is at 3.0 s; a reply after the window is ignored.
5. **Per-group operation** (§3.2.2). A reply goes out on the group of the
   inquiry that prompted it, and Discovery goes to each group of the endpoint
   (`CI_GROUP` is `MidiGroup::FIRST` in `bevy-tutti`'s `negotiation.rs` for every
   outbound message, so a Function Block on another group never sees it). The
   inbound path reassembles SysEx7 per group. Tests: an inquiry on group 5 is
   answered on group 5; Discovery on an endpoint with blocks on groups 0 and 3
   is sent to both; two interleaved reassemblies on different groups both
   complete.
6. **The v2 profile bodies** (§7, M2-102). Set Profile On carries the number of
   channels requested; Set Profile Off carries two reserved bytes; Profile
   Enabled and Profile Disabled carry the number of channels enabled or
   disabled. `ProfileState::{SetOn, SetOff, Enabled, Disabled}` encode only the
   five-byte profile id today (18 payload bytes, 20 required), and the doc on
   `CI_VERSION` says every v2 field is carried, which is false for these four.
   Tests: byte-exact encoding of each against the tables, a round trip, and a
   v1-format body still decoding with the channel count defaulted. The profile
   state also moves to `&mut self` so a Set On changes what the next Profile
   Inquiry lists.
7. **ACK and NAK** (§5.10, §5.11). ACK (sub-ID#2 `0x7D`) is encoded and decoded
   (`CiMessage::decode` returns `None` for it), with its status values for
   Timeout Wait and next-chunk flow control. NAK gains the statuses `Nak` has no
   constant for: `0x12` (resend the last chunk), `0x20` (terminate, with
   status data telling who terminates), `0x21` (chunks out of sequence) and
   `0x43` (busy, in 100 ms units); decoding already passes any code through.
   Tests: a round trip of each, and an ACK payload decoding.
8. **Function Block Info** (M2-104 §7.1.8). The notification sets the MIDI 1.0
   flag, the UI hint, the MIDI-CI Message Version (0 means none or unknown) and the
   maximum number of SysEx8 streams, so a block that `CiResponder` drives
   advertises that it speaks MIDI-CI. `MidiEvent::function_block_info` sets
   active, number, first group, group count and direction only. Tests: the
   encoded words for a CI v2 block with two SysEx8 streams.
9. **Property Exchange roles and the version gap** (§4.2, §8; M2-103). Roles are
   per transaction, not per device: a device that subscribed to a peer's
   resource is the initiator of that subscription and replies to its updates
   (the responder NAKs a peer's `full` update today, and `CiInitiator` has no
   Property Exchange). The Common Rules figure in the Capabilities messages is
   M2-101 Table 31's (1.0 and 1.1), while M2-103 is at 1.2 with flow control
   over ACK and NAK in place of Notify; the rebuild names the version it
   speaks, tests both messages against it, and treats a received Notify as
   M2-103 requires. Property Exchange does not ship without a named peer, so this
   item is a requirement on the day it does, not on the first rebuild.

Stale spec citations in the CI code. Stage 4 fixes each when it moves the code
(the corrected numbers are those of M2-101 v1.2.1; a citation that is already right
is not listed):

- `ci/discovery.rs`: the module title and the Discovery constants cite §5.2/5.5/5.6,
  they are §5.5 (Discovery, the category bitmap in §5.5.2), §5.9 (Invalidate MUID)
  and §5.11 (NAK; the `Nak` doc already says §5.11.1);
- `ci/mod.rs`: the Sub-ID#1, MUID, broadcast and preamble docs cite §5.1, they are
  §5.2.1 and §3.3/§3.3.3; the `CI_VERSION` doc cites §5.2 for the version number
  and §5.4 for the receiver rule, both are §5.3; the Discovery, Invalidate and
  NAK variant docs cite §5.2, §5.5 and §5.6 (§5.5, §5.9, §5.11); and the
  Property Exchange Capabilities variant docs cite §8.4/8.6 (inquiry §8.5);
- `ci/profile.rs`: the module and variant docs cite §6.x, they are §7.x (the Profile
  Id §7.3.1, Inquiry §7.2, Reply §7.3, Set On §7.8, Set Off §7.9, Enabled §7.10,
  Disabled §7.11);
- `ci/property.rs`: the module cites §7, §7.1.3 and §7.1.1, they are §8, §8.3
  (Tables 33 to 36) and §8.4; the Capabilities constants cite §8.4 for the
  inquiry (§8.5);
- `capability_inquiry.rs`: the Capabilities inquiry cites §8.4 (Request ID), it is
  §8.5 for the inquiry and §8.6 for the reply; the header JSON cites M2-103 §5,
  which is v1.1 numbering (v1.2: §6.1 and §7.1);
- `endpoint.rs`: `FunctionBlock` cites M2-104 §7.1.3 (Device Identity
  Notification), it is §7.1.8;
- `message.rs`: the assigned controller number cites M2-104 §7.4.5 (Per-Note
  Management), it is §7.4.4 and App. A;
- `tutti-ump`'s `midi_group.rs` says the Group field is in every UMP packet
  header; M2-104 §2.1.2 says Message Types 0x0 and 0xF have none;
- the `CI_GROUP` doc in `bevy-tutti`'s `negotiation.rs` says "function-block-wide",
  and it is group 0 only (item 5).

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
| `tutti-ump` | Packet | `Ump`, `Message`, codec, `MidiGroup`, `MidiChannel`, `CCNumber`, field types, MIDI 1.0 bytes and `Midi1Parser` | No tutti crate |
| `tutti-midi-protocol` | Translation | `Translator`, App. D, M2-115 scaling, MPE | `tutti-ump` |
| `tutti-midi-device` | Endpoint, Function Block | `Endpoint`, `Connection`, CI codec, JR | `tutti-ump` |
| `tutti-midi-hardware` | Transport | OS backends, no graph; `#![forbid(unsafe_code)]` once the `-ffi` crates exist (today the ALSA wrappers hold the `unsafe`) | `tutti-midi-device`, the two `-ffi` crates |
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
- It is removed from the workspace at stage 5b, with the old types. Until then, while
  the old types still depend on it anyway, it serves as a test-only oracle: random
  words go through both decoders, and each disagreement is settled by reading the
  specification and pinned as an analytic test. What survives its removal is what
  those tests name: a swap of two neighbouring numbers in a number-to-name table, or
  of two fields or flags, fails a spec-literal test in `tests/spec_words.rs` or
  `tests/spec_tables.rs` with `tests/midi2_oracle.rs` deleted. The round-trip
  properties and the fuzz target check the codec only against itself (decode,
  encode, decode), so a mistake made the same way in both directions passes them;
  they do not replace the oracle. What stage 5b does lose is independent agreement
  with a second decoder on words no table or figure pins, and the name-only
  comparison of the Flex and Stream families. The fuzz target is not run in CI
  yet; nothing here leans on it.

`midly` stays in `tutti-midi-file`. `tutti-midi-types` no longer parses with it:
`MidiEvent::from_midi1_bytes` goes through `tutti_ump::Midi1Parser::message`, and only
the re-export is left. The `midly` parse error allocates, so a plugin's stray status
byte (`[0xF7, 1, 2]`) aborted a debug build under `assert_no_alloc`.

## 8. Not losing events

### 8.1 What stage 0 fixed

A dense clip into a slow reader lost events at a fixed block capacity
(`CLIP_EVENT_CAPACITY`, 256). Measured at 32,000 events per second: 25% lost at
512 frames, 62% at 1,024. A real `render_to_buffers` export (1,024-frame blocks)
returned `Ok` while losing 62%, and a chord clip left 144 of 200 notes sounding to
the end of the file. The executor's drop counter reported about half of the loss,
because a refused note-on's later note-off is swallowed uncounted. A refused
note-off was not retried on ordinary playback (`owed` was set only in `release`).
Stage 0 (#261) fixed both: a withheld note-off is counted and a refused one goes
out at a later block's first frame.

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

### 8.4 The note ledger

`NoteLedger<T>` (`tutti-midi-runtime`) is the table of notes a node has started and
not ended, and the only way such a node puts a MIDI event on its port.
`NoteLedger::block(out, first)` pays every note-off the port refused earlier,
at `first`, and returns a `Sending`; `Sending::send(at, ump, owner)` is the only
door to the port, and `release_all(at)` ends every note. A node that writes
around the `Sending` does not compile (a `compile_fail` doctest on the module
holds the second borrow of the port).

- **The key is (group, channel, note).** M2-104 §7.4 and Appendix C name a note by
  that triple and give no other identity. `T` is an optional owner tag for a source
  that pairs its own messages; the clip's is `ClipNote`, a relay's would be `()`.
- **One held note per key.** A note-on of a key that sounds ends the sounding note
  first on the same frame and takes the key, as the clip did before. Appendix C.2
  allows several notes on one key, told apart by a Detach, which the clip does not
  carry, so the ledger does not model it.
- **Two states per held key,** `Sounding` and `Owing` (the port refused the off).
  Independently a key can be `refused` (the port refused a note-on of it), so that
  note's off is withheld and counted. Both can hold at once.
- **Full table.** The clip's table holds 2,048 keys, one group's 16 channels of 128
  notes; it is a chosen bound, not a measured one. A note-on that finds it full is
  withheld and counted as a refusal is (`EventWriter::withhold`, in
  `Executor::dropped_event_count` and `Editor::take_event_losses` against the
  clip's port). The note does not start, so no off is owed; its tagged off is
  swallowed and not counted again. A note whose on went out always has its row.
  An untagged source's off for a withheld note passes through to the port.
- **A refused on is counted at the on and again at its off.** The off of a note
  whose on the port refused is withheld and counted, tagged or not, and clears the
  row, so a note lost entire is two losses (the 44 + 44 of
  `a_loss_is_counted_exactly_and_leaves_no_orphan`). An off the port refused is a
  loss that is given back when the off goes out. A key counts its refused ons (to
  255), so notes that overlap on a key are counted one by one. A relay's table is therefore
  freed by its sender's offs without a `release_all`.
- **Memory and cost.** 12 bytes a key with the clip's tag, 24 KiB per clip node,
  allocated with the node. Per 512-frame block, against the bitsets it replaced
  (six alternated release runs): a dense clip 4.76 to 5.86 µs, a chord churn over
  128 held notes 0.31 to 0.41 µs, 1,500 held notes with 4 changing a block 0.39 to
  1.06 µs. The table is a sorted `Vec`, so a change to a large table is a
  `memmove`.
- **Fork and reset.** The table is not `Clone`; a fork builds a fresh node over a
  snapshot of the clip and starts with an empty table, and `Node::reset` clears it.
- **The clip node uses it; the relays do not yet.** The input and queue nodes, a
  plugin's MIDI output and `MpeIngest::set_mode` still send without a ledger, so
  audit #14 still holds for them (a note-off lost at the reserve, 8 stuck notes in
  the scenario that measured it) until stage 4 moves them.
- **Audit #7 is closed at the executor's edges, not in the ledger.** A debt that
  crosses an edge change cannot be paid by a value inside its source, so the
  executor keeps the keys each note-carrying event output port holds (`tutti-graph`'s
  `held.rs`: a bitset over `tutti_ump::Key`, plus the sustain pedals down) and, when
  a commit removes an edge, sends the sink that loses it the pedal-ups and
  note-offs, after the events a vanished delay flushed to it. It needs no node's
  cooperation, so it covers a clip, a relay and a plugin's MIDI output alike. A
  replaced or removed source closes its sinks too. A node whose event outputs never
  carry a note says so with `Shape::with_no_note_events`, and keeps no table.
  The ledger still owes its own port the offs it refused; the edge table owes the
  sink the offs of what crossed the edge.
- **What the close does not do.** An off for a key the sink never saw begin still
  reaches it (a conforming receiver ignores it). A sink fed by several sources cannot
  tell whose note a key is, so one source's close or off ends another's note on the
  same key. So can a close still owed after a replacement: past about 72 held
  keys (what a block's slot takes) it can end a note the new unit started on the
  same key. A fork starts with empty tables and closes what it has seen itself.
  Sostenuto, CC 120 and 123, per-note controllers and pitch bend are not tracked.

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

Smaller items that no stage owns:

- **No clock while stopped.** `ClockMaster` returns from its tick as soon as the
  transport is not playing (`tutti-midi-runtime/src/outbound/clock_master.rs`), so
  external gear that wants a running clock through Stop (arpeggiators, delays)
  gets none. Make it a setting of the master, off by default, with a test.
- **No headless live example.** `crates/tutti/examples` has no live-keyboard to
  plugin example (`headless_engine`, `headless_export`, `headless_modulation`
  and `plugin_midi_export` do not read a device). Add one when the live path is
  settled, as the doc for the device API.
- **The event sort.** `in_offset_order` in `tutti-midi-runtime/src/live/order.rs` is
  an insertion sort, O(n^2) for input out of order (57 us at 512 events, as measured
  in the audit). It never allocates, and a wire or ring is usually in order, so this
  is low priority; revisit with the `(Offset, Ump)` event of stage 5a.

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
  meta events. Adopting it in the transport is separate work: [020](020-tempo-map-transport.md)
  is that design, and it points `MidiFile::seconds_at` at the same `TempoMap`.
- **Integer ticks.** Events keep their tick and the track its division, so a write is
  exact with no rounding, and `events()` gives callers `Beat`.
- **The Clip File reader checks structure order.** `parse_clip` in
  `tutti-midi-types` records that a Start of Clip and an End of Clip were seen
  but not where: a file whose DCTPQ is missing from the front or repeated, whose
  events precede Start of Clip, or whose End of Clip is not last (data after it is
  already refused) still reads. M2-116 requires the DCTPQ first and once, Start
  of Clip before any event, and End of Clip last. One error variant per rule, with
  a test for each.
- **A lossless round trip** is the test: a file read and written holds the same
  events, including SysEx and meta. Running status and event spelling may differ.
- **Clip File moves here** from `tutti-midi-types`, its only production consumer.
- Measure `midly`'s `parallel` feature, which pulls in `rayon`, and check its `strict`
  feature for truncated files.

## 10. Stages

Each stage lands on its own, with renders bit-identical except where a stage names
a behaviour change. Stages 0 to 2 are done except the VST2 host's move to `Message` (stage 2); the order of the
rest is below.

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
   stays a plain Control Change there. A consumer's call is transitional: stages 3a
   and 3b move it to the input edge and into `dispatch`, and consumers drop it with
   their `Voice1` arm. Behaviour change: a MIDI 1.0 Note On at velocity 0 reaches the consumer
   as a Note Off with release velocity `0x8000`, not `0`.
2. **Consumers move to `Message`,** one per PR: polysynth, SoundFont, then each
   host (the CLAP host is done; it cannot forward `Unknown`, because it sends no
   `CLAP_EVENT_MIDI2`).
   A consumer with MIDI 1.0 resolution only (SoundFont, the hosts) narrows with
   `tutti_midi_protocol::{narrow_velocity, narrow_value, narrow_bend}` (M2-115
   section 3.4, a right shift), or `narrow(Voice2) -> Option<Voice1>` for a whole
   message, which keeps the Note On velocity 0 to 1 rule (§7.4.2). A host that sends
   MIDI 1.0 bytes writes them with `Voice1::bytes`, `System::bytes` and
   `tutti_midi_protocol::midi1(Message)` (a Program Change with a bank is three
   messages), and reads a fixed field with `Midi1Parser::message`; the CLAP and
   VST3 hosts do. `midi1` sends bytes for a Note On or Note Off with a reserved
   attribute type (MIDI 1.0 has no attribute; Table 8 gives a receiver no rule for
   it), where `to_midi1_messages` drops it. Its only caller, the VST2 host, still
   does until it moves to `Message`; no shipped path changes.
   Residue that belongs to this stage, each a consumer dropping part of a message:
   - **SoundFont, Program Change bank.** `Player` handles `Voice2::ProgramChange`
     with `..` and so ignores the bank of a MIDI 2.0 Program Change (the bank-valid
     flag and its two bytes); only a Bank Select Control Change reaches the channel.
     Select the bank when the flag is set (`tutti-soundfont/src/player.rs`).
   - **AU host, Program Change bank.** `tutti-au-host/src/instance.rs` writes a
     Program Change as the program byte alone and drops the bank, so an AU
     instrument never changes bank from a MIDI 2.0 message. Send the bank as Bank
     Select first, as `tutti_midi_protocol::midi1` does (plan item 15).
   - **CLAP host, per-note registered controllers.** The `Voice2::RegisteredPerNote`
     arm in `tutti-clap-host/src/events.rs` maps only Volume, Pan and Brightness
     to a note expression and returns `None` for the other registered controllers
     (Modulation, Breath, Pitch and the rest), so they never reach the plugin. Map
     each to the nearest CLAP note-expression type, or say in the arm why one has
     none (doc 023 §5.4).

Stages 3 to 5 run in the order below, not the numeric one: 5a, 3a, 3b, 3c, then 5b
(stage 4 anywhere). Three reasons, all from
the code. `MidiEvent` is the signature everything else is written against (§1.4), so
changing it once, first, stops later stages from rewriting a signature twice. The
`Translator` is then written over `Ump` and never over `MidiEvent`, and the old
translator, which speaks `MidiEvent`, is deleted rather than ported. The dedup is
not cheaper first: the AU decoder and the SysEx reassemblers both produce and take
`MidiEvent`, so moving them before 5a means converting them twice, and the AU side
cannot be exercised on Linux. Stage 4 does not depend on this order and can land
at any point; its CI codec move and `ci`/`negotiate` deletion shrink
`tutti-midi-types` the same way.

- **Stage 5a. One event shape.** Replace `MidiEvent` with `(Offset, Ump)` everywhere:
   the `MidiIn` and `MidiOut` element type, the plugin hosts and `tutti-plugin-types`,
   the SoundFont's `apply_event`, `BlockSplitter`, `gather_midi` and
   `MidiSession::send`. Deletes `MidiEvent` and its constructors. Oracle: none
   needed, since it is not a behaviour change: renders stay bit-identical, and the
   existing spec-pinned tests for each consumer must pass unchanged. Two
   constraints come with it. `Offset` is `tutti-graph`'s (`tutti-graph/src/time.rs`),
   and `tutti-midi-types` sits in the same layer as `tutti-graph`, so it cannot
   name it: `MidiIn` and `MidiOut` move to `tutti-midi-runtime`, which already
   has the graph (`MidiInputNode` is their consumer). `tutti-plugin-types`
   (a higher layer) takes the edge to `tutti-graph`. And `Offset` has no serde, and
   gains none: plugin IPC keeps carrying a raw `u32` frame offset and converts
   at the edge, because the types stop at IPC boundaries (CLAUDE.md, "Where the
   types stop"); a comment at the wire type says so.
- **Stage 3a. The `Translator`** (§4) in `tutti-midi-protocol`, over `Ump`. Deletes
   the SoundFont's RPN state machine (`Channel::{set_rpn_*, data_entry_*}`, which
   then reads a registered-controller `Voice2`, NRPN included), `mpe_ingest`'s
   `RpnRun` and the old `Midi1ToMidi2Translator`, and `MidiInputNode` runs the new
   one. Behaviour change: §4's remaining spec fixes. Oracle: M2-115 and M2-104
   App. D, as analytic tests. Residue the old translator
   (`tutti-midi-types`' `translation/rpn.rs`) has and the new one must not copy:
   - **Data Entry order.** A CC 6 on a selected parameter emits the Registered or
     Assignable Controller at once, with the LSB as last sent (0 if never), and a
     CC 38 emits it again, so a receiver first sees a value built on a stale LSB.
     App. D.3.3 gives the triggers: a CC 38, a later CC 6, or a new parameter
     select. Emit on those only, and test CC 6 alone, CC 6 then CC 38, and CC 38
     then CC 6.
   - **Parameter number halves.** An NRPN or RPN LSB select alone leaves the
     MSB at its default of 0 instead of keeping the one last selected on the
     channel; the test is a select of the LSB only after a full select.
- **Stage 3b. `dispatch`** (§4.1) in `tutti-midi-protocol`. Deletes the four widen
   copies (the two `widened` functions and the two `voice1` functions) and the
   seven-arm match in the polysynth and the SoundFont. Oracle: M2-104 App. D.3.
   Compare `tutti_graph::Io::sub_blocks` with `tutti_midi_runtime::BlockSplitter`
   here: both cut a block at its event offsets, and `dispatch` should call one.
   Keep the one with the tests, and delete the other with its callers moved.
- **Stage 3c. The MIDI dedup.** The AU host reads its bytes with
   `tutti_ump::Midi1Parser`, and `Midi1Decoder` and `Midi1Splitter` go. One SysEx7
   reassembler stays, in the layer that uses it, and the other goes
   (`SysExAssembler` or `Sysex7PacketReassembler`, whichever is left with no
   caller that the survivor cannot serve; choose in the PR from the callers).
   Oracle: M2-104 §7.7.1 for the SysEx7 packets and the byte-stream rules of
   MIDI 1.0 for the parser (M1-100 is not available, §13). The `Reassemble` trait of
   §4.1 is revisited here. Two further items for this stage (plan item 5):
   - **A checked SysEx type.** `tutti-plugin-types`' `sysex.rs` has `unframed` and
     `is_framed`, which accept a message with or without `F0` and `F7`. A
     `SysexMessage<'a>` newtype (checked `F0` to `F7`, every interior byte 7-bit)
     retires the leniency: a host that sends one builds it once and the check
     is the type's.
   - **Test copies.** `is_framed` is hand-coded, and five test helpers named
     `framed` rebuild the same `F0 .. F7` wrapper (`tutti-plugin-types`'
     `sysex.rs` and `midi1_decoder.rs`, `tutti-clap-host`'s `events.rs`,
     `tutti-vst2-ffi`'s `midi.rs`, `tutti-vst3-ffi`'s `event_list` tests). They
     go to `tutti_ump::frame`.
- **Stage 4. `tutti-midi-device` and the `-ffi` crates.** Device detection comes first
   (`Devices`, `DeviceTable`, `FakeBackend::plug`; §5), directly after stage 1:
   done, except the flush (§5, "Built").
   Then model, connection, identity, device loss, timing (§5), and the backends
   with no `unsafe` of their own.
   Behaviour change: real hot-plug, timestamps, output scheduling. CI codec moves
   here and the rest of `ci`/`negotiate` is deleted, and the rebuilt MIDI-CI
   minimum (§5, "MIDI-CI") lands with the citation fixes listed there.
   Residue for this stage:
   - **`clock_out.rs` doc is false.** The module doc of `bevy-tutti`'s
     `midi/hardware/clock_out.rs` says the drain cadence does not affect tick
     spacing because the timing is baked into each event. The wire carries no
     timestamp, so the spacing at the wire is the drain's (once per Bevy
     frame, §5 "Timing at the edge"). Correct the doc now, and again when
     scheduled output lands.
   - **`ChangeSink::seed`.** "A backend seeds once, before any `send`" is only a
     doc sentence (`tutti-midi-hardware/src/devices.rs`). Make a second or late
     seed an error the type returns, or take the list in the constructor.
   - **Not verified, needs a Mac or a check.** (a) The CoreMIDI input callback
     has no panic guard; only the watch thread has `catch_unwind`
     (`backend/coremidi/watch.rs`). (b) The session MIDI input ring size
     is 256 in one place and 2048 in another per the audit; the drain chunk is 256
     (`tutti-session/src/midi.rs`) and the other figure was not found. (c) `AlsaOutput` takes its
     mutex with `.expect("output mutex poisoned")`, `unpack_id` in
     `backend/alsa/enumerate.rs` decodes any `EndpointId` without checking it
     was minted by the backend, and the input thread's drop joins without a
     bound. (d) There is no benchmark of the hardware callback to the drain.
- **Stage 5b. Shrink `tutti-midi-types` away:** what 5a, 3a, 3c and 4 leave
   (`mpe/`, `clip_file`, `cc/mapping`, `sync/`, `MidiMessage`, the re-exports) moves
   to its owner (§6) or is deleted, then the crate, and `midi2` from the workspace
   and `Cargo.lock`. With `mpe/`: `MpeZoneConfig` has public fields, so a literal
   can hold a member count or master channel its constructors would refuse, and a
   `DualZone` pair totalling more than 15 members (the two spans overlap) is not
   rejected, only documented as the sender's duty. Give it private fields, a
   `new -> Option` that refuses what RP-053 does, and a `DualZone` constructor that
   refuses an overlap, with the range-types contract test.
7. **Capabilities** (§9).

## 11. Testing

- **Spec tables.** Each message's words from M2-104 App. F and the tables in M2-115
  are analytic tests.
- **`midi2` as an oracle** until stage 5b, a dev-dependency. What it finds becomes
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
design, or remove a measured user-visible problem, except item 3, whose small
measured cost is in §8.4:

1. Decode once per consumer, instead of the 3 to 6 decodes per event today.
2. Fewer conversions: a note takes up to 9 steps from key to CLAP synth, and its
   value changes in two. One `Ump` removes most of them, and `Pod` makes plugin
   IPC one memory copy (gain unmeasured).
3. The clip node's held notes are a `NoteLedger` of 24 KiB per node (§8.4), which
   costs 0.1 to 1.1 µs more per 512-frame block than the 143 KB of arrays before it.
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
