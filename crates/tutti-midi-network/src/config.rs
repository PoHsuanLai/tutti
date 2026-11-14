//! [`SessionConfig`]: the timing and data-integrity choices of a Session.

use std::num::{NonZeroU8, NonZeroUsize};
use std::time::Duration;

/// The longest the first Zero Length UMP Data Command may follow the last
/// UMP Data Command that carried data (M2-124 section 7.2.1).
const IDLE_INTERVAL_MAX: Duration = Duration::from_millis(300);
/// The range M2-124 section 6.2 recommends for the delay between a repeated
/// command's repeats.
const REPEAT_INTERVAL_MIN: Duration = Duration::from_millis(300);
const REPEAT_INTERVAL_MAX: Duration = Duration::from_secs(2);

const BUFFER_DEFAULT: NonZeroUsize = NonZeroUsize::new(256).unwrap();
const REQUESTS_DEFAULT: NonZeroU8 = NonZeroU8::new(5).unwrap();

/// Forward error correction (M2-124 section 7.2.2), an optional mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fec {
    /// No earlier command is sent again.
    Off,
    /// Each new command follows this many earlier ones.
    Repeats(NonZeroU8),
}

impl Fec {
    pub(crate) const fn repeats(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Repeats(n) => n.get(),
        }
    }
}

/// Serving Retransmit Requests (M2-124 section 7.2.3), an optional mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retransmit {
    /// Retransmit is not implemented: a request is answered with a NAK.
    Unsupported,
    /// The last this many sent UMP Data Commands are kept to be sent again.
    Buffer(NonZeroUsize),
}

impl Retransmit {
    pub(crate) const fn capacity(self) -> usize {
        match self {
            Self::Unsupported => 0,
            Self::Buffer(n) => n.get(),
        }
    }
}

/// Asking for missing commands with Retransmit Requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Requests {
    /// A gap is given up as lost without asking.
    Never,
    /// A gap is asked for up to this many times.
    UpTo(NonZeroU8),
}

impl Requests {
    pub(crate) const fn limit(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::UpTo(n) => n.get(),
        }
    }
}

/// How a Session sends, repeats, probes and recovers.
///
/// [`Default`] is a conservative choice for a local network: two forward
/// error correction repeats, a 256-command retransmit buffer, a Ping after
/// two silent seconds and a Session lost after six.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub(crate) fec: Fec,
    pub(crate) retransmit: Retransmit,
    pub(crate) idle_interval: Duration,
    pub(crate) reorder_wait: Duration,
    pub(crate) requests: Requests,
    pub(crate) ping_interval: Duration,
    pub(crate) liveness_timeout: Duration,
    pub(crate) repeat_interval: Duration,
    pub(crate) command_timeout: Duration,
    pub(crate) pending_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            fec: Fec::Repeats(NonZeroU8::MIN.saturating_add(1)),
            retransmit: Retransmit::Buffer(BUFFER_DEFAULT),
            idle_interval: Duration::from_millis(10),
            reorder_wait: Duration::from_millis(10),
            requests: Requests::UpTo(REQUESTS_DEFAULT),
            ping_interval: Duration::from_secs(2),
            liveness_timeout: Duration::from_secs(6),
            repeat_interval: Duration::from_millis(500),
            command_timeout: Duration::from_secs(5),
            pending_timeout: Duration::from_secs(60),
        }
    }
}

impl SessionConfig {
    /// Whether, and how far, each new UMP Data Command carries earlier ones
    /// again (forward error correction, M2-124 section 7.2.2). Two repeats
    /// is the recommended figure.
    #[must_use]
    pub const fn with_fec(mut self, fec: Fec) -> Self {
        self.fec = fec;
        self
    }

    /// Whether this end answers Retransmit Requests, and how many sent UMP
    /// Data Commands it keeps to do so.
    #[must_use]
    pub const fn with_retransmit(mut self, retransmit: Retransmit) -> Self {
        self.retransmit = retransmit;
        self
    }

    /// How long after sending data the first Zero Length UMP Data Command
    /// follows, and the interval of the next few. Held to at most 300 ms,
    /// which the specification requires of the first.
    #[must_use]
    pub fn with_idle_interval(mut self, interval: Duration) -> Self {
        self.idle_interval = interval.clamp(Duration::from_millis(1), IDLE_INTERVAL_MAX);
        self
    }

    /// How long a missing sequence number is waited for before a Retransmit
    /// Request asks for it (the specification suggests 10 ms), which lets
    /// an out-of-order command arrive first.
    #[must_use]
    pub const fn with_reorder_wait(mut self, wait: Duration) -> Self {
        self.reorder_wait = wait;
        self
    }

    /// Whether this end asks for missing commands, and how many Retransmit
    /// Requests it sends for one gap before giving it up as lost. The wait
    /// between requests doubles.
    #[must_use]
    pub const fn with_requests(mut self, requests: Requests) -> Self {
        self.requests = requests;
        self
    }

    /// How long a peer is silent before a Ping is sent.
    #[must_use]
    pub const fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// How long a peer is silent before the Session is ended with a Bye for
    /// timeout.
    #[must_use]
    pub const fn with_liveness_timeout(mut self, timeout: Duration) -> Self {
        self.liveness_timeout = timeout;
        self
    }

    /// The delay between the repeats of an Invitation, Session Reset or Bye,
    /// held to the 300 ms to 2 s M2-124 section 6.2 recommends.
    #[must_use]
    pub fn with_repeat_interval(mut self, interval: Duration) -> Self {
        self.repeat_interval = interval.clamp(REPEAT_INTERVAL_MIN, REPEAT_INTERVAL_MAX);
        self
    }

    /// How long a repeated command is repeated before the endpoint gives up
    /// and ends the Session with a Bye.
    #[must_use]
    pub const fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// How long a Client waits for an Invitation Reply: Accepted after the
    /// Host said it was Pending (a user may be deciding), and how long a Host
    /// holds a challenge or a confirmation open.
    #[must_use]
    pub const fn with_pending_timeout(mut self, timeout: Duration) -> Self {
        self.pending_timeout = timeout;
        self
    }
}
