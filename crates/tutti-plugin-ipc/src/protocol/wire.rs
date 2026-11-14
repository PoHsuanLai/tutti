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

/// A [`Normalized`](tutti_plugin_types::Normalized) carried as the `f64` it is,
/// as `#[serde(with = "wire::normalized")]`.
///
/// `Normalized` is an `f64` and every parameter value on this wire keeps its
/// width (a value such as 0.3 arrives as sent; protocol version 31 retired the
/// `f32` the set, get and changed messages carried).
///
/// Every `Normalized` on this wire is refused on decode when it is not in
/// `0..=1` (NaN included), as `Normalized::new` refuses: a peer that sends one
/// is broken, and the frame fails rather than carrying a value made up for it.
pub mod normalized {
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
/// optional `f64`, as `#[serde(with = "wire::normalized_opt")]`; see
/// [`normalized`].
pub mod normalized_opt {
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

/// An editor size as a [`dpi::Size`], in the encoding `dpi`'s `serde` feature gives it.
///
/// A logical extent is an `f64`, so on decode one that is not finite or is
/// negative is refused, as `Normalized::new` refuses its own: a peer that sends
/// one is broken, and the frame fails rather than carrying a size made up for
/// it. A physical size is any pair of `u32`. Used as
/// `#[serde(with = "wire::size")]`, and [`size_result`] and [`size_opt_result`]
/// for the answers that carry one.
pub mod size {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// A [`Size`] that decodes only if it is a size.
    #[derive(Deserialize)]
    #[serde(try_from = "Size")]
    pub(super) struct Checked(pub(super) Size);

    impl TryFrom<Size> for Checked {
        type Error = String;

        fn try_from(size: Size) -> Result<Self, String> {
            match size {
                Size::Logical(l)
                    if ![l.width, l.height]
                        .iter()
                        .all(|e| e.is_finite() && *e >= 0.0) =>
                {
                    Err(format!(
                        "a logical size needs finite, non-negative extents, got {}x{}",
                        l.width, l.height
                    ))
                }
                _ => Ok(Self(size)),
            }
        }
    }

    /// Encodes the size as `dpi` does.
    pub fn serialize<S: Serializer>(value: &Size, s: S) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes a size, refusing a logical one that is not finite and non-negative.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Size, D::Error> {
        Ok(Checked::deserialize(d)?.0)
    }
}

/// A `Result<Size, String>` carried as [`size`] does.
pub mod size_result {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::size::Checked;

    /// Encodes the result as it is.
    pub fn serialize<S: Serializer>(value: &Result<Size, String>, s: S) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes the result, refusing a bad size in `Ok`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Result<Size, String>, D::Error> {
        Ok(Result::<Checked, String>::deserialize(d)?.map(|c| c.0))
    }
}

/// A `Result<Option<Size>, String>` carried as [`size`] does.
pub mod size_opt_result {
    use dpi::Size;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::size::Checked;

    /// Encodes the result as it is.
    pub fn serialize<S: Serializer>(
        value: &Result<Option<Size>, String>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }

    /// Decodes the result, refusing a bad size in `Ok(Some(_))`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Result<Option<Size>, String>, D::Error> {
        Ok(Result::<Option<Checked>, String>::deserialize(d)?.map(|o| o.map(|c| c.0)))
    }
}

/// A byte field (a state chunk) as one byte string on the wire.
///
/// Serde writes a plain `Vec<u8>` as a sequence, which the codec encodes one
/// `u8` call at a time; a state chunk is megabytes. As bytes it is one copy
/// each way. The encoding is the same: postcard writes a byte
/// string and a byte sequence alike, as a varint length and the bytes.
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
    #[test]
    fn a_list_past_the_cap_is_rejected() {
        let n = MIDI_STACK_CAPACITY as u64 + 1;
        // The length is a varint, as postcard writes a sequence's.
        let mut bytes = postcard::to_allocvec(&n).unwrap();
        for i in 0..n as u32 {
            bytes.extend(postcard::to_allocvec(&(i, [0x2090_3C40u32, 0, 0, 0])).unwrap());
        }
        assert!(postcard::from_bytes::<Holder>(&bytes).is_err());

        let full: MidiEventVec = (0..MIDI_STACK_CAPACITY as u32).map(ev).collect();
        let at_cap = Holder { events: full };
        let back: Holder = postcard::from_bytes(&postcard::to_allocvec(&at_cap).unwrap()).unwrap();
        assert_eq!(back, at_cap);
    }

