//! [`ProcessInitiator`]: the Initiator's side of Process Inquiry with one
//! discovered peer (M2-101 §9).
//!
//! A pure state machine in the shape of [`ProfileInitiator`](super::ProfileInitiator):
//! every step returns the [`ProcessAction`]s to carry out, time is an [`Instant`]
//! argument, and nothing here reads a clock or sends a byte. The caller makes one
//! per peer that declared Process Inquiry in Discovery (Table 7, bit D4), routes
//! to it every decoded message of that Group, sends each [`ProcessAction::Send`]
//! on that Group, and drops it when the peer is lost.
//!
//! # The rules
//!
//! - Capabilities (§9.2, §9.3): [`new`](ProcessInitiator::new) sends an Inquiry:
//!   Process Inquiry Capabilities, to the Function Block, the only address it may
//!   use. The reply's features ([`ProcessAction::Capabilities`]) say whether the
//!   peer supports MIDI Message Report; [`report`](ProcessInitiator::report)
//!   refuses a request once the peer has said it does not.
//! - A MIDI Message Report (§9.4 to §9.8): [`report`](ProcessInitiator::report)
//!   sends the Inquiry; the peer answers with a Reply (the Begin), the MIDI
//!   messages themselves, and an End. The Reply must be on the Device ID the
//!   Inquiry used (§9.6) and is handed out as [`ProcessAction::ReportBegan`],
//!   holding the kinds the peer will send cut to the kinds asked, since it may
//!   not set another bit. The End, on that Device ID too, ends the transaction
//!   ([`ProcessAction::ReportEnded`]). The messages between them are
//!   not CI and do not pass through here: they arrive on the connection as MIDI,
//!   and are the report. One report is in flight at a time, since the MIDI
//!   messages carry nothing to tell two apart.
//! - A NAK of either inquiry, from the peer to this device, ends it, even with no
//!   End received (§9.8.1: a Responder that aborts or errs sends a NAK with
//!   Status `0x20` or `0x40`, and the Initiator assumes the transaction ended).
//!   [`ProcessAction::ReportRefused`] and [`ProcessAction::CapabilitiesRefused`]
//!   hand it out.
//! - Timeout Wait (§5.10, Table 14): an ACK with Status Code `0x10` from the
//!   peer, to this device, names the transaction by its Original Transaction
//!   Sub-ID#2 and the Device ID it is on, and moves the deadline to
//!   `now + wait` if that is later (a wait asks for more time, never less).
//!
//! A message from another source, or one not addressed to this device, is not
//! this peer's and changes nothing.
//!
//! # The timeout
//!
//! [`PROCESS_TIMEOUT`] is tutti's choice, 3 seconds: M2-101 §9 gives no limit for a
//! Process Inquiry transaction, and the only wait the document states is
//! Discovery's 3 seconds at least (§5.5.5). The capabilities inquiry waits that
//! long for its reply. A report waits that long for the Reply, and the Reply
//! starts the wait again, for the End; so the time a report takes is bounded by
//! the peer's silence, not by how much it has to say.

use std::time::{Duration, Instant};
use std::vec::Vec;

use super::TimeoutWait;
use super::link::Link;

use super::{
    CiDeviceId, CiHeader, CiMessage, DataControl, Decoded, Muid, Nak, ProcessFeatures, ReportScope,
    SUB_ID2_MESSAGE_REPORT_INQUIRY, SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
};

/// How long an Inquiry: Process Inquiry Capabilities waits for its reply, and a
/// MIDI Message Report for its Reply and then for its End: 3 seconds. See the
/// module docs: tutti's figure, from §5.5.5.
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(3);

/// What a MIDI Message Report asks (§9.5): how much, and of what.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReportRequest {
    /// How much the peer reports.
    pub control: DataControl,
    /// Where, and which kinds of message.
    pub scope: ReportScope,
}

