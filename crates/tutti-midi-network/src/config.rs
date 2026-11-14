//! [`SessionConfig`]: the timing and data-integrity choices of a Session.

use std::time::Duration;

/// The longest the first Zero Length UMP Data Command may follow the last
/// UMP Data Command that carried data (M2-124 section 7.2.1).
const IDLE_INTERVAL_MAX: Duration = Duration::from_millis(300);
/// The range M2-124 section 6.2 recommends for the delay between a repeated
/// command's repeats.
const REPEAT_INTERVAL_MIN: Duration = Duration::from_millis(300);
const REPEAT_INTERVAL_MAX: Duration = Duration::from_secs(2);

/// How a Session sends, repeats, probes and recovers.
///
/// [`Default`] is a conservative choice for a local network: two forward
/// error correction repeats, a 256-command retransmit buffer, a Ping after
/// two silent seconds and a Session lost after six.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub(crate) fec_repeats: u8,
    pub(crate) retransmit_buffer: usize,
    pub(crate) idle_interval: Duration,
    pub(crate) reorder_wait: Duration,
    pub(crate) max_retransmit_requests: u8,
    pub(crate) ping_interval: Duration,
    pub(crate) liveness_timeout: Duration,
    pub(crate) repeat_interval: Duration,
    pub(crate) command_timeout: Duration,
    pub(crate) pending_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            fec_repeats: 2,
            retransmit_buffer: 256,
            idle_interval: Duration::from_millis(10),
            reorder_wait: Duration::from_millis(10),
            max_retransmit_requests: 5,
            ping_interval: Duration::from_secs(2),
            liveness_timeout: Duration::from_secs(6),
            repeat_interval: Duration::from_millis(500),
            command_timeout: Duration::from_secs(5),
            pending_timeout: Duration::from_secs(60),
        }
    }
}

impl SessionConfig {
    /// How many earlier UMP Data Commands each new one carries again (forward
    /// error correction, M2-124 section 7.2.2). Two is the recommended
    /// figure; none turns it off.
    #[must_use]
    pub const fn with_fec_repeats(mut self, repeats: u8) -> Self {
        self.fec_repeats = repeats;
        self
    }

    /// How many sent UMP Data Commands are kept to answer a Retransmit
    /// Request. None means the Session does not support retransmit and
    /// answers a request with a NAK.
    #[must_use]
    pub const fn with_retransmit_buffer(mut self, commands: usize) -> Self {
        self.retransmit_buffer = commands;
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

    /// How many Retransmit Requests are sent for one gap before it is given
    /// up as lost. The wait between requests doubles.
    #[must_use]
    pub const fn with_max_retransmit_requests(mut self, requests: u8) -> Self {
        self.max_retransmit_requests = requests;
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
