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

// `departure` inverts `arrival`: an instant 30 ms after the reading is the
// clock's reading plus 30 ms, 30 ms before it minus 30 ms, and the clock never
// reads below zero.
#[test]
fn a_departure_is_the_clock_reading_at_the_instant() {
    let os = Instant::now() + Duration::from_secs(10);
    let clock = Duration::from_secs(5);
    let ms = Duration::from_millis;
    assert_eq!(departure(os, clock, os + ms(30)), Some(clock + ms(30)));
    assert_eq!(departure(os, clock, os - ms(30)), Some(clock - ms(30)));
    assert_eq!(departure(os, clock, os), Some(clock));
    assert_eq!(departure(os, ms(10), os - ms(30)), Some(Duration::ZERO));
    let stamped = clock - ms(30);
    assert_eq!(
        departure(os, clock, arrival(os, clock, stamped)),
        Some(stamped)
    );
}