impl ReportRequest {
    /// A request of `scope` at `control`.
    #[must_use]
    pub const fn new(control: DataControl, scope: ReportScope) -> Self {
        Self { control, scope }
    }
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessAction {
    /// Send this message to the peer, on the Group of the connection.
    Send(CiMessage),
    /// The peer's Reply to Process Inquiry Capabilities arrived.
    Capabilities(ProcessFeatures),
    /// The peer NAKed the capabilities inquiry: it has no Process Inquiry to
    /// ask about.
    CapabilitiesRefused(Nak),
    /// [`PROCESS_TIMEOUT`] passed with no reply to the capabilities inquiry.
    CapabilitiesTimedOut,
    /// The peer's Reply to MIDI Message Report arrived: it is about to send the
    /// kinds of message in `declared`, which are the kinds it was asked for that
    /// it supports.
    ReportBegan {
        /// What the peer will report.
        declared: ReportScope,
    },
    /// The End of MIDI Message Report arrived: the report is complete.
    ReportEnded,
    /// The peer NAKed the report, which ends it with whatever it had sent.
    ReportRefused(Nak),
    /// [`PROCESS_TIMEOUT`] passed with no Reply, or after the Reply with no End:
    /// the report is over, complete or not.
    ReportTimedOut,
}

/// A report that cannot be asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessError {
    /// A report is still waiting for its Reply or its End.
    #[error("a MIDI Message Report is already in flight")]
    InFlight,
    /// The peer's capabilities say it does not support MIDI Message Report.
    #[error("the peer does not support MIDI Message Report")]
    NotSupported,
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    /// The Inquiry is out; the Reply is not in.
    AwaitingReply,
    /// The Reply came; the End is not in.
    Reporting,
}

#[derive(Clone, Copy, Debug)]
struct Report {
    request: ReportRequest,
    stage: Stage,
    deadline: Instant,
}

/// The Initiator of Process Inquiry with one peer.
#[derive(Clone, Debug)]
pub struct ProcessInitiator {
    link: Link,
    features: Option<ProcessFeatures>,
    capabilities_deadline: Option<Instant>,
    report: Option<Report>,
}

