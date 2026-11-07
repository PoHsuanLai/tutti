# tutti-ump

Universal MIDI Packet (M2-104) storage and addressing for the Tutti engine.

- `Ump`: one packet as four `u32` words, `Pod`, with `words()` and
  `is_note_off()`.
- `MidiGroup`, `MidiChannel`, `CCNumber`: the 4-bit and 7-bit addresses a packet
  carries, as newtypes so a transposed argument does not compile.

The crate depends on no other tutti crate. Licensed under MIT or Apache-2.0.
