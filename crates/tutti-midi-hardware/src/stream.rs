//! What a device's UMP Stream conversation settled: [`DeviceStream`], and the
//! [`Negotiation`] that runs it (M2-104 §7.1.6.1, Table 4 steps 2 to 4).
//!
//! A [`Connection`] is the pure state machine; this is the part that has a
//! wire. One runs for each input the session opens on a device that is a UMP
//! Endpoint and has an output to send on:
//!
//! 1. The first [`poll`](Negotiation::poll) starts it: the Endpoint Discovery
//!    goes out on the device's output.
//! 2. Each poll hands the connection the Stream messages the device sent since
//!    (the port's stream ring, [`HardwareMidiInputs::drain_stream`](crate::HardwareMidiInputs::drain_stream)) and the
//!    time; what it answers goes out, and a step with no answer is sent again
//!    after [`STEP_TIMEOUT`](tutti_midi_device::STEP_TIMEOUT) and given up after [`MAX_ATTEMPTS`](tutti_midi_device::MAX_ATTEMPTS).
//! 3. It ends as a [`DeviceStream`].
//!
//! MIDI-CI then runs beside it for the life of the input: see the `ci` module's
//! docs. It is stepped by the same
//! [`poll`](Negotiation::poll), and what it finds is returned with the stream.
//!
//! Time is an argument. Nothing here reads a clock, waits or blocks: a device
//! that never answers (M2-104 Appendix I: a device that does not know UMP
//! Stream messages, or an OS that never delivers them) costs three sends and
//! then is a [`Basis::Unanswered`] device, described by the OS.
//!
//! # Identity Request
//!
//! A MIDI 1.0 port with an output has no UMP Stream to ask what it is
//! (M2-104 Appendix I), and is `Described` from the connect all the same. What
//! it can say of itself is its Identity Reply (M1 p.40), so the first poll also
//! sends it an Identity Request to every device (`F0 7E 7F 06 01 F7`, one SysEx7
//! packet), through `tutti_midi_device::IdentityInquiry`: sent again once if it
//! is not answered in
//! [`INQUIRY_TIMEOUT`](tutti_midi_device::INQUIRY_TIMEOUT), then given up on, and
//! a device that never answers is the same `Described` device as before. The
//! reply is read from a copy of the port's SysEx7 packets that the input edge
//! keeps while the request is out
//! ([`HardwareMidiInputs::hear_sysex`](crate::HardwareMidiInputs::hear_sysex));
//! the packets still reach the graph. An answer is returned by
//! [`MidiSession::poll`](crate::MidiSession::poll) as a [`DeviceStream`]
//! of basis [`Basis::Inquired`], whose Endpoint carries the identity.
//!
//! # What is asked for
//!
//! The MIDI 2.0 Protocol where the device offers it, else MIDI 1.0
//! ([`Connection::with_wanted`] narrows the request to what the Endpoint
//! supports), with **JR Timestamps off both ways**. Input time comes from the
//! OS's arrival stamp (`InputProducerHandle::push`), which JR would only
//! repeat, and the input edge drops a JR Clock or JR Timestamp a device sends
//! anyway; JR on the output side is the session's `UmpOut` stamping, which
//! follows the configuration the negotiation settled (`rx_jr`), so a device
//! that is not asked is not sent it, and one that turns it on itself is.
//!
//! # After the settle
//!
//! A device that answered goes on being heard: a Stream Configuration
//! Notification moves the Protocol and JR in use, and (when the Endpoint did
//! not declare its Function Blocks static) a Function Block Info or Name
//! changes a block (`Connection::on_message`, `Action::Changed`). A change is
//! returned by [`MidiSession::poll`](crate::MidiSession::poll) as the settle
//! is, and what the output sends in follows it.

use std::time::Instant;

use tutti_graph::Offset;
use tutti_midi_device::{
    Action, Connection, Ending, Endpoint, IdentityInquiry, InquiryAction, Learned,
};
use tutti_midi_protocol::{Addressed, Read, Universal, read};
use tutti_ump::{
    DeviceIdentity, MidiGroup, Protocol, StreamConfig, SysexAssembler, SysexStep, Ump,
};

use crate::capability::UmpCapability;
use crate::ci::{
    Ci, CiChange, CiChanges, CiPeerInfo, Heard, groups_after_settle, groups_of_a_midi1_port,
};
use crate::devices::OpenOutput;
use crate::process::{self, ProcessChange, ReportAsk, ReportError};
use crate::profiles::{ProfileError, ProfileSwitch, ProfilesChange, Snapshot};
use tutti_midi_device::DeviceId;

