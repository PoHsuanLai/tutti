//! The Device Inquiry of a MIDI 1.0 port (M1 p.40): the initiator's side of
//! Identity Request and Reply.
//!
//! A MIDI 1.0 port with no Function Blocks has no UMP Stream to ask what it is
//! (M2-104 Appendix I), so the Endpoint Discovery of a [`Connection`](crate::Connection)
//! has nobody to answer it. What a device of that kind can say about itself is
//! its Identity Reply:
//!
//! ```text
//! F0 7E 7F 06 01 F7                                  Identity Request, to every device
//! F0 7E <id> 06 02 mm ff ff dd dd ss ss ss ss F7     Identity Reply
//! ```
//!
//! An [`IdentityInquiry`] is the pure state machine over the reply and time,
//! shaped as `Connection` is: every step returns an [`InquiryAction`], and time
//! comes in as an [`Instant`] value. It sends nothing and reads no clock. The
//! reply itself is read by whoever owns the wire (`tutti_midi_protocol::read`
//! gives the [`DeviceIdentity`]).
//!
//! M1 gives no timeout, so the two figures are ours: a request that gets no
//! reply within [`INQUIRY_TIMEOUT`] is sent again, up to [`INQUIRY_ATTEMPTS`]
//! sends in all, and then the device is one that does not answer, which is an
//! ordinary MIDI 1.0 device and not an error (a device that does not implement
//! Device Inquiry simply never replies).

use std::time::{Duration, Instant};

use tutti_ump::{DeviceIdentity, MidiGroup, Sysex7Packets, Ump};

/// How long a request waits for its reply before it is sent again. One second
/// is ours: a reply is 15 bytes, under 5 ms on a 31.25 kbaud DIN cable, so a
/// device that has not answered in a second is not going to.
pub const INQUIRY_TIMEOUT: Duration = Duration::from_secs(1);

/// How many times the request is sent in all, the first included: the request
/// and at most one retry.
pub const INQUIRY_ATTEMPTS: u8 = 2;

/// The payload of an Identity Request to every device, between `F0` and `F7`:
/// Universal Non-Real Time, device id `7F` ("if `<device ID>` = 7FH then the
/// device should respond regardless of what `<device ID>` it is set to"),
/// General Information `06`, Identity Request `01`.
const REQUEST: [u8; 4] = [0x7E, 0x7F, 0x06, 0x01];

/// What the driver does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InquiryAction {
    /// Nothing; wait for a reply, or for the [`deadline`](IdentityInquiry::deadline).
    Wait,
    /// Send this packet to the device: the whole Identity Request, one SysEx7
    /// packet.
    Send(Ump),
    /// The device answered. Its identity is the one given.
    Answered(DeviceIdentity),
    /// The device never answered; it is a MIDI 1.0 device with no known
    /// identity.
    GaveUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The request was sent `attempts` times, the last at `sent`.
    Asking {
        sent: Instant,
        attempts: u8,
    },
    Done,
}

/// The initiator of one Identity Request on one MIDI 1.0 port.
#[derive(Clone, Debug)]
pub struct IdentityInquiry {
    group: MidiGroup,
    step: Step,
}

impl IdentityInquiry {
    /// An inquiry on `group`, started at `now`, and the request to send first.
    #[must_use]
    pub fn new(group: MidiGroup, now: Instant) -> (Self, InquiryAction) {
        let inquiry = Self {
            group,
            step: Step::Asking {
                sent: now,
                attempts: 1,
            },
        };
        let request = inquiry.request();
        (inquiry, InquiryAction::Send(request))
    }

    /// The Identity Request as one SysEx7 packet.
    fn request(&self) -> Ump {
        // Four 7-bit bytes are one complete packet (at most six fit).
        Sysex7Packets::checked(self.group, &REQUEST)
            .and_then(|mut packets| packets.next())
            .expect("the Identity Request is four 7-bit bytes")
    }

    /// When the inquiry next needs [`on_timeout`](Self::on_timeout), or `None`
    /// once it is over.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match self.step {
            Step::Asking { sent, .. } => Some(sent + INQUIRY_TIMEOUT),
            Step::Done => None,
        }
    }

    /// Whether the inquiry has ended, with an answer or without.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.step == Step::Done
    }

    /// The device sent an Identity Reply. Answered once: a second reply, or one
    /// after the inquiry gave up, is [`Wait`](InquiryAction::Wait).
    pub fn on_reply(&mut self, identity: DeviceIdentity) -> InquiryAction {
        match self.step {
            Step::Asking { .. } => {
                self.step = Step::Done;
                InquiryAction::Answered(identity)
            }
            Step::Done => InquiryAction::Wait,
        }
    }

    /// Time has passed: sends the request again once [`INQUIRY_TIMEOUT`] has
    /// gone by since the last send, or gives up after [`INQUIRY_ATTEMPTS`].
    /// Calling it early is harmless.
    pub fn on_timeout(&mut self, now: Instant) -> InquiryAction {
        let Step::Asking { sent, attempts } = self.step else {
            return InquiryAction::Wait;
        };
        if now.saturating_duration_since(sent) < INQUIRY_TIMEOUT {
            return InquiryAction::Wait;
        }
        if attempts >= INQUIRY_ATTEMPTS {
            self.step = Step::Done;
            return InquiryAction::GaveUp;
        }
        self.step = Step::Asking {
            sent: now,
            attempts: attempts + 1,
        };
        InquiryAction::Send(self.request())
    }
}

#[cfg(test)]
mod tests;
