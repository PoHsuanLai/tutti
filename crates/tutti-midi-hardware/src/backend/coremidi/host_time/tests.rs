use super::*;

// A packet stamped 10 ms of ticks before the reading is 10 ms before it; a 0
// stamp is the reading.
// Mutation: drop the `host_time == 0` branch: a 0 stamp reads as an event 50 s
// before the reading, and the last assertion fails.
#[test]
fn a_packets_stamp_places_it_before_the_reading() {
    let base = Timebase::new(1, 1).unwrap();
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