/// The configuration the session asks every Endpoint for (see the module).
const WANTED: StreamConfig = StreamConfig {
    protocol: Protocol::Midi2,
    rx_jr: false,
    tx_jr: false,
};

/// How a device's [`DeviceStream`] came to be known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basis {
    /// The device answered: the Endpoint is what it said, and the
    /// configuration is the last Stream Configuration Notification it sent
    /// (§7.1.6.1 step 3: the peer may refuse any part of a request).
    Negotiated,
    /// The device was asked and did not answer in time. The Endpoint is the
    /// OS's description of it, which for a device an OS has already
    /// discovered (the ALSA kernel does, for USB) is the true one.
    Unanswered,
    /// It was not asked what it is: a device with no output has no way to ask,
    /// and a MIDI 1.0 port that has one has no UMP Stream to ask but is sent an
    /// Identity Request, which it may never answer. The Endpoint is the OS's
    /// description.
    Described,
    /// A MIDI 1.0 port that answered an Identity Request (M1 p.40): the
    /// Endpoint is the OS's description with the identity the device gave
    /// ([`Endpoint::identity`]).
    Inquired,
}

/// What is known of the UMP Stream state of one device: its Endpoint, the
/// Protocol and JR Timestamps in use, and how that came to be known.
///
/// The output side follows [`config`](Self::config): the device's output is
/// sent in the Protocol it names, with JR Timestamps when it receives them
/// (`OpenOutput`'s wire form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceStream {
    endpoint: Endpoint,
    basis: Basis,
}

impl DeviceStream {
    /// The Endpoint: what the device declared, or what the OS says of it.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How it came to be known.
    #[must_use]
    pub const fn basis(&self) -> Basis {
        self.basis
    }

    /// Returns what the device itself said of itself: the Device Identity and
    /// Product Instance Id of a device that [`Negotiated`](Basis::Negotiated),
    /// the identity of one that was [`Inquired`](Basis::Inquired). The OS's
    /// description of an [`Unanswered`](Basis::Unanswered) or
    /// [`Described`](Basis::Described) one carries neither (only a device's own
    /// message sets them), so it says nothing.
    #[must_use]
    pub fn said(&self) -> Learned {
        let learned = self
            .endpoint
            .identity()
            .map_or_else(Learned::new, |identity| {
                Learned::new().with_identity(*identity)
            });
        match self.endpoint.instance_id() {
            Some(id) => learned.with_instance_id(*id),
            None => learned,
        }
    }

    /// The Protocol and JR Timestamps in use.
    #[must_use]
    pub const fn config(&self) -> StreamConfig {
        self.endpoint.active()
    }

    /// The Endpoint as a [`UmpCapability`]: the protocol in use and the blocks
    /// it declares.
    #[must_use]
    pub fn capability(&self) -> UmpCapability {
        UmpCapability::of(&self.endpoint)
    }
}

/// One device's negotiation: its [`Connection`], while one runs, and the output
/// it sends on. Control side.
pub(crate) struct Negotiation {
    os: UmpCapability,
    out: Option<OpenOutput>,
    run: Run,
    inquire: Inquire,
    ci: Ci,
}

/// What one [`Negotiation::poll`] did.
pub(crate) struct Stepped {
    /// The stream, when this call settled it or changed it.
    pub(crate) stream: Option<DeviceStream>,
    /// The MIDI-CI peers that came, changed or went.
    pub(crate) ci: Vec<CiChange>,
    /// The Profiles of the peers that changed.
    pub(crate) profiles: Vec<ProfilesChange>,
    /// The Process Inquiry of the peers that changed.
    pub(crate) process: Vec<ProcessChange>,
}

/// The Identity Request of a MIDI 1.0 port that has an output (the module's
/// "Identity Request"). It runs beside a `Run::Done` that is `Described` from
/// the connect, so a MIDI 1.0 port is settled at once and the inquiry only
/// adds to it.
enum Inquire {
    /// Nothing to ask: a UMP Endpoint (it has a `Run`), a device with no
    /// output, or an inquiry that has ended.
    No,
    /// Decided to ask, not yet started: the first poll sends the request.
    Idle,
    Asking(Box<Asking>),
}

/// The request out, and what hears the reply.
struct Asking {
    inquiry: IdentityInquiry,
    assembler: SysexAssembler<Box<[u8]>>,
}

/// The Group of a MIDI 1.0 port's UMPs.
const LEGACY_GROUP: MidiGroup = MidiGroup::of(0);

/// The longest message the reply reader takes in, `F0` and `F7` included: an
/// Identity Reply is 17 bytes with a three-byte manufacturer id.
const REPLY_ROOM: usize = 32;