    /// **The typed parameter and rate fields encode as bare numbers**: a `u32`
    /// id, an `f64` value (the width a `Normalized` holds, not an `f32`), an
    /// `f64` rate, an `Option<f64>` as a tag byte and the value. The expected bytes are
    /// written out from postcard's rules (an integer or a variant tag is a
    /// varint, seven bits a byte with the low group first; a float is its
    /// little-endian bytes), so a layout change fails here instead of as a
    /// desync between two binaries built apart.
    #[test]
    fn typed_parameter_and_rate_fields_keep_their_wire_widths() {
        use crate::protocol::{BridgeMessage, HostMessage, Normalized, ParamEdit, ParamId};
        use tutti_types::SampleRate;

        let set = HostMessage::SetParameter {
            param_id: ParamId::new(0x0102_0304),
            value: Normalized::of(0.25),
        };
        // variant 3, id 0x0102_0304 as a varint, 0.25f64
        assert_eq!(
            postcard::to_allocvec(&set).unwrap(),
            [3, 0x84, 0x86, 0x88, 0x08, 0, 0, 0, 0, 0, 0, 208, 63]
        );

        let rate = HostMessage::SetSampleRate {
            rate: SampleRate::SR_48K,
        };
        // variant 7, 48000.0f64
        assert_eq!(
            postcard::to_allocvec(&rate).unwrap(),
            [7, 0, 0, 0, 0, 0, 112, 231, 64]
        );

        let set_reply = BridgeMessage::SampleRateSet {
            rate: SampleRate::SR_48K,
            error: None,
        };
        // variant 22 (24 until v39 removed two earlier messages), 48000.0f64, no error
        assert_eq!(
            postcard::to_allocvec(&set_reply).unwrap(),
            [22, 0, 0, 0, 0, 0, 112, 231, 64, 0]
        );

        let some = BridgeMessage::ParameterValue {
            value: Some(Normalized::of(0.25)),
        };
        // variant 2, Some, 0.25f64
        assert_eq!(
            postcard::to_allocvec(&some).unwrap(),
            [2, 1, 0, 0, 0, 0, 0, 0, 208, 63]
        );
        let none = BridgeMessage::ParameterValue { value: None };
        assert_eq!(postcard::to_allocvec(&none).unwrap(), [2, 0]);

        let changed = BridgeMessage::ParameterChanged {
            edit: ParamEdit::change(ParamId::new(7), Normalized::of(0.5)),
        };
        // variant 5 (7 until v39 removed two earlier messages), then the `ParamEdit`: its variant (`Change` is 1), id 7, 0.5f64
        // and the recording flag (true).
        assert_eq!(
            postcard::to_allocvec(&changed).unwrap(),
            [5, 1, 7, 0, 0, 0, 0, 0, 0, 224, 63, 1]
        );
        let back: BridgeMessage =
            postcard::from_bytes(&[5, 1, 7, 0, 0, 0, 0, 0, 0, 224, 63, 1]).unwrap();
        assert!(matches!(
            back,
            BridgeMessage::ParameterChanged { edit }
                if edit == ParamEdit::change(ParamId::new(7), Normalized::of(0.5))
        ));

        // A gesture's begin and end hold the id alone: no value is on the wire
        // to read, and none is made up.
        let begin = BridgeMessage::ParameterChanged {
            edit: ParamEdit::Begin(ParamId::new(7)),
        };
        assert_eq!(postcard::to_allocvec(&begin).unwrap(), [5, 0, 7]);
        let end = BridgeMessage::ParameterChanged {
            edit: ParamEdit::End(ParamId::new(7)),
        };
        assert_eq!(postcard::to_allocvec(&end).unwrap(), [5, 2, 7]);
    }

    /// **A rate that is no rate fails the message**: a `SampleRate` decodes only
    /// from a finite `f64` above zero, so zero, a negative, NaN and infinity
    /// never reach the server's clock or a plugin.
    #[test]
    fn a_rate_that_is_no_rate_does_not_decode() {
        use crate::protocol::HostMessage;
        for bad in [
            0.0f64,
            -48_000.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let mut bytes = vec![7u8];
            bytes.extend(bad.to_le_bytes());
            assert!(
                postcard::from_bytes::<HostMessage>(&bytes).is_err(),
                "{bad} decoded"
            );
        }
        let mut ok = vec![7u8];
        ok.extend(48_000.0f64.to_le_bytes());
        assert!(matches!(
            postcard::from_bytes::<HostMessage>(&ok),
            Ok(HostMessage::SetSampleRate { .. })
        ));
    }

