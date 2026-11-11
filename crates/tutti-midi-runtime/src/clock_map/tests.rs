//! Every figure is arithmetic on a chosen `Instant` and rate: the map reads no
//! clock, so `base` is any instant and only the differences matter.

use super::*;

const R48: SampleRate = SampleRate(48_000.0);

fn base() -> Instant {
    // Far enough from the clock's origin to subtract a second from.
    Instant::now() + Duration::from_secs(3600)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn map_at(t0: Instant, frame: u64) -> ClockMap {
    ClockMap::new(R48, ClockReading::new(t0, Frame(frame))).unwrap()
}

// Frames follow the rate from the anchor: 1 s is 48 000 frames, 10 ms is 480.
#[test]
fn a_time_after_the_anchor_is_the_rate_times_the_distance() {
    let t0 = base();
    let map = map_at(t0, 1000);
    assert_eq!(map.frame_at(t0), Some(Frame(1000)));
    assert_eq!(
        map.frame_at(t0 + Duration::from_secs(1)),
        Some(Frame(49_000))
    );
    assert_eq!(map.frame_at(t0 + ms(10)), Some(Frame(1480)));
    assert_eq!(
        map.moment_at(t0 + ms(10)),
        Some(Moment::new(Frame(1480), R48))
    );
}

// A time before the anchor is extrapolated back, to `None` before frame 0.
#[test]
fn a_time_before_the_anchor_is_extrapolated_back_until_frame_zero() {
    let t0 = base();
    let map = map_at(t0, 1000);
    assert_eq!(map.frame_at(t0 - ms(10)), Some(Frame(520)));
    assert_eq!(map.frame_at(t0 - ms(20)), Some(Frame(40)));
    // 1000 frames is 20.83 ms: 21 ms back is frame -8, before the start.
    assert_eq!(map.frame_at(t0 - ms(21)), None);
}

// Half a frame rounds away from zero: at 48 kHz a frame is 20 833.33 ns, so
// 10 417 ns is 0.5000 frame and 10 416 ns is under half.
#[test]
fn a_time_lands_on_the_nearest_frame() {
    let t0 = base();
    let map = map_at(t0, 0);
    assert_eq!(
        map.frame_at(t0 + Duration::from_nanos(10_417)),
        Some(Frame(1))
    );
    assert_eq!(
        map.frame_at(t0 + Duration::from_nanos(10_416)),
        Some(Frame(0))
    );
}

// The inverse: frame 49 000 is 1 s after the anchor, frame 520 is 10 ms before,
// and a round trip returns every frame.
#[test]
fn a_frame_maps_back_to_its_time() {
    let t0 = base();
    let map = map_at(t0, 1000);
    assert_eq!(map.os_at(Frame(49_000)), Some(t0 + Duration::from_secs(1)));
    assert_eq!(map.os_at(Frame(520)), Some(t0 - ms(10)));
    for f in [0, 1, 519, 1000, 1001, 123_456] {
        let os = map.os_at(Frame(f)).unwrap();
        assert_eq!(map.frame_at(os), Some(Frame(f)), "frame {f}");
    }
    // A distance an `Instant` cannot hold is no instant (a rate of 1e-9 makes
    // 2^64 frames 1e28 seconds).
    let slow = ClockMap::new(SampleRate(1e-9), ClockReading::new(t0, Frame(0))).unwrap();
    assert_eq!(slow.os_at(Frame(u64::MAX)), None);
}

// One reading 8 ms late (frame 48 000 seen at 1 s + 8 ms) moves the map by one
// eighth of 8 ms: the anchor is then at frame 48 000, 1 s + 1 ms.
#[test]
fn a_late_reading_moves_the_map_by_an_eighth() {
    let t0 = base();
    let mut map = map_at(t0, 0);
    let late = ClockReading::new(t0 + Duration::from_secs(1) + ms(8), Frame(48_000));
    assert_eq!(map.observe(late), Observed::Followed);
    assert_eq!(map.anchor().frame(), Frame(48_000));
    assert_eq!(map.anchor().os(), t0 + Duration::from_secs(1) + ms(1));
    assert_eq!(
        map.frame_at(t0 + Duration::from_secs(1)),
        Some(Frame(47_952))
    );
}

// An early reading moves it the other way.
#[test]
fn an_early_reading_moves_the_map_back_by_an_eighth() {
    let t0 = base();
    let mut map = map_at(t0, 0);
    let early = ClockReading::new(t0 + Duration::from_secs(1) - ms(8), Frame(48_000));
    assert_eq!(map.observe(early), Observed::Followed);
    assert_eq!(map.anchor().os(), t0 + Duration::from_secs(1) - ms(1));
}

// A reading 50 ms off is a restart or a suspend, not jitter: exact re-anchor.
#[test]
fn a_reading_far_off_re_anchors_exactly() {
    let t0 = base();
    let mut map = map_at(t0, 0);
    let far = ClockReading::new(t0 + Duration::from_secs(1) + ms(50), Frame(48_000));
    assert_eq!(map.observe(far), Observed::Resynced);
    assert_eq!(map.anchor(), far);
}

// The clocks disagree by 100 ppm (the device's frame takes 100 ppm longer than
// the rate says) and a reading arrives every 512 frames for 2000 blocks (21 s,
// 2.1 ms of drift). The map stays within the documented bound at any query
// distance: `ε·(8T + Δ)` in time, plus half a frame.
#[test]
fn the_map_follows_a_drifting_clock_within_its_bound() {
    let t0 = base();
    let epsilon = 100e-6;
    let block = 512.0;
    let period = block / R48.get(); // T, seconds
    let at = |k: f64| t0 + Duration::from_secs_f64(k * period * (1.0 + epsilon));
    let mut map = map_at(t0, 0);
    for k in 1..=2000u64 {
        let reading = ClockReading::new(at(k as f64), Frame(k * 512));
        assert_eq!(map.observe(reading), Observed::Followed);
    }
    let delta = 0.005; // seconds past the last reading
    for query in [0.0, 0.0025, delta] {
        let os = at(2000.0) + Duration::from_secs_f64(query);
        // The device's true frame at `os`.
        let truth = 2000.0 * block + query * R48.get() / (1.0 + epsilon);
        let got = map.frame_at(os).unwrap().get() as f64;
        let bound = R48.get() * epsilon * (8.0 * period + query) + 0.5;
        assert!(
            (got - truth).abs() <= bound,
            "query +{query}s: {got} vs {truth}, bound {bound}"
        );
    }
}

// A reading earlier than the anchor in OS time, earlier in frames, or at the
// same frame at another time changes nothing.
#[test]
fn a_reading_that_goes_backwards_is_rejected() {
    let t0 = base();
    let mut map = map_at(t0, 48_000);
    let before = map;
    for bad in [
        ClockReading::new(t0 - ms(1), Frame(48_512)),
        ClockReading::new(t0 + ms(10), Frame(47_000)),
        ClockReading::new(t0 + ms(10), Frame(48_000)),
    ] {
        assert_eq!(map.observe(bad), Observed::Rejected, "{bad:?}");
        assert_eq!(map, before);
    }
    // The same reading again is the anchor, not a fault.
    assert_eq!(map.observe(map.anchor()), Observed::Followed);
    assert_eq!(map, before);
}

// A re-prepare at 44.1 kHz keeps the instant: the anchor at 48 000 frames
// (1 s at 48 kHz) becomes frame 44 100, and a second later is 88 200.
#[test]
fn a_rate_change_keeps_the_instant_of_the_anchor() {
    let t0 = base();
    let mut map = map_at(t0, 48_000);
    assert!(map.set_rate(SampleRate(44_100.0)));
    assert_eq!(map.rate(), SampleRate(44_100.0));
    assert_eq!(map.frame_at(t0), Some(Frame(44_100)));
    assert_eq!(
        map.frame_at(t0 + Duration::from_secs(1)),
        Some(Frame(88_200))
    );
    assert_eq!(map.os_at(Frame(88_200)), Some(t0 + Duration::from_secs(1)));
}

// A rate that is not a rate builds no map and changes none.
#[test]
fn a_rate_that_is_not_finite_and_positive_is_refused() {
    let t0 = base();
    let reading = ClockReading::new(t0, Frame(0));
    for bad in [0.0, -48_000.0, f64::NAN, f64::INFINITY] {
        assert_eq!(ClockMap::new(SampleRate(bad), reading), None, "{bad}");
    }
    let mut map = map_at(t0, 0);
    let before = map;
    assert!(!map.set_rate(SampleRate(0.0)));
    assert_eq!(map, before);
}
