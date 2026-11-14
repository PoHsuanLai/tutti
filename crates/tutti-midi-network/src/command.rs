//! [`Command`]: one Command Packet (M2-124 section 5.4) as a value, with the
//! one writer and the one reader of its bytes.

use tutti_midi_device::{EndpointName, NameError, ProductInstanceId};
use tutti_ump::{Ump, split_stream};

use crate::{Digest, Nonce, SequenceNumber, Text, Username};

/// Command Codes (M2-124 Table 8).
mod code {
    pub(super) const INVITATION: u8 = 0x01;
    pub(super) const INVITATION_AUTH: u8 = 0x02;
    pub(super) const INVITATION_USER_AUTH: u8 = 0x03;
    pub(super) const ACCEPTED: u8 = 0x10;
    pub(super) const PENDING: u8 = 0x11;
    pub(super) const AUTH_REQUIRED: u8 = 0x12;
    pub(super) const USER_AUTH_REQUIRED: u8 = 0x13;
    pub(super) const PING: u8 = 0x20;
    pub(super) const PING_REPLY: u8 = 0x21;
    pub(super) const RETRANSMIT_REQUEST: u8 = 0x80;
    pub(super) const RETRANSMIT_ERROR: u8 = 0x81;
    pub(super) const SESSION_RESET: u8 = 0x82;
    pub(super) const SESSION_RESET_REPLY: u8 = 0x83;
    pub(super) const NAK: u8 = 0x8F;
    pub(super) const BYE: u8 = 0xF0;
    pub(super) const BYE_REPLY: u8 = 0xF1;
    pub(super) const UMP_DATA: u8 = 0xFF;
}

/// The most 32-bit words a UMP Data Command carries (M2-124 Table 29).
pub const MAX_UMP_DATA_WORDS: usize = 64;

/// The most words an Endpoint Name takes in a command: 98 bytes.
const NAME_WORDS_MAX: usize = 25;
/// The most words a Product Instance Id takes in a command: 42 bytes.
const ID_WORDS_MAX: usize = 11;

/// The first word of a Command Packet: the Command Code, the payload length in
/// 32-bit words and 16 bits of command-specific data (M2-124 Table 7).
///
/// A NAK carries the header of the command it refuses, so the header is a
/// value of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Header(u32);

impl Header {
    /// The header with this code, payload length (in words) and data.
    #[must_use]
    pub const fn new(code: u8, payload_words: u8, data: u16) -> Self {
        Self((code as u32) << 24 | (payload_words as u32) << 16 | data as u32)
    }

    /// The header read from its word.
    #[must_use]
    pub const fn from_word(word: u32) -> Self {
        Self(word)
    }

    /// The header's word.
    #[must_use]
    pub const fn word(self) -> u32 {
        self.0
    }

    /// The Command Code.
    #[must_use]
    pub const fn code(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// The payload length in 32-bit words.
    #[must_use]
    pub const fn payload_words(self) -> u8 {
        (self.0 >> 16) as u8
    }

    /// The 16-bit command-specific data.
    #[must_use]
    pub const fn data(self) -> u16 {
        self.0 as u16
    }

    const fn data1(self) -> u8 {
        (self.0 >> 8) as u8
    }

    const fn data2(self) -> u8 {
        self.0 as u8
    }
}

/// The name and Product Instance Id an endpoint presents to its peer in an
/// Invitation and in every reply to one (M2-124 sections 6.4 and 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndpointIdentity {
    /// The UMP Endpoint Name.
    pub name: EndpointName,
    /// The Product Instance Id.
    pub instance_id: ProductInstanceId,
}

/// The authentication methods a Client can answer (M2-124 Table 11): a set of
/// [`SHARED_SECRET`](Self::SHARED_SECRET) and [`USER`](Self::USER).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AuthMethods(u8);

