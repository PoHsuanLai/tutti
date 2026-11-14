use super::*;
use tutti_ump::U7;

#[cfg(feature = "serde")]
#[test]
fn the_wait_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: TimeoutWait::new,
        get: TimeoutWait::tenths,
        of: Some(TimeoutWait::of),
        valid: &[0, 1, 127],
        invalid: &[128, 255],
    });
}

#[test]
fn only_a_timeout_wait_ack_is_a_wait() {
    let wait = Ack::new(U7::of(0x34), Ack::STATUS_TIMEOUT_WAIT).with_status_data(U7::of(30));
    assert_eq!(TimeoutWait::from_ack(&wait), Some(TimeoutWait::of(30)));
    assert_eq!(
        TimeoutWait::from_ack(&Ack::new(
            U7::of(0x34),
            Ack::STATUS_FLOW_CONTROL_NEXT_CHUNKS
        )),
        None
    );
}

#[test]
fn a_wait_pushes_a_deadline_out_and_never_pulls_it_in() {
    let now = Instant::now();
    let mut deadline = now + Duration::from_secs(3);
    TimeoutWait::of(10).extend(&mut deadline, now);
    assert_eq!(deadline, now + Duration::from_secs(3));
    TimeoutWait::of(50).extend(&mut deadline, now);
    assert_eq!(deadline, now + Duration::from_secs(5));
}
