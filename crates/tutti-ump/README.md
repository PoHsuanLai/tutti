# tutti-ump

The Universal MIDI Packet (M2-104) for the Tutti engine.

- `Ump`: one packet as four `u32` words, `Pod`, with `words()`, `from_words()`,
  `decode()` and `is_note_off()`; `Display` and hex show its words.
- `MidiGroup`, `MidiChannel`, `CCNumber`: the addresses a packet carries, as
  newtypes so a transposed argument does not compile.
- `Message`, `TryFrom<Ump> for Message`, `From<Message> for Ump`, `Unknown`: every message M2-104
  defines (Utility, System, MIDI 1.0 and 2.0 channel voice, System Exclusive 7
  and 8, Mixed Data Set, Flex Data, UMP Stream) as a `Copy` value, with named
  constructors for the common channel voice messages (`Message::note_on`, and
  `note_on2` for MIDI 2.0). `Ump::decode`
  never panics and ignores reserved bits; a packet no definition covers is
  `Unknown` and keeps its words.

- `split_stream`: a buffer of concatenated packets (a native-UMP transport's) cut into
  `Ump`s by message type (M2-104 Table 4); a truncated tail is dropped.
- `Midi1Parser`, `Voice1::bytes`, `System::bytes`, `Bank::bytes`, `frame`: MIDI
  1.0 on the wire. The parser reads a byte stream one byte at a time into
  `Message`s with no allocation, SysEx as `Sysex7Packet`s; the rest write bytes.

The crate depends on no other tutti crate. Licensed under MIT or Apache-2.0.