impl AuthMethods {
    /// None: the Client can answer no challenge.
    pub const NONE: Self = Self(0);
    /// An Invitation with Authentication: a shared secret.
    pub const SHARED_SECRET: Self = Self(0b01);
    /// An Invitation with User Authentication: a user name and password.
    pub const USER: Self = Self(0b10);

    /// The set both sets hold.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every method of `other` is in this set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The Capabilities byte. The reserved bits are zero.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The set a Capabilities byte names: the reserved bits are ignored.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0b11)
    }
}

/// An Invitation (M2-124 section 6.4): a Client asks a Host for a Session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Invitation {
    /// The authentication methods the Client can answer.
    pub methods: AuthMethods,
    /// Who the Client is.
    pub identity: EndpointIdentity,
}

/// Why a Host asks for the shared secret again (M2-124 Table 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthState {
    /// The first challenge, in answer to an Invitation.
    First,
    /// The digest in the last Invitation with Authentication was wrong.
    DigestWrong,
}

/// Why a Host asks for a user name and password again (M2-124 Table 17).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserAuthState {
    /// The first challenge, in answer to an Invitation.
    First,
    /// The digest in the last Invitation with User Authentication was wrong.
    DigestWrong,
    /// The user name in the last Invitation with User Authentication is not
    /// one the Host knows.
    UserNotFound,
}

/// An Invitation Reply: Authentication Required (M2-124 section 6.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuthChallenge {
    /// Why the Host asks.
    pub state: AuthState,
    /// The nonce the Client hashes with the shared secret.
    pub nonce: Nonce,
    /// Who the Host is.
    pub identity: EndpointIdentity,
}

/// An Invitation Reply: User Authentication Required (M2-124 section 6.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserAuthChallenge {
    /// Why the Host asks.
    pub state: UserAuthState,
    /// The nonce the Client hashes with the user name and password.
    pub nonce: Nonce,
    /// Who the Host is.
    pub identity: EndpointIdentity,
}

/// An Invitation with User Authentication (M2-124 section 6.10).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserAuthentication {
    /// The digest of the nonce, the user name and the password.
    pub digest: Digest,
    /// The user name.
    pub username: Username,
}

/// A Ping's identifier (M2-124 section 6.13): the sender's own, matched
/// against the Ping Reply and used for nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PingId(pub u32);

/// A Retransmit Request (M2-124 section 7.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RetransmitRequest {
    /// The first UMP Data Command asked for.
    pub first: SequenceNumber,
    /// How many commands are asked for; `0` asks for every one sent since
    /// `first`.
    pub count: u16,
}

/// Why a Retransmit Error (M2-124 Table 32).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetransmitErrorReason {
    /// Unknown.
    Unknown,
    /// The sender's transmit buffer no longer holds a requested command.
    BufferMiss,
    /// A reserved value, kept as sent.
    Reserved(u8),
}

impl RetransmitErrorReason {
    const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::BufferMiss => 1,
            Self::Reserved(code) => code,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Unknown,
            1 => Self::BufferMiss,
            code => Self::Reserved(code),
        }
    }
}

/// A Retransmit Error (M2-124 section 7.2.4): the sender cannot resend what
/// was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RetransmitRefusal {
    /// Why.
    pub reason: RetransmitErrorReason,
    /// The oldest UMP Data Command the sender still holds: every earlier
    /// number asked for is lost.
    pub first: SequenceNumber,
}

/// Why a NAK (M2-124 Table 25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NakReason {
    /// Another reason; the text says which.
    Other,
    /// The receiver does not implement the command.
    NotSupported,
    /// The receiver implements the command but did not expect it now.
    NotExpected,
    /// The command's payload is missing or holds a value that cannot be used.
    Malformed,
    /// A Ping Reply carried an Id that was not sent.
    BadPingReply,
    /// A reserved value, kept as sent.
    Reserved(u8),
}

