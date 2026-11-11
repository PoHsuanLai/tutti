# 017: MIDI, modelled on the specification

The MIDI stack is correct at the packet layer and careful inside the graph. What
sits above the packet is not MIDI's own model: devices are OS ports, a message
has no type a consumer has to go through, translation happens while reading, and
crates mix the specification's layers. This design gives the stack MIDI's
hierarchy, one typed message, and translation in one place, and closes the data
loss and stuck notes found on the way.

Citations are to the MIDI Association documents M2-100 v1.1, M2-101 v1.2.1,
M2-102 v1.1, M2-103 v1.2, M2-104 v1.1.2, M2-115 v1.0.1 and M2-116 v1.0, which
are not vendored, and no notes on them are in the repo (the ones that sat in
`tutti-midi-types` went with that crate, stage 5b part (c)).

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
- **Events cross the graph's edge through two primitives.** An event tap
  (`Op::EventTap`) copies any event output to a thread, and an event inlet
  (`Graph::open_event_inlet`) feeds a thread's events in, counted and never
  blocked on. The four MIDI edge nodes become users of them ("Event tap and event
  inlet").
- **Crates follow the layers** (§6).
- **`midi2` is removed** (§7).
- **Events are not silently lost.** Capacity follows the block size, note-offs
  have priority and are retried, drops are counted exactly, and an export reports
  them (§8).
- **One event shape, then the shared steps.** Where MIDI stands today is in §1.4,
  the spec's interfaces as traits (mostly not) in §4.1, and the order the remaining
  stages run in is §10: `(Offset, Ump)` first, then the `Translator`,
  `channel_voice` and the dedup.
- **Stages**, each landing on its own (§10).

## 1. What is wrong today

Findings were verified on `main` with probes through the public API unless
marked as read.

### 1.1 The model

This table is the survey as it stood before stage 5b; its "Today" column is
historical, and `MidiEvent`, `MidiMessage` and `tutti-midi-types` no longer exist.

| The specification | Today | Consequence |
|---|---|---|
| Endpoint, Function Block, Group (M2-100 §7, M2-104 App. H) | `PortId` and a name; `EndpointId` is ALSA's client and port | Identity changes across restarts; `UmpCapability::function_blocks` was never filled, and is deleted (stage 4a) |
| Connection steps 2 to 5 (M2-100 Table 4) | Never run on connect (steps 2 to 4 are since L6(b); step 5 is not) | Protocol was guessed, and JR timestamps were a host boolean, against M2-104 §7.1.6.2 (both follow the device's `StreamConfig` since L6(c)) |
| `FunctionBlock` is a device concept | Was defined in `tutti-midi-runtime`'s `negotiate`, deleted in stage 4a | The `Endpoint` model of stage 4b defines it in `tutti-midi-device`, so `tutti-midi-hardware` needs no runtime type for it |
| A translator bridges MIDI 1.0 connections (M2-104 §6.2) | One per input node, keyed by channel | RPN state crosses groups; output is forced to group 0 |
| Decoding and translating are separate | `MidiEvent::message()` calls `normalize` | A MIDI 2.0 Note On at velocity 0 becomes a Note Off (M2-104 §7.4.2 says it does not), while `clip_node` counts it as a note-on |
| Messages have a scope | One flat `MidiEvent` | An endpoint message can be routed like a note |
| One packet format | `tutti_graph::Ump`, `MidiEvent`, `MidiMessage` (all since removed) | Note-off decoding is written by hand in four places |

Three ways of reading a message compete. `MidiMessage` has 22 variants and almost
no production consumer outside its crate; it has no group, mints a `NoteId` on
decode, and repeats `frame_offset` in each variant. The polysynth, SoundFont,
the CLAP host and the VST3 host now read `tutti_ump::Message` (stage 2), and
`PerNoteBendRanges` is `tutti-plugin-types`' and reads the RPN through
`tutti_ump::decode`: no plugin crate reaches `midi2` or `tutti-midi-types`.
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

**One event shape, since stage 5a.** The legacy `MidiEvent { frame_offset,
data: [u32; 4] }` is deleted, with its constructors. What it carried is now
said by the type of the position beside a `Ump`:

- `(Offset, Ump)` in a block, named by the alias `tutti_graph::UmpAt`:
  `MidiIn`, `MidiOut` (both in `tutti-midi-runtime` now),
  `ProcessContext::midi_events`, `BlockSplitter`, `gather_midi`. The graph's
  `Event { offset, kind: EventKind::Midi(Ump) }` is the same pair. An alias
  and not a struct: both halves are checked types, so the pair adds no rule,
  and sites destructure it.
- A raw `u32` where the event crosses a thread or a process ahead of any
  block, and becomes an `Offset` where a block is known: the `MidiMailbox`
  ring's `(u32, Ump)` stamp, and `tutti-plugin-types`' `WireMidi` (plugin
  output and the IPC wire, clamped into the host's block by the batcher).
- An absolute time in storage: `(Beat, Ump)` from an SMF, `(Instant, Ump)` in
  the hardware input ring.
- Bare `Ump` or `Message` where no time is involved (`PolySynth::process_midi_event`,
  `SoundFontUnit::apply_event(&Ump)`, `Voicing::step`, and `tutti-midi-types` (deleted at stage 5b part (c); it sat in
  `tutti-graph`'s layer and could not name `Offset`).

A single message is built as `Ump::from(Message::..)`; tests use
`tutti_fixtures::midi`, one builder per message over the same encoder.

**The `Translator` is built (stage 3a).** `tutti_midi_protocol::Translator` runs
at the two input edges there are: `MidiInputNode`, per polled event, and a clip
as it is loaded (`MidiClipNode::new` and `MidiClipControls::set_events`, in beat
order, each result in its source's place). The old `Midi1ToMidi2Translator` is
deleted. A mailbox's `MidiSender` queue and a plugin's MIDI output are not translated: a
host builds MIDI 2.0 itself, or runs a `Translator` on what it sends.

**Duplication.**

- *Widening, one place since stage 3b.* An instrument reads a packet with
  `tutti_midi_protocol::channel_voice` (a `Voice2`, a MIDI 1.0 message widened,
  or `None`); the polysynth's and the SoundFont's `voice1` copies and their
  seven-arm match are gone. `tutti_midi_protocol::widen_message` widens a whole
  `Message` for an edge that forwards what it does not read; the hosts' two
  `fn widened` copies (`tutti-clap-host/src/events.rs`,
  `tutti-vst3-host/src/events/midi.rs`) move to it with the plugin lanes.
- *RPN and NRPN, one place since stage 3a.* The `Translator` assembles; the
  SoundFont, `MpeIngest` and `Voicing` read the `Voice2` registered-controller
  form. (Before: the old translator, the SoundFont's own Data Entry machine and
  `mpe_ingest`'s `RpnRun`.)
- *MIDI 1.0 byte parsers.* `tutti_ump::Midi1Parser` against `tutti-plugin-types`'
  `Midi1Splitter` and `Midi1Decoder` (about 700 lines), whose one production caller
  is `tutti-au-ffi/src/midi_out.rs`.
- *SysEx, one place since stage 3c.* `tutti_ump::SysexAssembler` (packets to whole
  messages over a caller's buffer) and `SysexMessage` (a checked `F0 .. F7`). Before:
  `tutti-plugin-types`' `SysExAssembler` (about 790 lines) and `tutti-midi-runtime`'s
  `Sysex7PacketReassembler`, with `Sysex8PacketReassembler` and the FlexText
  reassembly beside them. The plugin crates' `SysExAssembler`, `is_framed` and the
  public `unframed` are gone (doc 027 M6); `sysex_packets` stays as the plugin side's lenient bridge.
- *Note identity, one place since stage 3c.* `tutti_ump::NoteId`, `channel * 128 + note`,
  which is also the plugin formats' wire value. Before: `tutti-midi-types`' `NoteId`
  (`channel << 8 | note`) beside `tutti-plugin-types`' `note_id_for` (still standing,
  as above).

**Held-note structures answer different questions and stay separate.**
`tutti-voicing`'s `Holds` is why one voice is still open (key, sustain,
sostenuto). `NoteLedger` (§8.4) is which notes a node has started and not ended on
its own port, and what the port owes. `tutti-graph`'s `held.rs` is which keys an
event output port carries, so an edge that ends can close them. They have
different owners, keys and failure modes; unifying them would give one of them a
question it does not ask.

**`tutti-midi-types` was about 9,000 lines of pre-017 vocabulary:** `mpe/`, `translation/`, `cc/mapping`, and the `MidiIn` and
`MidiOut` traits, beside the `MidiEvent` above. Stage 5b part (b) left `MidiMessage` and `translation/`, and part (c) deleted both
with the crate.

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
  in `tutti-midi-types` (and, before stage 4a deleted them,
  `EndpointNegotiator::with_names` and `FunctionBlock::name`) take any `&str`, and
  the `midi2` crate adds no limit. The limits, from M2-104 v1.1.2 §7.1:
  - **Endpoint Name:** at most 98 bytes of UTF-8 (seven packets).
  - **Function Block Name:** at most 91 bytes (seven packets). Version 1.1.2
    (§7.1.9) lowered it from 98, so a name built to the old figure is now too long.
  - **Product Instance ID:** at most 42 bytes, every byte ASCII 32 to 126
    (§7.1.5), so a non-ASCII or control byte is refused, not passed through.

  **Done in stage 4b** (`tutti-midi-device`'s `Name<N>`, `EndpointName`, `BlockName`
  and `ProductInstanceId`, held inline; `new -> Option`, as the range-checked table
  of CLAUDE.md has it; the assembler caps at the same limit and drops a longer
  message whole). The next paragraph is the reasoning.

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
pub struct Translator { /* per (group, channel): RPN, NRPN and Bank Select state */ }

impl Translator {
    pub fn feed(&mut self, ump: Ump, out: impl FnMut(Ump));
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
- Translating back is D.2, `Narrower` (`tutti-midi-protocol`, built; no host
  calls it yet): an RPN or NRPN becomes its four Control Changes (D.2.3), the
  select skipped while the same parameter stays selected on that group and
  channel (MIDI 1.0 rule 6), and no Null Function after the run (D.2.3 gives
  four messages, D.3.3 says Null "is not translated"). Data narrows with
  rounding for RPN bank 0, index 0 to 31 (M2-115 §4.1, §4.4, the rule the
  `Translator` zero-extends by) and by the plain shift for every other
  controller. A Program Change's bank is sent only when it differs from the one
  last sent (D.2.4), and a Note On that narrows to velocity 0 is 1 (D.2.1). One
  `Narrower` serves one output, so each plugin host and the hardware out holds
  its own. Until a host calls it, a keyboard's pitch bend range does not reach
  a CLAP, VST2, AU or SoundFont consumer as MIDI 1.0.
- **The clip's end flushes a CC 6-only run** (`Translator::flush`, called by
  `MidiClipNode`'s loader after the last event): a file that sets a range with
  `B0 65 00, B0 64 00, B0 06 0C` and nothing after keeps it, on the CC 6's own
  beat. The live "short timeout after receiving a CC 6" of D.3.3 ("some
  translators may wish to") is not built: the `Translator` holds no clock, and an
  input that has one can call `flush` itself. CC 96 and 97 stay Control Changes
  (D.3.3, quoted in `Translator`'s docs).
- MPE is a dialect on MIDI 1.0, handled here, by M1-100 (MPE 1.1, the MCM) read in full.
  The receive rules are `MpeIngest`'s `MpeSpec`, a value, so the MIDI-CI MPE Profile
  (M2-120, L10b) is another variant of it and not a branch in the code:
  - Channels outside a zone pass through (§2.2.1, "remain available for conventional use").
  - A member's bend, pressure and CC 74 are tracked per channel "even when no note is
    playing" (§2.2.6 to 2.2.8) and sent as per-note messages ahead of the channel's next
    Note On (a note's number is known only then); a held note takes them at once. A
    manager's pressure and CC 74 reach every note of its zone, combined with the member's
    by Max (Appendix D leaves the choice); a manager's bend is `Voicing`'s, summed with the
    member's (Appendix C.5). After a Note Off the channel's messages wait for the next note.
  - A manager's pedal, All Notes Off and Reset All Controllers are the zone's
    (`Voicing`, §2.3.1: "all Sounding Notes across the Manager Channel and all Member
    Channels"); a member's is its own.
  - Table 5 (Appendix E), receive side: a prohibited message is ignored (§2.3.1, "it should
    ignore them"): 120, 126, 127 on a manager, 125 anywhere, RPN 6 on a member, Program
    Change on a member in Mode 3 (§2.3.3; Bank Select travels in it). 121 to 124 pass. A
    Mono or Poly Mode On on the zone's lowest member switches Mode 4 and 3 (§2.2.4.3); the
    ingest keeps only that, the instrument's own `Rules` decide what Mono means. Poly Key
    Pressure on a member is not filtered: it cannot be told from a native per-note message.
  - A zone change stops and resets "each Channel entering or leaving MPE control" (§2.2.3):
    All Sound Off and Reset All Controllers on each, ahead of the zone's default ranges, from
    an MCM that moves channels and from a host-set mode (`zone_defaults`); the channels that
    did not change keep their notes and what they were told. The per-note held values of a
    channel that left are not reset (one `PerNoteManagement` per note number would not fit a
    block): a note that starts there later is told the ingest's values again.
  - Sensitivity: an MCM sets a manager to 2 and every member to 48 (§2.2.5); a member's RPN 0
    is every member's (RPN #00/07 on each).

A plugin host that wants MIDI 2.0 data takes `Midi2` directly. One that speaks
MIDI 1.0 asks the translator for D.2, so the rule lives in one place. Hosts then
honour a plugin's declared dialect, including `CLAP_EVENT_MIDI2`, which is never
sent today.

### 4.1 The spec's interfaces as traits

The question was whether MIDI should get traits that follow the specification's
interfaces. Most should not, because the pure-step shape already used here (a value
in, a value out, no I/O) covers them with less surface. The decisions:

- **Channel-voice receiver: a function first.** `tutti-midi-protocol` has
  `channel_voice(ump) -> Option<(Address, Voice2)>` (stage 3b): it decodes and
  widens a MIDI 1.0 message, and is `None` for every non-voice message and
  `Unknown`, the match two crates copied (§1.4). The plan named it
  `dispatch(ump, handler)`; returning the message is the same step with no
  callback.
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
// tutti-midi-device (stage 4b, done)
pub struct Endpoint { /* version, Capabilities, active StreamConfig, identity, name,
                         instance_id, static flag, up to 32 FunctionBlock, all inline */ }
impl Endpoint {
    pub fn midi1_device(name: Option<EndpointName>) -> Self;
    pub fn reply(&mut self, message: Message, out: &mut impl Extend<Ump>);
}

pub struct Connection { /* M2-100 Table 4, steps 2 to 4 */ }

impl Connection {
    pub fn new(now: Instant) -> (Self, Ump);          // the Endpoint Discovery to send
    pub fn on_message(&mut self, msg: Message, now: Instant) -> Action;
    pub fn on_timeout(&mut self, now: Instant) -> Action;
    pub fn deadline(&self) -> Option<Instant>;
}
pub enum Action { Wait, Send(Ump), Settled(Ending) }
pub enum Ending { Negotiated, Midi1Fallback }
```

A MIDI 1.0 device has no steps 2 to 4 and appears as an `Endpoint` with one
inferred block. An endpoint with many groups is one device, not many.

**Wired (L6(b)).** `MidiSession::connect_input` starts a negotiation for the
device it opens, and `MidiSession::poll(now)` is its timer: the session's
`Phase::Poll` calls it with the phase's `now`, so no clock is read below it (the
core takes an `Instant`; the hardware layer only passes one on).

- *Where it lives.* One `Negotiation` per open input, in `MidiSession`'s map beside
  the `OpenInput` (`tutti-midi-hardware`'s `stream.rs`). It owns a `Connection`
  and its **own** output to the device (`Devices::open_output`): asking must not
  depend on, or disturb, the session's one chosen output. It runs only where the
  OS describes a UMP Endpoint (`UmpCapability::is_ump_endpoint`: the MIDI 2.0
  Protocol in use, or blocks declared) and the device has an output; a MIDI 1.0
  port has no Stream to ask (M2-104 Appendix I) and is `Basis::Described` at once.
- *Inbound.* `InputProducerHandle::push` keeps a type-`0xF` packet out of the event
  inlet: it goes to the port's stream ring (a `ringbuf` of 512, loss counted in
  `PortInfo::stream_lost`), which `poll` drains into the `Connection`. Stream messages
  never reach the graph. This is backend-independent: ALSA's pump, CoreMIDI's
  callback and the fake all push through the same handle.
- *Outbound.* What the `Connection` answers goes out the negotiation's output, at once.
- *What is asked.* The MIDI 2.0 Protocol where the device offers it, else MIDI 1.0,
  with JR Timestamps off both ways: input time is the OS arrival stamp (JR adds nothing
  and no graph node reads a JR clock), and output JR follows the settled configuration
  (L6(c), "The output protocol edge" below), so none is sent to a device that was not
  asked to receive it.
- *After the settle (L6(b3)).* A `Connection` that negotiated goes on hearing the
  device (`Action::Changed`, returned by `MidiSession::poll` as the settle is): a Stream
  Configuration Notification is the Protocol and JR in use whenever it comes (§7.1.6.1
  step 3: the Sender starts "using the Protocol and JR Timestamps configuration as
  declared in the Stream Configuration Notification"), and a Function Block Info or Name is
  read only when Endpoint Info did not declare the blocks static (§6.1.1: such an Endpoint
  "will not change any of the properties of its Function Blocks after initial discovery";
  §7.1.8 and §7.1.9 send the Notifications only from one that has not). The reading is
  normalised: a declared count above 32 is refused, not clamped (numbers are 0 to 31, §7.1.7),
  so the Endpoint Info is never heard and the step ends as a silent peer's; a block outside
  the declared count is not read (and no longer raises the count the connection waits for); a
  static Endpoint's block sent inactive is read active (§7.1.8: it "shall not have any
  inactive Function Blocks"); a MIDI 1.0 port over several Groups is read as a plain block
  (§7.1.8: "Number of Groups Spanned shall be 1").
- *The output protocol edge (L6(c)).* One place, `OpenOutput::queue` (`wire.rs`), puts a
  batch in the device's wire form from its `StreamConfig`: the OS's description when the
  output opens (`Devices::open_output`: ALSA's endpoint `protocol`, the kernel's record of
  what the USB discovery and Stream negotiation left, and CoreMIDI's
  `kMIDIPropertyProtocolID`; a legacy port is MIDI 1.0). USB's `bMIDIProtocol` has no
  field of its own in either API: it reaches tutti, if at all, as that protocol, and
  whether the kernel seeds it from the descriptor for a given device was not checked (no
  USB MIDI 2.0 device on the machine), then what the negotiation settled
  (`MidiSession::poll` sets the held output's config, and `connect_output` reads an
  already-settled one). A MIDI 2.0 Protocol endpoint gets the graph's packets as they are.
  A MIDI 1.0 Protocol endpoint, or a MIDI 1.0 port, gets MIDI 1.0 (§3.2: "UMP MIDI 1.0
  Devices shall not use any messages from Message Type 0x4"): each type-4 packet through
  one `Narrower` per output (D.2; a Registered Controller as CC 101, 100, 6, 38, the select
  skipped while it holds; the narrower is reset when the Protocol changes), type 2, System,
  SysEx7 and Stream as they are, a message with no MIDI 1.0 form (D.2.8) dropped. JR Clock
  and JR Timestamp go only when `rx_jr` is set (§7.1.6.2: the Sender "shall not send JR
  Timestamps until after a Stream Configuration Notification message ... has been
  received"); Delta Clockstamp and Ticks Per Quarter Note are a clip's and never go. The
  session's `UmpOut` JR stamping follows the same configuration each `Phase::Poll` and
  `Phase::MidiOut` (`MidiSession::output_config`): `Midi::set_jr_stamping`, the host's
  boolean, is gone, and the edge drops a JR message a stale stamper still produced. The
  count `queue` returns is in the caller's events (an event that became nothing counts as
  taken). A MIDI 1.0 port is never sent type 4: this was a violation on CoreMIDI, whose
  `Midi10` event list carried whatever the graph had.
  *Not done, with the reason.* (1) **No per-Group narrowing for a MIDI 1.0 Function Block
  inside a MIDI 2.0 endpoint.** §7.1.6.2 allows MIDI 1.0 Protocol "optionally" for "that
  Function Block", but §6.2.2 says "it is up to the Device to handle the required
  translation" for a MIDI 1.0 block, and §3.3.2 forbids type 2 in the MIDI 2.0 Protocol for
  the rest of the endpoint; a MIDI 2.0 packet is always valid to the endpoint, and the
  per-Group flag the blocks give (`FunctionBlock::midi1`) is kept as data. (2) **The graph
  carries no type 2** (the input edge's `Translator` widens it), so a MIDI 2.0 endpoint is
  not sent a widened type 2; a packet that is one passes as it is. (3) *Done in L6(b2):*
  **CoreMIDI's output event list is tagged with the Protocol the output is in**, not the one
  the OS reported at open: a backend's output is a `DeviceOut` (a `MidiOut` that is also told
  `follow(protocol)`), `OpenOutput` calls it as the output opens and at each `set_config`, and
  the event list is `Midi20` or `Midi10` by the last call.
- *Received JR (L6(c)).* A JR Clock or JR Timestamp a device sends is dropped at
  `InputProducerHandle::push` (`Ump::is_jr`), not read. §7.2.2.1 leaves the use to the
  Receiver ("the Receiver may use smoothing or averaging of time from each JR Clock message
  ... to determine a steady JR Clock to render against"), the session asks for JR off, an
  event is placed by its OS arrival stamp, and nothing in the graph reads a timestamp
  utility message. A `JrReceiver` per device, its delay folded into the arrival stamp, would
  refine a time the OS has already stamped. Revisit it with a transport that has no arrival
  stamp.
- *No hang.* `poll` never waits. A step unanswered for `STEP_TIMEOUT` is sent again,
  three sends in all, and then the device is `Basis::Unanswered`, taken as the OS describes
  it (the protocol the OS reports, JR off, its blocks), never the core's blanket
  MIDI 1.0 fallback: an OS that has already discovered the device knows better. The result
  is `DeviceStream` (`MidiSession::stream`, `SessionEvent::MidiStream`).
- *`UmpCapability`* is built from an `Endpoint` (`UmpCapability::of`): the protocol in
  use and the Function Blocks.
- *What each OS exposes.* ALSA: the kernel runs the USB discovery itself and keeps the
  Endpoint (`snd_seq_get_ump_endpoint_info`: protocol in use, protocol caps, name) and the
  blocks (`snd_seq_get_ump_block_info`: Groups, direction, UI hint, MIDI-CI version, SysEx 8,
  name); `tutti-alsa-ffi` reads both and the backend reads the *protocol*, no longer "has a
  block, so MIDI 2.0". Whether the kernel forwards a real device's replies on a UMP client's
  port 0 is not verified here (no USB MIDI 2.0 device on the machine); between two seq
  clients, Stream packets cross in both directions, which the sequencer suite runs. CoreMIDI
  (L6(b2)): macOS 15's `MIDIUMPEndpointManager`, `MIDIUMPEndpoint` and `MIDIUMPFunctionBlock`
  are Objective-C classes, which `coremidi-sys` cannot carry; `tutti-coremidi-ffi` binds them
  through `objc2-core-midi` (pure Rust, generated from the SDK headers; the only code that
  names it) behind `ump_endpoints()`, which asks the Objective-C runtime for the manager
  class first and answers `None` before macOS 15. The backend matches an endpoint to its
  `MIDIUMPEndpoint` by `kMIDIPropertyUniqueID` (of its source or destination) and reads the
  Protocol in use (`MIDIProtocol`) and the blocks, through the same `RawBlock` check as ALSA
  (`block.rs`: M2-104 §7.1.8's numbers, a block the spec cannot hold is left out, never
  clamped); an endpoint with no UMP object is the `kMIDIPropertyProtocolID` property as
  before, with no blocks. Not read: `MIDIUMPFunctionBlock` has no MIDI-CI version of its own
  (`midiCIDevice` is a separate object), so it is 0, and the endpoint's JR Timestamp
  capabilities (`hasJRTSReceiveCapability`/`hasJRTSTransmitCapability`) are not carried by
  `UmpCapability`, which reports JR off for every OS endpoint; the session's own
  negotiation is what turns JR on. No endpoint
  tutti publishes answers Discovery: the CoreMIDI virtual endpoints are types nothing in the
  session creates, and ALSA has none, so `Endpoint::reply` has no caller.

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

**Stage 4c, part 1: the clock map and the timestamped input. Built.** Output
scheduling (`OutputPlan`) is the next part and is not.

- **The clock map.** `ClockMap` (`tutti-midi-runtime`: it needs `tutti-types`'
  `Frame`, `SampleRate` and `Moment`, and `tutti-midi-device` is a layer too low
  for them) converts an OS time to a frame (`frame_at`, `moment_at`) and a frame to
  an OS time (`os_at`, which `OutputPlan` needs). It is pure and holds no clock:
  it is built from one `ClockReading`, the OS instant and the graph's frame at the
  same moment, and takes more as values (`observe`). The OS time is a
  `std::time::Instant`, the OS's monotonic clock on every target, so the map is
  tied to none; a backend whose stamps are on another scale converts them where it
  reads them (below).
  - **The rule.** The slope is the nominal rate, never fitted: the two crystals
    differ by a small relative error `ε` that a fitted slope would estimate from
    jittery readings. The map follows the *offset* instead. Each reading is
    compared with where the anchor predicts its frame, and the anchor moves one
    eighth of the way to the reading (`GAIN`), at the reading's frame. A reading
    more than `RESYNC` (20 ms) from the prediction is a restart or a suspend, not
    jitter, and re-anchors exactly.
  - **The bound.** A query `Δ` after the latest reading is off by at most
    `ε·(8·T + Δ)`, where `T` is the interval between readings, plus half a frame
    of rounding, plus any reading's jitter over 8. For `ε = 100 ppm` and
    `T = Δ = 10 ms` that is under 2 µs, a tenth of a frame at 48 kHz. One-sided
    jitter (a wake-up is late, never early) biases the map late by its mean over
    8, so the reader takes its readings at a fixed point of the device callback.
  - **What it refuses.** A reading earlier than the anchor in OS time or in
    frames, or at the anchor's frame at another time, is `Observed::Rejected` and
    changes nothing. Before the first reading nothing is known, so a map exists
    only from one, and a query extrapolates in either direction, to `None` only
    before frame 0. A rate change (`set_rate`) moves the anchor's frame to the same
    instant at the new rate (`Frame::rescaled`), as the executor does on a
    re-prepare. A rate that is not finite and positive builds no map.
- **Input.** Each backend gives its events the OS's own timestamp, as an
  `Instant`, in place of the instant the thread read them.
  - **ALSA.** The pump's port stamps every event with a queue's real time
    (`snd_seq_port_info_set_timestamping`, a queue of the client's own, started).
    The pump reads the queue's clock and `Instant::now()` together (one
    `snd_seq_get_queue_status` per run of events, refreshed when an event is
    stamped after it or the pairing is a second old) and places each event as long
    before that reading as its stamp is before the queue's (`stamp::arrival`).
    The 1 ms sleep this section once described is gone: the pump blocks in
    `poll` on the sequencer's descriptors and a stop descriptor (a socket pair
    closed on drop). A test holds the pump 400 ms after a wake and checks that
    three notes sent 50 and 100 ms apart keep their spacing to 20 ms; the read-time
    stamp collapsed it to microseconds. The drop waits for the pump at most 1 s
    (`JOIN_BOUND`) and then leaves it running, still subscribed, with a warning.
  - **CoreMIDI.** Each packet's `timeStamp` is mach host time. The callback reads
    `mach_absolute_time` and `Instant::now()` together (`host_time::Pairing`) and
    places the packet by `mach_timebase_info`'s ratio through the same
    `stamp::arrival`; a 0 stamp is "now". The hardware virtual destination does
    the same. **Not verified with real MIDI traffic**: it compiles and its unit
    tests pass on a Mac (clippy `-D warnings` clean on macOS), and the timestamp
    arithmetic is tested, but no device sent it a packet.
  - **Where the frame is made. Done (E5).** By the producer, on the device's own
    thread. The engine writes one reading per render call (`BlockClock`, in
    `tutti-engine`: a `tutti-rt` sequence cell of the OS time the call began and
    the graph frame it began at; `Engine::block_clock` hands out its
    `BlockClockReader`) and each device's thread holds a `ClockMap` fed those
    readings (`DeviceInput`, in `tutti-midi-runtime`). It
    places each arrival `Instant` on a frame, adds the **lead** (below) and
    pushes the event into that device's event inlet with `push_at`. The audio
    thread converts no time: the inlet holds each event until its block, and
    the ring's `Instant`s and `read_inputs` are gone. A reading the map rejects
    (the engine restarted and its frames began at 0 again) re-anchors it; before
    the first reading, or for an arrival the map puts before frame 0, the event
    is pushed for the next block.
  - **One timebase.** The engine stamps with `tutti_rt::monotonic_ns` (the OS's
    monotonic clock, on the OS's epoch, so a plugin server shares it) and
    `tutti-graph` holds no wall-clock type: it sees only `Env`. `ClockMap` stays
    pure over `Instant`, the type every backend stamps arrivals with (ALSA
    converts the queue's time to an `Instant`, CoreMIDI the mach host time, both
    through `stamp::arrival`); `Instant` is the same OS monotonic clock
    (`CLOCK_MONOTONIC`, `mach_absolute_time`, `QueryPerformanceCounter`) but
    opaque, so no backend can produce nanoseconds without a pairing. The map
    therefore keeps `Instant`, and the session, which owns both ends, converts
    the engine's nanoseconds with one pairing of the two read once per process
    (`arrival_clock`): an offset, never a second clock. `tutti-engine` and
    `tutti-midi-runtime` share no edge (both layer 4); the runtime takes the
    clock as a function (`ArrivalClock`).
  - **The lead.** An event that arrived between two blocks maps to a frame
    before the next block's start, so it would reach the graph already late and
    a run of events would collapse onto offset 0. Each arrival is therefore
    placed a fixed **lead** after its mapped frame, one block of the device's
    callback (`quantum`, else the largest block; the session sets it,
    `DeviceInlets::set_lead`). An event sounds exactly the lead after it
    arrived, whatever the block boundaries, and the spacing of events is the
    map's: the bound stated above, within the OS stamp's own accuracy. It is
    late (and counted, `InletCounts::late`, `PortInfo::late`) only when the
    thread pushes it more than the lead after it arrived.
- **Output.** Schedule from the event's frame, on the OS's timestamp or queue.
  Today output is flushed once per Bevy frame with no timestamp, so MIDI clock
  leaves in bursts, and a headless `Session` does no better. This is the largest
  timing defect that users can hear from external gear. `ClockMap::os_at` is the
  conversion it needs; the plan is the next part of stage 4c.

### MIDI-CI

**Stage 4a, done.** `ci/*` and `negotiate` were about 2,400 production lines with
no production path: no inbound SysEx reached them (`bevy-tutti`'s `negotiation.rs`
says its inbound message types have no producer, and `CiRes` is inserted by no
plugin). They also missed current requirements: no ACK, no Invalidate MUID response
(M2-101 App. F), profile bodies without Message Version 2 fields (18 bytes against
20), and Property Exchange built for v1.1.

What was kept moved to `tutti-midi-device` (about 640 lines and 320 of tests, a private
`ci` module): the header, `Muid`, `Destination`, Discovery and its Reply, Invalidate
MUID and NAK, as `CiMessage::encode` and `decode` over SysEx payload bytes. `Muid`
is checked (`Muid::new` refuses the reserved range and the broadcast value;
broadcast is `Destination::Broadcast`) and `Muid::from_seed` is gone; a MUID is
drawn from an OS random source when the rebuild lands. The codec frames nothing
itself (L1 gave it `tutti_ump`'s `U7`, `MidiChannel` and `FunctionBlockId` for its
fields); the caller frames the payload (`tutti_ump::frame`, or SysEx7 packets) and reassembles with
`tutti_ump::SysexAssembler` (`tutti-midi-hardware/tests/midi1_wire_ci.rs` runs the
whole MIDI 1.0 wire path). The UMP Stream constructors (`endpoint_info` and the
rest in `tutti-midi-types`' `ump/stream.rs`) were not part of that move; stage 4b
rebuilt them as the `Endpoint` and `Connection` and deleted them with their tests.

Deleted: the state machines (`CiInitiator`, `CiResponder`, `capability_inquiry`,
`DiscoveredCiDevice`, `CiProperty`, `ProfileSwitch`), the profile and property
bodies, and `EndpointNegotiator` with `EndpointInquiry`, `DiscoveredEndpoint`,
`FunctionBlock` and `DeviceIdentity` (`tutti_ump::DeviceIdentity` is the type), and
`UmpCapability::function_blocks`. The re-exports of all of it from
`tutti-midi-types`, `tutti-midi-runtime` and `tutti-midi-hardware` are gone.

What the deleted `EndpointNegotiator` did, as a checklist for the `Connection` of
stage 4b (which is rebuilt from M2-104 §7.1 and M2-100 Table 4, not from that
code): it was the responder half only. It held one endpoint's declared UMP version,
capabilities, active protocol and JR timestamps, identity, name, Product Instance
Id and Function Blocks. On an inbound Endpoint Discovery it replied in order
Endpoint Info, Device Identity, Endpoint Name, Product Instance Id, Stream
Configuration, then Function Block Info and Function Block Name per block, each
only if the request's filter bits asked for it, and on a Function Block Discovery
it replied for one block or for all (`ALL_FUNCTION_BLOCKS`). Its discoverer half
sent the request, folded each reply into a `DiscoveredEndpoint`, and never retried,
timed out, requested a Stream Configuration or confirmed the active protocol. A
connection also owes the steps of Table 4 after discovery (request a protocol and
JR timestamps, wait for the notification, fall back to MIDI 1.0), with time as a
value.

The App. F minimum below is the rebuild, for the stage that needs MIDI-CI, with a
real inbound path, in one change (an estimated 650 to 750 lines). Property Exchange
waits for a named peer and for M2-105, which is not in the core set.

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
   inbound path reassembles SysEx7 per group (**the assembler is done in L2**: one
   stream per Group; feeding it from the hardware input is L6(b)). Tests: an inquiry on group 5 is
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
7. **ACK and NAK** (§5.10, §5.11). **Done in L1** (below). ACK (sub-ID#2 `0x7D`) is encoded and decoded
   (`CiMessage::decode` returned `None` for it), with its status values for
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

Fixed in stage 4a in the code that moved or stayed: `ci/discovery.rs`, `ci/mod.rs`
(now `tutti-midi-device`'s `ci.rs`), `message.rs` and `midi_group.rs`, and the receiver rule that `discovery.rs` cites as §5.4, which is §5.3. The rest
described code that is deleted and is kept here as the record for the rebuild.

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
| Events across the graph's edge | `EventTapReader`, `EventInletWriter` (`tutti-graph`) |

The same pattern is already in use: the plugin hosts' pure `step` (design 016),
`Phase::Poll`, `PhaseCx { now }`, and `Session::update() -> Vec<SessionEvent>`.
Audio devices (design 018) follow the pattern and share `Phase::Poll`. They share
no code with MIDI.

## Event tap and event inlet

Audio crosses the graph's edge through a tap (`Op::Tap`, `TapReader`,
`Graph::open_tap`) and an inlet (`InletNode`, `Graph::open_inlet`). Events cross
it through four MIDI nodes that each copy by hand: `MidiOutNode`,
`MidiQueueNode`, `TimedQueueNode` and `MidiInputNode`. The owner decided on
2026-11-11 (design 013, "What else is, and is not, a graph primitive") that
copying events out of any event port and into the graph is the graph's own
primitive, and that this document, which owns the event side, specifies it. This
section is that amendment. The `tutti-graph` parts (the op, the node, the
writer and reader types) are reviewed by the graph's owner; the rest is
`tutti-session` and the MIDI crates.

The test design 013 applies: a primitive says where a signal goes, a node
computes one. Copying an event out to a thread, or in from one, is placement.
Translating MIDI 1.0, splitting by channel and merging MPE zones are
computation, and stay nodes.

### Survey: what exists

Audio, in `crates/tutti-graph`:

- A tap is declared on the `Topology` (`TapSpec { sources, align }`,
  `graph.rs:416`), opened by `Editor::open_tap` (`editor.rs:563`), which refuses a
  source that does not exist now (`MissingSource`, `editor.rs:312`, which is
  `#[non_exhaustive]`) and queues the writer in `pending_taps`.
- The compiler emits one `Op::Tap` per tap (`compile/mod.rs:951`). `TapAlign::ToOutput`
  adds, per lane, the delay an output channel of the same source gets
  (`DelayKey::Tap`, `plan.rs:99`); `Raw` reads the source as it leaves its node. A
  lane whose port is missing reads `ARef::Zero`. A closed tap is not in the
  topology and emits nothing.
- The writer (`TapWriter`, `tap.rs:42`) is a `ringbuf` `HeapProd` per lane. It rides
  the commit (`exec.rs:344`, carried by id at `exec.rs:1307`), is claimed by the op
  in the parallel executor (`par.rs:540`), writes nothing with no reader
  (`read_is_held`), and counts what the rings refused in an `Arc<AtomicU64>` the
  reader takes (`take_lost`, `tap.rs`). Both ends assert they are not dropped on
  the audio thread (`RtScope::check_outside`).
- `cost.rs:216` puts `Op::Tap` in the `COPY` work class. `compile/verify.rs:682`
  checks the tap index. A fork carries no tap (`tests/tap.rs`
  `a_fork_carries_no_tap_and_an_export_reads_its_own_losslessly`).
- The session opens it (`Graph::open_tap`, `tutti-session/src/graph.rs:1739`) and
  reaps readers dropped first in `Phase::Reclaim` (`reap_taps`, `graph.rs:1792`).
- The inlet is `InletNode` over a `TapReader::pair` ring (`inlet.rs`).
  `Graph::open_inlet` and `add_inlet` (`graph.rs:722`, `739`) refuse a producer of
  another rate (`InletError::RateMismatch`). The node is unforkable.

Events in the graph:

- An `Event` is `{ offset: Offset, kind: EventKind }`, `Copy`, at most 32 bytes
  (`event.rs:731`). `EventKind::Midi(Ump)` is one variant beside `Ramp`, `Segment`,
  `Harmony` and `Resync`. An offset is meaningless outside the block; `Env::frame`
  is the block's first `Frame` and `Env::frame_at` and `Env::offset_of` are the
  only conversions (`time.rs:297`, `302`). `Env::due(At)` already resolves a
  scheduled time to `Due::{In(Offset), Late, NotYet}` (`time.rs:306`).
- Ops reach event slots through `EventSlots` (`slots.rs:125`). `Op::EventMerge`
  (`plan.rs:364`) and `Op::EventDelay` are the event ops; `cost.rs:218` puts both in
  the `EVENTS` class. A port's capacity is `Shape::with_event_capacity`, and
  `note_off_room(cap)` is the extra room a note-off may use (`event.rs:1000`).
  `Io::count_lost` adds to the executor's dropped-event count (`io.rs:287`).

The four MIDI nodes, in `crates/tutti-midi-runtime/src/live`:

| Node | What it does | Ring and loss | Users | What it does beyond copying |
|---|---|---|---|---|
| `MidiOutNode` (`out_node.rs`) | Event input to a control thread | `MidiMailbox`, a `crossbeam` `ArrayQueue` of `(u32, Ump)`, 1024; a full ring drops the rest and counts (`dropped`) | The session's hardware out, fed by the clock (`MidiNodes::hardware_out`, drained in `Phase::MidiOut`, `midi.rs:482`); `ClockNode` docs | Stamps each event with `Env::frame + offset` truncated to `u32`, which wraps at about 25 hours. Fork discards |
| `MidiQueueNode` (`queue_node.rs`) | Control thread to one event output | `MidiMailbox` of 256, `MidiSender` is `Clone` and MPMC; a full ring refuses (`bool`) | The session's live inputs (`Live`, `midi.rs`), keyboards, tests | Sorts by stamp, clamps the stamp into the block (a control thread's stamp is 0). Fork is a new empty queue |
| `TimedQueueNode` (`timed_queue.rs`) | Control thread to one event output, each event on its frame | `ArrayQueue<Stamped>` of 256 plus a waiting list of the same size; `TimedSender` is `Clone`; refuses when full (`TimedSendError::Full`), counts evictions (`dropped`) | `InstrumentHandle` (`tutti-session/src/instrument.rs:323`), which is `Clone` | Holds an event until the block that contains its frame. A late event lands at offset 0, not dropped. `Moment` (frame and rate) rescales waiting events when the graph is re-prepared at another rate. A note-off or CC 120 or 123 is kept in preference to any other event. Reads the node's clock through a `SeqReader`. `At::Beat` is refused. Fork is silent |
| `MidiInputNode` (`input_node.rs`) | A `MidiIn` polled each block, to 17 event outputs | The hardware port rings, drained on the audio thread by `HardwareMidiDrain`; a block's translation that does not fit is dropped whole and counted (`dropped_events`, and `Io::count_lost`) | The session's hardware input | `Translator` (MIDI 1.0 to 2.0, RPN, Bank Select), `MpeIngest` (classic MPE to native), one port per channel and a channelless one, a note-off's room past the capacity, an MPE mode request read through an `ArcLend`. The arrival `Instant` to `Offset` conversion is on the audio thread, in the drain (`tutti-midi-hardware/src/port/manager.rs:173`) |

So three of the four do plain copying plus ordering and stamping, and the
fourth (`MidiInputNode`) is a translator with a ring on its front. That
splits what becomes the primitive from what stays a node.

### The event tap

`Op::EventTap`, `TapReader`'s event sibling: one event output port of any node,
copied into a ring a thread of your own drains.

| Question | Decision | Reason |
|---|---|---|
| Which port | One event **output** of a node (`EventOut`, as `connect_event` names a source). Not an input, and not a list | An input port is a merge of its sources and has a time of its own (aligned to its node's other inputs); the tap would have to choose between before and after that alignment. A reader that wants a node's input taps its sources. A list of sources would be a merge the compiler already has as `Op::EventMerge`; several taps merge in the reader by frame, which is what the frame is for |
| Which events | `EventKind::Midi` only. Ramps, segments, harmony and resync are graph control, and are neither copied nor counted | They mean nothing outside the graph, and an `Event` is not what a thread wants to hold. If a consumer needs them, that is a second kind of tap |
| Time | `TapAlign` as it is: `ToOutput` delays the source by the compiler's `Op::EventDelay` to the outputs' time, `Raw` reads the port as it leaves its node. `ToOutput` is the default | The same compensation an audio tap gets. A MIDI out to hardware should line up with the audio the graph outputs; a reader that wants the node's own time asks for `Raw`. A source that departs later than every output reads as `Raw`, as for audio. `DelayKey::EventTap { tap, from }` names the delay |
| What the reader gets | `TapEvent { frame: Frame, ump: Ump }`, 24 bytes, `Copy`. `frame` is `Env::frame_at(offset)` of the block the op ran in, after any alignment delay | An offset is relative to a block the reader never sees. An absolute frame on the graph's clock keeps the spacing between events whatever the block sizes were, orders events across taps and against the transport's `At::Frame`, and is what `ClockMap` converts to an OS time for output scheduling (§5, "Timing at the edge"). `u64`, so it does not wrap the way `MidiOutNode`'s `u32` stamp does |
| A bypassed or muted node | Doc 013's `NodeMode`: a `Bypassed` or `Muted` node's event outputs are empty, so a tap on one reads nothing for those blocks (no loss is counted). An `EventInletNode` is not bypassed in practice; if it is, its waiting list holds and late events land at offset 0 when it runs again | Keeps M1 and E1 in agreement: `NodeMode` decides what a port carries, and the tap copies what it carries |
| Capacity | An event count named at open (a count of events, a bare `usize` as `Shape::with_event_capacity` takes one: no unit covers it). The ring holds `note_off_room(capacity)`; an event that is not a note-off is refused once `capacity` are held | The ring that decides what a slow reader loses should keep what a hung note needs: the same rule the ports follow (§8). The check is the producer's own occupancy, which can only be an underestimate, as in `TapWriter` |
| Loss | A refused event is counted once in an `Arc<AtomicU64>` shared with the reader: `EventTapReader::take_lost() -> u64`, events, cleared by the read. A block with no reader writes nothing and counts nothing | As `TapReader::take_lost`. The count is also the session's drop report (`MidiOutDropped`) |
| Reader | One consumer: `&mut self`, not `Clone`. `pop_into(&mut self, &mut [TapEvent]) -> usize`, `available()`, `id()`, `watch()` | A second consumer is a second tap. Reading advances the read index |
| Missing port | The port goes missing later (node replaced by one with fewer event outputs, node removed): the compiler emits no op, the writer stays in the plan's tap table, the commit succeeds, the reader sees no events and is not told, and nothing counts as lost. Opening refuses a source that does not exist now: `MissingSource::EventPort { node, port, width }` (the enum is `#[non_exhaustive]`; `Port` names an audio output) | Silence is the absence of events, so there is nothing to write. The writer is kept so a later commit that restores the port resumes it |
| Closed and fork | A closed tap is not in the topology and compiles to no op, and its writer is freed on the control side with the old state. A fork carries none. Reopening an id replaces the tap. Reader dropped first: the tap writes nothing until reaped (`reap_taps` takes event taps too) | As `Op::Tap`. An export fork has no wire to send to, which is what `MidiOutNode`'s discarding fork did |
| Commit | `EventTapWriter` rides the commit in a new `Commit.event_taps: Vec<(EventTapId, EventTapWriter)>`, carried by id across plans, as the audio writers are (`exec.rs:1307`) | No new mechanism. `EventTapId` is its own type: the audio `TapId` names a `Topology.taps` key, and the two maps must not share a namespace |

**The type: its own `EventTapReader`, not a generic `TapReader`.** `TapReader`
is a set of `f32` lanes read in whole frames, and it is a `Source`, an
interleaved stream with a layout. An event tap has no lanes, no layout, a
variable count per block and a 24-byte element. Making `TapReader` generic would
carry a lane count and a `Source` impl into a type that has neither. The
boundary in one sentence: *`EventTapReader` pops the MIDI events a graph port
carried, each with the frame it was due on.* `TapId`, `Observer` and the
`lost`/`alive` plumbing are a few lines and are written again, not shared
through a trait, because the sharing would be the only thing the two readers
have in common. If a second event reader appears, a small private struct for
the shared `lost`, `alive` and `read_is_held` logic is the refactor, not a trait.
It lives in its own file beside `tap.rs` and does not change `TapWriter` or
`TapReader`.

**The op.** `Op::EventTap { tap: u32, src: u32 }` reads one event slot through
`EventSlots::ev` and pushes `Event`s with `frame = Env::frame_at(offset)`; the
frame reaches the op as the block's start frame, the one datum the executor
already holds per block. The writer is claimed through the same split-claim
as `taps` (`par.rs:282`), a new `event_taps` slice. The op never touches a unit,
a ring or an output directly. Its read of the slot makes the producing node a
task dependency, as `Op::Tap` does for an audio slot (`em.read_event`, beside
`em.read_aref`).

**The cost class.** `EVENTS`, with `Op::EventDelay` and `Op::EventMerge`
(`cost.rs:218`): a ring push per event, nothing per frame. The PR records a
measurement beside the audio tap's, as the cost model asks.

**Lock-free structure: none new.** `ringbuf`'s `HeapRb<TapEvent>` split, which
`tutti-graph` already depends on, one producer (the executor) and one consumer.
No `tutti-rt` addition and no loom model is needed, because nothing here is a
new structure; the tests below pin the code path as `tests/tap.rs` does.

```rust
// tutti-graph
pub struct EventTapId(pub u32);
pub struct TapEvent { pub frame: Frame, pub ump: Ump }       // Copy
pub struct EventTapSpec { pub source: EventOut, pub align: TapAlign }
pub struct EventTapReader { /* ring consumer, lost, alive */ }
impl EventTapReader {
    pub fn pop_into(&mut self, out: &mut [TapEvent]) -> usize;
    pub fn available(&self) -> usize;
    pub fn take_lost(&self) -> u64;
    pub fn id(&self) -> EventTapId;
    pub fn rate(&self) -> SampleRate;   // the scale of every frame it pops
    pub fn watch(&self) -> Weak<()>;
}
impl Editor {
    pub fn open_event_tap(&mut self, id: EventTapId, source: EventOut,
                          align: TapAlign, capacity: usize)
        -> Result<EventTapReader, MissingSource>;
    pub fn close_event_tap(&mut self, id: EventTapId);
}

// tutti-session
impl Graph {
    pub fn open_event_tap(&mut self, source: EventSource, align: TapAlign,
                          capacity: usize) -> Result<EventTapReader, MissingSource>;
}
```

### Stage E1 as built

Built as specified, except where the code's own pattern for the audio tap
decided otherwise:

- **`EventTapSpec::source` is `Option<EventOut>`.** `Editor::remove` sets it to
  `None`, as it sets an audio tap's lane to `Source::Zero`: a key is the
  author's and can be inserted again, and a tap left naming it would read the
  new node (`a_removed_node_reads_nothing_and_a_reused_key_is_not_read`). A node
  that is *replaced* keeps its key, so a narrowing and a restoring resume the
  stream, as the table says.
- **The op takes `&Env`, not a start frame.** `Op::EventTap { tap, src }` calls
  `Env::frame_at(offset)` of the block's own `Env`, which is the datum the
  survey named, held by both the serial and the pooled path. The ring's
  occupancy, read by the producer, can only be an *over*estimate (the reader
  only drains), so an event may be refused that the reader had just made room
  for, and none is taken past the ring; the design said "underestimate" of the
  vacancy.
- **`EventTapReader::available` is a `usize`** (events, like `capacity`).
- **`MissingSource` was already `#[non_exhaustive]`:** `EventPort` is a new
  variant, not a breaking change.
- **The reference interpreter has no version.** It knows no taps, audio or
  event, so there is no event-tap arm to add to it; the differential suite's
  audio-tap test holds the op to the reference's outputs, and its harness hands
  the reference its shapes without giving the editor any, which `open_event_tap`
  needs. The event tap is held to analytic figures instead (a script's frames,
  a declared latency, the ring's size) and to the serial render.
- **`NodeMode` is not in the graph yet** (lane 3, M1), so the bypassed-or-muted
  row has no test; M1 owns it.
- **Q4 is a pull, not a hook.** A reader belongs to the thread that drains it,
  and a re-prepare cannot hand that thread a new one, so
  `Graph::reopen_event_taps() -> Vec<EventTapReader>` reopens every tap at the
  graph's current rate (each replaces its id) and the caller that re-rates the
  graph (E3's hardware out, in the session) calls it and takes the readers. The
  session does not call it for a caller's taps.
- **No cost measurement was taken.** The op is priced in the `EVENTS` class
  (`cost.rs`), as the table says; the audio tap's `COPY` has no measurement
  beside it either, and a ring push per event is not a per-frame cost.

### The event inlet

`Graph::open_event_inlet`: a ring a thread of your own fills, drained into one
event output port of a node. Counted, never blocked on. The inverse of the tap,
as the audio inlet is.

| Question | Decision | Reason |
|---|---|---|
| The node | `EventInletNode` in `tutti-graph`, an inlet implementation like `InletNode`, not a node an author adds: no inputs, one event output, `Cost::Light`, event capacity the ring's | The graph already has a node-shaped place for "something outside feeds a port". An op would need the arena to wake it with no input edge; the node needs nothing new |
| What the producer pushes | `EventInletWriter::push(at: At, ump: Ump) -> Result<(), Refused>`. `At::NextBlock` is "as soon as possible": the next block's first frame. `At::Frame(f)` is a frame on the graph's clock. `At::Beat` is refused (`Refused::Beat`): the producer has no transport, as `TimedSender::send` has none | `At` is the type a command takes elsewhere (`MotionFsm::schedule`), and `Env::due(At)` resolves it already. The node does not invent a second rule for late or early |
| The clock the frame is on | The graph's, as `Moment { frame, rate }`: `EventInletWriter::clock() -> Moment` reads where the node's last block ended, published by the node through a `tutti-rt` `SeqWriter` (the `TimedQueueNode` mechanism). `push_at(moment, ump)` names an instant that survives a re-prepare at another rate: the node rescales waiting events with `Frame::rescaled`. `Moment` moves to `tutti-types`, which already holds `Frame` and `SampleRate` | The frame clock counts frames at the current rate; a re-prepare moves it to the same wall time at the new rate (`timed_queue.rs` docs). A raw frame read before and sent after is wrong, a `Moment` is not |
| A timestamp from another clock | **Not this stage.** The writer takes graph-clock frames or `At::NextBlock` only. A producer holding an OS timestamp needs `ClockMap` (§5, "Timing at the edge", built in stage 4c part 1) to give a `Moment` (`moment_at`), and calls `push_at`. A producer that has no better stamp pushes `At::NextBlock`, which is what `MidiKeys` does | `ClockMap` is pure and takes two clocks' readings. The inlet's contract does not change when it lands: it only gives producers a better `Moment` |
| What the node does with the ring | Each block: drains the ring into a waiting list sized once at open (the ring's capacity), resolves each entry with `Env::due`: `In(offset)` is emitted on that offset, `Late` is emitted at offset 0 and counted late, `NotYet` waits. Emission order is due frame, then arrival order, so a note-off sent after its note-on never overtakes it | The waiting list is `TimedQueueNode`'s, kept whole. It is scheduling, not MIDI, and every producer that names a frame needs it |
| Priority | When the waiting list is full, the latest-due event that is not a note-off is evicted; a note-off or CC 120 or 123 is kept in preference to any other. The predicate is one function in `tutti-ump` (`Ump::is_note_off` today; the panic controllers join it there, since `tutti-ump` owns the bit positions) | The rule `TimedQueueNode` has, and the one §8 gives ports. `tutti-graph` already knows the note-off (`note_off_room`) |
| Capacity | An event count named at open: the ring, the waiting list and the port's declared capacity are that number. The port refuses what it cannot take and the node counts it with `Io::count_lost`, so an export watching `dropped_event_count` sees it | As `MidiQueueNode` and `TimedQueueNode` today. Default 256 events, which is theirs |
| Loss | Three counts, each in a shared atomic read from the writer side (`EventInletWriter::lost()`, `late()`; one `take`-style read each): refused at the ring because it was full (the writer counts, never blocks), evicted from the waiting list, and refused by the port. The writer's `push` returns `Err(Refused::Full)` so a caller that can say "not now" does. Late is a count, not a loss | The audio inlet's rule: what fills or empties the ring is counted, never blocked on. The writer cannot block: `push` is a ring push |
| Producers | **One.** `EventInletWriter` is `Send`, not `Clone`, `push(&mut self, ..)`. Several producers are several inlets; the graph's event fan-in (`Op::EventMerge`, stable in source order) merges them, as audio fan-in sums | The ring is `ringbuf`'s SPSC, and a second producer would need a new lock-free structure with a loom model for a case the graph already answers. See Open questions: `InstrumentHandle` is `Clone` today |
| Fork | **Silent**: the fork is a node with the same port and an empty ring, and no writer. It is not unforkable, which the audio inlet is | An offline render has no live events: nothing arriving is the right result, and it is what `MidiQueueNode`, `TimedQueueNode` and `MidiOutNode` do now. Refusing the fork would stop the export of any project with a keyboard in it. The audio inlet differs because silence in place of a live signal would be an export that looks complete and is not |
| Reset | `reset` keeps the ring and the waiting list: a note-off waiting must still arrive | As both queue nodes |

```rust
// tutti-graph
pub struct EventInletWriter { /* ring producer, SeqReader<Reading>, counters */ }
impl EventInletWriter {
    pub fn push(&mut self, at: At, ump: Ump) -> Result<(), Refused>;
    pub fn push_at(&mut self, at: Moment, ump: Ump) -> Result<(), Refused>;
    pub fn clock(&self) -> Moment;
    pub fn lost(&self) -> u64;
    pub fn late(&self) -> u64;
}
pub enum Refused { Full, Beat }
pub struct EventInletNode { /* ring consumer, waiting list, SeqWriter */ }
impl EventInletNode {
    pub fn pair(capacity: usize) -> (Self, EventInletWriter);
}

// tutti-session
impl Graph {
    pub fn open_event_inlet(&mut self, capacity: usize) -> (NodeKey, EventInletWriter);
    pub fn add_event_inlet(&mut self, node: EventInletNode) -> NodeKey;   // a ring the producer's layer built first
}
```

There is no `RateMismatch`: the audio inlet refuses a producer of another rate
because it cannot resample. An event carries a frame and a `Moment` a rate, so
the rate is the stamp's, and nothing is resampled.

**Lock-free structure.** `ringbuf` SPSC for the ring and `tutti-rt`'s `SeqWriter`
and `SeqReader` for the clock reading; both exist, and the sequence lock
has its model. No new structure, so no new loom model. The waiting list is a
`Vec` allocated at open and sorted by insertion (the `in_offset_order` code
today); it allocates nothing in `process`.

### Stage E2 as built

Built as specified, except where the code decided otherwise:

- **`clock()` is `Option<Moment>`.** The node has published nothing before its
  first prepare, so there is no honest instant to return; the deleted
  `TimedSender::clock` was `Option` for the same reason, and the signature follows it.
- **The counts are four, and cumulative plain loads.** `lost()` (ring full),
  `evicted()` (waiting list), `refused()` (port) and `late()`, each an
  `AtomicU64` shared with the node and read with `Relaxed`; none is a
  `take`-style read that clears. The design named `lost()` and `late()` and
  said the other two in prose.
- **A port refusal is counted by the port already.** `EventWriter::push`
  counts every refusal in the executor's `dropped_event_count`, so the node
  does not call `Io::count_lost` too (that would count it twice); it adds
  only its own `refused()`. A port cannot refuse in a graph (its declared
  capacity is the list's, and a note-off has room past it), so the figure is
  reached only where the port is narrower than the node's capacity, as the
  `contract::Direct` harness's is.
- **Capacity 0 or above `u32::MAX` panics** (`# Panics`), where the design said
  nothing: a port's capacity is a non-zero `u32`.
- **`NextBlock` is not late.** It is stored as its own `When` and sorts at the
  block's first frame; only an `At::Frame` already behind the block counts
  late, at arrival or, for an event that waited through blocks the node did not
  run, when the block finds it behind.
- **The silence skip needs no tail.** The node has no audio input, so the
  executor never parks it (doc 013, "The silence skip"); it declares none,
  where the audio inlet declares `Tail::Unbounded`. A tail on a node whose fork
  is silent would also make an export's tail unbounded.
- **`TimedQueueNode` was a thin user until E4, which deleted it** (see
  "Stage E4, as built"). `Moment` left `tutti_midi_runtime` and `tutti_session`
  for `tutti_types`, with no re-export; the node-level tests of the waiting
  list moved to `tutti-graph/tests/event_inlet.rs`.
- **The panic predicate is `Ump::is_panic_controller`** (CC 120 and 123 of a
  MIDI 1.0 or 2.0 Control Change), beside `is_note_off`.
- **The parallel test is in the inlet's own file**, not in `tests/executor/parallel.rs`'s
  `build`: that graph has no handle to pre-fill a ring before each run. The
  test compares the serial and pooled audio bit for bit and inlet 1's tap
  against its script, with the ring pre-filled and every participant under
  `assert_no_alloc`.
- **`tutti_session` re-exports no inlet type.** Like `InletNode`, the node
  and writer are named from `tutti_graph` (and the `tutti` prelude).

### Stage E3 as built

The hardware out is an `EventTapReader` on the clock node's event output
(`TapAlign::ToOutput`, 1 024 events: the deleted ring's size), opened in the
builder and held by the session's MIDI state; `Phase::MidiOut` reads it dry
into the `UmpOut` and adds `take_lost` to the drop count that
`SessionEvent::MidiOutDropped` reports. `MidiOutNode`, `MidiOutControls`,
`MIDI_OUT_CAPACITY`, `MidiNodes::hardware_out` and `out_controls` and the
`Feeder::Clock` variant are deleted. Built as specified, except:

- **A single-tap reopen: `Graph::reopen_event_tap(id)`.** The doc's Q4 had the
  session call `reopen_event_taps()` and take its readers, but that replaces
  *every* tap's reader, a caller's included, and the readers dropped by a
  session that wanted only its own would be reaped and kill the others. The
  one-tap form reopens the hardware out's tap alone; `reopen_event_taps` is the
  loop over it.
- **The re-rate is followed in two places.** A device restart's hook
  (`Session::rerate`) reopens the tap after the graph's re-prepare and commits;
  `Phase::MidiOut` compares the reader's `rate()` with the graph's before each
  drain, which covers `Graph::set_sample_rate` (no hook). Either drains the old
  reader first. `Midi::hardware_out_rate` reports the scale the frames are on.
- **`MidiOutRouter::route` takes `(Frame, Ump)`, and the rebasing moved into
  `UmpOut::send_frames`, not away.** The stamper (`JrStream`) reads offsets in
  a block, and a drain is the block: the batch is rebased onto its earliest
  frame, so the stamper sees the events' spacing. The `u32` wrap-around
  rebasing remains in `drain_receiver_through` only, for the host's own mailbox
  (raw `u32` stamps), which E4 audits. What the stamper does not see is the
  gap *between* two drains (the JR Clock advances by each batch's own span, as
  before); stamping the gap needs `ClockMap`.
- **The tap's loss joins the drop count without the "no output" warning**
  (`MidiOutDrops::record_refused`): there is an output to blame, and the event
  says it.
- **Tests:** the nodes' tests that used `MidiOutNode` as a sink
  (`rt_no_alloc`, `cost_classes`, `clock_tempo_map`, `edge_nodes`, the hardware
  poll gate) read a tap through a small test sink (`tests/common`) with the same
  properties; the session's own are `tests/midi/midi_out_tap.rs`, the device restart
  test and `midi.rs`'s router tests. A note-off's room past the ring is the
  tap's, pinned in `tutti-graph/tests/event_tap.rs`; the hardware out's clock
  sends none.

### The parallel executor

- **Tap.** An op, `EVENTS` class, claims its writer and reads one slot through
  `EventSlots`. It reaches no unit, ring or output. The render must be
  bit-identical to the serial one: a tap's content is a function of its source
  slot, so the parallel suite compares the tap's events serial against pooled.
- **Inlet.** A node, `Cost::Light`, so a block of event-only work may be packed
  with its neighbours. Nodes communicate only through ports: the ring is not a
  port, it is the boundary, and exactly one node pops it. A render that includes
  an inlet is not bit-comparable to another run unless the ring was filled before
  the run, the same limit as the audio inlet; the serial-equality test pre-fills.
- **No allocation.** Both run inside `assert_no_alloc` on every participant of
  the pool (`TestPool::no_alloc`, as `tests/inlet_no_alloc.rs`).
- **`unsafe`.** None: `tutti-graph` stays `#![forbid(unsafe_code)]`.

### What becomes of the nodes

| Today | Becomes | What moves where |
|---|---|---|
| `MidiOutNode`, `MidiOutControls`, `MIDI_OUT_CAPACITY` | **Deleted (E3, built).** The session opens an `EventTapReader` on the clock node's event output, `ToOutput`, and `Phase::MidiOut` drains it | `tutti-session` owns the open and the policy. `drain_receiver_through`'s rebasing goes: a tap's frames are real, so `MidiOutRouter::route` takes `(Frame, Ump)`; `UmpOut`, `JrStream` and the stamper stay where they are. The drop report reads `take_lost`. `MidiNodes::hardware_out` and `out_controls` go. `tutti-midi-hardware` stays the OS backend and the pump |
| `MidiQueueNode` | **Deleted (E4).** `EventInletNode` with `At::NextBlock` | `MidiKeys` (`tutti-midi-runtime`) is the key helpers (`note_on`, `note_off`, which build a `Voice2` into a `Ump`) over an `EventInletWriter`, which it derefs to; the graph builds no message. The session's live input (`LiveInput`) holds it behind the session's lock |
| `TimedQueueNode`, `TimedSender`, `TimedSendError`, `Moment` | **Deleted (E4).** The node and its waiting list are `EventInletNode`, the sender is `EventInletWriter`, the error is `Refused`, `Moment` is in `tutti-types` | What remains in `tutti-midi-runtime` is nothing of it. `InstrumentHandle` (`instrument.rs`) holds the writer behind the session's lock (below) |
| `MidiMailbox`, `MidiReceiver`, the `MidiOut` trait | **Audited in E4; E3 has landed: no graph crossing remains, the mailbox stays for the host's own outbound path, the trait stays.** `MidiMailbox`, `MidiSender` (its queue half) and `MidiReceiver` are used by the host's own outbound path, read in `drain_receiver_through`, and the clock master's tests; they go when that path is its own. `MidiOut` is not a graph crossing at all: it is the hardware's terminal sink (`tutti-midi-hardware`'s `MidiSession`, every backend's output, `UmpOut`), and it stays | See "Stage E4, as built" |
| `MidiInputNode` | **Split (E5, built).** The wire side (the `MidiIn` poll, `HardwareMidiDrain`, the arrival-time offset) is gone: a device's thread stamps each arrival and pushes it into its own `EventInletNode` (`DeviceInlets`, `DeviceInput`). The node is `MidiIngestNode` (`tutti-midi-runtime`): the translator, MPE and per-channel split, with one event input for each device slot and 17 outputs for each Group | The merge across devices is the graph's fan-in at the listener; the translator and MPE state are per slot (see item 8 of the open questions, decided in E5). `MidiIn` is deleted |

`ClockNode`, `MidiClipNode`, the harmony node, `compile_routes` and
`MpeIngest` compute events and stay. `bevy-tutti` is parked and is not
updated: the names it uses disappear, as CLAUDE.md allows.

### Tests

From the definition, none against another render, as the rest of the graph's
tests are (CLAUDE.md, "Direction"). Each names its mutation.

The tap (`tutti-graph/tests/event_tap.rs`, and the differential and parallel suites):

- **Every event at its frame, across block sizes.** A scripted source (a clip's
  events at known frames) read through the tap at block sizes 1, 7, 64 and 513
  yields the same `(frame, ump)` list, equal to the script. Mutation: read
  `offset` as the frame; stamp the block's start with the previous block's.
- **Loss is counted.** A ring of capacity C and a reader that never drains: the
  tap holds C non-note-offs, `take_lost` is the rest, and a note-off past C is
  kept up to the reserve. Mutation: count each lost event twice; drop the
  reserve; push with no reader.
- **A port removed reads nothing.** Remove or narrow the tapped node: the commit
  succeeds, no op is emitted for it, `take_lost` is 0, and restoring the port
  resumes the stream. Mutation: fail the commit; count the missing port as loss.
- **Aligned against raw on a latent path.** A source behind a node of declared
  latency L, beside an output path of latency M greater than L: raw frames are the
  script's, aligned frames are the script's plus the compiler's gap, an
  analytic figure (`M - L`). Mutation: skip the delay; delay a raw tap.
- **Closed, fork, reopen.** A closed tap leaves no `Op::EventTap` in the plan; a
  fork carries none; reopening an id replaces the tap; a reader dropped first is
  reaped on the control side, and dropped on the audio thread panics in debug
  (as `a_reader_dropped_on_the_audio_thread_panics`).
- **Only MIDI is copied.** A port carrying a ramp and a note: the tap holds the
  note. **Opening refuses** a node or port that does not exist
  (`MissingSource::EventPort`).
- **Parallel.** The pooled render's tap list equals the serial one's, and the
  pool runs inside `assert_no_alloc` (`TestPool::no_alloc`).
- **One consumer**, a `compile_fail` doctest that `EventTapReader` is not `Clone`.

The inlet (`tutti-graph/tests/event_inlet.rs`):

- **An event pushed at frame F appears on F's block at offset F minus the block's
  start**, for block sizes 1, 7, 64 and 513 and for F at every position in a
  block, including the last frame and the first of the next. Mutation: off by one
  at the block edge; emit at offset 0.
- **As soon as possible** (`NextBlock`) is the next block's first frame; **late**
  lands at offset 0 and is counted late; **not yet** waits across blocks and is
  not lost. Mutation: drop a late event; emit an early one.
- **Order.** Equal frames leave in the order pushed; a note-off sent after its
  note-on does not overtake it.
- **Overflow is counted and nothing blocks.** A producer faster than the graph:
  `Refused::Full`, `lost()` equals the refusals. A full waiting list evicts the
  latest non-note-off and keeps the note-off; a port narrower than the burst
  counts the rest in `dropped_event_count`. Mutation: evict the note-off;
  count in `late` instead of `lost`.
- **Rate.** A `Moment` read at 48 kHz and sent after a re-prepare at 44.1 kHz
  lands at the same wall time (`timed_queue`'s existing figures, now on the node).
- **Fork is silent**: an export of a graph holding an inlet commits and
  renders no event from it.
- **Parallel serial-equality**, with the ring pre-filled before each run, and the
  **no-alloc gate** on every participant (`tests/inlet_no_alloc.rs`'s shape).
  A `compile_fail` doctest that the writer is not `Clone`.

The migrations keep their existing tests (`edge_nodes.rs`, `event_loss.rs`,
`rt_no_alloc.rs`, `cost_classes.rs`, `rt_no_alloc_hardware_poll.rs`) moved to
the new owner, not rewritten: the property each pins is the one the primitive
now carries.

### Open questions

Each with the recommendation to take unless the owner says otherwise.

1. **Which event kinds a tap copies.** Recommended: `Midi` only. If a consumer
   wants the param stream out of the graph, it is a second tap, not a wider one.
2. **One source per tap.** Recommended: one; a reader merges taps by frame. If
   the hardware out ever has two sources, the session merges two readers (a
   small k-way merge by `frame`). The alternative is a list of sources on one
   `Op::EventMerge` and a tap on its result.
3. **Several producers.** `InstrumentHandle` is `Clone` and `MidiSender` is MPMC
   today. Recommended: the primitive is SPSC, the session wraps the writer in a
   `Mutex` for the handles that clone (the lock is on control threads, never the
   audio thread), and a hardware callback gets an inlet of its own. The
   alternative is an MPSC ring in `tutti-rt` with a loom model, which is a
   larger change for a case a lock on the control side already covers.
   **Decided in E4:** the lock, as recommended: `InstrumentHandle` and
   `LiveInput` stay `Clone`, because a host hands a handle to a UI thread and
   a feeder, and `Midi::live_input` returns the one it has already.
4. **Re-prepare at another rate and the tap. Decided (review of #458).** A
   tap's frames are on the graph clock at the rate prepared when they were
   written, so frames from before and after a re-prepare at another rate are on
   different scales. The session reopens its event taps when it re-prepares, so
   one reader's frames are always on one scale, the rate it was opened at
   (`EventTapReader::rate() -> SampleRate`), which is what a reader converts
   with. A rate field per `TapEvent` was rejected: it costs every element for a
   rare event.
5. **`At::Beat` on the inlet.** Recommended: refused, as the deleted `TimedSender` refused it:
   a beat waits on the transport and may wait forever, which makes the waiting
   list's bound unclear. Revisit if a producer needs it.
6. **The inlet's fork is silent, the audio inlet's is refused.** Recommended:
   silent, as above. The owner should confirm the two differ on purpose.
7. **Where the panic-controller predicate lives.** Recommended: `tutti-ump`, beside
   `is_note_off`, because it reads bit positions; the name is the PR's.
8. **One inlet and one ingest node per hardware device. Decided in E5: one inlet
   per device, one ingest node with a slot per device.** The recommendation was a
   node per device, which changes where the translator's state lives (per
   device, not shared across merged ports). The state is per device as asked,
   but a device connects from any thread and the routes a host declares are
   compiled from closures the session does not keep, so a node per device would
   leave the routes to be rebuilt at every hot-plug. The ingest node therefore
   has a fixed number of event inputs, one per slot
   (`SessionBuilder::midi_devices`, 8 by default), holds a `Translator` and a
   `MpeIngest` for each, and a hot-plug wires the new device's inlet to a free
   slot's input (`Graph::set_event_sources`) and restarts that slot
   (`MidiIngestControls::restart_slot`): the routes, which name the ingest
   node's outputs, never change. A device past the slots is refused
   (`Error::NoFreeSlot`).
9. **The default alignment of the hardware out's tap.** Recommended: `ToOutput`,
   so MIDI leaves with the audio. The pump still owes the device's own output
   latency, which only `ClockMap`'s output side can know.
10. **`MidiMailbox`'s other users.** A host's outbound mailbox is not a graph
    crossing. Recommended: audit in E4; if one remains it is named and kept, and
    `MidiOut` stays for it. **Audited in E4:** the mailbox's remaining users
    are the out node and `drain_receiver_through` (E3's) and the clock master's
    tests; `MidiOut` is the hardware sink. Both stay; the mailbox goes after E3.

### Staging

Each PR passes CI by itself. PRs that touch `tutti-graph` are reviewed by the
graph's owner. CLAUDE.md asks that design 013 be updated when the code it
describes changes: each graph PR edits that document's primitive table.

| PR | Holds | Touches `tutti-graph` |
|---|---|---|
| **E1** (done) | The event tap: `EventTapId`, `EventTapSpec`, `Topology.event_taps`, `EventTapWriter` and `EventTapReader`, `Editor::open_event_tap`, `MissingSource::EventPort`, the compiler's op and `DelayKey::EventTap`, `verify`, `cost`, the serial, pooled and reference interpreters, the commit carry, reap. The tests above. Then `tutti-session`: `Graph::open_event_tap` and the `Reclaim` reap | Yes |
| **E2** (done) | The event inlet: `EventInletNode`, `EventInletWriter`, `Refused`, `Moment` to `tutti-types`, the panic predicate in `tutti-ump`, the waiting list moved from `TimedQueueNode`. Then `tutti-session`: `Graph::open_event_inlet` and `add_event_inlet` | Yes, and `tutti-types` and `tutti-ump` |
| **E3** (done) | The hardware out: the session reads a tap on the clock; `MidiOutNode`, `MidiOutControls`, `MIDI_OUT_CAPACITY` and `MidiNodes::hardware_out` deleted; `drain_receiver_through` and `MidiOutRouter` take frames | No. After E1 |
| **E4** (done) | The queues: `MidiQueueNode` and `TimedQueueNode` deleted, `MidiSender` slimmed to the key helpers over `EventInletWriter`, `InstrumentHandle` and the session's live inputs moved, `MidiMailbox` audited | No. After E2 |
| **E5** (done) | `MidiInputNode` split into the OS-side inlet and `MidiIngestNode`; the engine's block clock to the device threads; one inlet per device; the translator per device | No. After E2 and `ClockMap` |

**Stage E4, as built.** Deleted: `MidiQueueNode` and `MIDI_QUEUE_CAPACITY`,
`TimedQueueNode`, `TimedSender`, `TimedSendError` and `TIMED_QUEUE_CAPACITY`,
and the key helpers (`note_on`, `note_off`) of the mailbox's `MidiSender`. Added:
`MidiKeys` and `LIVE_INLET_CAPACITY` (256) in `tutti-midi-runtime`. Callers
moved to `Graph::open_event_inlet` (the session's live input and an
instrument's queue, through one private `SharedKeys`: a `MidiKeys` behind an
`Arc<Mutex<_>>` taken on control threads only), to `EventInletNode::pair` (the
plugin tests' keyboard, the `rt_no_alloc` gates) and to `EventInletWriter::push`
(`At::NextBlock` replaces an offset of 0). `LiveInput` is no longer a `Deref` to
a mailbox sender: it has `send`, `send_at`, `note_on`, `note_off`, `clock` and
`dropped`, as `InstrumentHandle` does. Behaviour that differs from
`MidiQueueNode`: an event stamped past the block waits for its block instead of
being clamped onto its last frame (the inlet's rule), and a full ring refuses
with `SendError::Full` and counts in `lost()`. Two crate-level deviations from
the plan above: the key-helper wrapper is not named `MidiSender` (that is the
mailbox's push half, which `MidiOutNode` still uses, so it cannot change
shape before E3), and the mailbox is not deleted (above).

**Stage E5, as built.** Added: `BlockClockReader` and `BlockReading` (`tutti-engine`: the
engine's block start as `monotonic_ns` and frame, over `tutti-rt`'s sequence cell, which
already has a loom model; `Engine::block_clock`), `ArrivalClock` (`tutti-midi-runtime`), `InletCounts`
(`EventInletWriter::counts`: the inlet's four counts as a view readable off the
writer's thread), and in `tutti-midi-runtime` `DeviceInlets` (the slots, the
changes the graph's owner applies, the clock lent to each thread through an
`ArcLend`, and the lead), `DeviceInput` (one device's end: it holds a `ClockMap`,
stamps and pushes; `Send`, not `Clone`, no allocation, no lock) and
`MidiIngestNode`, `MidiIngestControls`. In `tutti-midi-hardware`,
`InputProducerHandle` wraps a `DeviceInput` and `HardwareMidiInputs` is now a
control-side list of ports over the `DeviceInlets` (`open_input_port` can refuse
with `NoFreeSlot`; a port can be inactive, which discards its events at the
thread instead of queueing them). The session builds the `DeviceInlets`, hands
the devices the engine's clock (`SessionBuilder::build_offline` and the device
build), and in `Phase::Poll` adds each connected device's inlet to the graph,
wires it to its slot, commits, and takes the inlet out when the device closes
(`midi_input_phase`); a device that connects and closes before the poll never
reaches the graph. Deleted: `MidiIn` and `MidiInputNode`, `HardwareMidiDrain`
and `cycle_start_read_all_inputs` (with its `Instant` to `Offset` conversion,
`CYCLE_SCRATCH_CAP`, the port set's `ArcLend` and its `collect` and
`retired_sets`), `HardwareMidiInputs::set_sample_rate` (the map follows the
rate from the readings) and `DEFAULT_FIFO`. Behaviour that differs: an event
reaches the graph in time order, not arrival order across ports; a block no
longer defers events it had no room to take (the inlet's list holds a ring's
worth, and what does not fit is counted, `lost` and `evicted`); `MidiInputDropped`
counts the inlets' lost events. The hardware crate still names `tutti_graph::UmpAt`
on its output side (`MidiOut::queue`), which the output scheduling lane
retires; its input side names no graph type.

E3 and E4 are independent of each other. The loom, miri and doc gates need
nothing new: nothing here adds `unsafe` or a lock-free structure.

## 6. Crates

| Crate | Layer | Holds | Depends on |
|---|---|---|---|
| `tutti-ump` | Packet | `Ump`, `Message`, codec, `MidiGroup`, `MidiChannel`, `CCNumber`, field types, MIDI 1.0 bytes and `Midi1Parser`, `Rpn` (an RPN address), `SYSEX7_MAX_PAYLOAD` and `split_stream` (a buffer of concatenated packets) | No tutti crate |
| `tutti-midi-protocol` | Translation | `Translator`, App. D, M2-115 scaling, MPE zone values (`MpeZoneConfig`, `MpeZones`, `MpeMode`), `PitchBendSensitivity` | `tutti-ump` |
| `tutti-midi-device` | Endpoint, Function Block | `Endpoint`, `Connection`, CI codec, JR | `tutti-ump` |
| `tutti-midi-hardware` | Transport | OS backends, no graph on the input side (a device's thread pushes into a `tutti-midi-runtime` `DeviceInput`; the output side still takes `tutti_graph::UmpAt`, until output scheduling); `#![forbid(unsafe_code)]` (stage 4d: the `-ffi` crates hold the `unsafe`) | `tutti-midi-device`, `tutti-midi-runtime`, the two `-ffi` crates |
| `tutti-alsa-ffi` | FFI | All ALSA `unsafe`, safe wrappers (`tutti-alsa-ump-sys` folded in as a private module) | Linux only |
| `tutti-coremidi-ffi` | FFI | The raw CoreMIDI calls the `coremidi` crate lacks, and the Mach host clock | macOS only |
| `tutti-midi-runtime` | Engine | Graph nodes, `ClockMaster`, and the inbound sync decoders (`MidiClockDecoder`, `MtcDecoder`, `SmpteTimecode`, `SmpteFrameRate`, `ClockTransportState`) | `tutti-graph`, `tutti-midi-protocol` |
| `tutti-graph` | Graph | Event ports hold `Ump`; the event tap and the event inlet | `tutti-types`, `tutti-ump` |
| `tutti-midi-file` | Files | SMF, Clip File (the byte codec and the path-level reader and writer) | `tutti-ump`, `tutti-tempo` |
| `tutti-tempo` | Time | `TempoMap` (beat to sample); not MIDI | `tutti-types` |

`tutti-midi-types` goes away. This is not a split for compile time, which is
already small. Dependencies enforce the layering: `tutti-ump` cannot call the
translator, so reading cannot translate, and `tutti-midi-hardware` cannot reach
the graph. The cycle that keeps `ump` and `translation` together today is mostly
`message()` calling `normalize`, and it ends with that call.

Moved out of `tutti-midi-types` in stage 5b: `SYSEX7_MAX_PAYLOAD` to `tutti-ump`
(tutti's cap on one SysEx7 message, for sizing a `SysexAssembler`; M2-104 section 7.7
sets no maximum), the four RPN address constants (`RPN_BANK_MPE`, `RPN_INDEX_MCM`,
`RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY`, `RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY`)
to one `tutti_ump::Rpn` with named addresses, and `PitchBendSensitivity` to
`tutti-midi-protocol` (an optional `serde` feature, because `MpeZoneConfig` persists
it). `PerNoteBendRanges` moves with the Plugins lane after these.

Stage 5b part (a) moved three more groups, with no shim left behind:

- `clip_file` (`write_clip_file*`, `ParsedClipFile`, `ClipHeader`, `ClipEvent`,
  `ClipNote`, `ClipFileError`, `CLIP_FILE_MAGIC`) to `tutti-midi-file`, which already
  held its path-level half. Its only production users were `tutti-midi-file` itself;
  `tutti-midi-runtime` (the same layer) named it in a doc comment alone and
  `tutti-midi-hardware` re-exported it, which it no longer does (it depends on no
  file crate). Part (b) moved its Flex Data onto `tutti_ump::Flex`.
- `sync/` (`MidiClockDecoder`, `MtcDecoder`, `SmpteTimecode`, `SmpteFrameRate`,
  `ClockTransportState`) to **`tutti-midi-runtime`**, not `tutti-midi-device`. The
  decoders speak `Bpm` and `Beat`, and `tutti-midi-device` may not depend on
  `tutti-types` (the same layer, and it takes no workspace crate but `tutti-ump`);
  the alternative was bare `f64` at its edge, which the unit rule forbids. The
  runtime is also where the one production user already is: `ClockMaster` builds
  `SmpteTimecode` and `SmpteFrameRate` to send MTC, and the decoders are its inverse
  and its test oracle (the loopback tests). `SmpteTimecode::to_seconds` now returns
  `Seconds` and `to_beats` a `Beat`. The decoders still have no consumer of their own;
  they are deleted, not kept, if a clock slave does not arrive.
- `PerNoteMap` to `tutti-polysynth` as a crate-private type, its only user (the
  slot table's note-id index).
- `fixed` (`q7_9_to_f32`, `q7_25_to_f32`) was deleted, with no caller: `tutti_ump::Pitch`
  holds the Q7.9 and Q7.25 words and `Pitch::note_number` is the one conversion.

Stage 5b part (b) emptied `tutti-midi-types` of everything but `MidiMessage` and
`translation/` (part (c) deleted those), again with no shim, and `midi2` had no user
left but them and `tutti-ump`'s oracle test. The rule that decided each item: what `tutti_ump` already has is
deleted and its callers move to `tutti_ump` (`Ump::from(Message)` to build, `decode` to
read); a packet-level item `tutti_ump` lacks is added there; state and policy go to the
one crate that owns them; an item with no caller outside its own tests is deleted.

- **`mpe/`** to `tutti-midi-protocol` (it holds only `tutti-ump` and
  `PitchBendSensitivity`, so it fits; nothing published or atomic is in it). The
  values are `MpeZone` (`Lower` and `Upper`), `MpeZoneConfig`, `MpeZones` (the one or two
  zones an instrument holds) and `MpeMode` (`Disabled`, `Zones(MpeZones)`,
  `SingleChannelRotation { channel: MidiChannel }`). `MpeZoneConfig` has private fields
  and `MpeZoneConfig::new(zone, member_count) -> Option<Self>`, which refuses a count
  outside `1..=15` (0 is "disable the zone" in an MCM and is no zone; nothing clamps),
  with `lower` and `upper` as `new` for each zone; `MpeZoneConfig::from_mcm` returns the
  `MpeZone` and the count (`0` included) and refuses an MCM off a master channel or
  above 15. `MpeZones::new(lower, upper) -> Option<Self>` refuses no zone at all, a zone
  in the wrong role, and two zones whose members share a channel. The lower zone takes
  `1..=l` and the upper `15-u..=14`, so they are disjoint exactly when `l + u <= 14`
  (sixteen channels less two masters; this section's earlier "more than 15" was off by
  one: 8 and 7 share channel 8). The persisted forms (`serde` feature) are read back
  through the same checks (`try_from` a private raw), so a document cannot hold what
  `new` refuses; the single-raw `check_range_contract` does not fit a composite, and
  the tests state each refusal. An MCM that would overlap the zones is ignored by
  `MpeIngest`. What went: `MpeZone::SingleChannel`, `MpeZoneConfig::single_channel`
  and `enabled` (no user; a zone was never disabled), the clamping constructors,
  `NoteRotationAllocator` (`MpeIngest` minted its ids and dropped them: the mode now
  keeps the one channel and ignores the rest, which is all a receiver owes a rotation
  sender, M2-104 Appendix C.3), and `MpeChannelVoiceMap`'s host-allocates half
  (`assign_note`, `release_note`, voice stealing: "not wired into any runtime path").
  What `MpeIngest` really used of the map, which note a member channel holds, is a
  private table in `mpe_ingest.rs` (one for both zones, since they share no channel).
  `ZoneInfo` was a private helper's return type and is `MpeIngest`'s `Role`.
- **`ump/`**: everything was a `midi2` constructor of what `tutti_ump::Message` builds,
  except these. Deleted as dead (no caller outside its tests): the chord, key,
  metronome and Flex text builders and readers (`flex_set_chord_name`,
  `flex_set_key_signature`, `flex_set_metronome`, `flex_text`, `push_flex_text`,
  `FlexTextReassembler` and the `Tonic`, `ChordType`, `Alteration`, ... aliases, which
  are `tutti_ump`'s `ChordName`, `KeySignature`, `Flex::Text` and kin), SysEx8
  (`sysex8_fragments`, `sysex8_header`, `sysex8_status`, `is_sysex8_abort`: the packet
  is `Data128::Sysex8`; no code fragments a SysEx8 payload, so no writer is added until
  one does), `jr_clock_value` and `jr_timestamp_value` (the callers match
  `Utility::JrClock` and `Utility::JrTimestamp` from `decode`), `Protocol`
  (`tutti_ump::Protocol`, which also keeps a reserved value; `UmpCapability` takes it,
  and an unknown protocol opens as MIDI 1.0), `program_change_bank_of` (a private
  `decode` in `message.rs`, until it goes) and the `midi2`-oracle test files. Added to
  `tutti_ump`: `split_stream`, the walk of a buffer of concatenated packets by Table 4,
  from `split_ump_stream` (it reads the same one size table as `Ump::words`; the old
  copy had a second). Not packet-level: the tempo helpers (`Bpm` against the Set Tempo
  field, 10 ns units per quarter note, M2-104 section 7.5.3), whose only users are the
  Clip File and `MidiFile::tempo_map`, are `tutti-midi-file`'s private `flex.rs`;
  `tutti-ump` has no unit crate. The Clip File now builds and reads its Set Tempo and
  Set Time Signature as `tutti_ump::Flex`.
- **`cc/`**: every constant was an alias of a `CCNumber` constant (nothing missing from
  `CCNumber`), so the aliases are deleted and callers write `CCNumber::BRIGHTNESS`.
  `cc::mapping` (`CCMapping`, `CCTarget`, `MappingId`) had no user but
  `tutti-midi-hardware`'s re-export, public fields and an unvalidated range, and is
  replaced by the CC modulation source of §9: deleted, not moved.
- **Root re-exports**: `midi2`, `midly`, `ArcLend`, `Beat`, `BeatDuration` and `Bpm` go,
  from `tutti-midi-types` and with them `tutti-midi-hardware`'s `midi2`, `midly`,
  `Protocol`, MPE and `CCMapping`, and `tutti-midi-runtime`'s MPE types. A user imports
  from the real crate. `tutti-midi-types` loses its `serde` feature and its
  `tutti-types`, `tutti-rt`, `bitflags`, `thiserror`, `serde` and `midly`
  dependencies. `tutti-polysynth`, `tutti-soundfont` and `tutti-session` drop theirs
  (the session takes `tutti-midi-protocol` for `MpeMode`); `tutti` re-exports
  `tutti-midi-protocol` as `tutti::midi_protocol`.

Stage 5b part (c) deleted `MidiMessage`, `translation/` and then `tutti-midi-types`
and `midi2` themselves, with no shim and no re-export (`tutti::midi` became `tutti::ump`,
the one place `tutti_ump` is reachable from the umbrella). Each user moved to what the
earlier stages built:

- **`MidiMessage` is `tutti_ump::Message`**, through `decode` and `From<Message> for Ump`;
  its four companions (`ControllerNamespace`, `NoteAttribute`, `PerNoteController`,
  `UnencodableMessage`) are deleted, because `Voice2` already distinguishes them
  (`Registered` and `Assignable`, `Attribute`, `RegisteredPerNote` and `AssignablePerNote`)
  and nothing used them. Its accessors had users only in tests, and each is covered by
  what `tutti_ump` has: `Ump::key_event` (a note on or off by key, either protocol),
  `Ump::is_note_off`, `tutti_midi_protocol::channel_voice` (the packet as MIDI 2.0, so
  a velocity is 16 bits whichever protocol it came in), and `Voice2::note`, added here
  (the note byte of the seven note-addressed messages, M2-104 section 7.4, with a
  spec-words test). `MidiMessage::NoteId` minting is gone: `NoteId::from_channel_note`
  is the one formula.
- **MIDI 1.0 bytes in** (`from_midi1_bytes`, `parse_midi1_bytes`, `normalize`) are
  `tutti_ump::Midi1Parser` (`message` for one padded message, `feed` for a stream),
  widened by `tutti_midi_protocol::widen_message` or a `Translator` where MIDI 2.0 is
  wanted. **Bytes out** (`to_midi1_bytes`, `to_midi1_messages`, `Midi1Messages`) are
  `tutti_midi_protocol::midi1` over `Voice1::bytes` and `System::bytes`.
- **The velocity-0 fold.** `normalize` folded a MIDI 1.0 Note On at velocity 0 into a Note
  Off (right: MIDI 1.0 Detailed Specification, "a Note On with a velocity of 0 is a Note
  Off"; M2-104 Appendix D.3.1) and, wrongly by §7.4.2, a MIDI 2.0 one too. Its one user
  was `MidiFile::iter`, which is where the fold now lives (`as_midi2` in `file.rs`), with
  the Note Off's release at 0 as a Clip File writes it. The MIDI 2.0 half is kept as it
  was and is pinned by a test that says it is this reader's choice and not the
  specification's; changing it is a decision for the owner.

`tutti-midi-runtime` keeps only graph nodes and what they need: the input, queue,
out, clip, harmony and clock nodes (the queue nodes went in E4) with `ClockMaster`, plus `MidiMailbox` and
routes (stage 3b deleted `gather_midi` and `BlockSplitter`), about 3,100 of its 5,700 production lines. Its
layout flattens to match. `jr_timestamp` moves to `tutti-midi-device` (`negotiate` was deleted in stage 4a),
`mpe_ingest` to `tutti-midi-protocol`, and the SysEx reassemblers are gone (stage 3c:
`tutti_ump::SysexAssembler` is the one, and `Sysex8PacketReassembler` had no production
caller). `outbound` goes away, along with the re-exports from
`tutti-midi-types`, and `pub mod` inside a private parent becomes `mod`.

`tutti-midi-hardware` is a transport and nothing else. Its re-exports go: `midi2`,
`midly`, the translator, the Clip File codec, MPE, `CCMapping`, the sync
decoders, the graph nodes, and a prelude that hands out `MidiInputNode`. A consumer
names the owner. So does its vocabulary: one crate says port, endpoint, connection
and device for overlapping things, and `MidiSession` reads as if it were
`tutti_session::Session`. It becomes `Endpoint` from `tutti-midi-device`, with `Port`
only for an OS's handle to a connection, and `async_port` (an SPSC ring, not async)
is named for what it is. A byte-stream source is read by `tutti_ump::Midi1Parser`;
`Sysex7ByteAssembler` had no product caller and is deleted.

MIDI follows the plugin hosts' FFI rule (design 016 §2.9): `unsafe` lives only in the
`-ffi` crates, and `scripts/check-midi-ffi.sh` (a sibling of `check-plugin-ffi.sh`) enforces it.

`sync`'s clock and MTC decoders have no consumer today beyond `ClockMaster`'s loopback
tests. They are in the runtime (stage 5b), and are deleted if a clock slave (§9) does
not use them. `MtcDecoder` ignores the quarter-frame bits the MTC text reserves ("receiver should
ignore!") and reports the assembled position plus the two frames the text says a receiver
"must keep" ("the information is now actually 2 frames old"). `cc::mapping` was deleted (stage 5b part (b); it had no user and is
replaced by a CC modulation source, §9), and `jr_timestamp` is rewritten in
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
- It was removed from the workspace at stage 5b part (c), with the old types, and
  `Cargo.lock` no longer names it. Until then, while the old types still depended on
  it anyway, it served as a test-only oracle: random words went through both
  decoders, and each disagreement was settled by reading the specification and pinned
  as an analytic test. The owner decided on 2026-11-07 that the specification is the
  only oracle, so `tests/midi2_oracle.rs` and the dev-dependency are deleted. What
  survives is what those tests named: a swap of two neighbouring numbers in a
  number-to-name table, or of two fields or flags, fails a spec-literal test in
  `tests/messages/spec_words.rs` or `tests/messages/spec_tables.rs`. Its SysEx7 comparison (the
  fragmenter's packets for every length around the 6-byte boundary) is
  `tests/streams/sysex_packets.rs`'s literal cases (one, seven, twelve and thirteen bytes, and
  a long run). The round-trip properties and the fuzz target check the codec only
  against itself (decode, encode, decode), so a mistake made the same way in both
  directions passes them; the spec-literal tests are what catch it. What the removal
  loses is independent agreement with a second decoder on words no table or figure
  pins, and the name-only comparison of the Flex and Stream families. The fuzz target
  is not run in CI yet; nothing here leans on it.

`midly` stayed in `tutti-midi-file` for the SMF container until stage 6b, which wrote
the reader and writer itself (below); the dependency is gone. Its parse error allocated,
so a plugin's stray status byte (`[0xF7, 1, 2]`) aborted a debug build under
`assert_no_alloc`.

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
- **Controllers go back with the notes.** The ledger also keeps, per protocol, Group
  and Channel, whether the hold pedals (CC 64, 66, 67, 69) are down and whether the
  pitch bend is off centre (512 `u16`s, `ledger/controllers.rs`; a MIDI 1.0 value of 64
  or more, a MIDI 2.0 one at or above `0x8000_0000`, is down). MIDI 1.0 Detailed Spec
  p. 31 (Stop): "any controllers not in their initialized position (pitch wheels,
  sustain pedal, etc.) should be returned to their normal positions", and A-5: a
  pedal on "takes priority over Note Off and All Notes Off until it is released", so
  `release_all` (the clip's stop, seek, loop wrap and replacement) writes the
  note-offs first, then a pedal-up (value 0) and a centred bend (`0x2000` or
  `0x8000_0000`) in the protocol the controller was sent in, for each that is off
  and only those. A reset the port refuses is owed like an off and goes out on a
  later block, after every owed off. Per-note pitch bend is not tracked: it ends with
  its note (M2-104), and every note is ended first.
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
  Sostenuto, CC 120 and 123, per-note controllers and pitch bend are not tracked at the edge (the clip's ledger returns the pedals and the bend, above).

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
6. **Files**, done (below).

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

**Stage 6, done.** `tutti-midi-file` is one file model over both containers, read
losslessly. Before it, the crate added real work on top of `midly` (musical time, note
pairing, one reader for SMF and Clip File) but had two read models from one parse
(`SmfFile`'s tracks and `ParsedMidiFile`'s merged stream, and a third for a Clip
File), dropped SysEx, time and key signatures and markers, merged Format 2 as if
parallel, rejected SMPTE division, and returned `Ok` on a truncated file. Writing
had no tempo map, track names or SysEx. (`pub use midly` and `SmfMessage` had exposed
`midly`'s types and `iter()` wrote each parsed message to wire bytes to parse it
again; the M3 lane below fixed both.)

What is built, and where it differs from the sketch this section started from:

```rust
pub struct MidiFile { /* private */ }          // container, division, tracks, tempo maps
pub struct Track { pub header: Vec<(Tick, FileEvent)>, pub events: Vec<(Tick, FileEvent)>, pub end: Tick }  // name() is derived; header is a Clip File's

pub enum FileEvent {
    Message(Message),      // SMF: Midi1 on group 0. Clip File: as stored.
    Unknown(Unknown),      // a Clip File packet no definition covers, as its words
    SysEx(Box<[u8]>),      // F0, any length, unlike a packet
    Escape(Box<[u8]>),     // F7: a continuation or an escape
    Meta(Meta),            // Text { kind, bytes }, Tempo, TimeSignature, KeySignature, Other { kind, data }
}

impl MidiFile {
    pub fn smf(format: SmfFormat, ticks_per_quarter: u16, tracks: Vec<Track>) -> Result<Self>;
    pub fn clip(ticks_per_quarter: u16, track: Track) -> Result<Self>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self>;   // truncation is Error::Truncated
    pub fn to_bytes(&self) -> Result<Vec<u8>>;
    pub fn tempo_map(&self) -> &TempoMap;               // tutti_tempo
    pub fn tempo_map_of(&self, track: usize) -> Option<&TempoMap>;
    pub fn seconds_at(&self, at: Beat) -> Seconds;      // tempo_map().seconds_at
    pub fn events(&self) -> Option<impl Iterator<Item = (Beat, &FileEvent)>>;  // merged, None for Format 2
}
impl Track { pub fn notes(&self) -> impl Iterator<Item = PairedNote>; }
```

- **The tempo map is derived, not a field to set.** Set Tempo stays an event of its
  track (`Meta::Tempo { micros_per_quarter }`, a Clip File's Flex Data Set Tempo), so a
  write gives back the exact event, a zero ("no valid tempo") and two on one tick
  included; the file builds its `TempoMap` from them in its constructors, steps at the
  tick each lands on, 120 BPM until the first when none is at tick 0, the later of two
  on one tick governing. The two cannot disagree. `seconds_at` is `TempoMap::seconds_at`
  (doc 020's "point `MidiFile::seconds_at` at `TempoMap`" is done here); the old
  step-only integration and `tempo()` are gone, and a Clip File with no tempo now has
  the 120 BPM default in its map where `tempo()` was `None`: adopting a tempo is
  `tempo_map().tempo_at(Beat(0.0))`.
- **Format 2 is kept apart.** Its tracks are independent sequences: `events()` is `None`
  (there is no one timeline to merge on), `tempo_map_of(i)` is each sequence's own map
  and `tempo_map()` is the first's. Formats 0 and 1 and a Clip File merge their tracks
  by tick, ties in track order.
- **SMPTE division is refused** (`Error::MidiUnsupportedTiming`), not supported. A
  timecode file has no beat grid, so no `Beat`, no `TempoMap` and no `events()`;
  supporting it means a second timing model through every accessor, and importing one
  means choosing a tempo, which is the caller's to say. It is the one SMF this
  crate does not read, and it is refused with its own error rather than guessed.
- **`Track` has `end`, and a name that is derived.** The End of Track (a Clip File's
  End of Clip, whose Delta Clockstamp is the clip's length) is where the track ends,
  which a read must give back and an event list cannot say; `name()` is the first Track
  Name event rather than a field beside it, so the two never disagree.
- **`FileEvent` adds `Unknown` and `Escape`** to the sketch: a Clip File's packet that
  no definition covers must write back as its words, and an SMF distinguishes `F0` from
  `F7`. Text is bytes (the specification names no encoding); the registered meta events
  `Meta` does not type (sequence number, channel prefix, port, SMPTE offset, sequencer
  specific) are `Other { kind, data }`.
- **A fixed-size meta event can be longer than it defines.** SMF 1.0 section 3.3:
  programs "shouldn't be surprised if it's bigger than they expected. If so, they must
  ignore everything past what they know about." Set Tempo, Time Signature and Key
  Signature each carry an `extra` (the bytes past the ones they define, empty for an
  exact event), so the typed event is read and the file still writes back as it was (a
  real Time Signature with the compound-signature bytes is `FF 58 06 ...`). An End of
  Track that is not a track's last event is refused (`Error::MidiFileParse`): section
  3.3 says it "must occur as the last event in the track", and ending the track there
  would drop what follows.
- **A `PairedNote`** is `(group, channel, note, 16-bit velocity, start, end)` in ticks,
  for both containers (a MIDI 1.0 velocity widened by the specification's scaling, a
  MIDI 2.0 Note On at velocity 0 neither a start nor an end); `SmfNote`, `ClipNote` and
  `SmfTrack` are gone.
- **The Clip File codec is private** under the model (`ParsedClipFile`, `ClipEvent`,
  `ClipHeader` and the `write_clip_file*` functions, `read_clip` and `write_clip` are
  deleted; `MidiFile::read` and `write` are the path calls). A tempo or a time
  signature is an event of the track like any other.
- **A constructor checks what the container can hold** (`Error::InvalidConfig`): events
  in order, a gap that fits the wire (28 bits in an SMF, 32 in a Clip File), group 0
  MIDI 1.0 messages in an SMF, packets only in a Clip File and not its own structure,
  a Set Tempo of 24 bits, a format 0 file of one track. A write cannot then drop or
  clamp an event.
- **A read is a write's inverse on the model**, not on the bytes: running status and the
  spelling of a Delta Clockstamp (NOOP restarts) may differ, and a Clip File's reserved
  bits are written as zero. Message, Note On at velocity 0 and all, the file layer does
  not translate: `Translator` runs when a clip is loaded for playback.

- **Our types in the API, the byte parser private.** `midly`'s events borrowed the input
  bytes, and exposing them would have made it a public dependency; it was the SMF byte
  parser until stage 6b replaced it with our own (below), behind `smf.rs`.
- **`TempoMap` is not a MIDI type.** It maps beats to samples and lives in
  `tutti-tempo`, a small pure crate above `tutti-types` with
  `#![forbid(unsafe_code)]`, because the transport, the export plan, plugin
  transport info and SMF import all want it. The file layer builds one from tempo
  events; [020](020-tempo-map-transport.md) is the design that adopts it in the
  transport.
- **Integer ticks.** Events keep their tick and the file its division, so a write is
  exact with no rounding, and `events()` gives callers `Beat`.
- **The Clip File reader checks structure order** (M2-116 v1.0 sections 6 to 7.3), one
  error variant and one test per rule. Everything from the file header up to the DCS of
  Start of Clip is the **Clip Configuration Header** (section 6), kept as
  `Track::header` (ticks on the header's own clock, from the DCTPQ; not on `events()`,
  and not sounding at the sequence's start):
  - Section 6: Set Profile On messages (a Universal SysEx `7E dev 0D 22`, recognised
    packet by packet) may lead the file with no DCS; the DCTPQ follows them, behind a DCS
    of 0. Any other message before the DCTPQ is `DctpqNotFirst`; a second DCTPQ is
    `DuplicateDctpq`. The writer puts the leading tick-0 Set Profile On packets back
    before the DCTPQ.
  - Section 6, 6.3: any other UMP messages between the DCTPQ and Start of Clip
    (Program Change, Control Change, System Exclusive, ...) are header messages, each
    behind its own DCS, kept in order with their delta times and written back. They are
    not an error.
  - Sections 6.1 to 6.1.2: at most one Set Tempo and one Set Time Signature
    (`DuplicateHeaderTempo`, `DuplicateHeaderTimeSignature`). The "shall"s on placement
    are refused too: the Set Tempo must be the first message after the DCTPQ at a delta
    of 0 or sharing the DCTPQ's DCS (`MisplacedHeaderTempo`), and the Set Time Signature
    right behind it, or first if there is no Set Tempo (`MisplacedHeaderTimeSignature`).
    The header's Set Tempo is the tempo from tick 0 of the sequence (the header's clock
    runs before Start of Clip, so its tick is not a sequence tick); a Set Tempo in the
    sequence is an ordinary change, and one at tick 0 overrides the header's. The
    constructor `MidiFile::clip` holds a header to the same rules (`Error::Clip`).
  - Section 7: Start of Clip is the first UMP of the sequence data (`MissingStartOfClip`,
    `DuplicateStartOfClip`); section 7.3: End of Clip is last and nothing follows it
    (`TrailingData`, `MissingEndOfClip`). The DCS before End of Clip is the clip's
    length (`Track::end`).
  - Section 3.2.2: a gap past 1,048,575 ticks is written as a DCS of 1,048,575 and a
    Null message, once per restart, then the remainder's DCS (pinned by word in
    `a_long_gap_is_written_as_dcs_then_a_null`).
  - **L1 (M2-116 conformance).** The reader and `MidiFile::clip` also hold: the DCS right
    before the DCTPQ exists and is 0 (`DctpqDelta`; sections 3.2.1 and 6: "The DCTPQ shall
    have a preceding Delta Clockstamp ... set to 0x00000"); a Set Profile On has no DCS
    before it (`ProfileOnDelta`; section 6: "shall not have a prepended Delta Clockstamp
    (DCS)"); the header's Set Time Signature is at delta 0 like the Set Tempo
    (`MisplacedHeaderTimeSignature`; section 6.1.2: "shall use the Delta Clockstamp which
    precedes the DCTPQ message or shall have a preceding Delta Clockstamp with Number of
    Ticks Since Last Event set to 0x00000"); no Property Exchange message, a MIDI-CI
    `7E dev 0D 30..3F` System Exclusive 7 (`PropertyExchange` in the reader,
    `Error::InvalidConfig` from the constructor; sections 6.4 and 7.2). The DCS before Start
    of Clip is kept as `Track::header_end` (a `Tick` on the header's clock, not before the
    last header message; section 6.1: "the cumulative values of all Delta Clockstamp up to
    and including the Delta Clockstamp for the Start of Clip Message determines the total
    length of time allocated to the Clip Configuration Header"), read and written.
  - **Ruling: one DCS for the parts of a Lyric or Ruby message.** M2-116 section 3: "A
    Delta Clockstamp message precedes every UMP to define the timing of each message."
    M2-104 sections 7.5.10 and 7.5.12: "When placed in a MIDI Clip File, all parts of the
    Lyric Data Message shall be placed at the same time, sharing a single Delta Clock
    Stamp" (Ruby the same). They agree through the Clip File's own definition of a DCS,
    M2-116 section 3.2.2: "The Delta Clockstamp message declares the time of all following
    messages which occur before the next Delta Clockstamp message ... Simultaneous events
    may share a single Delta Clockstamp" (M2-104 section 7.2.3.2 likewise): a DCS "precedes"
    a UMP by governing it. A zero DCS between the parts is not a way to satisfy both: it
    is a second Delta Clock Stamp, and "a single" is what the shall says. So `write` puts
    one DCS before the Start packet and none before that message's Continue and End
    packets (same Group, address and kind, same tick); every other event keeps its own DCS
    (the Language messages, other texts and SysEx are not covered by the rule). The reader
    was already liberal: a DCS between the parts reads as before. The first reading in the
    L1 brief, that a zero DCS between the parts would do, does not hold.
  A Delta Clockstamp, a Null and a JR message are not content and may come anywhere
  before the DCTPQ. The refused-event-before-Start-of-Clip rule of the first draft of
  this stage was wrong: such a message is a header message.
- **A lossless round trip** is the test (`tests/smf_model.rs`, `tests/clip_model.rs`):
  a file read and written holds the same events, including SysEx and meta, over hand-built
  files with every event kind in formats 0, 1 and 2, checked also by a decoder written
  from the specification that shares no code with the crate, and every strict prefix of a
  file is refused.
- **Clip File moves here** from `tutti-midi-types`, its only production consumer.

**Measured (2026-11).** `midly`'s `parallel` feature pulls `rayon`: this crate's normal
dependency tree goes from 21 crates to 27 (`rayon` is in the lock for others, so the
workspace gains nothing). A 16 track, 1.6 million event, 16 MB file, release build:
read 58 ms with `parallel`, 100 ms without; write 35 ms against 47 ms. A 16 KB file:
write 743 us with it (the first call starts the pool) against 60 us, read 162 us against
107 us. It pays only past a few megabytes, which a file here is not, and is off.
`strict` is on. Without it `midly` keeps what it could read of a cut file: a chunk
longer than the file is read to the end of the file, a track that ends mid-event stops
silently, a track count that disagrees with the header is ignored. With it each of
those is an error (`Error::MidiFileParse`), and it also refuses a duplicate header and an
invalid chunk. `strict` alone refuses every strict prefix of a valid file (the sweep
passes with `smf::scan` removed), but as a string, so a cut file is told apart by the
crate's own `scan` (a chunk header short of 8 bytes, a chunk running past the end, fewer
`MTrk` chunks than the header's count: `Error::Truncated`); `strict` is what catches
the one `scan` cannot, a chunk whose length is right and whose last event is not.

**Stage 6b, done: our own SMF reader and writer; `midly` goes.** `midly` was the one
third-party codec under the file model, and the costs measured above (a feature that
pulls `rayon`, a `strict` flag backed by a pre-scan of our own for `Truncated`, a
conversion layer from its types to ours) were the price of not owning a few hundred
lines. The reader (`tutti-midi-file/src/smf/read.rs`) is `winnow` combinators over the
framing: `MThd` and `MTrk` chunks with `length_take`, the variable-length quantity (at
most four bytes: a fifth is refused), meta (`FF type len`) and the `F0` and `F7`
length-prefixed events. Running status is a value: `event(running)` returns the next
status beside the event and `successors` threads it (SMF 1.0 section 3: a channel event
may omit its status when it repeats the last; sysex and meta cancel it; `F1` to `F6` and
`F8` to `FE` are not in a file). What decodes a message is `tutti-ump`'s, never
re-derived: `Voice1::data_len` (new) says how many data bytes a channel status takes and
`Midi1Parser::message` turns status and data into a message. The streaming
`Midi1Parser::feed` is not used: in a file the delta bytes would read as running-status
data and `FF` as a real-time Reset. The writer (`smf/write.rs`) is a fold of pure
functions: an event to its bytes, a track body as a fold over the running status
(eliding a repeated channel status), a chunk's length computed from the body it frames.
A meta event that is not exactly a typed shape (a Set Tempo whose data is not three
bytes) is kept as `Meta::Other`, so no byte is lost. A cut file is `Truncated` as in
stage 6 (a framing fault, or fewer tracks than the header declares); a track with no End
of Track still ends at its last event. `winnow` is named only in `read.rs` and no type
of it is public (`scripts/check-layout.sh`). Measured against the figures above
(release, best of runs, 16 tracks of channel messages at about 4 bytes an event with
running status): 16 MB, 4 million events, read 222 ms (55 ns an event, against 62 ns
for `midly` without `parallel`), write 33 ms (8 ns an event, against 29 ns); 16 KB,
4000 events, read 174 us, write 31 us. A third of the read is `Midi1Parser::message`.

**Stage 6c, done: an SMF converts to a Clip File.** `MidiFile::to_clip` (`tutti-midi-file/src/file/to_clip.rs`)
turns a format 0 or 1 SMF into a Clip File as M2-116 intends: section 3.3, "Messages
Replace the Former Meta Events in SMF v1", says the meta events "have been replaced by new
MIDI messages in the UMP Format", mostly Flex Data (type D), and Appendix B lists them.
It returns `Converted { clip, dropped }`: a conversion that returned the clip alone would
lose events without a word, and one that failed on the first would make a file with one
marker unconvertible, so each event the clip lacks comes back as a `Dropped { track,
tick, event, reason: DropReason }` in clip order. A format 2 file is `ToClipError::Sequential`
(a clip is one sequence: one Start of Clip, one End of Clip, section 7) and a Clip File
`ToClipError::AlreadyClip`. Fields are M2-104 section 7.5's, as `tutti_ump::Flex` models them.

| SMF | Clip File |
|---|---|
| channel message | the same `Midi1` packet, group 0 |
| Set Tempo `FF 51`, us per quarter | Flex Set Tempo, `us * 100` (10 ns units per quarter; 24 bits times 100 fits `u32`) |
| Time Signature `FF 58` `nn dd cc bb` | Flex Set Time Signature (`nn`, `dd`, `bb`) then Set Metronome (`cc` clocks per click; the bar undivided, so Bar Accent Part 1 is `nn` and Parts 2 and 3 are 0; no Subdivision Clicks: M2-104 §7.5.5) |
| Key Signature `FF 59` `sf mi` | Flex Set Key Signature on the group: `sf` sharps or flats, and the tonic's letter (the field has no accidental or mode: the circle of fifths F C G D A E B at `sf + 1`, a minor key's from `sf + 3`, so 3 sharps minor is F) |
| Copyright `FF 02` | Flex Copyright Notice |
| Lyric `FF 05` | Flex Lyrics |
| Track Name `FF 03`, format 0 or track 0 of format 1 | Flex Composition Name: SMF 1.0 section 3.3, the first track's name is the sequence's |
| Text `FF 01` | Flex Unknown Metadata Text (M2-104 Table 16: the message for text of no named kind) |
| `F0` event, complete 7-bit `F0 .. F7` | SysEx7 packets (`SysexMessage::packets`) |
| division | the DCTPQ; deltas carry over as ticks |
| End of Track | End of Clip at the longest track's end |

Long text is split over Flex packets by `tutti_ump::flex_text_packets` (12 bytes a
packet, Start, Continue, End, zero padded; a text with a NUL is refused as zero is the
padding). Format 1 merges by tick, ties in track order then the track's own, the rule of
`MidiFile::events`. Placement follows sections 7.1.1 and 7.1.2: at tick 0 the Set Tempo
messages come first, then the Set Time Signature messages (each with its Set Metronome),
then the rest, all in the Clip Sequence Data right after Start of Clip (the header stays
empty); a file with no Set Tempo at tick 0 gets the SMF default 120 bpm and one with no
Time Signature 4/4 with 24 clocks and 8 32nds (SMF 1.0 section 3.3: "If no tempo is
specified, 120 beats per minute is assumed", 4/4 likewise), so the clip states what an
SMF reader assumed. A later tempo or signature stays on its tick: "where MIDI clocks
would be located" and "the beginning of a bar" are asks of the author's grid, and
moving an event would change the music.

No successor (`DropReason`), each reported with its tick: Sequence Number, Marker, Cue
Point, Instrument, Program and Device Name, Sequencer Specific, any other meta event,
and an `F7` escape (`NoSuccessor`: Appendix B lists no message for them); SMPTE Offset
(`SuccessorUndefined`: Appendix B lists an SMPTE Offset message "defined in" M2-104, and
so Extended MIDI Time Code, but M2-104 v1.1.2 defines neither); Channel Prefix and MIDI Port Prefix (`ReplacedByAddressing`: the UMP carries a
channel in the packet and a port as a group, and every meta message here is addressed
to the group); the name of any track but the first (`TrackName`: a clip has no tracks);
a text that is not UTF-8 or has a NUL (`TextNotUtf8`, `TextHasNul`: Flex text is UTF-8,
M2-104 §1.8 and §7.5.9; a text that is carried loses a leading BOM and is composed to
NFC, as §1.8 requires); a Key Signature beyond 7 sharps or flats, a Time Signature whose
denominator power is 0 and one whose numerator is 0 (`KeySignatureOutOfRange`,
`WholeNoteDenominator`, `ZeroNumerator`: the Flex fields hold no such value, a
denominator of 0 reads "non-standard" and a numerator is "a value from 1 to 256",
§7.5.4); a `F0` event that is not one complete 7-bit message
(`SysexNotCompleteSevenBit`: a divided message and its `F7` continuations are each
reported, not joined); a MIDI-CI Property Exchange message (`PropertyExchange`: M2-116
§6.4 and §7.2 keep PE out of a clip); and an empty text (`EmptyText`: an all-zero
Complete Lyric is a melisma, M2-104 §7.5.10.1, which an empty SMF event never said).
A later tempo or time signature keeps its tick even where M2-104 §7.5.3/§7.5.4 ask a
Standard MIDI File for a MIDI-clock or bar-line position: the source's grid may hold no
such tick, and moving an event would change the music. The SMF 1.0 defaults (120 bpm, 4/4) are quoted from SMF 1.0,
whose text is not among the specs in hand.

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
   and 3b move it to the input edge and into `channel_voice`, and consumers drop it with
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
   - **SoundFont, Program Change bank.** Done in stage 3a: the bank's MSB selects
     the bank, as CC 0 does.
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
at any point; its CI codec move and `ci`/`negotiate` deletion (4a, done) shrink
`tutti-midi-types` the same way.

- **Stage 5a. One event shape.** Done (§1.4 lists what replaced `MidiEvent` where).
   What the stage settled beyond the plan: the mailbox ring and plugin output
   carry a raw `u32` position, not an `Offset` built against a block that does
   not exist yet; `MidiMessage` lost `frame_offset`; multi-packet builders
   (SysEx, Flex Data, UMP Stream) are free functions over `Ump` in
   `tutti-midi-types` until 3c, 4 and 5b move them. The plan was:
   Replace `MidiEvent` with `(Offset, Ump)` everywhere:
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
- **Stage 3a. The `Translator`.** Done. What the stage settled beyond the plan:
   - It takes and gives `Ump`, not `Message`: an input edge holds packets, and
     one that does not decode passes through unchanged. It sends at most one
     packet per packet fed (a select or CC 6 that ends a run is replaced by the
     run's controller), which lets a clip put each result in its source's place.
   - D.3.3's triggers, exactly: a CC 6 waits for a CC 38, a later CC 6 or a new
     select (LSB 0 when none came); a CC 38 before any CC 6 waits for it and goes
     with it; a new CC 6 never reuses an earlier run's LSB. A sender that ends an
     RPN with a lone CC 6 and nothing after it is never heard live: that is the
     spec's rule, and the reason senders close a run with the Null Function. A
     clip's loader flushes it (`Translator::flush`, §4).
   - Each half of a parameter number is kept per kind (RPN and NRPN apart) until
     selected again; Reset All Controllers (CC 121, RP-015) sets both to null,
     drops what the run had not sent and passes on.
   - Registered controllers 0 to 31 of bank 0 are zero-extended (M2-115 §4.1);
     every other controller is scaled Min-Center-Max. The group is kept (the old
     translator sent everything on group 1).
   - MPE stays in `tutti-midi-runtime`'s `MpeIngest`, after the translator; it
     moves with `mpe/` in 5b. Its output is a callback now (`translate(event,
     out)`), which deleted `Translated` and `RpnRun`, and a MIDI 2.0 Note On at
     velocity 0 binds its member channel like any Note On.
   - The SoundFont reads Registered Controllers 0, 1 and 2 (pitch bend
     sensitivity, fine and coarse tuning) and a Program Change's bank, which is
     stage 2's SoundFont residue, done here because the translator absorbs the
     Bank Select the SoundFont followed before. The SoundFont holds a Bank Select (CC 0 and 32; an
     MSB zeroes the held LSB, M1 p.12) until the next Program Change applies it (M1 p.13).
   The plan was: Deletes
   the SoundFont's RPN state machine (`Channel::{set_rpn_*, data_entry_*}`, which
   then reads a registered-controller `Voice2`, NRPN included), `mpe_ingest`'s
   `RpnRun` and the old `Midi1ToMidi2Translator`, and `MidiInputNode` runs the new
   one. Behaviour change: §4's remaining spec fixes. Oracle: M2-115 and M2-104
   App. D, as analytic tests.
- **Stage 3b. `channel_voice`** (§4.1) in `tutti-midi-protocol`. Done.
   `channel_voice(ump) -> Option<(Address, Voice2)>` and `widen_message` replace
   the polysynth's and the SoundFont's `voice1` copies and their seven-arm match;
   the hosts' two `fn widened` copies move to `widen_message` with the plugin
   lanes. `Io::sub_blocks` survives: the two instruments walk it, so
   `BlockSplitter`, `Overrun`, `Step` and `gather_midi` are deleted, with the
   fixed `midi_buffer` each instrument copied its events into (256 events in
   the SoundFont, 512 in the polysynth, while a port can deliver more; the rest
   were dropped). `SortedEvents::sub_blocks` takes its block as `Samples`; no
   graph event is past its block, so `Overrun` had no case left. Behaviour
   change, the one App. D.3 and §7.4.2 asked of this stage: a MIDI 2.0 Note On
   at velocity 0 is a Note On in both instruments (the SoundFont strikes it at
   MIDI 1.0 velocity 1), and `InstrumentHandle::note_on` sends MIDI 2.0 at a
   16-bit velocity. The graph's contract rows are excited with MIDI 2.0. The
   plan was: Deletes the four widen copies (the two `widened` functions and the
   two `voice1` functions) and the seven-arm match in the polysynth and the
   SoundFont. Oracle: M2-104 App. D.3. Compare `tutti_graph::Io::sub_blocks`
   with `tutti_midi_runtime::BlockSplitter` here: both cut a block at its event
   offsets, and `dispatch` should call one. Keep the one with the tests, and
   delete the other with its callers moved. The survivor takes its block length
   as `Samples`, not the `usize` `BlockSplitter::new` takes now.
- **Stage 3c. The MIDI dedup.** Part done (the SysEx and note-identity items below,
   marked); what remains is the Plugins session's (see "Remaining" at the end of this
   stage's list). The AU host reads its bytes with
   `tutti_ump::Midi1Parser`, and `Midi1Decoder` and `Midi1Splitter` go. One SysEx7
   reassembler stays, in the layer that uses it, and the other goes
   (`SysExAssembler` or `Sysex7PacketReassembler`, whichever is left with no
   caller that the survivor cannot serve; choose in the PR from the callers).
   Oracle: M2-104 §7.7.1 for the SysEx7 packets and the byte-stream rules of
   MIDI 1.0 for the parser (M1-100 is not available, §13). The `Reassemble` trait of
   §4.1 is revisited here. Two further items for this stage (plan item 5):
   - **One SysEx7 reassembler. Done.** The survivor is the real-time one
     (`SysExAssembler`: bytes out, never allocating, one open run, spans blocks),
     because it serves MIDI-CI as well as a plugin: `CiMessage::decode` takes the
     bytes it completes (`tests/midi1_wire_ci.rs`). It is now `tutti_ump::SysexAssembler<B>`,
     `SysexStep` and `SysexRef`, over a buffer the caller provides (sized with
     `SysexAssembler::room_for`; `tutti-ump` allocates nothing), reading packets with
     `decode`. `Sysex7PacketReassembler`, `Sysex8PacketReassembler` (no production
     caller) and their types and tests are deleted from `tutti-midi-runtime`.
     Differences: a type-3 packet that does not decode (length past six, a reserved
     status, a data byte of 128 or more) is `Held` and ignored, as a length past six
     already was; `SysexRef::new` is gone. `ci::sysex7_to_ci` is gone with the
     rest of `ci/` (stage 4a): the codec takes the bytes the assembler completes.
   - **A checked SysEx type. Done:** `tutti_ump::SysexMessage<'a>`. `tutti-plugin-types`' `sysex.rs` has `unframed` and
     `is_framed`, which accept a message with or without `F0` and `F7`. A
     `SysexMessage<'a>` newtype (checked `F0` to `F7`, every interior byte 7-bit)
     retires the leniency: a host that sends one builds it once and the check
     is the type's.
   - **Test copies.** `is_framed` is hand-coded, and five test helpers named
     `framed` rebuild the same `F0 .. F7` wrapper (`tutti-plugin-types`'
     `sysex.rs` and `midi1_decoder.rs`, `tutti-clap-host`'s `events.rs`,
     `tutti-vst2-ffi`'s `midi.rs`, `tutti-vst3-ffi`'s `event_list` tests). They
     go to `tutti_ump::frame`.
   - **One note identity. Done in `tutti-ump`:** `NoteId` lives there, one formula
     (`channel * 128 + note`, `MAX_CHANNEL_NOTE == 2047`), `from_channel_note` takes
     `(MidiChannel, NoteNumber)`, `channel_note()` returns `Option` (a minted id names
     no pair), and `NoteRotationAllocator` minted from `MAX_CHANNEL_NOTE + 1` (deleted in 5b part (b): nothing used
     the ids). The plugin
     side below is the Plugins session's. Was: There are two encodings of `(channel, note)` as an id:
     `tutti-midi-types`' `NoteId::from_channel_note` (`channel << 8 | note`, a
     `u32`, typed `MidiChannel`) and `tutti-plugin-types`' `note_id_for` /
     `note_id_to_channel_note` (`channel * 128 + note`, an `i32`, raw `u8`s, with
     `MAX_HOST_NOTE_ID`), which the CLAP and VST3 hosts use. The plugin id is
     the formats' wire value, so it stays an `i32` at the FFI edge. It is
     derived from the one `NoteId` (which moves to its owner in 5b), not
     minted by a second formula, and it takes `MidiChannel` and a 7-bit
     note, not `u8`s. Oracle: the round trip for every channel and note, and
     a host-minted id never decoding a plugin's own id space (the existing
     `None` rule).
   - **Remaining for the Plugins session.** (The `SysExAssembler`, `unframed` and
     `is_framed` callers have moved to `SysexAssembler` and `SysexMessage`: doc 027 M6.) Their `note_id_for` callers move to
     `NoteId` (the `i32` wire value is `NoteId::raw` at the FFI edge); the AU host's
     `Midi1Parser` move (the `Midi1Decoder` and `Midi1Splitter` deletion); and the five
     `framed` test helpers go to `tutti_ump::frame`.
   - **Per-note expression as MIDI 2.0 per-note controllers.**
     `tutti-plugin-types`' `NoteExpressionType` (volume, pan, tuning, vibrato,
     brightness, pressure, expression, custom) is a second per-note vocabulary
     beside the `Ump` the graph carries. Doc 023 G2 records that no graph event
     carries it. The CLAP host drops every per-note registered controller but
     Volume, Pan and Brightness (stage 2's residue). Decided (owner, 2026-11-10): the graph carries per-note
     expression only as `Ump` (per-note pitch bend, poly pressure, and the
     per-note registered and assignable controllers M2-104 defines), and each
     host maps that to its format's note expression at the edge, one table per
     format. `NoteExpressionType` then stays only as the format-side
     description, if a host still needs one, and the plugin's own
     `ExpressionInfo` (023 G2) describes what a plugin accepts. Oracle: M2-104
     for the per-note messages, and per format, a mapping test in each
     direction. Settle with doc 023 G2, which owns the plugin-side
     description.
- **Stage 4. `tutti-midi-device` and the `-ffi` crates.** Device detection comes first
   (`Devices`, `DeviceTable`, `FakeBackend::plug`; §5), directly after stage 1:
   done, except the flush (§5, "Built").
   Then model, connection, identity, device loss, timing (§5), and the backends
   with no `unsafe` of their own.
   Behaviour change: real hot-plug, timestamps, output scheduling.
   **Stage 4a, MIDI-CI. Done:** the CI codec moved to `tutti-midi-device` and the rest of
   `ci`/`negotiate` is deleted (§5, "MIDI-CI", which says what was kept and
   what the deleted negotiator did); the citations in kept code are fixed. The
   rebuilt MIDI-CI minimum (App. F, items 1 to 9 there) lands with the stage that
   needs MIDI-CI.
   **Stage 4b, the UMP Endpoint and Connection. Done, as a pure core.**
   `tutti-midi-device` takes its `tutti-ump` edge and gains `Endpoint` (what one
   endpoint declares, M2-104 §7.1: version, supported and active protocol and JR,
   identity, name, Product Instance Id, up to 32 `FunctionBlock`s with their
   MIDI 1.0 flag, UI hint, MIDI-CI version and SysEx 8 streams, all inline; a MIDI
   1.0 device is `Endpoint::midi1_device`, one inferred block), `Endpoint::reply`
   (the responder: Endpoint Discovery, Function Block Discovery and a Stream
   Configuration Request, into a caller's `Extend<Ump>`, no allocation), and
   `Connection` (the discoverer, M2-100 Table 4 steps 2 to 4: Discovery, a Stream
   Configuration Request when the Notification is not what is wanted, Function
   Block Discovery; time an `Instant` argument; a step is sent three times, one
   second apart (`MAX_ATTEMPTS`, `STEP_TIMEOUT`, ours: the spec gives none), then
   Discovery and the Request fall back to a MIDI 1.0 device and Function Blocks
   settle with what came; the Names get `NAME_GRACE`, 100 ms, after the last Info
   since a block with no name sends none). The three bounded names are checked
   values. The `midi2`-backed Stream constructors in `tutti-midi-types` are
   deleted with their tests; `Protocol` stays there for `tutti-midi-hardware`'s
   `UmpCapability`.
   **Wired in L6(b)** (§5, "Wired"): `tutti-midi-hardware` runs a `Connection` per
   open UMP input, the session's `Phase::Poll` is its timer, and `UmpCapability` is
   built from an `Endpoint`. Deferred: MIDI-CI Discovery (Table 4 step 5), and the
   output side's use of the negotiated `StreamConfig` (done in L6(c), §5, "The output
   protocol edge"). A Connection settles on the
   Notification it gets, so a device that is on MIDI 2.0 already is not asked again.
   Residue for this stage:
   - **`clock_out.rs` doc is false.** The module doc of `bevy-tutti`'s
     `midi/hardware/clock_out.rs` says the drain cadence does not affect tick
     spacing because the timing is baked into each event. The wire carries no
     timestamp, so the spacing at the wire is the drain's (once per Bevy
     frame, §5 "Timing at the edge"). Correct the doc now, and again when
     scheduled output lands.
   - **`ChangeSink::seed`. Done (4c).** `seed` returns `Result<(), SeedError>`: a
     second seed, or one after any `send`, is refused and the list is left as it
     was. The constructor does not take the list, because a backend reads the OS
     after it has the sink (a change between the read and the registration needs
     the watch to exist), so the type holds the order instead.
   - **Not verified, needs a Mac or a check.** (a) The CoreMIDI input callback
     has no panic guard; only the watch thread has `catch_unwind`
     (`backend/coremidi/watch.rs`). (b) The session MIDI input ring size
     is 256 in one place and 2048 in another per the audit; the drain chunk is 256
     (`tutti-session/src/midi.rs`) and the other figure was not found. (c) **Done (4c).** `AlsaOutput` took its
     mutex with `.expect("output mutex poisoned")`: it now recovers the guard, since
     `Sender` is a client handle and two ids and is never left half-written, and a
     panic in the output drain over a fault already past is worse than the send.
     `unpack_id` returns `None` for an id with bits above either byte, which
     `pack_id` cannot make, and `open` reports it (a watch's own handles are always
     in range). The input thread's drop waits at most `JOIN_BOUND` (1 s) and then
     detaches the pump with a warning. (d) There is no benchmark of the hardware callback to the drain.
   - **Bare frame counts at the edge. Done (4c).** `MidiIn::poll_block` and
     `ClockMaster::tick` take the block as `Samples`. In `outbound/jr_timestamp.rs`
     the origin and the frame a tick is read at are `Frame` (`JrClock::ticks_at`,
     `JrClockEmitter::due`, `JrStream::origin`, `JrStamper::stamp_block`), and the
     counts are `Samples` (`JrClockEmitter::interval`, `JrStamper::block_span`,
     `JrStream::stamp_span`); the session's `send_stamped_span` and `origin` follow.
     `HardwareMidiDrain::cycle_start_read_all_inputs` took `nframes: usize`; it went
     with the drain (E5: the drain is deleted, not changed to take `Samples`).
   **Stage 4d, the `-ffi` crates. Done.** `unsafe` left `tutti-midi-hardware`, which is
   `#![forbid(unsafe_code)]` and has no `libc`, `coremidi-sys`, `mach2` or raw ALSA
   dependency. `tutti-alsa-ffi` (Linux; `tutti-alsa-ump-sys` was folded into it as a
   private `sys` module, nothing else used it, and its `build.rs` probe and
   `links = "asound"` are unchanged, so `cfg(alsa_ump)` and `DEP_ASOUND_*` still
   reach the backend) holds every ALSA call behind `Client` (open in UMP mode, ports,
   the subscriptions, `read_event`, `send_to_subscribers`, `stamp_port` and its
   `QueueClock`, `list_ports`), `Poller` (the sequencer's descriptors and a stop
   descriptor, over `poll(2)`) and `Received` (an event copied out of alsa-lib's
   buffer, with `announced()` for the announce port). `tutti-coremidi-ffi` (macOS)
   holds `UmpSource` (`MIDISourceCreateWithProtocol`, the one call the safe
   `coremidi` crate lacks; the endpoint disposes on drop) and the Mach host clock
   (`Timebase`, `host_ticks`). What the safe `coremidi` crate wraps stays in the
   backend. Both crates are layer 0 (no workspace dependency) and `deny` the
   `undocumented_unsafe_blocks` and `missing_safety_doc` lints.
   `scripts/check-midi-ffi.sh` is the gate (no `unsafe` token in the backend, its
   forbid, no raw-binding dependency, the `-ffi` lints). Behaviour is unchanged
   except that the unneeded `unsafe impl Send`/`Sync` on `UmpVirtualSource`,
   `CoreMidiInput` and `CoreMidiOutput` are gone (their fields are plain `u32`
   handles and the compiler proves it), and `port/manager.rs`'s one match was a
   comment. `unsafe` tokens in code: 46 in `tutti-midi-hardware` and 2 in
   `tutti-alsa-ump-sys` before; 0, 49 (`tutti-alsa-ffi`, the extern declarations
   among them) and 6 (`tutti-coremidi-ffi`) after.
- **Stage 3d. The helpers the plugin crates drop their own for. Done.** The float
   converters (`u7_to_unit_f32`, `u16_to_unit_f32`, `u32_to_unit_f32`,
   `bend_u14_to_signed_f32`, `bend_u32_to_signed_f32`, `unit_f32_to_u16`,
   `unit_f32_to_u32`, `signed_f32_to_bend_u32`) live in `tutti-midi-protocol`;
   `tutti-midi-types` depends on `tutti-midi-protocol` (layer 2 over layer 1). The
   SysEx7 writer is `tutti_ump::sysex7_packets` / `SysexMessage::packets` (an
   allocation-free `Sysex7Packets`, `ExactSizeIterator`; `Sysex7Packets::checked`
   takes a payload of plain bytes and refuses one with a byte of 128 or more,
   where a caller's bytes are unchecked), `Sysex7Packet::data` is the data bytes
   as `&[u8]` (the `bits!` fields are `repr(transparent)` and `NoUninit`), and
   `SysexRef::new` names a range in a caller's own arena. The wrappers are
   deleted, not retired: `tutti_midi_types::convert` (the integer scaling and the
   re-export of the float converters), the `sysex7_*` helpers, the `SYSEX7_STATUS_*`
   constants, `message_type` and `UmpMessageType` (`Ump::message_type`, `decode`
   and `SysexStatus` replace them) and `tutti_ump::sysex7_packets_masked`. Every
   caller, the plugin crates included, is on `tutti_midi_protocol` (`widen`,
   `narrow_*`, the float converters) and `tutti_ump`; a test widens through
   `tutti_fixtures::midi::wide_*`, which call `widen`. A plugin's SysEx whose
   payload has a byte of 128 or more is malformed: it is dropped (counted where
   the site counts drops), no longer masked into another message.
   `MidiSender::note_on` and `SoundFontUnit::note_on` take a MIDI 2.0 16-bit
   velocity, as `InstrumentHandle::note_on` does since 3b: a velocity of 0 is a
   Note On.
- **Stage 5b. Shrink `tutti-midi-types` away:** what 5a, 3a, 3c and 4 leave
   (`mpe/`, `clip_file`, `cc/mapping`, `sync/`, `MidiMessage`, the re-exports) moves
   to its owner (§6) or is deleted (`clip_file` is in `tutti-midi-file`, `sync/` in
   `tutti-midi-runtime`, `PerNoteMap` in `tutti-polysynth` and `fixed` is gone), then the crate, and `midi2` from the workspace
   and `Cargo.lock`. **Done.** Part (a) did `clip_file`, `sync/` and `PerNoteMap`; part
   (b) did the rest but `MidiMessage` and `translation/` (§6, "Stage 5b part (b)");
   part (c) deleted those, the crate and `midi2` (§6, "Stage 5b part (c)"). With `mpe/` (part (b), done): `MpeZoneConfig`
   had public fields, so a literal could hold a member count or master channel its
   constructors would refuse, and a `DualZone` pair totalling more than 14 members (the
   two spans overlap) was not rejected, only documented as the sender's duty. It has
   private fields, a `new -> Option` that refuses what RP-053 does, and `MpeZones::new`
   refuses an overlap, with tests that state each refusal and a mutation for each.
- **Stage E. The event tap and the event inlet** (the section of that name).
   E1 the tap in `tutti-graph` and its session API, E2 the inlet (built), E3 the hardware
   out on a tap (`MidiOutNode` deleted, built), E4 the queues on the inlet
   (`MidiQueueNode`, `TimedQueueNode` deleted), E5 `MidiInputNode` split, after
   `ClockMap` (built in stage 4c part 1). E1 and E2 touch `tutti-graph`. Behaviour change: a tap's frame is
   a `u64` where `MidiOutNode`'s stamp was a wrapping `u32`, and a note-off has
   room past the ring's capacity.
7. **Capabilities** (§9).

### After 5b: the audit cleanup lanes

An audit of the MIDI crates after stage 5b (2026-11) split its fixes into five
lanes. They change no stage above.

- **M1, API.** `tutti-ump`, `tutti-midi-protocol`, `tutti-graph`, `tutti-nodes`,
  `tutti-session` and `tutti-midi-runtime`'s ledger, live, outbound and clock.
- **M2, voicing and SoundFont.** `tutti-voicing`, `tutti-soundfont`,
  `tutti-polysynth`.
- **M3, shape.** `tutti-midi-file` (`SmfNote` and `ClipNote` carry `NoteNumber` and
  `U7`; `SmfTimedEvent::msg` is a `Voice1`, so no `midly` type is public and
  `pub use midly` is gone; the Clip File reader decodes through `tutti_ump::decode`
  and `split_stream`; the `smf` and `clip` modules are private, their functions
  `smf_to_bytes`, `write_smf`, `read_clip` and `write_clip` at the root),
  `SmpteTimecode` with private fields and a refusing `new`, `tutti-midi-hardware`
  with private modules and no re-export of the runtime's sync types, and
  `tutti-midi-runtime` without its unused `bytemuck` dependency.
- **M4, docs.** What a comment, a README or this document claims, brought to what the
  code is.
- **M5, tests.**

### L1: spec contradictions (done)

The first step of "The one MIDI lane" of the audit plan: what the code does against a
spec quote. Four parts.

- **The CI codec** (`tutti-midi-device`, M2-101 v1.2.1). `CiMessage::decode` returns a
  `Decoded`: `Message`, `Unread` or `NotCi`. An `Unread` keeps what the 13-byte preamble
  said (Device ID, sub-ID#2, version, source and destination as read) and an
  `UnreadReason`, and `Unread::nak(own)` addresses the NAK that sections 5.3 and 5.11
  require (status `0x02` for reserved version bits or version 0, `0x01` for a message it
  does not model, `0x41` otherwise; never a NAK to an ACK or NAK, tutti's rule). The
  header is checked: `CiVersion` (`0x01..=0x1F`: section 5.3's reserved bits 7 to 5,
  section 5.4's "Message Format Version 0x00 is deprecated and shall not be used") and
  `CiDeviceId` (Channel, Group `0x7E`, Function Block `0x7F`; Table 5's `10-7D` reserved)
  keep the range-checked shape and `check_range_contract` (under `--features serde`).
  The destination is the message's: Discovery and Invalidate MUID are broadcast, Discovery
  Reply, ACK and NAK go to one `Muid` (section 3.3.3's list), so `CiMessage` has no
  destination field to get wrong and the wrong kind of destination decodes to
  `UnreadReason::WrongDestination`. `DiscoveryData` holds `MaxSysexSize` (128 to 2^28 - 1,
  section 5.5.3; the 512 floor of an initiator is the caller's, it needs the role) and
  `Option<FunctionBlockId>` (0 to 31, or `0x7F` as `None`, section 5.6.2). ACK is built
  (`Ack`, Table 13 and 14's codes) with the NAK codes `0x12`, `0x20`, `0x21` and `0x43`
  added. `MessageText` writes section 5.10.4's text: `\` for the backslash, every
  character outside `0x20..=0x7E` but the line feed as `\uXXXX` per UTF-16 unit, at most
  103 escaped bytes; the reader unescapes and ignores a raw control character.
- **Names** (`tutti-midi-device`, M2-104 §1.8). A `Name` is held in NFC and refuses a
  leading BOM, at `new` and at the wire. A received name that is not NFC is composed, not
  refused: section 1.8 says what MIDI messages "only use" and says nothing of a receiver, so a
  receiver that composes shows and compares one spelling. `tutti-ump` stays
  dependency-free; `unicode-normalization` is `tutti-midi-device`'s (and the file crate's).
  The BOM rule is the leading one, the byte order mark's place.
- **The Clip File** (`tutti-midi-file`, M2-116): the stage 6 notes above.
- **Stale comments.** `BlockName`'s doc said section 7.1.9 gives 98 bytes (it says 91);
  five comments in `tutti-midi-hardware` and `tutti-coremidi-ffi` named the deleted
  `to_midi1_bytes`.

### L1b: the Clip File reader is a grammar (done)

The reader (`tutti-midi-file/src/clip_file/read.rs`) is `winnow` combinators, as the SMF
reader is. The bytes after `SMF2CLIP` are cut into packets once (`tutti_ump::split_stream`)
and each is classified once (`tutti_ump::decode`: a DCS, a Null, a DCTPQ, Start or End of
Clip, a message), so the grammar runs over a `&[Packet]` token slice and a rule matches a
whole packet; no bit field is read in it. Each M2-116 placement rule is one rule of the
grammar and the hand checks of L1 are gone:

```text
clip      := "SMF2CLIP" preamble dctpq config start event* end eof
preamble  := ( gap(no DCS) SetProfileOn )*          -- section 6
dctpq     := gap(DCS 0) DCTPQ                        -- sections 3.2.1, 6
config    := [SetTempo @0] [SetTimeSignature @0] ( gap Message )*   -- 6.1 to 6.1.2
start     := gap StartOfClip                         -- section 7
event     := gap Message                             -- sections 3.2.2, 7.2
end       := gap EndOfClip                           -- section 7
eof                                                  -- section 7.3
gap       := ( DCS | Null | JR | undefined Utility )*
```

Every `ClipFileError` variant is kept, each from the rule it names; a rule broken past
the point where an alternative could still match is a cut, so the error names that rule.
Two readings sharpened: a Set Profile On message must be complete (a Start with no End,
adjacent Continues, then End), a DCS between its parts is `ProfileOnDelta` as before, and
the DCTPQ is `DctpqNotFirst` only when one follows something else, `MissingDctpq`
when there is none. `MidiFile::clip` checks the header through the same `config` rule
(`check_header`), and the writer's count of leading Set Profile On packets is the same
`set_profile_on` rule. `scripts/check-layout.sh` holds winnow per crate: a dependency of
`tutti-midi-file` only, named only in `smf/read.rs` and `clip_file/read.rs`.

### L2: SysEx is per Group (done)

`tutti_ump::SysexAssembler` keeps one stream per Group and ends it by the Group's next
message, as M2-104 §7.7.1 has it: "If any Message or UMP on the same Group, other than a
System Exclusive Continue UMP or a System Real Time Message, is sent after a System
Exclusive Start UMP and before the associated System Exclusive End UMP, then that UMP shall
terminate the System Exclusive Message", and "Messages which are Groupless (MT = 0x0 and
0xF) and those which are sent to a different Group may be interspersed". It replaces the
earlier policy (one open run across all Groups, ended by another SysEx7 packet only), which
the spec does not say. A Group's message is dropped and counted when a channel voice,
System Common, Flex Data, Complete, Start, SysEx8 or Mixed Data Set UMP of its Group
arrives; a Utility or UMP Stream message, a System Real Time message (type 1, `0xF8` to
`0xFF`) and anything on another Group end nothing. `push` therefore takes every UMP of the
stream, not only SysEx7 packets.

The assembler is built with `lanes` (at most sixteen): that many Groups can hold an open
message at once, each in a `max_message` slot at the end of the caller's buffer, and a
`Start` that finds every slot held is dropped and counted. A completed message is copied
out of its slot into the block's completed area, so `room_for(max, packets, lanes)` is
`2 * lanes * max + 8 * packets`. A plugin host takes a format's one stream of whole
messages (VST2 bytes, a CLAP MIDI 1.0 SysEx, all on the first Group) with one lane.

SysEx8 and Mixed Data Set get **no assembler**. Nothing consumes them (the audit's
`Sysex8PacketReassembler` was deleted in 3c for having no caller), and the plan's "by the same
rule" does not hold: M2-104 §7.8.1 and §7.9.1 say "there are no prohibitions against
interspersing other message UMPs, as there are with the 7-bit System Exclusive Messages". A
SysEx8 message ends on its own End (or the abort of `# of bytes` `0xF`) and is keyed by
(Group, Stream ID); a Mixed Data Set ends when its chunk number reaches the declared count or
its Number of this Chunk is zero, and is keyed by (Group, MDS ID). Whoever first needs one
writes it to that, in `tutti-ump`, with the same caller-sized storage.

### L3: the input routes and instruments key by Group (done)

M2-104 section 2.1.2: "Each Group's set of 16 MIDI Channels shall be separate and independent
from any other Group's set of MIDI Channels"; System and Data messages "shall apply to, and
shall affect, all MIDI Channels within that Group"; and Appendix B.1: "Functionality of Mode
Messages received in one Group does not apply to Channels in any other Group in the device".
The input node routed by channel alone and the instruments ignored the Group, so Group 2
channel 1 played as Group 1 channel 1. Now:

- **The input node's ports are per Group.** `MidiInputPorts` is the layout: Group `g`
  owns ports `g * 17 ..= g * 17 + 16`, its sixteen channels and a channelless port for its
  System, SysEx and Flex messages. The node hears the first Group unless built with
  `hearing(MidiInputPorts::through(last))`, so a one-Group device is the seventeen ports it
  was (`MIDI_INPUT_PORTS`, `CHANNELLESS_PORT`) and a Group costs ports only where it is
  heard. An event of a Group not heard has no port and is not delivered. A message with no
  Group (Utility, UMP Stream: they address the Endpoint, §6.1) leaves the first Group's
  channelless port. A Function Block's Groups (section 6.1.1: it starts on any Group and spans
  1 to 16) are wired by `listening` per Group; a Function Block declared as a map from
  Groups to ports is left to L6, which splits the node. `RouteRule` and the session's
  `MidiRouteRule` carry a Group (the first by default; `in_group`, `in_any_group`), and
  `SessionBuilder::midi_groups` chooses how many Groups the engine's input hears.
- **An instrument plays one Group.** `SynthConfig::group` and `Settings::with_group` (the first
  by default) name the Group whose sixteen channels the instrument is, and a message of
  another Group is not read. So `tutti-voicing`'s channel tables (notes, pedals, wheels, held
  keys, MPE zones) stay sixteen wide and `step` stays a pure step with tables sized in `new`:
  an instrument never holds two Groups' channels to merge. Two Groups are two instruments,
  each wired to its Group's ports. The alternative, state keyed by (Group, channel) in the
  core, would size every table by 256 for an instrument that one Group fills.
- **MPE is per Group.** `MpeIngest` keeps the zone layout and the held member notes of each
  Group. The configured mode is every Group's starting layout; an MPE Configuration Message
  configures the Group it arrives in, and its reset ranges leave on that Group (a host-set
  mode sends them on every Group heard). The per-note messages it builds keep the Group of the
  message they came from (they were built on the first Group). `MpeIngest::mode` is the
  configured mode and `mode_of(group)` the one in force.
- **Already per Group:** `Translator` (RPN, NRPN and Bank Select state is kept per Group and
  channel), `NoteLedger`, and the SysEx assembler (L2).
- **Not done, in the plugin hosts** (a format with no Group is one Group per port): the CLAP and
  VST3 hosts' event conversion read `address.channel` and drop the Group, and
  `tutti_plugin_types::PerNoteBendRanges` is keyed by channel, so a plugin fed from two Groups
  merges them; a served plugin is one Group's wire until those take the Group.

### L4: per-note controllers are state (done)

M2-104 section 7.4.15.2: "Controllers create persistent state, so all notes that follow this
message use the tuning of the Registered Per-Note Controller #3: Pitch 7.25", and Appendix C.3's
rotation sequence sends Per-Note Management and the Pitch *before* the Note On. The instruments
applied a per-note message only to a voice that already sounded, so a value sent first was lost, a
Pitch after a Detach retuned the detached voice, D=1 with S=1 skipped the Reset, the per-note bend
range was one value for every channel (section 7.4.13: one per channel), and the SoundFont
ignored all of it. Now:

- **The state is `PerNoteState`**, a pure type in `tutti-voicing` that `Voicing` uses
  inside and a plugin host (which allocates no voices) can use as it is: one copy of the rules.
  `PerNoteState::new(groups, default_range)` sizes it (about 270 KiB for each Group; `Voicing`
  passes 1, since an instrument plays one Group); `step(address, &Voice2) -> &[NoteEvent]` reads
  one message, `start` is the Note On path, and `clear` drops everything held in place. It keeps
  one slot for each (Group, channel, Note Number) (`NOTE_CONTROLS` distinct controllers each, the
  one set longest ago forgotten past that): a Registered or Assignable Per-Note Controller,
  Polyphonic Pressure and Per-Note Pitch Bend are held when they arrive, whether or not a note
  sounds. Reset All Controllers leaves them (Appendix B.2). `Voicing::reset` is `clear`;
  `forget_notes` keeps the state, as it keeps the bends.
- **Every started note has an id**, a `NoteId` above `NoteId::MAX_CHANNEL_NOTE` that names no
  (channel, note) pair, so two notes on one key are two ids and a Detach separates them. A number
  keeps the set of its notes that still respond (`LIVE_NOTES`, 4: tutti's bound, the oldest
  dropped past it), and a controller or a Reset reaches every one, a releasing tail included
  (section 7.4.5: after a Detach "all currently playing notes and previous notes" stop
  responding, so before it they respond). Detach empties the set; `end(id)` removes one note,
  for an owner that knows it is over (`Voicing` calls it when the last voice of a strike is
  freed; a host on the plugin's note-end); a Note Off is not an end.
- **What it says** is a `NoteEvent`: `Started { id, address, note }`, then `Control { id,
  control: NoteControl }` for each value the number holds in the order told (`NoteControl`:
  `Pitch`, `Pressure`, `Registered`, `Assignable`), also sent when a message arrives for the
  current note, and `Reset { id }`.
  `Voicing` turns them into its own `Command`s: `Control { id, control }` for each voice of the
  noted id (the voices of one strike share it) and `ResetControls(id)`. An instrument applies
  what its voice has a place for and drops the rest; it holds no per-note state of its own and
  no detach flag.
- **Detach and Reset** are section 7.4.5. Detach empties the number's live set: those notes keep
  their values and answer nothing more (no event is sent; the next Note On has a new id). Reset
  clears the number's held values and resets the live notes. With D=1 and S=1 the Detach runs first, so the playing note is told nothing and the
  next one starts at the defaults.
- **A note has one sounding pitch** (`NoteControl::Pitch`, absolute), because the plugin hosts
  have one tuning per note and a consumer without a per-note table cannot add two controls.
  Section 7.4.15: the Note Number is the default pitch, Pitch 7.25 and the Attribute Pitch
  override it (the attribute for its one note, "not valid for any subsequent Notes", and over
  the number's), and "Pitch Bend and Per-Note Pitch Bend act as offsets from" either. So the
  sounding pitch is the told pitch (attribute, else the number's Pitch 7.25, else the key) plus
  the Per-Note Pitch Bend, the wheel over the channel's sensitivity (section 7.4.13: the range
  "from the current sounding pitch"). `PerNoteState` keeps the told pitch for each live note, the
  number's bend and the channel's sensitivity, and sends the sum when a note starts (after the
  held values; not for a note whose sounding pitch is its key's) and whenever the told pitch, the
  bend or the sensitivity changes, in any order; it saturates at Q7.25's ends. The channel's Pitch
  Bend is not in it: the instrument applies it. A Reset returns a live note's sounding pitch to its
  key and sends only `Reset` (the consumer returns to the key with its other controls).
- **The per-note bend sensitivity** (RPN 0/7) is one value for each channel
  (`Rules::per_note_bend_range` until it is set). A new one re-sends the sounding pitch of each
  live note whose number holds a bend, since a held bend is a position over the sensitivity.
- **The polysynth** carries `Control` out on its voices (the sounding pitch, pressure, Volume and
  Brightness; the assignable indexes 7 and 74 by its convention) and has no global per-note range
  and no detach of its own. **The SoundFont** carries the sounding pitch (the key still chooses the sample and
  the envelope scaling), Volume (the GM law) and Pan (replacing the channel's for that note);
  pressure, modulation, breath, balance, the sound controllers, effects depths and the assignable
  controllers have no generator in a SoundFont voice and are dropped.
- **Not done, in the plugin hosts.** CLAP and VST3 address a note by an id minted per (channel,
  key), drop Pitch 7.25 and Attribute 7.9, and drop Per-Note Management, so after a Detach their
  per-note expressions still reach the previous note. They take `PerNoteState` events instead: the
  minted id as the note id, `Control`s as the note's expressions (the held values as the new
  note's initial ones), `Reset` as an expression reset.

## 11. Testing

- **Spec tables.** Each message's words from M2-104 App. F and the tables in M2-115
  are analytic tests.
- **`midi2` was an oracle** until stage 5b part (c), a dev-dependency. What it found is
  spec-pinned tests, and the specification is the only oracle now (§7).
- **Properties.** `decode` never panics on any words, and `encode(decode(x)) == x`
  for a canonical `x`.
- **Fuzzing.** `cargo-fuzz` targets for UMP, SysEx, MIDI-CI and clip files. Only
  `crates/tutti-ump/fuzz/fuzz_targets/decode.rs` exists; the others do not.
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
- **MPE details.** M1-100 v1.1 is read (see "MPE is a dialect on MIDI 1.0"); the Manager
  Channel rules of its Table 5 that M2-120 changes (the Profile's manager-only sensitivity
  and 48 default, §3.5) are L10b's.
- **The MPE Profile, GM2 and other profiles** are out of the core set and unread.
