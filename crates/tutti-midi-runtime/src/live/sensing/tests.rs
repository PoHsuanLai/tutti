use super::*;

const G0: MidiGroup = MidiGroup::FIRST;
const G1: MidiGroup = MidiGroup::of(1);

/// 48 kHz: the limit is 330 ms, 15,840 frames.
fn watch() -> ActiveSensing {
    ActiveSensing::at_rate(SampleRate::SR_48K)
}

const LIMIT: u64 = 15_840;

/// **A receiver that never sees an Active Sensing never judges a cable gone**
/// (p. 32: "If a device never receives Active Sensing it should operate
/// normally"), however long it is silent.
#[test]
fn a_cable_that_never_sent_active_sensing_is_never_judged() {
    let mut w = watch();
    w.heard(G0, Frame::new(0), false);
    assert_eq!(w.next_lapse(Frame::new(10 * LIMIT)), None);
    assert!(!w.is_watched(G0));
}

/// **Once an FE is seen, a message is expected within the limit, and over it
/// the cable lapses** (p. 32: "over 330 ms"): the lapse is at the last
/// message plus the limit, not before and not at it, reported once, and the
/// Group is unwatched after it.
#[test]
fn a_watched_cable_lapses_over_the_limit_once() {
    let mut w = watch();
    w.heard(G0, Frame::new(1_000), true);
    assert!(w.is_watched(G0));
    assert_eq!(
        w.next_lapse(Frame::new(1_000 + LIMIT)),
        None,
        "at the limit"
    );
    assert_eq!(
        w.next_lapse(Frame::new(1_000 + LIMIT + 1)),
        Some((G0, Frame::new(1_000 + LIMIT)))
    );
    assert!(!w.is_watched(G0), "the flag is reset");
    assert_eq!(w.next_lapse(Frame::new(100 * LIMIT)), None, "once");
}

/// **Any message restarts the timer** (the flowchart's "Receive Data: Clear
/// Timer", whatever the byte): a note just before the limit moves the lapse a
/// limit on.
#[test]
fn any_message_clears_the_timer() {
    let mut w = watch();
    w.heard(G0, Frame::new(0), true);
    w.heard(G0, Frame::new(LIMIT - 10), false);
    assert_eq!(w.next_lapse(Frame::new(LIMIT + 1_000)), None);
    assert_eq!(
        w.next_lapse(Frame::new(2 * LIMIT)),
        Some((G0, Frame::new(LIMIT - 10 + LIMIT)))
    );
}

/// **Each Group is a cable of its own**: Group 2 is watched and goes quiet
/// while Group 1 keeps sending; only Group 2 lapses. Two lapses come out in
/// the order they ran out.
#[test]
fn each_group_is_judged_alone() {
    let mut w = watch();
    w.heard(G0, Frame::new(0), true);
    w.heard(G1, Frame::new(5_000), true);
    w.heard(G0, Frame::new(LIMIT), false);
    assert_eq!(
        w.next_lapse(Frame::new(5_000 + LIMIT + 1)),
        Some((G1, Frame::new(5_000 + LIMIT)))
    );
    assert_eq!(w.next_lapse(Frame::new(5_000 + LIMIT + 1)), None);
    assert!(w.is_watched(G0));
    w.heard(G1, Frame::new(1_000_000), true);
    assert_eq!(
        w.next_lapse(Frame::new(1_000_000 + 3 * LIMIT)),
        Some((G0, Frame::new(2 * LIMIT))),
        "the earlier deadline first"
    );
    assert_eq!(
        w.next_lapse(Frame::new(1_000_000 + 3 * LIMIT)),
        Some((G1, Frame::new(1_000_000 + LIMIT)))
    );
}

/// The limit is 330 ms of the rate it is built at.
#[test]
fn the_limit_is_330_ms_at_the_rate() {
    assert_eq!(watch().limit(), Samples(15_840));
    assert_eq!(
        ActiveSensing::at_rate(SampleRate::SR_44K1).limit(),
        Samples(14_553)
    );
}
