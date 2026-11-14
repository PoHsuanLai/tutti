use super::*;

const MS: Duration = Duration::from_millis(1);

/// A SplitMix64 stream from a fixed seed: the jitter of the arrivals.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A sender whose clock runs `1 + drift` times the host's, started at
/// `phase` ticks, with a connection that delays every message by `latency`
/// plus up to `jitter`.
struct Link {
    base: Instant,
    drift: f64,
    phase: f64,
    latency: Duration,
    jitter: Duration,
    rng: Rng,
}

impl Link {
    fn new(drift: f64, phase: f64, jitter: Duration) -> Self {
        Self {
            base: Instant::now(),
            drift,
            phase,
            latency: 3 * MS,
            jitter,
            rng: Rng(0x5EED),
        }
    }

    /// The sender's 16 bits at `t` seconds after the base.
    fn ticks(&self, t: f64) -> u16 {
        let ticks = self.phase + t * (1.0 + self.drift) * f64::from(TICK_HZ);
        (ticks.round() as u64 & 0xFFFF) as u16
    }

    fn send(&self, t: f64) -> Instant {
        self.base + Duration::from_secs_f64(t)
    }

    /// When a message sent at `t` arrives.
    fn arrival(&mut self, t: f64) -> Instant {
        self.send(t) + self.latency + self.jitter.mul_f64(self.rng.unit())
    }

    /// Sends a JR Clock at `t`.
    fn clock(&mut self, rx: &mut JrReceiver, t: f64) -> ClockHeard {
        let arrival = self.arrival(t);
        rx.clock(self.ticks(t), arrival)
    }

    /// The error of the send time the receiver gives a message sent at `t`,
    /// against the send time plus the least latency (which is what a
    /// receiver can know), and the outcome.
    fn error(&mut self, rx: &JrReceiver, t: f64) -> (f64, Instant, JrTime) {
        let arrival = self.arrival(t);
        let time = rx.resolve(self.ticks(t), arrival);
        let JrTime::Sent { at, .. } = time else {
            panic!("a locked receiver maps a timestamp: {time:?}");
        };
        let truth = self.send(t) + self.latency;
        let error = if at >= truth {
            at.duration_since(truth)
        } else {
            truth.duration_since(at)
        };
        (error.as_secs_f64(), arrival, time)
    }
}

/// Send times are recovered over a sender clock 200 ppm fast, across five
/// wraps of its 16 bits, with each arrival up to 2 ms late: within a quarter of
/// the jitter, and never after the message arrived.
#[test]
fn send_times_recovered_within_a_quarter_of_the_jitter_over_200_ppm_drift() {
    let jitter = 2 * MS;
    let mut link = Link::new(200e-6, 40_000.0, jitter);
    let mut rx = JrReceiver::new();
    let mut t = 0.0;
    while t < 10.0 {
        assert_eq!(link.clock(&mut rx, t), ClockHeard::Taken);
        t += 0.1;
    }
    let mut worst = 0.0f64;
    while t < 13.0 {
        let _ = link.clock(&mut rx, t);
        for k in 0..14 {
            let sent = t + f64::from(k) * 0.007;
            let (error, arrival, time) = link.error(&rx, sent);
            worst = worst.max(error);
            let JrTime::Sent { at, jitter: spread } = time else {
                unreachable!()
            };
            assert!(at <= arrival, "a message is never sent after it arrives");
            assert!(spread <= jitter, "the spread is the jitter's at most");
        }
        t += 0.1;
    }
    assert!(
        worst <= jitter.as_secs_f64() / 4.0,
        "worst error {worst} s over a jitter of {jitter:?}"
    );
}

