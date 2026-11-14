//! One endpoint's relationship with one peer: the session state machine of
//! M2-124 section 6, for either role, over no socket and no clock of its own.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tutti_ump::Ump;

use crate::link::{Link, Loss};
use crate::{
    Admission, AuthChallenge, AuthFailure, AuthMethods, AuthState, Bye, ByeReason, Command,
    Credentials, Digest, EndpointIdentity, Event, Header, Initiator, Invitation, Nak, NakReason,
    Nonce, PingId, SendError, SessionConfig, SessionState, UserAuthChallenge, UserAuthState,
    UserAuthentication,
};

/// Failed authentications a Host tolerates before it ends the invitation.
const MAX_AUTH_FAILURES: u8 = 5;

/// The delay before a Host answers the first failed authentication; each
/// further failure doubles it (M2-124 section 6.7).
const AUTH_FAILURE_DELAY: Duration = Duration::from_millis(100);

/// A command sent again until it is answered or `command_timeout` passes.
#[derive(Clone, Debug)]
struct Repeat {
    command: Command,
    started: Instant,
    next: Instant,
}

impl Repeat {
    fn deadline(&self, cfg: &SessionConfig) -> Instant {
        self.next.min(self.started + cfg.command_timeout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Challenge {
    SharedSecret,
    User,
}

#[derive(Clone, Debug)]
struct Challenged {
    nonce: Nonce,
    kind: Challenge,
    since: Instant,
    failures: u8,
    /// A reply held back so a guessing peer is slowed.
    delayed: Option<(Instant, Command)>,
}

#[derive(Clone, Debug)]
enum Phase {
    Idle,
    /// A Client's Invitation, or its answer to a challenge, is repeated.
    Inviting(Repeat),
    /// A Client waits for a Host that said Pending, repeating nothing.
    AwaitingAcceptance {
        until: Instant,
    },
    /// A Host waits for its application to accept or decline.
    Confirming {
        since: Instant,
    },
    /// A Host waits for the answer to its challenge.
    Challenged(Challenged),
    Established,
    Resetting(Repeat),
    Closing {
        repeat: Repeat,
        reason: ByeReason,
    },
}

/// How a Host admits, and the one thing a Host needs that is not data.
#[derive(Clone, Debug)]
pub(crate) enum Role {
    Client(Credentials),
    Host {
        admission: Admission,
        nonce_source: fn() -> Nonce,
    },
}

/// Whether a Host has room for another Client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capacity {
    Available,
    Full,
}

#[derive(Debug)]
pub(crate) struct Session {
    role: Role,
    capacity: Capacity,
    cfg: SessionConfig,
    local: EndpointIdentity,
    peer: SocketAddr,
    phase: Phase,
    remote: Option<EndpointIdentity>,
    link: Link,
    last_heard: Instant,
    last_ping: Option<Instant>,
    ping_counter: u32,
    outstanding_pings: VecDeque<PingId>,
    out: Vec<Command>,
    events: Vec<Event>,
}

/// The most Pings that may be unanswered at once before the oldest is dropped.
const OUTSTANDING_PINGS: usize = 8;

impl Session {
    pub(crate) fn new(
        role: Role,
        cfg: SessionConfig,
        local: EndpointIdentity,
        peer: SocketAddr,
        now: Instant,
    ) -> Self {
        Self {
            role,
            capacity: Capacity::Available,
            cfg,
            local,
            peer,
            phase: Phase::Idle,
            remote: None,
            link: Link::new(),
            last_heard: now,
            last_ping: None,
            ping_counter: 0,
            outstanding_pings: VecDeque::new(),
            out: Vec::new(),
            events: Vec::new(),
        }
    }

    pub(crate) fn set_capacity(&mut self, capacity: Capacity) {
        self.capacity = capacity;
    }

    /// Whether the peer is in a Session, or an Invitation or challenge is
    /// open with it: what a Host keeps an entry for.
    pub(crate) fn holds_session(&self) -> bool {
        matches!(
            self.state(),
            SessionState::PendingInvitation
                | SessionState::AuthenticationRequired
                | SessionState::Established
        )
    }

    pub(crate) fn state(&self) -> SessionState {
        match self.phase {
            Phase::Idle => SessionState::Idle,
            Phase::Inviting(_) | Phase::AwaitingAcceptance { .. } | Phase::Confirming { .. } => {
                SessionState::PendingInvitation
            }
            Phase::Challenged(_) => SessionState::AuthenticationRequired,
            Phase::Established => SessionState::Established,
            Phase::Resetting(_) => SessionState::PendingSessionReset,
            Phase::Closing { .. } => SessionState::PendingBye,
        }
    }

    /// The commands and events the last calls produced.
    pub(crate) fn take_output(&mut self) -> (Vec<Command>, Vec<Event>) {
        (
            std::mem::take(&mut self.out),
            std::mem::take(&mut self.events),
        )
    }

    fn start_repeat(&mut self, now: Instant, command: Command) -> Repeat {
        self.out.push(command.clone());
        Repeat {
            command,
            started: now,
            next: now + self.cfg.repeat_interval,
        }
    }

    fn nak(&mut self, reason: NakReason, refused: Header) {
        self.out.push(Command::Nak(Nak {
            reason,
            refused,
            text: crate::Text::default(),
        }));
    }

    /// Ends the Session or the invitation with a Bye that is repeated until
    /// answered.
    pub(crate) fn close(&mut self, now: Instant, reason: ByeReason) {
        if matches!(self.phase, Phase::Closing { .. } | Phase::Idle) {
            return;
        }
        let repeat = self.start_repeat(now, Command::Bye(Bye::new(reason)));
        self.phase = Phase::Closing { repeat, reason };
    }

    /// Ends the exchange with a peer that was never in a Session: a Bye
    /// repeated until answered, with no state before it.
    fn refuse(&mut self, now: Instant, reason: ByeReason) {
        let repeat = self.start_repeat(now, Command::Bye(Bye::new(reason)));
        self.phase = Phase::Closing { repeat, reason };
    }

    fn finish(&mut self, initiator: Initiator, reason: ByeReason) {
        self.phase = Phase::Idle;
        self.link = Link::new();
        self.remote = None;
        self.outstanding_pings.clear();
        self.last_ping = None;
        self.events.push(Event::Closed {
            peer: self.peer,
            initiator,
            reason,
        });
    }

    /// A Client's first move: the Invitation.
    pub(crate) fn invite(&mut self, now: Instant) {
        let Role::Client(credentials) = &self.role else {
            return;
        };
        if !matches!(self.phase, Phase::Idle) {
            return;
        }
        let invitation = Command::Invitation(Invitation {
            methods: credentials.methods(),
            identity: self.local,
        });
        self.last_heard = now;
        let repeat = self.start_repeat(now, invitation);
        self.phase = Phase::Inviting(repeat);
    }

    /// Begins the Host's side of a Session after its application accepted
    /// the Invitation it was asked about.
    pub(crate) fn accept(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Confirming { .. }) {
            self.establish(now);
        }
    }

    pub(crate) fn decline(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Confirming { .. }) {
            self.refuse(now, ByeReason::UserDeclined);
        }
    }