impl Asking {
    fn new(now: Instant) -> (Self, InquiryAction) {
        let (inquiry, first) = IdentityInquiry::new(LEGACY_GROUP, now);
        let room = SysexAssembler::room_for(REPLY_ROOM, 1, 1);
        let assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), REPLY_ROOM, 1);
        (Self { inquiry, assembler }, first)
    }

    /// Takes in one SysEx7 packet: the identity, if it completed an Identity
    /// Reply. The reply's device id is the responder's own and is not looked
    /// at: the request went to every device (`7F`) of one port.
    fn hear(&mut self, ump: Ump) -> Option<DeviceIdentity> {
        self.assembler.begin_block();
        let SysexStep::Complete(done) = self.assembler.push(ump) else {
            return None;
        };
        match read(self.assembler.message(done)) {
            Read::Universal(Addressed {
                message: Universal::IdentityReply(identity),
                ..
            }) => Some(identity),
            _ => None,
        }
    }
}

enum Run {
    /// Decided to ask, not yet started: the first poll starts it.
    Idle,
    Asking(Box<Connection>),
    /// Settled. `listening` is the connection of a device that answered, which
    /// still hears what it changes; a device described by the OS has none.
    Done {
        stream: Box<DeviceStream>,
        listening: Option<Box<Connection>>,
    },
}

impl Negotiation {
    /// The negotiation for a device the OS describes as `os`, on `out` when it
    /// has an output. It asks only a UMP Endpoint that has an output; any
    /// other is [`Basis::Described`] at once.
    pub(crate) fn new(device: DeviceId, os: UmpCapability, out: Option<OpenOutput>) -> Self {
        let asks = os.is_ump_endpoint() && out.is_some();
        let inquire = if !os.is_ump_endpoint() && out.is_some() {
            Inquire::Idle
        } else {
            Inquire::No
        };
        let run = if asks {
            Run::Idle
        } else {
            Run::Done {
                stream: Box::new(DeviceStream {
                    endpoint: os.endpoint(),
                    basis: Basis::Described,
                }),
                listening: None,
            }
        };
        Self {
            os,
            out,
            run,
            inquire,
            ci: Ci::new(device),
        }
    }

    /// What is known, once it is settled.
    pub(crate) fn settled(&self) -> Option<DeviceStream> {
        match &self.run {
            Run::Done { stream, .. } => Some(**stream),
            _ => None,
        }
    }

    /// Whether the port should copy the SysEx7 packets the device sends to the
    /// stream ring: an Identity Request is about to go out or is out, or
    /// MIDI-CI is enabled (`ci`) and running or about to.
    pub(crate) fn hears_sysex(&self, ci: bool) -> bool {
        matches!(self.inquire, Inquire::Idle | Inquire::Asking(_)) || self.ci.hears_sysex(ci)
    }

    /// The input is closing: MIDI-CI ends on the device's output (see
    /// [`Ci::close`]).
    pub(crate) fn close(&mut self) {
        if let Some(out) = &mut self.out {
            self.ci.close(out);
        }
    }

    /// The MIDI-CI peers found on this device.
    pub(crate) fn ci_peers(&self) -> std::sync::Arc<[CiPeerInfo]> {
        self.ci.peers()
    }

    /// The Profiles of the MIDI-CI peers found on this device.
    pub(crate) fn profiles(&self) -> Snapshot {
        self.ci.profiles()
    }

    /// The MIDI-CI peers of this device that can be asked for a MIDI Message
    /// Report.
    pub(crate) fn report_peers(&self) -> process::Snapshot {
        self.ci.report_peers()
    }

    /// Asks that a peer be sent a MIDI Message Report request at the next poll.
    pub(crate) fn report(&mut self, ask: ReportAsk) -> Result<(), ReportError> {
        self.ci.report(ask)
    }

    /// Asks that a peer's Profile be switched at the next poll.
    pub(crate) fn set_profile(&mut self, switch: ProfileSwitch) -> Result<(), ProfileError> {
        self.ci.set_profile(switch)
    }

