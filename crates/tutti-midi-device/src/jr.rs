//! The receiver side of JR Timestamps: [`JrReceiver`] turns a sender's JR Clock
//! messages, with the instants they arrived at, into the sender-to-host time
//! map, and a JR Timestamp into the host time of the message it precedes
//! (M2-104 §7.2.2).
//!
//! A sender's clock counts [`TICK_HZ`] ticks a second in 16 bits, so it wraps
//! every 2.097 s, and a clock message arrives at least every 250 ms with the
//! jitter of the connection. The receiver keeps the last [`WINDOW`] clock
//! messages as pairs of (unwrapped sender time, arrival) and fits one line
//! through them:
//!
//! - **Drift** is the slope of a least-squares fit, used once the window spans
//!   [`DRIFT_SPAN`]; before that the sender's clock is taken as exactly
//!   [`TICK_HZ`], since a short span would read jitter as drift.
//! - **Offset** is the line's lowest point: a message is never sent after it
//!   arrives, so the arrival nearest the line is the one with the least delay,
//!   and the line through it is the sender's clock on the host's.
//! - **Jitter** is how far the window's arrivals lie above that line, at most.
//!   A message arrives within it of its [`JrTime::Sent`] time, so a caller that
//!   renders at the send time plus the jitter renders each message at the same
//!   delay.
//!
//! The wrap is resolved against the line itself: a timestamp's 16 bits are
//! taken to be the sender time nearest the one the line gives at the
//! timestamp's arrival, which is right for a timestamp within half a wrap
//! (1.048 s) of it. A sender silent for [`LOST_AFTER`] is [`JrLock::Lost`]: its
//! timestamps are not mapped, and the next clock message starts the window
//! over.

use std::time::{Duration, Instant};

/// The sender clock's ticks per second (M2-104 §7.2.2.1: ticks of 1/31250 s).
pub const TICK_HZ: u32 = 31_250;

/// How many JR Clock messages the fit keeps: 3.2 s of them at the 100 ms
/// cadence a sender may use, and 8 s at the 250 ms ceiling.
pub const WINDOW: usize = 32;

/// How long the window must span before its slope is taken as the sender's
/// drift.
pub const DRIFT_SPAN: Duration = Duration::from_secs(1);

/// How long without a JR Clock message before the sender's clock is lost: just
/// under half a wrap, the longest silence after which a timestamp is still
/// unambiguous. The spec's ceiling on the interval is 250 ms
/// (M2-104 §7.2.2.1), so a sender that keeps it never gets here.
pub const LOST_AFTER: Duration = Duration::from_secs(1);

/// A sender clock's wrap, in ticks.
const WRAP: f64 = 65_536.0;

/// Where a receiver is with a sender's clock, at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JrLock {
    /// No JR Clock message has been heard: a message is rendered as soon as it
    /// arrives (M2-104 §7.2.2.3).
    Waiting,
    /// The last JR Clock message is recent enough that a timestamp is mapped.
    Locked,
    /// The sender has been silent for [`LOST_AFTER`]: a timestamp is not
    /// mapped until the clock is heard again.
    Lost,
}

/// Why a JR Timestamp has no host time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unmapped {
    /// The sender's clock was never heard.
    NoClock,
    /// The sender's clock was heard, and not lately ([`JrLock::Lost`]).
    ClockLost,
    /// The mapped time cannot be an `Instant`.
    OutOfRange,
}

/// The host time of a message a JR Timestamp precedes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "an `Arrival` outcome means the message keeps its arrival time"]
pub enum JrTime {
    /// The instant the sender sent the message at, on the host's clock, never
    /// after the message arrived.
    Sent {
        /// When the message was sent.
        at: Instant,
        /// How far above the line the window's arrivals lie, at most: the
        /// delay that renders every message after its arrival, if the jitter
        /// holds.
        jitter: Duration,
    },
    /// The timestamp cannot be mapped: the message is placed at its arrival.
    Arrival(Unmapped),
}

impl JrTime {
    /// When to render the message: the send time plus the jitter, or `arrival`
    /// where the timestamp was not mapped.
    #[must_use]
    pub fn render_at(self, arrival: Instant) -> Instant {
        match self {
            Self::Sent { at, jitter } => at.checked_add(jitter).unwrap_or(at),
            Self::Arrival(_) => arrival,
        }
    }
}

/// What a JR Clock message did to the receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use]
pub enum ClockHeard {
    /// It joined the window.
    Taken,
    /// It came after a silence of more than [`LOST_AFTER`]: the window started
    /// over with it.
    Restarted,
    /// Its time is not after the last one's (it was reordered, or sent twice):
    /// it is ignored. A clock is monotonic but for the wrap (M2-104 §7.2.2.1).
    Late,
}

/// One JR Clock message: the sender's time, unwrapped, and when it arrived,
/// in seconds from the receiver's origin.
#[derive(Clone, Copy, Debug)]
struct Reading {
    ticks: i64,
    at: f64,
}

/// A sender's clock on the host's; see the module.
#[derive(Clone, Debug)]
pub struct JrReceiver {
    window: [Reading; WINDOW],
    len: usize,
    /// Where the oldest reading is.
    start: usize,
    /// The instant the readings' seconds are counted from: the first
    /// arrival.
    origin: Option<Instant>,
    /// The last clock message's 16 bits and arrival, for the wrap and the
    /// silence.
    last: Option<(u16, Instant)>,
}

impl Default for JrReceiver {
    fn default() -> Self {
        Self::new()
    }
}

/// The line the window fits: host seconds = `slope` * sender seconds from
/// `ticks0` + `offset`.
#[derive(Clone, Copy, Debug)]
struct Line {
    ticks0: i64,
    slope: f64,
    offset: f64,
    jitter: f64,
}