impl NakReason {
    const fn code(self) -> u8 {
        match self {
            Self::Other => 0x00,
            Self::NotSupported => 0x01,
            Self::NotExpected => 0x02,
            Self::Malformed => 0x03,
            Self::BadPingReply => 0x20,
            Self::Reserved(code) => code,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0x00 => Self::Other,
            0x01 => Self::NotSupported,
            0x02 => Self::NotExpected,
            0x03 => Self::Malformed,
            0x20 => Self::BadPingReply,
            code => Self::Reserved(code),
        }
    }
}

/// A NAK (M2-124 section 6.15): a report that a received command could not be
/// acted on. It ends no Session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Nak {
    /// Why.
    pub reason: NakReason,
    /// The header of the command that is refused.
    pub refused: Header,
    /// Words for the user; may be empty.
    pub text: Text,
}

/// Why a Session ends (M2-124 Table 27).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ByeReason {
    /// Unknown or undefined.
    Unknown,
    /// The user ended the Session.
    UserTerminated,
    /// The sender is powering down.
    PowerDown,
    /// Missing UMP Data Commands cannot be recovered.
    TooManyMissingPackets,
    /// The peer stopped answering.
    Timeout,
    /// The sender is in no Established Session with the peer.
    SessionNotEstablished,
    /// A reply arrived to an Invitation that was never sent.
    NoPendingInvitation,
    /// A command broke the protocol, such as an Invitation without a name.
    ProtocolError,
    /// The Host has as many Sessions as it serves (Host to Client).
    TooManySessions,
    /// An authenticated Invitation came with no Invitation before it (Host to
    /// Client).
    NoPriorInvitation,
    /// The user at the Host did not accept the Session (Host to Client).
    UserDeclined,
    /// The digest was wrong (Host to Client).
    AuthenticationFailed,
    /// The user name is unknown (Host to Client).
    UsernameNotFound,
    /// The Host and the Client share no authentication method (Host to
    /// Client).
    NoMatchingAuthentication,
    /// The Client gave up on its Invitation (Client to Host).
    InvitationCanceled,
    /// A reserved value, kept as sent.
    Reserved(u8),
}

impl ByeReason {
    /// The Bye Reason code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0x00,
            Self::UserTerminated => 0x01,
            Self::PowerDown => 0x02,
            Self::TooManyMissingPackets => 0x03,
            Self::Timeout => 0x04,
            Self::SessionNotEstablished => 0x05,
            Self::NoPendingInvitation => 0x06,
            Self::ProtocolError => 0x07,
            Self::TooManySessions => 0x40,
            Self::NoPriorInvitation => 0x41,
            Self::UserDeclined => 0x42,
            Self::AuthenticationFailed => 0x43,
            Self::UsernameNotFound => 0x44,
            Self::NoMatchingAuthentication => 0x45,
            Self::InvitationCanceled => 0x80,
            Self::Reserved(code) => code,
        }
    }

    /// The reason a Bye Reason code names.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0x00 => Self::Unknown,
            0x01 => Self::UserTerminated,
            0x02 => Self::PowerDown,
            0x03 => Self::TooManyMissingPackets,
            0x04 => Self::Timeout,
            0x05 => Self::SessionNotEstablished,
            0x06 => Self::NoPendingInvitation,
            0x07 => Self::ProtocolError,
            0x40 => Self::TooManySessions,
            0x41 => Self::NoPriorInvitation,
            0x42 => Self::UserDeclined,
            0x43 => Self::AuthenticationFailed,
            0x44 => Self::UsernameNotFound,
            0x45 => Self::NoMatchingAuthentication,
            0x80 => Self::InvitationCanceled,
            code => Self::Reserved(code),
        }
    }
}

/// A Bye (M2-124 section 6.16): the sender ends the Session, or an Invitation
/// that is pending.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bye {
    /// Why.
    pub reason: ByeReason,
    /// Words for the user; may be empty.
    pub text: Text,
}

impl Bye {
    /// A Bye with no text.
    #[must_use]
    pub fn new(reason: ByeReason) -> Self {
        Self {
            reason,
            text: Text::default(),
        }
    }
}

