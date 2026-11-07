# tutti-midi-protocol

MIDI 1.0 to MIDI 2.0 translation for the Tutti engine (design 017 §4 and §6).

- `widen`: a MIDI 1.0 channel voice message as the MIDI 2.0 message that means
  the same, per M2-104 Appendix D.3 and the Min-Center-Max scaling of M2-115.
  Pure and stateless, so Control Changes that need per-channel state to
  translate (RPN, NRPN and Bank Select) stay plain Control Changes.

The crate depends on `tutti-ump` alone. Licensed under MIT or Apache-2.0.