/// A timestamp whose 16 bits wrapped after the last clock is the sender time
/// the wrap makes it, to the tick: clocks at 65 000 and 65 500 ticks, then a
/// message at tick 1 000 (a wrap later), with no jitter and no drift.
#[test]
fn a_timestamp_past_the_wrap_is_the_next_wrap() {
    let mut link = Link::new(0.0, 65_000.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    let tick = 1.0 / f64::from(TICK_HZ);
    assert_eq!(link.clock(&mut rx, 0.0), ClockHeard::Taken);
    assert_eq!(link.clock(&mut rx, 500.0 * tick), ClockHeard::Taken);
    // 1 536 ticks after the first clock: tick 65 000 + 1 536 = 66 536 = 1 000.
    let sent = 1_536.0 * tick;
    let arrival = link.arrival(sent);
    assert_eq!(link.ticks(sent), 1_000);
    let JrTime::Sent { at, .. } = rx.resolve(1_000, arrival) else {
        panic!("locked")
    };
    let error = at.duration_since(link.send(sent));
    assert!(error < Duration::from_micros(1), "{error:?}");
}

/// A timestamp slightly behind the last clock (a message sent before it) is
/// before it, not a wrap ahead.
#[test]
fn a_timestamp_behind_the_last_clock_is_not_a_wrap_ahead() {
    let mut link = Link::new(0.0, 100.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    assert_eq!(link.clock(&mut rx, 0.2), ClockHeard::Taken);
    let sent = 0.19;
    let arrival = link.arrival(0.2);
    let JrTime::Sent { at, .. } = rx.resolve(link.ticks(sent), arrival) else {
        panic!("locked")
    };
    let error = if at > link.send(sent) {
        at.duration_since(link.send(sent))
    } else {
        link.send(sent).duration_since(at)
    };
    assert!(error < Duration::from_micros(40), "{error:?}");
}

/// Three clocks lost (a 900 ms gap, under the half wrap) leave the
/// receiver locked, and a timestamp after the gap is mapped to the tick.
#[test]
fn a_gap_of_lost_clocks_under_the_limit_stays_locked() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    for k in 0..5 {
        assert_eq!(link.clock(&mut rx, f64::from(k) * 0.25), ClockHeard::Taken);
    }
    // Clocks at 1.25, 1.5 and 1.75 are lost; the next is 0.9 s after the last.
    assert_eq!(link.clock(&mut rx, 1.9), ClockHeard::Taken);
    let (error, ..) = link.error(&rx, 2.0);
    assert!(error < 40e-6, "{error}");
}

/// A silence of more than [`LOST_AFTER`] loses the clock: timestamps keep
/// their arrival, the next clock starts the window over, and it is mapped
/// again to the tick, with the wraps of the silence unknown and unneeded.
#[test]
fn a_silence_loses_the_clock_and_the_next_clock_relocks() {
    let mut link = Link::new(0.0, 7.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    for k in 0..8 {
        let _ = link.clock(&mut rx, f64::from(k) * 0.1);
    }
    let silent = link.arrival(0.7 + 1.5);
    assert_eq!(rx.lock(silent), JrLock::Lost);
    assert_eq!(
        rx.resolve(link.ticks(2.2), silent),
        JrTime::Arrival(Unmapped::ClockLost)
    );
    // 5.0 s later, a clock of an unknown wrap.
    assert_eq!(link.clock(&mut rx, 5.0), ClockHeard::Restarted);
    assert_eq!(rx.lock(link.arrival(5.0)), JrLock::Locked);
    let (error, ..) = link.error(&rx, 5.05);
    assert!(error < 40e-6, "{error}");
}

/// A timestamp before any clock keeps the message's arrival, typed as such
/// (M2-104 §7.2.2.3: render as soon as possible).
#[test]
fn a_timestamp_before_any_clock_falls_back_to_arrival() {
    let rx = JrReceiver::new();
    let now = Instant::now();
    assert_eq!(rx.lock(now), JrLock::Waiting);
    let time = rx.resolve(1_234, now);
    assert_eq!(time, JrTime::Arrival(Unmapped::NoClock));
    assert_eq!(time.render_at(now), now);
}

/// A clock that is not after the last (reordered or repeated) is ignored and
/// leaves the mapping as it was.
#[test]
fn a_late_or_repeated_clock_is_ignored() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    let _ = link.clock(&mut rx, 0.2);
    let _ = link.clock(&mut rx, 0.3);
    let before = rx.resolve(link.ticks(0.35), link.arrival(0.35));
    assert_eq!(link.clock(&mut rx, 0.3), ClockHeard::Late);
    // Earlier by 50 ms, arriving now.
    let stale = rx.clock(link.ticks(0.25), link.arrival(0.31));
    assert_eq!(stale, ClockHeard::Late);
    assert_eq!(rx.resolve(link.ticks(0.35), link.arrival(0.35)), before);
}

/// The render time is the send time plus the jitter, which is after every
/// arrival of the window.
#[test]
fn render_time_is_send_time_plus_jitter_and_after_the_arrival() {
    let jitter = 2 * MS;
    let mut link = Link::new(0.0, 0.0, jitter);
    let mut rx = JrReceiver::new();
    let mut t = 0.0;
    while t < 3.0 {
        let _ = link.clock(&mut rx, t);
        t += 0.1;
    }
    let mut late = 0;
    for k in 0..50 {
        let sent = t + f64::from(k) * 0.001;
        let (_, arrival, time) = link.error(&rx, sent);
        let JrTime::Sent { at, jitter: spread } = time else {
            unreachable!()
        };
        assert_eq!(time.render_at(arrival), at + spread);
        // Only an arrival that is later than any the window held is after it.
        late += usize::from(time.render_at(arrival) + MS / 2 < arrival);
    }
    assert!(
        late <= 2,
        "{late} of 50 arrive well after their render time"
    );
}

/// After a reset the receiver is as one that never heard a clock.
#[test]
fn reset_forgets_the_clock() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    let mut rx = JrReceiver::new();
    let _ = link.clock(&mut rx, 0.0);
    let now = link.arrival(0.0);
    assert_eq!(rx.lock(now), JrLock::Locked);
    rx.reset();
    assert_eq!(rx.lock(now), JrLock::Waiting);
}
