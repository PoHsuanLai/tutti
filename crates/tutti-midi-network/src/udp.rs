//! [`UdpDriver`]: an [`Endpoint`] over a `std::net::UdpSocket`.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use crate::{Endpoint, Event, MAX_DATAGRAM_BYTES};

/// The longest a [`poll`](UdpDriver::poll) blocks when the endpoint has no
/// deadline of its own.
const IDLE_WAIT_MAX: Duration = Duration::from_millis(100);

/// The shortest a poll blocks: a socket read timeout of zero means forever.
const WAIT_MIN: Duration = Duration::from_millis(1);

/// Runs an [`Endpoint`] over a UDP socket: it reads datagrams, keeps the
/// endpoint's clock, and sends what the endpoint queues.
///
/// One thread calls [`poll`](Self::poll) in a loop; between calls the
/// application reads [`Event`]s and hands the endpoint UMP through
/// [`endpoint_mut`](Self::endpoint_mut).
#[derive(Debug)]
pub struct UdpDriver<E: Endpoint> {
    socket: UdpSocket,
    endpoint: E,
    buffer: Vec<u8>,
}

impl<E: Endpoint> UdpDriver<E> {
    /// A driver over `socket`, which is the one source address the peer sees:
    /// a Host's listening port, a Client's own.
    #[must_use]
    pub fn new(socket: UdpSocket, endpoint: E) -> Self {
        Self {
            socket,
            endpoint,
            // Room for the largest datagram: a longer one is cut and its
            // trailing commands refused, never read past the buffer.
            buffer: vec![0; 2 * MAX_DATAGRAM_BYTES],
        }
    }

    /// A driver over a new socket bound to `addr`.
    ///
    /// # Errors
    ///
    /// The error from binding.
    pub fn bind(addr: impl ToSocketAddrs, endpoint: E) -> io::Result<Self> {
        Ok(Self::new(UdpSocket::bind(addr)?, endpoint))
    }

    /// The address the socket is bound to.
    ///
    /// # Errors
    ///
    /// The error from the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &E {
        &self.endpoint
    }

    /// The endpoint, to hand it UMP or end a Session. Call [`poll`](Self::poll)
    /// after, to send what it queued.
    pub fn endpoint_mut(&mut self) -> &mut E {
        &mut self.endpoint
    }

    /// The next event for the application.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.endpoint.poll_event()
    }

    /// Sends what is queued, then waits for a datagram or the endpoint's next
    /// deadline, at most `max_wait`, takes what arrived and does the work
    /// that is due, and sends what that queued.
    ///
    /// # Errors
    ///
    /// The error from the socket. A datagram that cannot be sent to an
    /// unreachable peer is dropped, as UDP does.
    pub fn poll(&mut self, max_wait: Duration) -> io::Result<()> {
        self.flush()?;
        let now = Instant::now();
        let until_deadline = self
            .endpoint
            .poll_timeout()
            .map_or(IDLE_WAIT_MAX, |at| at.saturating_duration_since(now));
        let wait = until_deadline.min(max_wait).max(WAIT_MIN);
        self.socket.set_read_timeout(Some(wait))?;
        match self.socket.recv_from(&mut self.buffer) {
            Ok((len, from)) => {
                self.endpoint
                    .handle_datagram(Instant::now(), from, &self.buffer[..len]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            // A previous send reached a port with nobody behind it.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e),
        }
        self.endpoint.handle_timeout(Instant::now());
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        while let Some(transmit) = self.endpoint.poll_transmit() {
            match self.socket.send_to(&transmit.payload, transmit.to) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