impl ProcessInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: an Inquiry: Process Inquiry Capabilities.
    #[must_use]
    pub fn new(own: Muid, peer: Muid, now: Instant) -> (Self, Vec<ProcessAction>) {
        let mut initiator = Self {
            link: Link { own, peer },
            features: None,
            capabilities_deadline: None,
            report: None,
        };
        let actions = initiator.inquire(now);
        (initiator, actions)
    }

    /// The peer's MUID.
    #[must_use]
    pub fn peer(&self) -> Muid {
        self.link.peer
    }

    /// The features the peer's last reply declared, `None` before one arrived.
    #[must_use]
    pub fn features(&self) -> Option<ProcessFeatures> {
        self.features
    }

    /// Asks for the capabilities again: a wait already running starts over. The
    /// features are kept until the reply replaces them.
    pub fn inquire(&mut self, now: Instant) -> Vec<ProcessAction> {
        self.capabilities_deadline = Some(now + PROCESS_TIMEOUT);
        vec![self.send(CiDeviceId::FunctionBlock, |header, to| {
            CiMessage::ProcessCapabilitiesInquiry { header, to }
        })]
    }

    /// Asks the peer for a MIDI Message Report; its Reply, End or refusal is a
    /// [`ProcessAction`] of a later step.
    ///
    /// # Errors
    ///
    /// [`ProcessError::InFlight`] while a report is waiting, and
    /// [`ProcessError::NotSupported`] once the peer's capabilities lack MIDI
    /// Message Report. A request made before the capabilities are known is sent:
    /// the spec does not require the inquiry first.
    pub fn report(
        &mut self,
        request: ReportRequest,
        now: Instant,
    ) -> Result<Vec<ProcessAction>, ProcessError> {
        if self.report.is_some() {
            return Err(ProcessError::InFlight);
        }
        if self
            .features
            .is_some_and(|features| !features.contains(ProcessFeatures::MIDI_MESSAGE_REPORT))
        {
            return Err(ProcessError::NotSupported);
        }
        self.report = Some(Report {
            request,
            stage: Stage::AwaitingReply,
            deadline: now + PROCESS_TIMEOUT,
        });
        let ReportRequest { control, scope } = request;
        Ok(vec![self.send(scope.address(), |header, to| {
            CiMessage::MessageReportInquiry {
                header,
                to,
                control,
                scope,
            }
        })])
    }

    /// Whether a report is waiting for its Reply or its End.
    #[must_use]
    pub fn is_reporting(&self) -> bool {
        self.report.is_some()
    }

    /// When the Initiator next needs [`on_timeout`](Self::on_timeout), or `None`
    /// with nothing waiting.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.capabilities_deadline
            .into_iter()
            .chain(self.report.map(|report| report.deadline))
            .min()
    }

    /// Time has passed: everything whose wait is over ends. Calling it early is
    /// harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        if self.capabilities_deadline.is_some_and(|at| now >= at) {
            self.capabilities_deadline = None;
            actions.push(ProcessAction::CapabilitiesTimedOut);
        }
        if self.report.is_some_and(|report| now >= report.deadline) {
            self.report = None;
            actions.push(ProcessAction::ReportTimedOut);
        }
        actions
    }

    /// A CI message of the connection's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        let Decoded::Message(message) = decoded else {
            return actions;
        };
        if !self.link.is_from_peer(message) || !self.link.is_to_us(message) {
            return actions;
        }
        let address = message.header().device_id;
        match message {
            CiMessage::ProcessCapabilitiesReply { features, .. }
                if self.capabilities_deadline.is_some() =>
            {
                self.capabilities_deadline = None;
                self.features = Some(*features);
                actions.push(ProcessAction::Capabilities(*features));
            }
            CiMessage::MessageReportReply { scope, .. } => {
                if let Some(report) = self.report.as_mut()
                    && matches!(report.stage, Stage::AwaitingReply)
                    && scope.address() == report.request.scope.address()
                {
                    report.stage = Stage::Reporting;
                    report.deadline = now + PROCESS_TIMEOUT;
                    actions.push(ProcessAction::ReportBegan {
                        declared: scope.within(&report.request.scope),
                    });
                }
            }
            CiMessage::MessageReportEnd { .. } => {
                if self.report.is_some_and(|report| {
                    matches!(report.stage, Stage::Reporting)
                        && report.request.scope.address() == address
                }) {
                    self.report = None;
                    actions.push(ProcessAction::ReportEnded);
                }
            }
            CiMessage::Ack { ack, .. } => {
                if let Some(wait) = TimeoutWait::from_ack(ack) {
                    self.on_wait(address, ack.ack_sub_id2.get(), wait);
                }
            }
            CiMessage::Nak { nak, .. } => self.on_nak(address, nak, &mut actions),
            _ => {}
        }
        actions
    }

    /// A Timeout Wait: the transaction it names waits until `now` plus the time
    /// asked, when that is later than it already would.
    fn on_wait(&mut self, address: CiDeviceId, original: u8, wait: TimeoutWait) {
        match original {
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY if address == CiDeviceId::FunctionBlock => {
                if let Some(deadline) = self.capabilities_deadline.as_mut() {
                    wait.extend(deadline);
                }
            }
            SUB_ID2_MESSAGE_REPORT_INQUIRY => {
                if let Some(report) = self
                    .report
                    .as_mut()
                    .filter(|report| report.request.scope.address() == address)
                {
                    wait.extend(&mut report.deadline);
                }
            }
            _ => {}
        }
    }

    fn on_nak(&mut self, address: CiDeviceId, nak: &Nak, actions: &mut Vec<ProcessAction>) {
        match nak.nak_sub_id2.get() {
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY
                if address == CiDeviceId::FunctionBlock
                    && self.capabilities_deadline.take().is_some() =>
            {
                actions.push(ProcessAction::CapabilitiesRefused(nak.clone()));
            }
            SUB_ID2_MESSAGE_REPORT_INQUIRY
                if self
                    .report
                    .is_some_and(|report| report.request.scope.address() == address) =>
            {
                self.report = None;
                actions.push(ProcessAction::ReportRefused(nak.clone()));
            }
            _ => {}
        }
    }

    fn send(
        &self,
        address: CiDeviceId,
        message: impl FnOnce(CiHeader, Muid) -> CiMessage,
    ) -> ProcessAction {
        ProcessAction::Send(self.link.message(address, message))
    }
}

#[cfg(test)]
mod tests;
