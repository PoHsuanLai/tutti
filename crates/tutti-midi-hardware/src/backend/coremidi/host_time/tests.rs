use super::*;

// A packet stamped 10 ms of ticks before the reading is 10 ms before it; a 0
// stamp is the reading.
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

// A packet leaving 10 ms after the reading is stamped 10 ms of ticks after the
// reading's host time; the instant of that stamp is the instant asked for.
#[test]
fn a_departure_is_stamped_in_host_ticks_after_the_reading() {
    let base = Timebase::new(1, 1).unwrap();
    let pairing = Pairing {
        base,
        os: Instant::now() + Duration::from_secs(100),
        host: Duration::from_secs(50),
    };
    let at = pairing.os + Duration::from_millis(10);
    assert_eq!(pairing.host_time_of(at), 50_000_000_000 + 10_000_000);
    assert_eq!(pairing.instant_of(pairing.host_time_of(at)), at);
}
