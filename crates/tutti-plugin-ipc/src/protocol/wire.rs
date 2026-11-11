//! Serde adapters for wire fields whose in-memory type is not serde.

/// A block's MIDI list on the wire: a sequence of `(offset, [u32; 4])`.
///
/// [`WireMidi`](tutti_plugin_types::WireMidi) is serde-free on purpose: its
/// offset is relative to one block, so it is not a persistable value. The wire
/// is the one place it is encoded, and only through this adapter, used as
/// `#[serde(with = "wire::midi")]`. The offset is a raw `u32` because the
/// types stop at the IPC boundary (CLAUDE.md, "Where the types stop"); it
/// becomes an `Offset` where the receiving side knows the block length.
///
/// Encoding walks the list in place and allocates nothing. Decoding builds the
/// inline [`MidiEventVec`](tutti_plugin_types::MidiEventVec) and rejects a list longer than
/// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) instead of truncating it.
pub mod midi {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec, WireMidi};
    use tutti_ump::Ump;

    /// Encodes each event as `(offset, packet words)`.
    pub fn serialize<S: Serializer>(events: &MidiEventVec, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(events.iter().map(|e| (e.offset, e.ump.0)))
    }

    /// Decodes a list of at most [`MIDI_STACK_CAPACITY`] events.
    ///
    /// # Errors
    ///
    /// A longer list is a decode error.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MidiEventVec, D::Error> {
        struct Events;

        impl<'de> Visitor<'de> for Events {
            type Value = MidiEventVec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MIDI_STACK_CAPACITY} MIDI events")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if let Some(len) = seq.size_hint()
                    && len > MIDI_STACK_CAPACITY
                {
                    return Err(de::Error::invalid_length(len, &self));
                }
                let mut out = MidiEventVec::new();
                while let Some((offset, data)) = seq.next_element::<(u32, [u32; 4])>()? {
                    out.try_push(WireMidi {
                        offset,
                        ump: Ump(data),
                    })
                    .map_err(|_| de::Error::invalid_length(MIDI_STACK_CAPACITY + 1, &self))?;
                }
                Ok(out)
            }
        }

        d.deserialize_seq(Events)
    }
}

/// A [`Normalized`](tutti_plugin_types::Normalized) carried as the `f32` the
/// wire has always held, as `#[serde(with = "wire::normalized")]`.
///
/// `Normalized` is an `f64`, and its own serde encoding is that `f64`; the
/// parameter messages carry an `f32`, so they go through here.
pub mod normalized {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    /// Encodes the value as an `f32`.
    pub fn serialize<S: Serializer>(value: &Normalized, s: S) -> Result<S::Ok, S::Error> {
        (value.get() as f32).serialize(s)
    }

    /// Decodes an `f32` and clamps it onto `0..=1` ([`Normalized::clamped`]).
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Normalized, D::Error> {
        Ok(Normalized::clamped(f64::from(f32::deserialize(d)?)))
    }
}

/// An optional [`Normalized`](tutti_plugin_types::Normalized) carried as an
/// optional `f32`, as `#[serde(with = "wire::normalized_opt")]`.
pub mod normalized_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    /// Encodes `Some(v)` as `Some(v as f32)`.
    pub fn serialize<S: Serializer>(value: &Option<Normalized>, s: S) -> Result<S::Ok, S::Error> {
        value.map(|v| v.get() as f32).serialize(s)
    }

    /// Decodes an optional `f32`, clamping a present one onto `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Normalized>, D::Error> {
        Ok(Option::<f32>::deserialize(d)?.map(|v| Normalized::clamped(f64::from(v))))
    }
}

/// A [`Normalized`](tutti_plugin_types::Normalized) carried as the `f64` it is,
/// as `#[serde(with = "wire::normalized_f64")]`, clamped onto `0..=1` on decode.
///
/// Every `Normalized` on this wire clamps on decode, whatever its width: a
/// corrupt peer's value is pinned rather than failing the message, which would
/// end the session (a frame that does not decode ends the server's phase).
/// `Normalized`'s own serde, used outside the wire, refuses instead.
pub mod normalized_f64 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    /// Encodes the value as an `f64`.
    pub fn serialize<S: Serializer>(value: &Normalized, s: S) -> Result<S::Ok, S::Error> {
        value.get().serialize(s)
    }

    /// Decodes an `f64` and clamps it onto `0..=1` ([`Normalized::clamped`]).
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Normalized, D::Error> {
        Ok(Normalized::clamped(f64::deserialize(d)?))
    }
}

