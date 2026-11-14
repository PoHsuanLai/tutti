//! [`Client`] and [`Host`]: the two roles over any number of datagrams, and
//! [`Endpoint`], the boundary a socket driver feeds.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::Instant;

use tutti_ump::Ump;

use crate::datagram::{self, pack};
use crate::session::{Capacity, Role, Session};
use crate::{
    Admission, ByeReason, Credentials, EndpointIdentity, Event, Nonce, SendError, SessionConfig,
    SessionState, Transmit,
};

/// An endpoint whose every input is a datagram or the clock, and whose every
/// output is a datagram, an event or the next time it wants the clock.
///
/// [`Client`] and [`Host`] are the two; a driver ([`UdpDriver`](crate::UdpDriver))
/// feeds one of them from a socket. Nothing here blocks, reads a clock or
/// touches the network.
pub trait Endpoint {
    /// Takes a datagram received from `from` at `now`.
    fn handle_datagram(&mut self, now: Instant, from: SocketAddr, datagram: &[u8]);

    /// Does the work that is due at `now`: repeats, pings, retransmit
    /// requests, idle declarations and timeouts.
    fn handle_timeout(&mut self, now: Instant);

    /// The next datagram to send, if one is queued.
    fn poll_transmit(&mut self) -> Option<Transmit>;

    /// The next event for the application, if one is queued.
    fn poll_event(&mut self) -> Option<Event>;

    /// When [`handle_timeout`](Self::handle_timeout) next has work, if ever.
    fn poll_timeout(&self) -> Option<Instant>;
}

#[derive(Debug, Default)]
struct Outbox {
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

impl Outbox {
    /// Moves a session's output here: its commands packed into datagrams for
    /// `peer`, its events in order.
    fn drain(&mut self, peer: SocketAddr, session: &mut Session) {
        let (commands, events) = session.take_output();
        self.transmits.extend(
            pack(&commands)
                .into_iter()
                .map(|payload| Transmit { to: peer, payload }),
        );
        self.events.extend(events);
    }
}

/// Feeds the commands of a datagram to a session. A datagram that does not
/// carry the signature is ignored whole.
fn feed(session: &mut Session, now: Instant, datagram: &[u8]) {
    let Ok(commands) = datagram::parse(datagram) else {
        return;
    };
    for command in commands {
        match command {
            Ok(command) => session.receive(now, command),
            Err(refused) => session.refuse_command(now, refused.header, refused.error),
        }
    }
}

/// A Client: invites one Host and holds the Session with it.
#[derive(Debug)]
pub struct Client {
    host: SocketAddr,
    session: Session,
    outbox: Outbox,
}

impl Client {
    /// A Client that will invite the Host at `host`. Nothing is sent until
    /// [`connect`](Self::connect).
    #[must_use]
    pub fn new(
        identity: EndpointIdentity,
        credentials: Credentials,
        config: SessionConfig,
        host: SocketAddr,
        now: Instant,
    ) -> Self {
        Self {
            host,
            session: Session::new(Role::Client(credentials), config, identity, host, now),
            outbox: Outbox::default(),
        }
    }

    /// Sends the Invitation, and repeats it until the Host answers.
    pub fn connect(&mut self, now: Instant) {
        self.session.invite(now);
        self.outbox.drain(self.host, &mut self.session);
    }

    /// The state with respect to the Host.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.session.state()
    }

    /// Who the Host says it is, once it has answered.
    #[must_use]
    pub fn remote(&self) -> Option<EndpointIdentity> {
        self.session.remote()
    }

    /// Sends UMP to the Host.
    ///
    /// # Errors
    ///
    /// [`SendError::NotEstablished`] when no Session is Established.
    pub fn send(&mut self, now: Instant, packets: &[Ump]) -> Result<(), SendError> {
        self.session.send_ump(now, packets)?;
        self.outbox.drain(self.host, &mut self.session);
        Ok(())
    }

    /// Ends the Session with a Bye.
    pub fn close(&mut self, now: Instant, reason: ByeReason) {
        self.session.close(now, reason);
        self.outbox.drain(self.host, &mut self.session);
    }

    /// Resets the sequence numbers at both ends (M2-124 section 6.11).
    pub fn reset(&mut self, now: Instant) {
        self.session.reset(now);
        self.outbox.drain(self.host, &mut self.session);
    }
}

impl Endpoint for Client {
    fn handle_datagram(&mut self, now: Instant, from: SocketAddr, datagram: &[u8]) {
        // A Client talks to its Host's address alone.
        if from != self.host {
            return;
        }
        feed(&mut self.session, now, datagram);
        self.outbox.drain(self.host, &mut self.session);
    }

    fn handle_timeout(&mut self, now: Instant) {
        self.session.handle_timeout(now);
        self.outbox.drain(self.host, &mut self.session);
    }

    fn poll_transmit(&mut self) -> Option<Transmit> {
        self.outbox.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.outbox.events.pop_front()
    }

    fn poll_timeout(&self) -> Option<Instant> {
        self.session.poll_timeout()
    }
}

const DEFAULT_MAX_SESSIONS: NonZeroUsize = NonZeroUsize::new(16).unwrap();

/// How a Host is set up.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Who the Host is, as it tells every Client.
    pub identity: EndpointIdentity,
    /// How an Invitation becomes a Session.
    pub admission: Admission,
    /// How many Clients are served at once.
    pub max_sessions: NonZeroUsize,
    /// How each Session sends, repeats and recovers.
    pub session: SessionConfig,
}

