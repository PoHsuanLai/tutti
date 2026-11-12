//! ACK and NAK (M2-101 §5.10 and §5.11, Tables 13 to 16): the two share one
//! body, an original sub-ID#2, a status code and data, five bytes of detail and
//! a text.

use std::vec::Vec;

use tutti_ump::U7;

use super::MessageText;

/// Fixes the two bodies as one shape: the spec lays out Table 13 and Table 15
/// field for field alike, and what differs is the codes and the sub-ID#2.
macro_rules! status_message {
    (
        $(#[$doc:meta])*
        $name:ident, $sub:ident, $table:literal, $classification:literal
    ) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name {
            #[doc = $classification]
            pub $sub: U7,
            /// The status code: why.
            pub status_code: U7,
            /// Status data, the secondary detail some codes require, 0 when "no
            /// additional information is needed".
            pub status_data: U7,
            /// "Details for each Sub ID Classification": 5 bytes whose meaning
            /// depends on the original sub-ID#2 (the Profile ID, or a Property
            /// Exchange stream id and chunk number; reserved for Process
            /// Inquiry). Zero when a transaction has no detail to add.
            pub details: [U7; 5],
            /// The human-readable reason (Message Length and Message Text).
            pub message: MessageText,
        }

        impl $name {
            /// A message for the original sub-ID#2 `sub_id2` with a status
            /// code, no status data, no details and no text.
            #[must_use]
            pub fn new(sub_id2: U7, status_code: U7) -> Self {
                Self {
                    $sub: sub_id2,
                    status_code,
                    ..Self::default()
                }
            }

            /// With status data.
            #[must_use]
            pub fn with_status_data(mut self, data: U7) -> Self {
                self.status_data = data;
                self
            }

            /// With the 5-byte detail block.
            #[must_use]
            pub fn with_details(mut self, details: [U7; 5]) -> Self {
                self.details = details;
                self
            }

            /// With a text.
            #[must_use]
            pub fn with_message(mut self, message: MessageText) -> Self {
                self.message = message;
                self
            }

            #[doc = concat!("The body of ", $table, ".")]
            pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
                out.extend([
                    self.$sub.get(),
                    self.status_code.get(),
                    self.status_data.get(),
                ]);
                out.extend(self.details.iter().map(|d| d.get()));
                let mut text = Vec::new();
                self.message.encode(&mut text);
                // The length is a 14-bit field; the text of a message we hold
                // is at most 103 bytes, and one read from a peer fits it.
                let len = text.len().min(0x3FFF);
                out.extend([(len & 0x7F) as u8, ((len >> 7) & 0x7F) as u8]);
                out.extend(text.into_iter().take(len));
            }

            /// Reads the body, or `None` when it is malformed: a byte with its
            /// top bit set, or a Message Length that runs past the message.
            ///
            /// Everything past the first byte was added in Message Format
            /// Version 2, and an older device sent a NAK with no body at all, so
            /// a body that stops before a field defaults it: "the fields
            /// defined in the received version" (§5.3).
            pub(crate) fn decode_body(b: &[u8]) -> Option<Self> {
                let byte = |i: usize| -> Option<U7> {
                    b.get(i).map_or(Some(U7::default()), |&v| U7::new(v))
                };
                let mut details = [U7::default(); 5];
                for (i, slot) in details.iter_mut().enumerate() {
                    *slot = byte(3 + i)?;
                }
                let message = match (b.get(8), b.get(9)) {
                    (Some(&lo), Some(&hi)) => {
                        if lo & 0x80 != 0 || hi & 0x80 != 0 {
                            return None;
                        }
                        let len = usize::from(lo) | (usize::from(hi) << 7);
                        MessageText::decode(b.get(10..10 + len)?)
                    }
                    _ => MessageText::default(),
                };
                Some(Self {
                    $sub: byte(0)?,
                    status_code: byte(1)?,
                    status_data: byte(2)?,
                    details,
                    message,
                })
            }
        }
    };
}

status_message!(
    /// An ACK (M2-101 §5.10, Table 13): the positive answer or the notice of
    /// an activity under way, such as a Timeout Wait. "This MIDI-CI message is
    /// defined for new mechanisms which are not published at the time of this
    /// specification. Look for usage explanations in other specifications which
    /// reference this message."
    Ack,
    ack_sub_id2,
    "Table 13",
    "\"Original Transaction Sub-ID#2 Classification\": the sub-ID#2 of the message this answers (§5.10.1)."
);

status_message!(
    /// A NAK (M2-101 §5.11, Table 15): the answer to a message a device does not
    /// understand. Everything past the first byte was added in Message Format
    /// Version 2; an older device sent an empty NAK, and each absent field reads
    /// as zero or empty.
    Nak,
    nak_sub_id2,
    "Table 15",
    "\"Original Transaction Sub-ID#2 Classification\": the sub-ID#2 of the message being NAK'd (0 if unknown); with the other fields it \"is used ... to link the NAK message with the original Transaction\" (§5.11.1)."
);

impl Ack {
    /// "ACK", success, do not retry (Table 14).
    pub const STATUS_ACK: U7 = U7::of(0x00);
    /// "Timeout Wait": the status data is the time to wait in multiples of 100
    /// milliseconds (0 to 12.7 seconds).
    pub const STATUS_TIMEOUT_WAIT: U7 = U7::of(0x10);
    /// "Flow Control: Send next Chunks" (Property Exchange).
    pub const STATUS_FLOW_CONTROL_NEXT_CHUNKS: U7 = U7::of(0x11);
}

impl Nak {
    /// "NAK": generic failure, do not retry (Table 16, `0x00` to `0x1F`).
    pub const STATUS_NAK: U7 = U7::of(0x00);
    /// "MIDI-CI message not supported".
    pub const STATUS_MESSAGE_NOT_SUPPORTED: U7 = U7::of(0x01);
    /// "MIDI-CI version not supported".
    pub const STATUS_VERSION_NOT_SUPPORTED: U7 = U7::of(0x02);
    /// "Channel/Group/Function Block Not in use".
    pub const STATUS_NOT_IN_USE: U7 = U7::of(0x03);
    /// "Profile not supported on the requested Channel, Group, or Function
    /// Block".
    pub const STATUS_PROFILE_NOT_SUPPORTED: U7 = U7::of(0x04);
    /// "Flow Control: Resend the most recent Chunk" (do not retry class).
    pub const STATUS_RESEND_LAST_CHUNK: U7 = U7::of(0x12);
    /// "Responder Terminates Transaction" (status data 0) or "Initiator
    /// Terminates Transaction" (status data 1); `0x20` to `0x3F` are
    /// notifications for which further action is recommended.
    pub const STATUS_TERMINATE: U7 = U7::of(0x20);
    /// "Property Exchange Chunks are out of sequence (cannot recover)".
    pub const STATUS_CHUNKS_OUT_OF_SEQUENCE: U7 = U7::of(0x21);
    /// "Error occurred, please retry": unlike `0x00` to `0x1F`, retry is
    /// recommended (`0x40` to `0x5F`).
    pub const STATUS_RETRY: U7 = U7::of(0x40);
    /// "Message was malformed".
    pub const STATUS_MALFORMED: U7 = U7::of(0x41);
    /// "Timeout has occurred".
    pub const STATUS_TIMEOUT: U7 = U7::of(0x42);
    /// "Busy, try again": the status data is the time to wait in multiples of
    /// 100 milliseconds (0 to 12.7 seconds).
    pub const STATUS_BUSY: U7 = U7::of(0x43);
}

#[cfg(test)]
mod tests;
