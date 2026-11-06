//! Serde adapters for wire fields whose in-memory type is not serde.

/// A block's MIDI list on the wire: a sequence of `(frame_offset, [u32; 4])`.
///
/// [`MidiEvent`](tutti_midi_types::ump::MidiEvent) is serde-free on purpose:
/// its `frame_offset` is relative to one block, so it is not a persistable
/// value. The wire is the one place it is encoded, and only through this
/// adapter, used as `#[serde(with = "wire::midi")]`.
///
/// Encoding walks the list in place and allocates nothing. Decoding builds the
/// inline [`MidiEventVec`] and rejects a list longer than
/// [`MIDI_STACK_CAPACITY`] instead of truncating it.
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
}