impl HostConfig {
    /// A Host that accepts every Invitation, serves 16 Clients and uses the
    /// default [`SessionConfig`].
    #[must_use]
    pub fn new(identity: EndpointIdentity) -> Self {
        Self {
            identity,
            admission: Admission::Open,
            max_sessions: DEFAULT_MAX_SESSIONS,
            session: SessionConfig::default(),
        }
    }
}

/// A Host: serves any number of Clients, each known by its address and port.
#[derive(Debug)]
pub struct Host {
    config: HostConfig,
    nonce_source: fn() -> Nonce,
    sessions: HashMap<SocketAddr, Session>,
    outbox: Outbox,
}

impl Host {
    /// A Host that issues operating-system random nonces.
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        Self::with_nonce_source(config, Nonce::random)
    }

    /// A Host that takes its nonces from `nonce_source`, which a test
    /// replaces to make the run repeatable.
    #[must_use]
    pub fn with_nonce_source(config: HostConfig, nonce_source: fn() -> Nonce) -> Self {
        Self {
            config,
            nonce_source,
            sessions: HashMap::new(),
            outbox: Outbox::default(),
        }
    }

    /// The state with respect to a Client.
    #[must_use]
    pub fn state(&self, peer: SocketAddr) -> SessionState {
        self.sessions
            .get(&peer)
            .map_or(SessionState::Idle, Session::state)
    }

    /// Sends UMP to a Client.
    ///
    /// # Errors
    ///
    /// [`SendError::NotEstablished`] when no Session is Established with it.
    pub fn send(
        &mut self,
        now: Instant,
        peer: SocketAddr,
        packets: &[Ump],
    ) -> Result<(), SendError> {
        let Some(session) = self.sessions.get_mut(&peer) else {
            return Err(SendError::NotEstablished {
                state: SessionState::Idle,
            });
        };
        session.send_ump(now, packets)?;
        self.outbox.drain(peer, session);
        Ok(())
    }

    /// Accepts an Invitation that was reported as an
    /// [`Event::Confirm`].
    pub fn accept(&mut self, now: Instant, peer: SocketAddr) {
        self.with_session(peer, |session| session.accept(now));
    }

    /// Declines an Invitation that was reported as an [`Event::Confirm`].
    pub fn decline(&mut self, now: Instant, peer: SocketAddr) {
        self.with_session(peer, |session| session.decline(now));
    }

    /// Ends the Session with a Client with a Bye.
    pub fn close(&mut self, now: Instant, peer: SocketAddr, reason: ByeReason) {
        self.with_session(peer, |session| session.close(now, reason));
    }

    /// Resets the sequence numbers at both ends of the Session with a Client.
    pub fn reset(&mut self, now: Instant, peer: SocketAddr) {
        self.with_session(peer, |session| session.reset(now));
    }

    /// Ends every Session, as a Host that powers down does.
    pub fn close_all(&mut self, now: Instant) {
        let peers: Vec<SocketAddr> = self.sessions.keys().copied().collect();
        for peer in peers {
            self.close(now, peer, ByeReason::PowerDown);
        }
    }

    fn with_session(&mut self, peer: SocketAddr, act: impl FnOnce(&mut Session)) {
        if let Some(session) = self.sessions.get_mut(&peer) {
            act(session);
            self.outbox.drain(peer, session);
            self.forget_if_idle(peer);
        }
    }

    fn forget_if_idle(&mut self, peer: SocketAddr) {
        if self
            .sessions
            .get(&peer)
            .is_some_and(|s| s.state() == SessionState::Idle)
        {
            self.sessions.remove(&peer);
        }
    }

    /// How many peers the Host holds a Session, an open Invitation or a
    /// challenge with. A peer that only sent a stray command is not counted.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Endpoint for Host {
    fn handle_datagram(&mut self, now: Instant, from: SocketAddr, datagram: &[u8]) {
        if let Some(session) = self.sessions.get_mut(&from) {
            feed(session, now, datagram);
            self.outbox.drain(from, session);
            self.forget_if_idle(from);
            return;
        }
        // No Session with this peer: answer it from a session that lives for
        // this datagram. Only an Invitation that opens something is kept, so a
        // stranger costs the Host no state; over the limit the Invitation is
        // refused the same way.
        let mut session = Session::new(
            Role::Host {
                admission: self.config.admission.clone(),
                nonce_source: self.nonce_source,
            },
            self.config.session,
            self.config.identity,
            from,
            now,
        );
        session.set_capacity(if self.session_count() >= self.config.max_sessions.get() {
            Capacity::Full
        } else {
            Capacity::Available
        });
        feed(&mut session, now, datagram);
        self.outbox.drain(from, &mut session);
        if session.holds_session() {
            self.sessions.insert(from, session);
        }
    }

    fn handle_timeout(&mut self, now: Instant) {
        let peers: Vec<SocketAddr> = self.sessions.keys().copied().collect();
        for peer in peers {
            if let Some(session) = self.sessions.get_mut(&peer) {
                session.handle_timeout(now);
                self.outbox.drain(peer, session);
            }
            self.forget_if_idle(peer);
        }
    }

    fn poll_transmit(&mut self) -> Option<Transmit> {
        self.outbox.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.outbox.events.pop_front()
    }

    fn poll_timeout(&self) -> Option<Instant> {
        self.sessions
            .values()
            .filter_map(Session::poll_timeout)
            .min()
    }
}
