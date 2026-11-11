use super::*;

// Apple Silicon's base is 125/3: 3 ticks are 125 ns, so 24 000 000 ticks (one
// of its seconds is 24 MHz) are 1 s.
// Mutation: `numer` and `denom` swapped in `duration`: reads 0.576 ms.
#[test]
fn a_tick_is_the_timebases_ratio_of_a_nanosecond() {
    let base = Timebase {
        numer: 125,
        denom: 3,
    };
    assert_eq!(base.duration(24_000_000), Duration::from_secs(1));
    assert_eq!(base.duration(3), Duration::from_nanos(125));
    // An uptime of years of ticks does not overflow the product.
    assert!(base.duration(u64::MAX) > Duration::from_secs(1 << 30));
}

// A packet stamped 10 ms of ticks before the reading is 10 ms before it; a 0
// stamp is the reading.
// Mutation: drop the `host_time == 0` branch: a 0 stamp reads as an event 50 s
// before the reading, and the last assertion fails.
#[test]
fn a_packets_stamp_places_it_before_the_reading() {
    let base = Timebase { numer: 1, denom: 1 };
    let pairing = Pairing {
        base,
        os: Instant::now() + Duration::from_secs(100),
        host: Duration::from_secs(50),
    };
    let ten_ms_ago = 50_000_000_000 - 10_000_000;
    assert_eq!(
        pairing.instant_of(ten_ms_ago),
        pairing.os - Duration::from_millis(10)
    );
    assert_eq!(pairing.instant_of(0), pairing.os);
}