    /// A wire value in `0..=1` decodes; one outside it, or NaN, fails the
    /// message, in each width and in the optional forms.
    /// **The editor messages are appended after every earlier variant, in the
    /// widths the spec's rules give.** A variant tag is a varint over declaration
    /// order, so the earlier variants keep their numbers (`SetSampleRate` is 8
    /// above) and the new ones sit at the end (host: 23 to 25; server: 28 to 31);
    /// a size is a unit tag then two varints (physical) or two `f64` (logical), a
    /// scale an `f64`, a parent an `Option<u64>`.
    #[test]
    fn the_editor_messages_follow_every_earlier_variant() {
        use crate::protocol::{BridgeMessage, HostMessage};
        use dpi::{LogicalSize, PhysicalSize, Size};
        use tutti_plugin_types::ContentScale;

        let size = Size::Physical(PhysicalSize::new(640, 480));
        // Tag 0 (physical), then 640 = 0x280 and 480 = 0x1e0, seven bits a byte, low
        // group first.
        let size_bytes = [0x00, 0x80, 0x05, 0xe0, 0x03];
        // Tag 1 (logical), then 1.0 and 2.0 as little-endian `f64`.
        assert_eq!(
            postcard::to_allocvec(&Size::Logical(LogicalSize::new(1.0, 2.0))).unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0, 0, 0, 0, 0, 0, 0, 0x40]
        );
        let open = HostMessage::OpenEditor {
            parent: None,
            scale: ContentScale::IDENTITY,
        };
        assert_eq!(
            postcard::to_allocvec(&open).unwrap(),
            [22, 0, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f]
        );
        let embedded = HostMessage::OpenEditor {
            parent: Some(300),
            scale: ContentScale::IDENTITY,
        };
        assert_eq!(
            postcard::to_allocvec(&embedded).unwrap(),
            [22, 1, 0xac, 0x02, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f]
        );
        assert_eq!(
            postcard::to_allocvec(&HostMessage::CloseEditor).unwrap(),
            [23]
        );
        let mut resize = vec![24];
        resize.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&HostMessage::SetEditorSize { size }).unwrap(),
            resize
        );

        let mut opened = vec![26, 0, 1];
        opened.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&BridgeMessage::EditorOpened {
                result: Ok(Some(size))
            })
            .unwrap(),
            opened
        );
        assert_eq!(
            postcard::to_allocvec(&BridgeMessage::EditorOpened {
                result: Err("x".into())
            })
            .unwrap(),
            [26, 1, 1, b'x']
        );
        let mut sized = vec![27, 0];
        sized.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&BridgeMessage::EditorSized { result: Ok(size) }).unwrap(),
            sized
        );
        let mut wants = vec![28];
        wants.extend(size_bytes);
        assert_eq!(
            postcard::to_allocvec(&BridgeMessage::EditorWantsSize { size }).unwrap(),
            wants
        );
        assert_eq!(
            postcard::to_allocvec(&BridgeMessage::EditorClosed).unwrap(),
            [29]
        );
        // The scale is a checked value on the wire: a NaN is refused.
        let mut nan = vec![22, 0];
        nan.extend(f64::NAN.to_le_bytes());
        assert!(postcard::from_bytes::<HostMessage>(&nan).is_err());
    }

    /// **A logical size that is not finite and non-negative is refused on
    /// decode**, in each message that carries a size; a physical one and a good
    /// logical one decode.
    #[test]
    fn a_bad_logical_size_is_refused_on_decode() {
        use crate::protocol::{BridgeMessage, HostMessage};
        use dpi::{LogicalSize, PhysicalSize, Size};

        let logical = |w: f64, h: f64| Size::Logical(LogicalSize::new(w, h));
        let bad = [
            logical(f64::NAN, 1.0),
            logical(1.0, f64::INFINITY),
            logical(-1.0, 1.0),
            logical(1.0, f64::NEG_INFINITY),
        ];
        let roundtrips = |size: Size| {
            let host = postcard::to_allocvec(&HostMessage::SetEditorSize { size }).unwrap();
            let wants = postcard::to_allocvec(&BridgeMessage::EditorWantsSize { size }).unwrap();
            let sized =
                postcard::to_allocvec(&BridgeMessage::EditorSized { result: Ok(size) }).unwrap();
            let opened = postcard::to_allocvec(&BridgeMessage::EditorOpened {
                result: Ok(Some(size)),
            })
            .unwrap();
            [
                postcard::from_bytes::<HostMessage>(&host).is_ok(),
                postcard::from_bytes::<BridgeMessage>(&wants).is_ok(),
                postcard::from_bytes::<BridgeMessage>(&sized).is_ok(),
                postcard::from_bytes::<BridgeMessage>(&opened).is_ok(),
            ]
        };
        for size in bad {
            assert_eq!(roundtrips(size), [false; 4], "{size:?}");
        }
        for size in [
            logical(0.0, 480.5),
            Size::Physical(PhysicalSize::new(0, u32::MAX)),
        ] {
            assert_eq!(roundtrips(size), [true; 4], "{size:?}");
        }
    }

    #[test]
    fn a_normalized_outside_the_unit_range_is_refused() {
        use crate::protocol::{BridgeMessage, HostMessage, Normalized, ParamId};
        let set_message = |raw: f64| {
            // variant 3, id 1 (one varint byte each), then the f64.
            let mut bytes = vec![3, 1];
            bytes.extend(raw.to_le_bytes());
            postcard::from_bytes::<HostMessage>(&bytes)
        };
        let f64_tail = |message: &dyn Fn() -> Vec<u8>, raw: f64| {
            let mut bytes = message();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            bytes
        };
        let get_text = || {
            postcard::to_allocvec(&HostMessage::GetParameterText {
                param_id: ParamId::new(1),
                value: Normalized::of(0.5),
            })
            .unwrap()
        };
        let value_of = |raw: f64| {
            let mut bytes = postcard::to_allocvec(&BridgeMessage::ParameterValue {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap();
            let end = bytes.len();
            bytes[end - 8..].copy_from_slice(&raw.to_le_bytes());
            postcard::from_bytes::<BridgeMessage>(&bytes)
        };
        let from_text = || {
            postcard::to_allocvec(&BridgeMessage::ParameterValueFromText {
                value: Some(Normalized::of(0.5)),
            })
            .unwrap()
        };

        assert!(matches!(
            set_message(0.25).unwrap(),
            HostMessage::SetParameter { value, .. } if value == Normalized::of(0.25)
        ));
        assert!(matches!(
            postcard::from_bytes::<HostMessage>(&f64_tail(&get_text, 0.25)).unwrap(),
            HostMessage::GetParameterText { value, .. } if value == Normalized::of(0.25)
        ));
        assert!(matches!(
            postcard::from_bytes::<BridgeMessage>(&f64_tail(&from_text, 0.25)).unwrap(),
            BridgeMessage::ParameterValueFromText { value } if value == Some(Normalized::of(0.25))
        ));
        assert!(matches!(
            value_of(0.25).unwrap(),
            BridgeMessage::ParameterValue { value } if value == Some(Normalized::of(0.25))
        ));
        for raw in [7.5, -3.0, f64::NAN, f64::INFINITY] {
            assert!(set_message(raw).is_err(), "set {raw}");
            assert!(value_of(raw).is_err(), "optional reply {raw}");
            assert!(
                postcard::from_bytes::<HostMessage>(&f64_tail(&get_text, raw)).is_err(),
                "f64 {raw}"
            );
            assert!(
                postcard::from_bytes::<BridgeMessage>(&f64_tail(&from_text, raw)).is_err(),
                "optional f64 {raw}"
            );
        }
    }

    /// **`wire::bytes` is not a protocol change**: a byte field encodes to the
    /// bytes a plain `Vec<u8>` did, and decodes them. (What it buys, one copy
    /// rather than a call per byte, is speed, which no test here asserts.)
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
        let plain = postcard::to_allocvec(&Plain {
            bytes: data.clone(),
        })
        .unwrap();
        let coded = postcard::to_allocvec(&Coded {
            bytes: data.clone(),
        })
        .unwrap();
        assert_eq!(coded, plain);
        let back: Coded = postcard::from_bytes(&plain).unwrap();
        assert_eq!(back.bytes, data);
    }
}