/// An optional [`Normalized`](tutti_plugin_types::Normalized) carried as an
/// optional `f64`, as `#[serde(with = "wire::normalized_f64_opt")]`; see
/// [`normalized_f64`].
pub mod normalized_f64_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    /// Encodes `Some(v)` as `Some(v)` in `f64`.
    pub fn serialize<S: Serializer>(value: &Option<Normalized>, s: S) -> Result<S::Ok, S::Error> {
        value.map(Normalized::get).serialize(s)
    }

    /// Decodes an optional `f64`, clamping a present one onto `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Normalized>, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.map(Normalized::clamped))
    }
}

/// A byte field (a state chunk) as one byte string on the wire.
///
/// Serde writes a plain `Vec<u8>` as a sequence, which bincode encodes one
/// `u8` call at a time; a state chunk is megabytes. As bytes it is one copy
/// each way (doc 027 P2b). The encoding is the same: bincode writes a byte
/// string and a byte sequence alike, as a `u64` length and the bytes, so this
/// is not a protocol change.
pub mod bytes {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    /// Encodes the bytes as one byte string.
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    /// Decodes a byte string (or, from a format that writes one, a sequence of
    /// bytes).
    ///
    /// # Errors
    ///
    /// The input is neither.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct Bytes;

        impl<'de> Visitor<'de> for Bytes {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
                while let Some(b) = seq.next_element()? {
                    out.push(b);
                }
                Ok(out)
            }
        }

        d.deserialize_byte_buf(Bytes)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec, WireMidi};
    use tutti_ump::Ump;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "super::midi")]
        events: MidiEventVec,
    }

    fn ev(i: u32) -> WireMidi {
        WireMidi {
            offset: i,
            ump: Ump([0x2090_3C40, 0, 0, 0]),
        }
    }

    /// **A list past the cap is refused, not truncated.** The bytes of a list
    /// one event over the cap are built by hand (a length and that many
    /// events), since no `MidiEventVec` can hold them.
    ///
    /// Mutation (run): the `size_hint` check and the `try_push` error both
    /// replaced by dropping the excess event → the decode succeeds → fails.
    #[test]
    fn a_list_past_the_cap_is_rejected() {
        let n = MIDI_STACK_CAPACITY as u64 + 1;
        let mut bytes = n.to_le_bytes().to_vec();
        for i in 0..n as u32 {
            bytes.extend(bincode::serialize(&(i, [0x2090_3C40u32, 0, 0, 0])).unwrap());
        }
        assert!(bincode::deserialize::<Holder>(&bytes).is_err());

        let full: MidiEventVec = (0..MIDI_STACK_CAPACITY as u32).map(ev).collect();
        let at_cap = Holder { events: full };
        let back: Holder = bincode::deserialize(&bincode::serialize(&at_cap).unwrap()).unwrap();
        assert_eq!(back, at_cap);
    }

    /// **The typed parameter and rate fields encode as bare numbers**: a `u32`
    /// id, an `f32` value (not the `f64` a `Normalized` holds), an `f64` rate,
    /// an `Option<f32>` as a tag byte and the value. The expected bytes are
    /// written out from those widths, so a layout change fails here instead of
    /// as a desync between two binaries built apart.
    ///
    /// Mutation (run): `normalized::serialize` writing the `f64` →
    /// `SetParameter`, `ParameterValue` and `ParameterChanged` differ → fails.
    #[test]
    fn typed_parameter_and_rate_fields_keep_their_wire_widths() {
        use crate::protocol::{BridgeMessage, HostMessage, Normalized, ParamId};
        use tutti_types::SampleRate;

        let set = HostMessage::SetParameter {
            param_id: ParamId::new(0x0102_0304),
            value: Normalized::of(0.25),
        };
        // variant 3, id, 0.25f32
        assert_eq!(
            bincode::serialize(&set).unwrap(),
            [3, 0, 0, 0, 4, 3, 2, 1, 0, 0, 128, 62]
        );

        let rate = HostMessage::SetSampleRate {
            rate: SampleRate(48_000.0),
        };
        // variant 8, 48000.0f64
        assert_eq!(
            bincode::serialize(&rate).unwrap(),
            [8, 0, 0, 0, 0, 0, 0, 0, 0, 112, 231, 64]
        );

        let set_reply = BridgeMessage::SampleRateSet {
            rate: SampleRate(48_000.0),
            error: None,
        };
        // variant 24 (26 until v25 removed `EditorOpened` and `EditorClosed`
        // ahead of it), 48000.0f64, no error
        assert_eq!(
            bincode::serialize(&set_reply).unwrap(),
            [24, 0, 0, 0, 0, 0, 0, 0, 0, 112, 231, 64, 0]
        );

        let some = BridgeMessage::ParameterValue {
            value: Some(Normalized::of(0.25)),
        };
        // variant 3, Some, 0.25f32
        assert_eq!(
            bincode::serialize(&some).unwrap(),
            [3, 0, 0, 0, 1, 0, 0, 128, 62]
        );
        let none = BridgeMessage::ParameterValue { value: None };
        assert_eq!(bincode::serialize(&none).unwrap(), [3, 0, 0, 0, 0]);

        let changed = BridgeMessage::ParameterChanged {
            index: ParamId::new(7),
            value: Normalized::of(0.5),
        };
        // variant 7 (9 until v25 removed `EditorOpened` and `EditorClosed` ahead
        // of it), index 7, 0.5f32
        assert_eq!(
            bincode::serialize(&changed).unwrap(),
            [7, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 63]
        );

        let back: BridgeMessage =
            bincode::deserialize(&[7, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 63]).unwrap();
        assert!(matches!(
            back,
            BridgeMessage::ParameterChanged { index, value }
                if index == ParamId::new(7) && value == Normalized::of(0.5)
        ));
    }

    /// A wire value outside `0..=1` is clamped on decode, and NaN becomes `0`.
    ///
    /// Mutation (run): `normalized::deserialize` mapping an out-of-range
    /// value to `0.5` instead of clamping it → the 7.5 case fails.
    #[test]
    fn a_decoded_normalized_is_clamped() {
        use crate::protocol::{HostMessage, Normalized, ParamId};
        for (wire, want) in [(7.5f32, 1.0), (-3.0, 0.0), (f32::NAN, 0.0)] {
            let mut bytes = vec![3, 0, 0, 0, 1, 0, 0, 0];
            bytes.extend(wire.to_le_bytes());
            match bincode::deserialize::<HostMessage>(&bytes).unwrap() {
                HostMessage::SetParameter { param_id, value } => {
                    assert_eq!(param_id, ParamId::new(1));
                    assert_eq!(value, Normalized::clamped(want));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// The `f64` `Normalized` fields (`GetParameterText`, `ParameterValueFromText`)
    /// clamp on decode too, with the wire width unchanged: a peer's out-of-range
    /// or NaN value is pinned, and the message still decodes.
    ///
    /// Mutation (run): `normalized_f64::deserialize` returning
    /// `Normalized::of(0.5)` for every value -> the 7.5 case fails;
    /// `normalized_f64_opt::deserialize` the same -> the second assertion fails.
    #[test]
    fn a_decoded_f64_normalized_is_clamped() {
        use crate::protocol::{BridgeMessage, HostMessage, Normalized, ParamId};
        for (raw, want) in [(7.5f64, 1.0), (-3.0, 0.0), (f64::NAN, 0.0), (0.25, 0.25)] {
            let mut bytes = bincode::serialize(&HostMessage::GetParameterText {
                param_id: ParamId::new(1),
                value: Normalized::of(0.5),
            })
            .unwrap();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            match bincode::deserialize::<HostMessage>(&bytes).unwrap() {
                HostMessage::GetParameterText { value, .. } => {
                    assert_eq!(value, Normalized::clamped(want), "{raw}");
                }
                other => panic!("{other:?}"),
            }

            let mut bytes = bincode::serialize(&BridgeMessage::ParameterValueFromText {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            match bincode::deserialize::<BridgeMessage>(&bytes).unwrap() {
                BridgeMessage::ParameterValueFromText { value } => {
                    assert_eq!(value, Some(Normalized::clamped(want)), "{raw}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// **`wire::bytes` is not a protocol change**: a byte field encodes to the
    /// bytes a plain `Vec<u8>` did, and decodes them. (What it buys, one copy
    /// rather than a call per byte, is speed, which no test here asserts.)
    ///
    /// Mutation (run): `serialize` writing `&bytes[1..]` → the encodings differ
    /// → fails.
    #[test]
    fn a_byte_field_encodes_as_a_plain_vec_did() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Plain {
            bytes: Vec<u8>,
        }
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Coded {
            #[serde(with = "super::bytes")]
            bytes: Vec<u8>,
        }
        let data: Vec<u8> = (0..=255).collect();
        let plain = bincode::serialize(&Plain {
            bytes: data.clone(),
        })
        .unwrap();
        let coded = bincode::serialize(&Coded {
            bytes: data.clone(),
        })
        .unwrap();
        assert_eq!(coded, plain);
        let back: Coded = bincode::deserialize(&plain).unwrap();
        assert_eq!(back.bytes, data);
    }
}
