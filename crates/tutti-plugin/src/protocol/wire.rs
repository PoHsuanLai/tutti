//! Serde adapters for wire fields whose in-memory type is not serde.

/// A block's MIDI list on the wire: a sequence of `(frame_offset, [u32; 4])`.
///
/// [`MidiEvent`](tutti_midi_types::ump::MidiEvent) is serde-free on purpose:
/// its `frame_offset` is relative to one block, so it is not a persistable
/// value. The wire is the one place it is encoded, and only through this
/// adapter, used as `#[serde(with = "wire::midi")]`.
///
/// Encoding walks the list in place and allocates nothing. Decoding builds the
/// inline [`MidiEventVec`](tutti_plugin_types::MidiEventVec) and rejects a list longer than
/// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) instead of truncating it.
pub mod midi {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec};

    /// Encodes each event as `(frame_offset, data)`.
    pub fn serialize<S: Serializer>(events: &MidiEventVec, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(events.iter().map(|e| (e.frame_offset, e.data)))
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
                while let Some((frame_offset, data)) = seq.next_element::<(u32, [u32; 4])>()? {
                    out.try_push(MidiEvent { frame_offset, data })
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

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "super::midi")]
        events: MidiEventVec,
    }

    fn ev(i: u32) -> MidiEvent {
        MidiEvent {
            frame_offset: i,
            data: [0x2090_3C40, 0, 0, 0],
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
        // variant 4, id, 0.25f32
        assert_eq!(
            bincode::serialize(&set).unwrap(),
            [4, 0, 0, 0, 4, 3, 2, 1, 0, 0, 128, 62]
        );

        let rate = HostMessage::SetSampleRate {
            rate: SampleRate(48_000.0),
        };
        // variant 9, 48000.0f64
        assert_eq!(
            bincode::serialize(&rate).unwrap(),
            [9, 0, 0, 0, 0, 0, 0, 0, 0, 112, 231, 64]
        );

        let set_reply = BridgeMessage::SampleRateSet {
            rate: SampleRate(48_000.0),
            error: None,
        };
        // variant 26, 48000.0f64, no error
        assert_eq!(
            bincode::serialize(&set_reply).unwrap(),
            [26, 0, 0, 0, 0, 0, 0, 0, 0, 112, 231, 64, 0]
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
        // variant 9, index 7, 0.5f32
        assert_eq!(
            bincode::serialize(&changed).unwrap(),
            [9, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 63]
        );

        let back: BridgeMessage =
            bincode::deserialize(&[9, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 63]).unwrap();
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
            let mut bytes = vec![4, 0, 0, 0, 1, 0, 0, 0];
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
}
