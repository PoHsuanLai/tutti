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
// reading's host time, and one leaving 10 ms before it 10 ms before: the
// departure is the arrival's inverse wherever the arrival reads (`instant_of`
// reads a packet's stamp at or before the reading, as a received packet's is).
#[test]
fn a_departure_is_stamped_in_host_ticks_after_the_reading() {
    let base = Timebase::new(1, 1).unwrap();
    let pairing = Pairing {
        base,
        os: Instant::now() + Duration::from_secs(100),
        host: Duration::from_secs(50),
    };
    let later = pairing.os + Duration::from_millis(10);
    assert_eq!(pairing.host_time_of(later), 50_000_000_000 + 10_000_000);
    let earlier = pairing.os - Duration::from_millis(10);
    assert_eq!(pairing.host_time_of(earlier), 50_000_000_000 - 10_000_000);
    assert_eq!(pairing.instant_of(pairing.host_time_of(earlier)), earlier);
}