/// A UMP Data Command (M2-124 section 7.1): zero or more whole UMPs under one
/// sequence number. No UMP is split across commands, so the words always
/// divide into packets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UmpData {
    sequence: SequenceNumber,
    words: Vec<u32>,
}

/// Why a UMP Data Command cannot be built or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum UmpDataError {
    /// The packets take more than 64 words.
    #[error("the packets take {words} words, and a UMP Data Command carries at most 64")]
    TooLong {
        /// The words the packets take.
        words: usize,
    },
    /// The words end inside a packet.
    #[error("the words end inside a UMP")]
    Truncated,
}

impl UmpData {
    /// A command carrying `packets` under `sequence`; empty `packets` make the
    /// zero-length command that declares an idle period.
    ///
    /// # Errors
    ///
    /// [`UmpDataError::TooLong`] when the packets take more than 64 words.
    pub fn new(sequence: SequenceNumber, packets: &[Ump]) -> Result<Self, UmpDataError> {
        let words: usize = packets.iter().map(|p| p.words().len()).sum();
        if words > MAX_UMP_DATA_WORDS {
            return Err(UmpDataError::TooLong { words });
        }
        Ok(Self {
            sequence,
            words: packets
                .iter()
                .flat_map(|p| p.words().iter().copied())
                .collect(),
        })
    }

    /// The zero-length command that declares an idle period (M2-124 section
    /// 7.2.1).
    #[must_use]
    pub const fn idle(sequence: SequenceNumber) -> Self {
        Self {
            sequence,
            words: Vec::new(),
        }
    }

    /// `packets` cut into commands of at most 64 words, none splitting a UMP,
    /// numbered `first`, `first + 1` and on.
    #[must_use]
    pub fn chunks(first: SequenceNumber, packets: &[Ump]) -> Vec<Self> {
        let mut out: Vec<Self> = Vec::new();
        let mut sequence = first;
        let mut current: Vec<u32> = Vec::new();
        for packet in packets {
            let words = packet.words();
            if current.len() + words.len() > MAX_UMP_DATA_WORDS {
                out.push(Self {
                    sequence,
                    words: std::mem::take(&mut current),
                });
                sequence = sequence.next();
            }
            current.extend_from_slice(words);
        }
        if !current.is_empty() {
            out.push(Self {
                sequence,
                words: current,
            });
        }
        out
    }

    /// A command read from its payload words.
    fn from_words(sequence: SequenceNumber, words: Vec<u32>) -> Result<Self, UmpDataError> {
        // A payload ends on a packet boundary, or its last packet is cut.
        let whole: usize = split_stream(&words).map(|p| p.words().len()).sum();
        if whole != words.len() {
            return Err(UmpDataError::Truncated);
        }
        if words.len() > MAX_UMP_DATA_WORDS {
            return Err(UmpDataError::TooLong { words: words.len() });
        }
        Ok(Self { sequence, words })
    }

    /// The sequence number.
    #[must_use]
    pub const fn sequence(&self) -> SequenceNumber {
        self.sequence
    }

    /// The payload words.
    #[must_use]
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Whether this declares an idle period: it carries no UMP.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.words.is_empty()
    }

    /// The packets, in the order sent.
    pub fn packets(&self) -> impl Iterator<Item = Ump> + '_ {
        split_stream(&self.words)
    }
}