    /// The Identity Request of a MIDI 1.0 port: starts it, or hands it the
    /// packets the port copied (`incoming`) and the time. Returns the stream
    /// when the device answered, with the identity it gave.
    fn poll_inquiry(&mut self, now: Instant, heard: &Heard<'_>) -> Option<DeviceStream> {
        let mut actions = Vec::new();
        match &mut self.inquire {
            Inquire::No => {}
            Inquire::Idle => {
                let (asking, first) = Asking::new(now);
                actions.push(first);
                self.inquire = Inquire::Asking(Box::new(asking));
            }
            Inquire::Asking(asking) => {
                for &ump in heard.sysex() {
                    if let Some(identity) = asking.hear(ump) {
                        actions.push(asking.inquiry.on_reply(identity));
                    }
                }
                if heard.gap() {
                    // A reply cut by a gap is not completed by what follows it.
                    asking.assembler.lost_tail();
                }
                if asking.inquiry.deadline().is_some_and(|at| now >= at) {
                    actions.push(asking.inquiry.on_timeout(now));
                }
            }
        }
        for action in actions {
            match action {
                InquiryAction::Wait => {}
                InquiryAction::Send(ump) => self.send(ump),
                InquiryAction::GaveUp => {
                    self.inquire = Inquire::No;
                    self.ci.decide(groups_of_a_midi1_port());
                }
                InquiryAction::Answered(identity) => {
                    self.inquire = Inquire::No;
                    self.ci.decide(groups_of_a_midi1_port());
                    let Run::Done { stream, .. } = &mut self.run else {
                        return None;
                    };
                    stream.endpoint = stream.endpoint.with_identity(identity);
                    stream.basis = Basis::Inquired;
                    return Some(**stream);
                }
            }
        }
        None
    }

    /// Takes what the port heard (`heard`: the device's Stream messages and,
    /// while MIDI-CI or the Identity Request listens, its SysEx7 packets) and
    /// the time `now`: starts the negotiation, or steps it, and steps MIDI-CI.
    /// Returns the stream when this call settled it, or, after that, changed
    /// it, and the peers' changes.
    ///
    /// `ci` is the identity MIDI-CI is enabled with, if it is.
    ///
    /// MIDI-CI steps first: what a settle or an inquiry's end makes it start is
    /// started by the next poll, after the port has been asked to copy SysEx.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        heard: &Heard<'_>,
        ci: Option<DeviceIdentity>,
    ) -> Stepped {
        let changes = match &mut self.out {
            Some(out) => self.ci.poll(now, heard, ci, out),
            None => CiChanges::default(),
        };
        let stream = self.poll_stream(now, heard);
        Stepped {
            stream,
            ci: changes.peers,
            profiles: changes.profiles,
            process: changes.process,
        }
    }

    fn poll_stream(&mut self, now: Instant, heard: &Heard<'_>) -> Option<DeviceStream> {
        let incoming = heard.packets;
        if !matches!(self.inquire, Inquire::No) {
            return self.poll_inquiry(now, heard);
        }
        let mut actions = Vec::new();
        match &mut self.run {
            Run::Idle => {
                let (connection, first) = Connection::with_wanted(WANTED, now);
                actions.push(first);
                self.run = Run::Asking(Box::new(connection));
            }
            Run::Asking(connection) => {
                for &ump in incoming {
                    if let Ok(message) = ump.decode() {
                        actions.push(connection.on_message(message, now));
                    }
                }
                if connection.deadline().is_some_and(|at| now >= at) {
                    actions.push(connection.on_timeout(now));
                }
            }
            Run::Done { stream, listening } => {
                let connection = listening.as_mut()?;
                let mut changed = false;
                for &ump in incoming {
                    if let Ok(message) = ump.decode() {
                        changed |= connection.on_message(message, now) == Action::Changed;
                    }
                }
                if !changed {
                    return None;
                }
                stream.endpoint = *connection.endpoint();
                // MIDI-CI follows: its Groups are the blocks' now, taken up
                // by the next poll (`Ci::poll`).
                self.ci.decide(groups_after_settle(stream));
                return Some(**stream);
            }
        }
        for action in actions {
            match action {
                Action::Wait | Action::Changed => {}
                Action::Send(ump) => self.send(ump),
                Action::Settled(ending) => return self.settle(ending),
            }
        }
        None
    }

    fn send(&mut self, ump: Ump) {
        if let Some(out) = &mut self.out
            && out.queue(&[(Offset::ZERO, ump)]) == 0
        {
            // The step is sent again when it times out.
            tracing::debug!("a UMP Stream message was refused by {}", out.name());
        }
    }

    fn settle(&mut self, ending: Ending) -> Option<DeviceStream> {
        let Run::Asking(connection) = &self.run else {
            return None;
        };
        let (stream, listening) = match ending {
            Ending::Negotiated => (
                DeviceStream {
                    endpoint: *connection.endpoint(),
                    basis: Basis::Negotiated,
                },
                Some(connection.clone()),
            ),
            Ending::Midi1Fallback => (
                DeviceStream {
                    endpoint: self.os.endpoint(),
                    basis: Basis::Unanswered,
                },
                None,
            ),
        };
        self.ci.decide(groups_after_settle(&stream));
        self.run = Run::Done {
            stream: Box::new(stream),
            listening,
        };
        Some(stream)
    }
}

#[cfg(test)]
mod tests;