    pub(crate) fn remote(&self) -> Option<EndpointIdentity> {
        self.remote
    }

    /// Starts a Session Reset (M2-124 section 6.11).
    pub(crate) fn reset(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Established) {
            let repeat = self.start_repeat(now, Command::SessionReset);
            self.phase = Phase::Resetting(repeat);
        }
    }

    /// Queues UMP for the peer.
    pub(crate) fn send_ump(&mut self, now: Instant, packets: &[Ump]) -> Result<(), SendError> {
        if !matches!(self.phase, Phase::Established) {
            return Err(SendError::NotEstablished {
                state: self.state(),
            });
        }
        self.link.send(now, &self.cfg, packets, &mut self.out);
        Ok(())
    }

    fn establish(&mut self, now: Instant) {
        self.link.reset();
        self.last_heard = now;
        self.last_ping = None;
        self.phase = Phase::Established;
        // A Host tells its Client; a Client is told.
        if matches!(self.role, Role::Host { .. }) {
            self.out.push(Command::Accepted(self.local));
        }
        self.report_established();
    }

    fn report_established(&mut self) {
        if let Some(remote) = self.remote {
            self.events.push(Event::Established {
                peer: self.peer,
                remote,
            });
        }
    }

    /// Takes one command of a received datagram.
    pub(crate) fn receive(&mut self, now: Instant, command: Command) {
        self.last_heard = now;
        match command {
            Command::Ping(id) => self.out.push(Command::PingReply(id)),
            Command::PingReply(id) => {
                if let Some(at) = self.outstanding_pings.iter().position(|&p| p == id) {
                    self.outstanding_pings.remove(at);
                } else {
                    self.nak(NakReason::BadPingReply, Command::PingReply(id).header());
                }
            }
            Command::Nak(nak) => self.on_nak(now, &nak),
            Command::Bye(bye) => self.on_bye(&bye),
            Command::ByeReply => {
                if let Phase::Closing { reason, .. } = self.phase {
                    self.finish(Initiator::Local, reason);
                }
            }
            Command::Invitation(inv) => self.on_invitation(now, inv),
            Command::InvitationWithAuth(digest) => self.on_auth(now, &digest),
            Command::InvitationWithUserAuth(auth) => self.on_user_auth(now, &auth),
            Command::Accepted(identity) => self.on_accepted(now, identity),
            Command::Pending(identity) => self.on_pending(now, identity),
            Command::AuthRequired(challenge) => self.on_challenge(now, &challenge),
            Command::UserAuthRequired(challenge) => self.on_user_challenge(now, &challenge),
            Command::UmpData(data) => match self.phase {
                Phase::Established => {
                    let mut delivered = Vec::new();
                    let mut lost = Vec::new();
                    self.link
                        .receive(now, &self.cfg, data, &mut delivered, &mut lost);
                    self.report(delivered, lost);
                }
                Phase::Resetting(_) => {}
                _ => self.not_established(now),
            },
            Command::RetransmitRequest(request) => match self.phase {
                Phase::Established | Phase::Resetting(_) if self.cfg.retransmit_buffer == 0 => {
                    self.nak(
                        NakReason::NotSupported,
                        Command::RetransmitRequest(request).header(),
                    );
                }
                Phase::Established => self.link.retransmit(request, &mut self.out),
                Phase::Resetting(_) => {}
                _ => self.not_established(now),
            },
            Command::RetransmitError(error) => match self.phase {
                Phase::Established => {
                    let mut delivered = Vec::new();
                    let mut lost = Vec::new();
                    self.link
                        .on_retransmit_error(error, &mut delivered, &mut lost);
                    self.report(delivered, lost);
                }
                Phase::Resetting(_) => {}
                _ => self.not_established(now),
            },
            Command::SessionReset => match self.phase {
                Phase::Established | Phase::Resetting(_) => {
                    self.out.push(Command::SessionResetReply);
                    self.link.reset();
                    self.events.push(Event::Reset { peer: self.peer });
                }
                _ => self.not_established(now),
            },
            Command::SessionResetReply => match self.phase {
                Phase::Resetting(_) => {
                    self.link.reset();
                    self.phase = Phase::Established;
                    self.events.push(Event::Reset { peer: self.peer });
                }
                Phase::Established => self.reset(now),
                _ => self.not_established(now),
            },
        }
    }

    /// Takes a command that could not be read: unknown codes are NAKed as
    /// unsupported, anything else as malformed. A malformed Invitation is a
    /// protocol error that ends the exchange.
    pub(crate) fn refuse_command(
        &mut self,
        now: Instant,
        header: Header,
        error: crate::ParseError,
    ) {
        self.last_heard = now;
        match error {
            crate::ParseError::UnknownCode(_) => self.nak(NakReason::NotSupported, header),
            crate::ParseError::Truncated => {}
            _ if header.code() == 0x01 => {
                if matches!(self.phase, Phase::Idle) {
                    self.refuse(now, ByeReason::ProtocolError);
                }
            }
            _ => self.nak(NakReason::Malformed, header),
        }
    }

    fn not_established(&mut self, now: Instant) {
        if !matches!(self.phase, Phase::Closing { .. }) {
            self.refuse(now, ByeReason::SessionNotEstablished);
        }
    }

    fn report(&mut self, delivered: Vec<Ump>, lost: Vec<Loss>) {
        let peer = self.peer;
        self.events
            .extend(lost.into_iter().map(|l| Event::DataLost {
                peer,
                first: l.first,
                count: l.count,
            }));
        self.events.extend(
            delivered
                .into_iter()
                .map(|packet| Event::Ump { peer, packet }),
        );
    }

    fn on_nak(&mut self, now: Instant, nak: &Nak) {
        if nak.reason == NakReason::NotSupported
            && nak.refused.code() == 0x80
            && matches!(self.phase, Phase::Established)
        {
            self.link.on_retransmit_refused(now);
        }
        self.events.push(Event::Nak {
            peer: self.peer,
            reason: nak.reason,
            refused: nak.refused,
        });
    }

    fn on_bye(&mut self, bye: &Bye) {
        // Every Bye is answered, repeated ones and ones from a peer that is
        // in no Session with this endpoint included.
        self.out.push(Command::ByeReply);
        match self.phase {
            Phase::Idle => {}
            Phase::Closing { reason, .. } => self.finish(Initiator::Local, reason),
            _ => self.finish(Initiator::Remote, bye.reason),
        }
    }

    // ---- Client replies ----

    fn on_accepted(&mut self, now: Instant, identity: EndpointIdentity) {
        match (&self.role, &self.phase) {
            (Role::Host { .. }, _) => {
                self.nak(NakReason::NotExpected, Command::Accepted(identity).header())
            }
            (Role::Client(_), Phase::Inviting(_) | Phase::AwaitingAcceptance { .. }) => {
                self.remote = Some(identity);
                self.establish(now);
            }
            (Role::Client(_), Phase::Established | Phase::Resetting(_)) => {}
            (Role::Client(_), _) => self.refuse(now, ByeReason::NoPendingInvitation),
        }
    }

    fn on_pending(&mut self, now: Instant, identity: EndpointIdentity) {
        match (&self.role, &self.phase) {
            (Role::Host { .. }, _) => {
                self.nak(NakReason::NotExpected, Command::Pending(identity).header())
            }
            (Role::Client(_), Phase::Inviting(_) | Phase::AwaitingAcceptance { .. }) => {
                self.remote = Some(identity);
                self.phase = Phase::AwaitingAcceptance {
                    until: now + self.cfg.pending_timeout,
                };
            }
            (Role::Client(_), Phase::Established | Phase::Resetting(_)) => {}
            (Role::Client(_), _) => self.refuse(now, ByeReason::NoPendingInvitation),
        }
    }

    fn client_is_inviting(&self) -> bool {
        matches!(self.role, Role::Client(_))
            && matches!(
                self.phase,
                Phase::Inviting(_) | Phase::AwaitingAcceptance { .. }
            )
    }

    fn authentication_failed(&mut self, now: Instant, cause: AuthFailure) {
        self.events.push(Event::AuthenticationFailed {
            peer: self.peer,
            cause,
        });
        self.close(now, ByeReason::InvitationCanceled);
    }

    fn on_challenge(&mut self, now: Instant, challenge: &AuthChallenge) {
        if !self.client_is_inviting() {
            self.nak(
                NakReason::NotExpected,
                Command::AuthRequired(*challenge).header(),
            );
            return;
        }
        let Role::Client(credentials) = &self.role else {
            return;
        };
        if challenge.state == AuthState::DigestWrong {
            return self.authentication_failed(now, AuthFailure::DigestWrong);
        }
        let Some(secret) = &credentials.secret else {
            return self.authentication_failed(now, AuthFailure::NoCredentials);
        };
        let digest = Digest::of_secret(&challenge.nonce, secret);
        self.remote = Some(challenge.identity);
        let repeat = self.start_repeat(now, Command::InvitationWithAuth(digest));
        self.phase = Phase::Inviting(repeat);
    }

    fn on_user_challenge(&mut self, now: Instant, challenge: &UserAuthChallenge) {
        if !self.client_is_inviting() {
            self.nak(
                NakReason::NotExpected,
                Command::UserAuthRequired(*challenge).header(),
            );
            return;
        }
        let Role::Client(credentials) = &self.role else {
            return;
        };
        match challenge.state {
            UserAuthState::DigestWrong => {
                return self.authentication_failed(now, AuthFailure::DigestWrong);
            }
            UserAuthState::UserNotFound => {
                return self.authentication_failed(now, AuthFailure::UserNotFound);
            }
            UserAuthState::First => {}
        }
        let Some((username, password)) = &credentials.user else {
            return self.authentication_failed(now, AuthFailure::NoCredentials);
        };
        let command = Command::InvitationWithUserAuth(UserAuthentication {
            digest: Digest::of_user(&challenge.nonce, username, password),
            username: username.clone(),
        });
        self.remote = Some(challenge.identity);
        let repeat = self.start_repeat(now, command);
        self.phase = Phase::Inviting(repeat);
    }

    // ---- Host side of the Invitation ----

    fn on_invitation(&mut self, now: Instant, invitation: Invitation) {
        let Role::Host {
            admission,
            nonce_source,
        } = &self.role
        else {
            // A Client with no Host behind this port.
            return self.nak(
                NakReason::NotExpected,
                Command::Invitation(invitation).header(),
            );
        };
        let (admission, nonce_source) = (admission.clone(), *nonce_source);
        match &self.phase {
            // A peer the Host already admitted asks again: it missed the reply.
            Phase::Established | Phase::Resetting(_) => {
                return self.out.push(Command::Accepted(self.local));
            }
            Phase::Confirming { .. } => {
                return self.out.push(Command::Pending(self.local));
            }
            Phase::Challenged(c) => {
                let (nonce, kind) = (c.nonce, c.kind);
                return self
                    .out
                    .push(challenge_command(kind, nonce, Because::Invited, self.local));
            }
            Phase::Closing { .. } => return,
            Phase::Idle | Phase::Inviting(_) | Phase::AwaitingAcceptance { .. } => {}
        }
        if self.capacity == Capacity::Full {
            return self.refuse(now, ByeReason::TooManySessions);
        }
        self.remote = Some(invitation.identity);
        let needed = admission.required();
        match admission {
            Admission::Open => self.establish(now),
            Admission::Confirm => {
                self.out.push(Command::Pending(self.local));
                self.phase = Phase::Confirming { since: now };
                self.events.push(Event::Confirm {
                    peer: self.peer,
                    remote: invitation.identity,
                });
            }
            Admission::SharedSecret(_) | Admission::Users(_) => {
                if !invitation.methods.contains(needed) {
                    return self.refuse(now, ByeReason::NoMatchingAuthentication);
                }
                let kind = if needed == AuthMethods::USER {
                    Challenge::User
                } else {
                    Challenge::SharedSecret
                };
                let nonce = nonce_source();
                self.out
                    .push(challenge_command(kind, nonce, Because::Invited, self.local));
                self.phase = Phase::Challenged(Challenged {
                    nonce,
                    kind,
                    since: now,
                    failures: 0,
                    delayed: None,
                });
            }
        }
    }

    fn on_auth(&mut self, now: Instant, digest: &Digest) {
        let command = Command::InvitationWithAuth(*digest);
        let verdict =
            self.verify(
                Challenge::SharedSecret,
                &command,
                |admission, nonce| match admission {
                    Admission::SharedSecret(secret) => {
                        verdict(&Digest::of_secret(nonce, secret), digest)
                    }
                    _ => Verdict::Wrong,
                },
            );
        self.conclude(now, verdict);
    }

    fn on_user_auth(&mut self, now: Instant, auth: &UserAuthentication) {
        let command = Command::InvitationWithUserAuth(auth.clone());
        let verdict = self.verify(
            Challenge::User,
            &command,
            |admission, nonce| match admission {
                Admission::Users(users) => users
                    .iter()
                    .find(|(name, _)| name == &auth.username)
                    .map_or(Verdict::UnknownUser, |(name, password)| {
                        verdict(&Digest::of_user(nonce, name, password), &auth.digest)
                    }),
                _ => Verdict::Wrong,
            },
        );
        self.conclude(now, verdict);
    }

    /// Checks an authenticated Invitation against the challenge in hand.
    fn verify(
        &mut self,
        kind: Challenge,
        command: &Command,
        check: impl FnOnce(&Admission, &Nonce) -> Verdict,
    ) -> Verdict {
        let Role::Host { admission, .. } = &self.role else {
            self.nak(NakReason::NotExpected, command.header());
            return Verdict::Ignored;
        };
        match &self.phase {
            Phase::Challenged(c) if c.kind == kind => check(admission, &c.nonce),
            Phase::Challenged(_) => {
                self.nak(NakReason::NotExpected, command.header());
                Verdict::Ignored
            }
            Phase::Established | Phase::Resetting(_) => {
                self.out.push(Command::Accepted(self.local));
                Verdict::Ignored
            }
            Phase::Closing { .. } | Phase::Confirming { .. } => Verdict::Ignored,
            Phase::Idle | Phase::Inviting(_) | Phase::AwaitingAcceptance { .. } => {
                Verdict::NoInvitation
            }
        }
    }

    fn conclude(&mut self, now: Instant, verdict: Verdict) {
        match verdict {
            Verdict::Ignored => {}
            Verdict::NoInvitation => self.refuse(now, ByeReason::NoPriorInvitation),
            Verdict::Right => self.establish(now),
            Verdict::Wrong | Verdict::UnknownUser => {
                let local = self.local;
                let Phase::Challenged(c) = &mut self.phase else {
                    return;
                };
                c.failures += 1;
                if c.failures >= MAX_AUTH_FAILURES {
                    let reason = if verdict == Verdict::UnknownUser {
                        ByeReason::UsernameNotFound
                    } else {
                        ByeReason::AuthenticationFailed
                    };
                    return self.refuse(now, reason);
                }
                let because = if verdict == Verdict::UnknownUser {
                    Because::UserNotFound
                } else {
                    Because::DigestWrong
                };
                let again = challenge_command(c.kind, c.nonce, because, local);
                let delay = AUTH_FAILURE_DELAY * 2u32.pow(u32::from(c.failures) - 1);
                c.delayed = Some((now + delay, again));
            }
        }
    }

    // ---- Time ----

    /// The next instant `handle_timeout` has work at.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        let cfg = &self.cfg;
        let earliest = |times: &[Option<Instant>]| times.iter().flatten().copied().min();
        match &self.phase {
            Phase::Idle => None,
            Phase::Inviting(r) | Phase::Resetting(r) | Phase::Closing { repeat: r, .. } => {
                Some(r.deadline(cfg))
            }
            Phase::AwaitingAcceptance { until } => Some(*until),
            Phase::Confirming { since } => Some(*since + cfg.pending_timeout),
            Phase::Challenged(c) => earliest(&[
                Some(c.since + cfg.pending_timeout),
                c.delayed.as_ref().map(|(at, _)| *at),
            ]),
            Phase::Established => earliest(&[
                Some(self.last_heard + cfg.liveness_timeout),
                Some(self.next_ping()),
                self.link.idle_deadline(),
                self.link.rx_deadline(),
            ]),
        }
    }

    fn next_ping(&self) -> Instant {
        let quiet_until = self.last_heard + self.cfg.ping_interval;
        match self.last_ping {
            Some(sent) => quiet_until.max(sent + self.cfg.ping_interval),
            None => quiet_until,
        }
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let cfg = self.cfg;
        match &mut self.phase {
            Phase::Idle => {}
            Phase::Inviting(r) => {
                if now >= r.started + cfg.command_timeout {
                    return self.close(now, ByeReason::InvitationCanceled);
                }
                if now >= r.next {
                    r.next = now + cfg.repeat_interval;
                    self.out.push(r.command.clone());
                }
            }
            Phase::Resetting(r) => {
                if now >= r.started + cfg.command_timeout {
                    return self.close(now, ByeReason::Timeout);
                }
                if now >= r.next {
                    r.next = now + cfg.repeat_interval;
                    self.out.push(r.command.clone());
                }
            }
            Phase::Closing { repeat, reason } => {
                let reason = *reason;
                if now >= repeat.started + cfg.command_timeout {
                    return self.finish(Initiator::Local, reason);
                }
                if now >= repeat.next {
                    repeat.next = now + cfg.repeat_interval;
                    self.out.push(repeat.command.clone());
                }
            }
            Phase::AwaitingAcceptance { until } => {
                if now >= *until {
                    self.close(now, ByeReason::InvitationCanceled);
                }
            }
            Phase::Confirming { since } => {
                if now >= *since + cfg.pending_timeout {
                    self.refuse(now, ByeReason::UserDeclined);
                }
            }
            Phase::Challenged(c) => {
                if let Some((at, _)) = &c.delayed
                    && now >= *at
                    && let Some((_, command)) = c.delayed.take()
                {
                    self.out.push(command);
                }
                if now >= c.since + cfg.pending_timeout {
                    self.phase = Phase::Idle;
                    self.remote = None;
                }
            }
            Phase::Established => self.tick_established(now),
        }
    }

    fn tick_established(&mut self, now: Instant) {
        let cfg = self.cfg;
        if now >= self.last_heard + cfg.liveness_timeout {
            return self.close(now, ByeReason::Timeout);
        }
        if now >= self.next_ping() {
            let id = PingId(self.ping_counter);
            self.ping_counter = self.ping_counter.wrapping_add(1);
            self.outstanding_pings.push_back(id);
            if self.outstanding_pings.len() > OUTSTANDING_PINGS {
                self.outstanding_pings.pop_front();
            }
            self.last_ping = Some(now);
            self.out.push(Command::Ping(id));
        }
        self.link.on_idle(now, &cfg, &mut self.out);
        let mut delivered = Vec::new();
        let mut lost = Vec::new();
        self.link
            .on_rx_timeout(now, &cfg, &mut self.out, &mut delivered, &mut lost);
        self.report(delivered, lost);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// The digest is right.
    Right,
    /// The digest is wrong.
    Wrong,
    /// The user name is not known.
    UnknownUser,
    /// The Invitation came with no Invitation before it.
    NoInvitation,
    /// Nothing more to do: the command was answered where it was matched.
    Ignored,
}

/// The verdict on a digest: right when it is the one the Host computes.
fn verdict(expected: &Digest, got: &Digest) -> Verdict {
    if expected.matches(got) {
        Verdict::Right
    } else {
        Verdict::Wrong
    }
}

/// Why a challenge is issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Because {
    /// The first challenge, in answer to an Invitation.
    Invited,
    DigestWrong,
    UserNotFound,
}

/// The challenge command of a kind, with the state the reason maps to.
fn challenge_command(
    kind: Challenge,
    nonce: Nonce,
    because: Because,
    identity: EndpointIdentity,
) -> Command {
    match kind {
        Challenge::SharedSecret => Command::AuthRequired(AuthChallenge {
            state: match because {
                Because::Invited => AuthState::First,
                Because::DigestWrong | Because::UserNotFound => AuthState::DigestWrong,
            },
            nonce,
            identity,
        }),
        Challenge::User => Command::UserAuthRequired(UserAuthChallenge {
            state: match because {
                Because::Invited => UserAuthState::First,
                Because::DigestWrong => UserAuthState::DigestWrong,
                Because::UserNotFound => UserAuthState::UserNotFound,
            },
            nonce,
            identity,
        }),
    }
}