/// One Command Packet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Invitation, `0x01` (Client to Host).
    Invitation(Invitation),
    /// Invitation with Authentication, `0x02` (Client to Host).
    InvitationWithAuth(Digest),
    /// Invitation with User Authentication, `0x03` (Client to Host).
    InvitationWithUserAuth(UserAuthentication),
    /// Invitation Reply: Accepted, `0x10` (Host to Client).
    Accepted(EndpointIdentity),
    /// Invitation Reply: Pending, `0x11` (Host to Client).
    Pending(EndpointIdentity),
    /// Invitation Reply: Authentication Required, `0x12` (Host to Client).
    AuthRequired(AuthChallenge),
    /// Invitation Reply: User Authentication Required, `0x13` (Host to
    /// Client).
    UserAuthRequired(UserAuthChallenge),
    /// Ping, `0x20`.
    Ping(PingId),
    /// Ping Reply, `0x21`.
    PingReply(PingId),
    /// Retransmit Request, `0x80`.
    RetransmitRequest(RetransmitRequest),
    /// Retransmit Error, `0x81`.
    RetransmitError(RetransmitRefusal),
    /// Session Reset, `0x82`.
    SessionReset,
    /// Session Reset Reply, `0x83`.
    SessionResetReply,
    /// NAK, `0x8F`.
    Nak(Nak),
    /// Bye, `0xF0`.
    Bye(Bye),
    /// Bye Reply, `0xF1`.
    ByeReply,
    /// UMP Data, `0xFF`.
    UmpData(UmpData),
}

/// Why a Command Packet is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ParseError {
    /// The command runs past the end of the datagram. The datagram cannot be
    /// read further: the next command's start is not known.
    #[error("the command runs past the end of the datagram")]
    Truncated,
    /// The Command Code is not one this crate knows.
    #[error("unknown command code {0:#04x}")]
    UnknownCode(u8),
    /// The payload length is outside what the command allows.
    #[error("payload of {words} words is not valid for command {code:#04x}")]
    Length {
        /// The Command Code.
        code: u8,
        /// The payload length in words.
        words: u8,
    },
    /// A name or a text is empty where the command requires one.
    #[error("a required text is empty")]
    EmptyText,
    /// A text is not UTF-8.
    #[error("a text is not UTF-8")]
    Utf8,
    /// The Endpoint Name is not a valid name.
    #[error("the Endpoint Name is invalid: {0}")]
    Name(NameError),
    /// The Product Instance Id is not a valid Id.
    #[error("the Product Instance Id is invalid: {0}")]
    InstanceId(NameError),
    /// A text is longer than its field allows.
    #[error("a text is longer than its field allows")]
    TextTooLong,
    /// The nonce is not ASCII.
    #[error("the nonce is not ASCII")]
    Nonce,
    /// An enumerated field holds a reserved value that has no meaning to act
    /// on.
    #[error("a reserved value in an enumerated field")]
    Reserved,
    /// A UMP Data Command does not hold whole UMPs, or holds too many words.
    #[error("{0}")]
    UmpData(UmpDataError),
}

fn push_word(out: &mut Vec<u8>, word: u32) {
    out.extend_from_slice(&word.to_be_bytes());
}

/// Appends `bytes` padded with zero to a word boundary (at least one word, as
/// a name takes) and returns the words written.
fn push_padded(out: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let words = bytes.len().div_ceil(4).max(1);
    out.extend_from_slice(bytes);
    out.resize(out.len() + words * 4 - bytes.len(), 0);
    words
}

/// Appends `bytes` padded to a word boundary, and no word at all when empty.
fn push_text(out: &mut Vec<u8>, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    push_padded(out, bytes)
}

fn byte(words: usize) -> u8 {
    u8::try_from(words).unwrap_or(u8::MAX)
}

/// Starts a command packet: the header word, with the payload length patched
/// in by [`finish`]. Returns the header's offset.
fn begin(out: &mut Vec<u8>, code: u8, data1: u8, data2: u8) -> usize {
    let at = out.len();
    out.extend_from_slice(&[code, 0, data1, data2]);
    at
}

fn finish(out: &mut [u8], at: usize) {
    let payload_words = (out.len() - at - 4) / 4;
    out[at + 1] = byte(payload_words);
}