impl JrReceiver {
    /// A receiver that has heard no clock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            window: [Reading { ticks: 0, at: 0.0 }; WINDOW],
            len: 0,
            start: 0,
            origin: None,
            last: None,
        }
    }

    /// Forgets the sender's clock: the state of one never heard.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Where the receiver is with the sender's clock at `now`.
    pub fn lock(&self, now: Instant) -> JrLock {
        match self.last {
            None => JrLock::Waiting,
            Some((_, at)) if now.saturating_duration_since(at) > LOST_AFTER => JrLock::Lost,
            Some(_) => JrLock::Locked,
        }
    }

    /// Takes a JR Clock message of sender time `ticks` that arrived at
    /// `arrival`.
    pub fn clock(&mut self, ticks: u16, arrival: Instant) -> ClockHeard {
        let mut heard = ClockHeard::Taken;
        let unwrapped = match self.last {
            None => i64::from(ticks),
            Some((_, at)) if arrival.saturating_duration_since(at) > LOST_AFTER => {
                // The wrap count of a clock heard after this long is unknown.
                self.len = 0;
                self.start = 0;
                heard = ClockHeard::Restarted;
                i64::from(ticks)
            }
            Some((last, _)) => {
                // The sender's step, a signed 16-bit difference: under half a
                // wrap, because the silence is.
                let step = i64::from(ticks.wrapping_sub(last).cast_signed());
                if step <= 0 {
                    return ClockHeard::Late;
                }
                self.newest().map_or(i64::from(ticks), |r| r.ticks + step)
            }
        };
        let origin = *self.origin.get_or_insert(arrival);
        let reading = Reading {
            ticks: unwrapped,
            at: seconds_from(origin, arrival),
        };
        if self.len == WINDOW {
            self.start = (self.start + 1) % WINDOW;
            self.len -= 1;
        }
        self.window[(self.start + self.len) % WINDOW] = reading;
        self.len += 1;
        self.last = Some((ticks, arrival));
        heard
    }

    /// The host time of a message that a JR Timestamp of sender time `ticks`
    /// precedes and that arrived at `arrival`.
    pub fn resolve(&self, ticks: u16, arrival: Instant) -> JrTime {
        match self.lock(arrival) {
            JrLock::Waiting => return JrTime::Arrival(Unmapped::NoClock),
            JrLock::Lost => return JrTime::Arrival(Unmapped::ClockLost),
            JrLock::Locked => {}
        }
        let (Some(origin), Some(line)) = (self.origin, self.line()) else {
            return JrTime::Arrival(Unmapped::NoClock);
        };
        // The sender time the line gives at the arrival, and the timestamp's
        // wrap nearest it.
        let seen = (seconds_from(origin, arrival) - line.offset) / line.slope * f64::from(TICK_HZ)
            + line.ticks0 as f64;
        let wraps = ((seen - f64::from(ticks)) / WRAP).round();
        let sender = f64::from(ticks) + wraps * WRAP;
        let host = line.slope * (sender - line.ticks0 as f64) / f64::from(TICK_HZ) + line.offset;
        let sent = instant_at(origin, host).map(|at| at.min(arrival));
        let jitter = Duration::try_from_secs_f64(line.jitter);
        match (sent, jitter) {
            (Some(at), Ok(jitter)) => JrTime::Sent { at, jitter },
            _ => JrTime::Arrival(Unmapped::OutOfRange),
        }
    }

    fn newest(&self) -> Option<Reading> {
        (self.len > 0).then(|| self.window[(self.start + self.len - 1) % WINDOW])
    }

    fn readings(&self) -> impl Iterator<Item = Reading> + '_ {
        (0..self.len).map(|i| self.window[(self.start + i) % WINDOW])
    }

    /// The line through the window: `None` while it is empty.
    fn line(&self) -> Option<Line> {
        let first = self.readings().next()?;
        let x = |r: Reading| (r.ticks - first.ticks) as f64 / f64::from(TICK_HZ);
        let n = self.len as f64;
        let (mx, my) = self
            .readings()
            .fold((0.0, 0.0), |(sx, sy), r| (sx + x(r), sy + r.at));
        let (mx, my) = (mx / n, my / n);
        let (cov, var) = self.readings().fold((0.0, 0.0), |(c, v), r| {
            let dx = x(r) - mx;
            (c + dx * (r.at - my), v + dx * dx)
        });
        let span = self.readings().last().map_or(0.0, x);
        let slope = if self.len >= 2 && span >= DRIFT_SPAN.as_secs_f64() && var > 0.0 {
            cov / var
        } else {
            1.0
        };
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for r in self.readings() {
            let offset = r.at - slope * x(r);
            low = low.min(offset);
            high = high.max(offset);
        }
        Some(Line {
            ticks0: first.ticks,
            slope,
            offset: low,
            jitter: high - low,
        })
    }
}

/// Seconds from `origin` to `at`, negative before it.
fn seconds_from(origin: Instant, at: Instant) -> f64 {
    match at.checked_duration_since(origin) {
        Some(after) => after.as_secs_f64(),
        None => -origin.duration_since(at).as_secs_f64(),
    }
}

/// The instant `seconds` after `origin`, `None` where it is not one.
fn instant_at(origin: Instant, seconds: f64) -> Option<Instant> {
    if seconds >= 0.0 {
        origin.checked_add(Duration::try_from_secs_f64(seconds).ok()?)
    } else {
        origin.checked_sub(Duration::try_from_secs_f64(-seconds).ok()?)
    }
}

#[cfg(test)]
mod tests;
