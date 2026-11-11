use super::*;

// An event stamped 30 ms before the clock's reading arrived 30 ms before the
// OS's.
#[test]
fn an_event_arrived_as_long_before_the_reading_as_its_stamp_is() {
    let os = Instant::now() + Duration::from_secs(10);
    let clock = Duration::from_secs(5);
    assert_eq!(
        arrival(os, clock, clock - Duration::from_millis(30)),
        os - Duration::from_millis(30)
    );
    assert_eq!(arrival(os, clock, clock), os);
}

// A stamp after the reading lands on it.
#[test]
fn a_stamp_after_the_reading_lands_on_it() {
    let os = Instant::now();
    let clock = Duration::from_secs(5);
    assert_eq!(arrival(os, clock, clock + Duration::from_millis(1)), os);
}
