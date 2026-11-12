use super::*;
use tutti_ump::{ManufacturerId, U7};

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::new(0x41).unwrap(),
        family: [U7::of(1), U7::of(2)],
        model: [U7::of(3), U7::of(4)],
        revision: [U7::of(5), U7::of(6), U7::of(7), U7::of(8)],
    }
}

/// M1 p.40: the request is `F0 7E 7F 06 01 F7`. As SysEx7 UMP (M2-104 §7.7) the
/// payload `7E 7F 06 01` is one complete packet of four bytes: type 3, the
/// Group, status 0, count 4, then the bytes.
#[test]
fn the_request_is_the_all_call_identity_request_in_one_sysex7_packet() {
    let t0 = Instant::now();
    let (_, first) = IdentityInquiry::new(MidiGroup::of(5), t0);
    let InquiryAction::Send(packet) = first else {
        panic!("the first step is the request, got {first:?}");
    };
    assert_eq!(packet.words(), [0x3504_7E7F, 0x0601_0000]);
}

#[test]
fn a_reply_answers_once_and_ends_the_inquiry() {
    let t0 = Instant::now();
    let (mut inquiry, _) = IdentityInquiry::new(MidiGroup::of(0), t0);
    assert_eq!(inquiry.deadline(), Some(t0 + INQUIRY_TIMEOUT));
    assert_eq!(
        inquiry.on_reply(identity()),
        InquiryAction::Answered(identity())
    );
    assert!(inquiry.is_done());
    assert_eq!(inquiry.deadline(), None);
    assert_eq!(inquiry.on_reply(identity()), InquiryAction::Wait);
    assert_eq!(
        inquiry.on_timeout(t0 + INQUIRY_TIMEOUT * 5),
        InquiryAction::Wait
    );
}

#[test]
fn silence_sends_the_request_once_more_after_the_timeout() {
    let t0 = Instant::now();
    let (mut inquiry, first) = IdentityInquiry::new(MidiGroup::of(0), t0);
    let early = t0 + INQUIRY_TIMEOUT - Duration::from_millis(1);
    assert_eq!(inquiry.on_timeout(early), InquiryAction::Wait);
    let retry = inquiry.on_timeout(t0 + INQUIRY_TIMEOUT);
    assert_eq!(retry, first, "the retry is the same request");
    // The wait restarts from the retry.
    assert_eq!(
        inquiry.deadline(),
        Some(t0 + INQUIRY_TIMEOUT + INQUIRY_TIMEOUT)
    );
    // And a reply to the retry still answers.
    assert_eq!(
        inquiry.on_reply(identity()),
        InquiryAction::Answered(identity())
    );
}

#[test]
fn a_device_that_never_answers_costs_two_sends_and_then_nothing_hangs() {
    let t0 = Instant::now();
    let (mut inquiry, _) = IdentityInquiry::new(MidiGroup::of(0), t0);
    let mut sends = 1;
    let mut now = t0;
    let mut ended = false;
    for _ in 0..10 {
        now += INQUIRY_TIMEOUT;
        match inquiry.on_timeout(now) {
            InquiryAction::Send(_) => sends += 1,
            InquiryAction::GaveUp => {
                ended = true;
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(ended, "the inquiry gives up");
    assert_eq!(sends, INQUIRY_ATTEMPTS);
    assert!(inquiry.is_done());
    assert_eq!(inquiry.deadline(), None);
    // A reply after giving up is ignored, and nothing more is sent.
    assert_eq!(inquiry.on_reply(identity()), InquiryAction::Wait);
    assert_eq!(
        inquiry.on_timeout(now + INQUIRY_TIMEOUT),
        InquiryAction::Wait
    );
}
