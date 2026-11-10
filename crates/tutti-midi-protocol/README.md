# tutti-midi-protocol

MIDI 1.0 to MIDI 2.0 translation for the Tutti engine (design 017 §4 and §6).

- `widen`: a MIDI 1.0 channel voice message as the MIDI 2.0 message that means
  the same, per M2-104 Appendix D.3 and the Min-Center-Max scaling of M2-115.
  Pure and stateless, so Control Changes that need per-channel state to
  translate (RPN, NRPN and Bank Select) stay plain Control Changes.
- `narrow_velocity`, `narrow_value`, `narrow_bend`: the M2-115 downscale (a
  right shift) for a consumer with MIDI 1.0 resolution only.
- `narrow`, `midi1`: a MIDI 2.0 voice message as the MIDI 1.0 one that means
  it, and a `Message` as the MIDI 1.0 bytes a host or a wire takes (a Program
  Change with a bank is three messages).

- `MpeZoneConfig`, `MpeZones`, `MpeMode`, `MpeZone`: the MPE (RP-053) zone layout
  and the MPE Configuration Message. `MpeZoneConfig::new` refuses a member count
  outside 1 to 15, `MpeZones::new` refuses a zone in the wrong role and two zones that
  share a member channel, and a persisted value (feature `serde`) is read back through
  the same checks. The state that drives zones from incoming MIDI is
  `tutti-midi-runtime`'s `MpeIngest`.

The crate depends on `tutti-ump` alone. Licensed under MIT or Apache-2.0.