/// The text of a field: bytes up to the first NUL.
fn trimmed(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

fn read_identity(name: &[u8], id: &[u8]) -> Result<EndpointIdentity, ParseError> {
    let (name, id) = (trimmed(name), trimmed(id));
    if name.is_empty() || id.is_empty() {
        return Err(ParseError::EmptyText);
    }
    let name = std::str::from_utf8(name).map_err(|_| ParseError::Utf8)?;
    let id = std::str::from_utf8(id).map_err(|_| ParseError::Utf8)?;
    Ok(EndpointIdentity {
        name: EndpointName::parse(name).map_err(ParseError::Name)?,
        instance_id: ProductInstanceId::parse(id).map_err(ParseError::InstanceId)?,
    })
}

fn read_text(field: &[u8]) -> Result<Text, ParseError> {
    let text = std::str::from_utf8(trimmed(field)).map_err(|_| ParseError::Utf8)?;
    Text::new(text).map_err(|_| ParseError::TextTooLong)
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

impl Command {
    /// The Command Code.
    #[must_use]
    pub const fn code(&self) -> u8 {
        match self {
            Self::Invitation(_) => code::INVITATION,
            Self::InvitationWithAuth(_) => code::INVITATION_AUTH,
            Self::InvitationWithUserAuth(_) => code::INVITATION_USER_AUTH,
            Self::Accepted(_) => code::ACCEPTED,
            Self::Pending(_) => code::PENDING,
            Self::AuthRequired(_) => code::AUTH_REQUIRED,
            Self::UserAuthRequired(_) => code::USER_AUTH_REQUIRED,
            Self::Ping(_) => code::PING,
            Self::PingReply(_) => code::PING_REPLY,
            Self::RetransmitRequest(_) => code::RETRANSMIT_REQUEST,
            Self::RetransmitError(_) => code::RETRANSMIT_ERROR,
            Self::SessionReset => code::SESSION_RESET,
            Self::SessionResetReply => code::SESSION_RESET_REPLY,
            Self::Nak(_) => code::NAK,
            Self::Bye(_) => code::BYE,
            Self::ByeReply => code::BYE_REPLY,
            Self::UmpData(_) => code::UMP_DATA,
        }
    }

    /// The header this command is written with.
    #[must_use]
    pub fn header(&self) -> Header {
        let mut bytes = Vec::new();
        self.write(&mut bytes);
        Header::from_word(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Appends the Command Packet to `out`: the header and the payload, with
    /// every reserved byte zero and every string padded with zero to a word.
    pub fn write(&self, out: &mut Vec<u8>) {
        let code = self.code();
        let at = match self {
            Self::Invitation(inv) => {
                let at = begin(out, code, 0, inv.methods.bits());
                let words = write_identity(out, &inv.identity);
                out[at + 2] = byte(words.0);
                at
            }
            Self::InvitationWithAuth(digest) => {
                let at = begin(out, code, 0, 0);
                out.extend_from_slice(&digest.get());
                at
            }
            Self::InvitationWithUserAuth(auth) => {
                let at = begin(out, code, 0, 0);
                out.extend_from_slice(&auth.digest.get());
                push_padded(out, auth.username.as_str().as_bytes());
                at
            }
            Self::Accepted(identity) | Self::Pending(identity) => {
                let at = begin(out, code, 0, 0);
                let words = write_identity(out, identity);
                out[at + 2] = byte(words.0);
                at
            }
            Self::AuthRequired(c) => {
                let at = begin(out, code, 0, auth_state_code(c.state));
                out.extend_from_slice(&c.nonce.get());
                let words = write_identity(out, &c.identity);
                out[at + 2] = byte(words.0);
                at
            }
            Self::UserAuthRequired(c) => {
                let at = begin(out, code, 0, user_auth_state_code(c.state));
                out.extend_from_slice(&c.nonce.get());
                let words = write_identity(out, &c.identity);
                out[at + 2] = byte(words.0);
                at
            }
            Self::Ping(id) | Self::PingReply(id) => {
                let at = begin(out, code, 0, 0);
                push_word(out, id.0);
                at
            }
            Self::RetransmitRequest(req) => {
                let at = begin(out, code, 0, 0);
                out[at + 2..at + 4].copy_from_slice(&req.first.get().to_be_bytes());
                push_word(out, u32::from(req.count) << 16);
                at
            }
            Self::RetransmitError(err) => {
                let at = begin(out, code, err.reason.code(), 0);
                push_word(out, u32::from(err.first.get()) << 16);
                at
            }
            Self::SessionReset | Self::SessionResetReply | Self::ByeReply => begin(out, code, 0, 0),
            Self::Nak(nak) => {
                let at = begin(out, code, nak.reason.code(), 0);
                push_word(out, nak.refused.word());
                push_text(out, nak.text.as_str().as_bytes());
                at
            }
            Self::Bye(bye) => {
                let at = begin(out, code, bye.reason.code(), 0);
                push_text(out, bye.text.as_str().as_bytes());
                at
            }
            Self::UmpData(data) => {
                let at = begin(out, code, 0, 0);
                out[at + 2..at + 4].copy_from_slice(&data.sequence.get().to_be_bytes());
                for word in &data.words {
                    push_word(out, *word);
                }
                at
            }
        };
        finish(out, at);
    }

    /// The command a header and its payload make. `payload` is exactly the
    /// header's payload length, in bytes.
    ///
    /// # Errors
    ///
    /// A [`ParseError`] naming what is wrong. Reserved bytes and bits are not
    /// read, and payload words past the defined fields are ignored (M2-124
    /// section 5.3).
    pub fn parse(header: Header, payload: &[u8]) -> Result<Self, ParseError> {
        let words = usize::from(header.payload_words());
        debug_assert_eq!(payload.len(), words * 4);
        let bad_length = || ParseError::Length {
            code: header.code(),
            words: header.payload_words(),
        };
        match header.code() {
            code::INVITATION => {
                let (name, id) =
                    split_identity(usize::from(header.data1()), payload).ok_or_else(bad_length)?;
                Ok(Self::Invitation(Invitation {
                    methods: AuthMethods::from_bits(header.data2()),
                    identity: read_identity(name, id)?,
                }))
            }
            code::INVITATION_AUTH => {
                let digest = payload.get(..32).ok_or_else(bad_length)?;
                Ok(Self::InvitationWithAuth(read_digest(digest)))
            }
            code::INVITATION_USER_AUTH => {
                let digest = payload.get(..32).ok_or_else(bad_length)?;
                let name =
                    std::str::from_utf8(trimmed(&payload[32..])).map_err(|_| ParseError::Utf8)?;
                let username = Username::new(name).map_err(|_| ParseError::EmptyText)?;
                Ok(Self::InvitationWithUserAuth(UserAuthentication {
                    digest: read_digest(digest),
                    username,
                }))
            }
            code::ACCEPTED | code::PENDING => {
                let (name, id) =
                    split_identity(usize::from(header.data1()), payload).ok_or_else(bad_length)?;
                let identity = read_identity(name, id)?;
                Ok(if header.code() == code::ACCEPTED {
                    Self::Accepted(identity)
                } else {
                    Self::Pending(identity)
                })
            }
            code::AUTH_REQUIRED | code::USER_AUTH_REQUIRED => {
                let (nonce, rest) = payload.split_at_checked(16).ok_or_else(bad_length)?;
                let (name, id) =
                    split_identity(usize::from(header.data1()), rest).ok_or_else(bad_length)?;
                let nonce = Nonce::new(nonce.try_into().map_err(|_| bad_length())?)
                    .ok_or(ParseError::Nonce)?;
                let identity = read_identity(name, id)?;
                if header.code() == code::AUTH_REQUIRED {
                    let state = match header.data2() {
                        0 => AuthState::First,
                        1 => AuthState::DigestWrong,
                        _ => return Err(ParseError::Reserved),
                    };
                    Ok(Self::AuthRequired(AuthChallenge {
                        state,
                        nonce,
                        identity,
                    }))
                } else {
                    let state = match header.data2() {
                        0 => UserAuthState::First,
                        1 => UserAuthState::DigestWrong,
                        2 => UserAuthState::UserNotFound,
                        _ => return Err(ParseError::Reserved),
                    };
                    Ok(Self::UserAuthRequired(UserAuthChallenge {
                        state,
                        nonce,
                        identity,
                    }))
                }
            }
            code::PING | code::PING_REPLY => {
                let id = PingId(u32::from_be_bytes(
                    payload
                        .get(..4)
                        .and_then(|b| b.try_into().ok())
                        .ok_or_else(bad_length)?,
                ));
                Ok(if header.code() == code::PING {
                    Self::Ping(id)
                } else {
                    Self::PingReply(id)
                })
            }
            code::RETRANSMIT_REQUEST => {
                let count = payload.get(..2).ok_or_else(bad_length)?;
                Ok(Self::RetransmitRequest(RetransmitRequest {
                    first: SequenceNumber::new(header.data()),
                    count: be16(count),
                }))
            }
            code::RETRANSMIT_ERROR => {
                let first = payload.get(..2).ok_or_else(bad_length)?;
                Ok(Self::RetransmitError(RetransmitRefusal {
                    reason: RetransmitErrorReason::from_code(header.data1()),
                    first: SequenceNumber::new(be16(first)),
                }))
            }
            code::SESSION_RESET => Ok(Self::SessionReset),
            code::SESSION_RESET_REPLY => Ok(Self::SessionResetReply),
            code::NAK => {
                let (refused, text) = payload.split_at_checked(4).ok_or_else(bad_length)?;
                Ok(Self::Nak(Nak {
                    reason: NakReason::from_code(header.data1()),
                    refused: Header::from_word(u32::from_be_bytes([
                        refused[0], refused[1], refused[2], refused[3],
                    ])),
                    text: read_text(text)?,
                }))
            }
            code::BYE => Ok(Self::Bye(Bye {
                reason: ByeReason::from_code(header.data1()),
                text: read_text(payload)?,
            })),
            code::BYE_REPLY => Ok(Self::ByeReply),
            code::UMP_DATA => {
                if words > MAX_UMP_DATA_WORDS {
                    return Err(bad_length());
                }
                let words = payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|w| u32::from_be_bytes(*w))
                    .collect();
                UmpData::from_words(SequenceNumber::new(header.data()), words)
                    .map(Self::UmpData)
                    .map_err(ParseError::UmpData)
            }
            code => Err(ParseError::UnknownCode(code)),
        }
    }
}

/// Writes the name, then the Product Instance Id, each padded; returns the
/// words of each.
fn write_identity(out: &mut Vec<u8>, identity: &EndpointIdentity) -> (usize, usize) {
    let name = push_padded(out, identity.name.as_bytes());
    let id = push_padded(out, identity.instance_id.as_str().as_bytes());
    (name, id)
}

/// The name and Product Instance Id fields of a payload: the name is
/// `name_words` words long, the Id the words after it. `None` when the
/// lengths are outside what M2-124 Tables 10 to 17 allow.
fn split_identity(name_words: usize, payload: &[u8]) -> Option<(&[u8], &[u8])> {
    let total = payload.len() / 4;
    let id_words = total.checked_sub(name_words)?;
    let in_range =
        (1..=NAME_WORDS_MAX).contains(&name_words) && (1..=ID_WORDS_MAX).contains(&id_words);
    in_range.then(|| payload.split_at(name_words * 4))
}

fn read_digest(bytes: &[u8]) -> Digest {
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes[..32]);
    Digest::new(digest)
}

const fn auth_state_code(state: AuthState) -> u8 {
    match state {
        AuthState::First => 0,
        AuthState::DigestWrong => 1,
    }
}

const fn user_auth_state_code(state: UserAuthState) -> u8 {
    match state {
        UserAuthState::First => 0,
        UserAuthState::DigestWrong => 1,
        UserAuthState::UserNotFound => 2,
    }
}
