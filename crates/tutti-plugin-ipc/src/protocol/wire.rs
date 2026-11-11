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
///
/// Every `Normalized` on this wire, whatever its width, is refused on decode
/// when it is not in `0..=1` (NaN included), as `Normalized::new` refuses: a
/// peer that sends one is broken, and the frame fails rather than carrying a
/// value made up for it.
pub mod normalized {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    use super::checked;

    /// Encodes the value as an `f32`.
    pub fn serialize<S: Serializer>(value: &Normalized, s: S) -> Result<S::Ok, S::Error> {
        (value.get() as f32).serialize(s)
    }

    /// Decodes an `f32`, refusing one outside `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Normalized, D::Error> {
        checked(f64::from(f32::deserialize(d)?))
    }
}

/// An optional [`Normalized`](tutti_plugin_types::Normalized) carried as an
/// optional `f32`, as `#[serde(with = "wire::normalized_opt")]`.
pub mod normalized_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    use super::checked;

    /// Encodes `Some(v)` as `Some(v as f32)`.
    pub fn serialize<S: Serializer>(value: &Option<Normalized>, s: S) -> Result<S::Ok, S::Error> {
        value.map(|v| v.get() as f32).serialize(s)
    }

    /// Decodes an optional `f32`, refusing a present one outside `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Normalized>, D::Error> {
        Option::<f32>::deserialize(d)?
            .map(|v| checked(f64::from(v)))
            .transpose()
    }
}

/// A [`Normalized`](tutti_plugin_types::Normalized) carried as the `f64` it is,
/// as `#[serde(with = "wire::normalized_f64")]`, refused on decode outside
/// `0..=1` as [`normalized`] is.
pub mod normalized_f64 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    use super::checked;

    /// Encodes the value as an `f64`.
    pub fn serialize<S: Serializer>(value: &Normalized, s: S) -> Result<S::Ok, S::Error> {
        value.get().serialize(s)
    }

    /// Decodes an `f64`, refusing one outside `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Normalized, D::Error> {
        checked(f64::deserialize(d)?)
    }
}

/// An optional [`Normalized`](tutti_plugin_types::Normalized) carried as an
/// optional `f64`, as `#[serde(with = "wire::normalized_f64_opt")]`; see
/// [`normalized_f64`].
pub mod normalized_f64_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tutti_plugin_types::Normalized;

    use super::checked;

    /// Encodes `Some(v)` as `Some(v)` in `f64`.
    pub fn serialize<S: Serializer>(value: &Option<Normalized>, s: S) -> Result<S::Ok, S::Error> {
        value.map(Normalized::get).serialize(s)
    }

    /// Decodes an optional `f64`, refusing a present one outside `0..=1`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Normalized>, D::Error> {
        Option::<f64>::deserialize(d)?.map(checked).transpose()
    }
}

/// `raw` as a [`Normalized`](tutti_plugin_types::Normalized), or the decode
/// error naming it when `Normalized::new` refuses it.
fn checked<E: serde::de::Error>(raw: f64) -> Result<tutti_plugin_types::Normalized, E> {
    tutti_plugin_types::Normalized::new(raw).ok_or_else(|| {
        E::invalid_value(
            serde::de::Unexpected::Float(raw),
            &"a normalized value in 0..=1",
        )
    })
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

    /// A wire value in `0..=1` decodes; one outside it, or NaN, fails the
    /// message, in each width and in the optional forms.
    #[test]
    fn a_normalized_outside_the_unit_range_is_refused() {
        use crate::protocol::{BridgeMessage, HostMessage, Normalized, ParamId};
        let f32_message = |raw: f32| {
            let mut bytes = vec![3, 0, 0, 0, 1, 0, 0, 0];
            bytes.extend(raw.to_le_bytes());
            bincode::deserialize::<HostMessage>(&bytes)
        };
        let f64_tail = |message: &dyn Fn() -> Vec<u8>, raw: f64| {
            let mut bytes = message();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            bytes
        };
        let get_text = || {
            bincode::serialize(&HostMessage::GetParameterText {
                param_id: ParamId::new(1),
                value: Normalized::of(0.5),
            })
            .unwrap()
        };
        let value_of = |raw: f32| {
            let mut bytes = bincode::serialize(&BridgeMessage::ParameterValue {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap();
            let end = bytes.len();
            bytes[end - 4..].copy_from_slice(&raw.to_le_bytes());
            bincode::deserialize::<BridgeMessage>(&bytes)
        };
        let from_text = || {
            bincode::serialize(&BridgeMessage::ParameterValueFromText {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap()
        };

        assert!(matches!(
            f32_message(0.25).unwrap(),
            HostMessage::SetParameter { value, .. } if value == Normalized::of(0.25)
        ));
        assert!(matches!(
            bincode::deserialize::<HostMessage>(&f64_tail(&get_text, 0.25)).unwrap(),
            HostMessage::GetParameterText { value, .. } if value == Normalized::of(0.25)
        ));
        assert!(matches!(
            bincode::deserialize::<BridgeMessage>(&f64_tail(&from_text, 0.25)).unwrap(),
            BridgeMessage::ParameterValueFromText { value } if value == Some(Normalized::of(0.25))
        ));
        assert!(matches!(
            value_of(0.25).unwrap(),
            BridgeMessage::ParameterValue { value } if value == Some(Normalized::of(0.25))
        ));
        for raw in [7.5, -3.0, f64::NAN, f64::INFINITY] {
            assert!(f32_message(raw as f32).is_err(), "f32 {raw}");
            assert!(value_of(raw as f32).is_err(), "optional f32 {raw}");
            assert!(
                bincode::deserialize::<HostMessage>(&f64_tail(&get_text, raw)).is_err(),
                "f64 {raw}"
            );
            assert!(
                bincode::deserialize::<BridgeMessage>(&f64_tail(&from_text, raw)).is_err(),
                "optional f64 {raw}"
            );
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
