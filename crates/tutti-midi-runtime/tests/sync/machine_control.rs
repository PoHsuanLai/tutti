//! `MachineControlFollower` and `MachineControlSender`: the commands that
//! control a transport, and the commands a transport's motion makes.
//!
//! The oracles are RP-013's: the device ID addressing of p.5, the groups of
//! p.39, the time code of pp.8 and 10 (a drop-frame minute skips two labels)
//! and the commands of pp.20 to 28.

use tutti_midi_protocol::{MtcRate, Recipient, SysexDeviceId};
use tutti_midi_runtime::{MachineControlEvent, MachineControlFollower, MachineControlSender};
use tutti_types::{Frame, SampleRate, Seconds};
use tutti_ump::{MidiGroup, Sysex7Packets};

const OURS: SysexDeviceId = SysexDeviceId::of(5);

fn feed(
    follower: &mut MachineControlFollower,
    device: u8,
    commands: &[u8],
) -> Vec<MachineControlEvent> {
    let mut payload = vec![0x7F, device, 0x06];
    payload.extend_from_slice(commands);
    let mut heard = Vec::new();
    for ump in Sysex7Packets::from_payload(MidiGroup::FIRST, &payload).expect("a 7-bit payload") {
        follower.step(ump, &mut |e| heard.push(e));
    }
    heard
}

fn seconds(event: &MachineControlEvent) -> f64 {
    match event {
        MachineControlEvent::Locate(s) => s.get(),
        other => panic!("{other:?}"),
    }
}

/// **Addressing (RP-013 p.5)**: own ID and all-call are heard, another ID is not.
#[test]
fn own_id_and_all_call_are_heard_and_another_is_not() {
    let mut f = MachineControlFollower::new(OURS);
    assert_eq!(feed(&mut f, 5, &[0x02]), [MachineControlEvent::Play]);
    assert_eq!(feed(&mut f, 0x7F, &[0x01]), [MachineControlEvent::Stop]);
    assert!(feed(&mut f, 6, &[0x02]).is_empty());
}

/// **Groups (RP-013 p.39)**: ASSIGN joins only a listed device, DIS-ASSIGN of one
/// group leaves the others, MMC RESET (p.25) leaves all.
#[test]
fn groups_are_joined_left_and_reset() {
    let mut f = MachineControlFollower::new(OURS);
    feed(&mut f, 0x7F, &[0x52, 0x03, 0x00, 0x21, 0x06]);
    assert!(!f.in_group(SysexDeviceId::of(0x21)), "not in the list");
    feed(&mut f, 0x7F, &[0x52, 0x03, 0x00, 0x21, 0x05]);
    feed(&mut f, 0x7F, &[0x52, 0x03, 0x00, 0x22, 0x05]);
    assert!(f.in_group(SysexDeviceId::of(0x21)) && f.in_group(SysexDeviceId::of(0x22)));
    feed(&mut f, 0x7F, &[0x52, 0x03, 0x01, 0x21, 0x05]);
    assert!(!f.in_group(SysexDeviceId::of(0x21)) && f.in_group(SysexDeviceId::of(0x22)));
    assert_eq!(feed(&mut f, 0x22, &[0x02]), [MachineControlEvent::Play]);
    feed(&mut f, 0x7F, &[0x0D]);
    assert!(!f.in_group(SysexDeviceId::of(0x22)));
}

/// **A time code is read from 00:00:00:00 (RP-013 pp.8 and 10)**:
/// `01:02:08:20` at 30 frames is (3600 + 120 + 8) x 30 + 20 frames; drop-frame
/// `00:01:00:02` is frame 1800.
#[test]
fn a_locate_time_is_counted_in_frames() {
    let mut f = MachineControlFollower::new(OURS);
    let heard = feed(&mut f, 5, &[0x44, 0x06, 0x01, 0x61, 0x02, 0x08, 0x14, 0x00]);
    assert!((seconds(&heard[0]) - 111_860.0 / 30.0).abs() < 1e-3);
    let heard = feed(&mut f, 5, &[0x44, 0x06, 0x01, 0x40, 0x01, 0x00, 0x02, 0x00]);
    assert!((seconds(&heard[0]) - 1800.0 * 1001.0 / 30000.0).abs() < 1e-3);
}

/// **MOVE between registers copies the time (RP-013 p.32)**; an empty register
/// cannot be located to (the spec's "blank time code").
#[test]
fn a_register_holds_what_was_written_or_moved() {
    let mut f = MachineControlFollower::new(OURS);
    assert!(matches!(
        feed(&mut f, 5, &[0x44, 0x02, 0x00, 0x09])[..],
        [MachineControlEvent::Ignored(_)]
    ));
    feed(&mut f, 5, &[0x40, 0x06, 0x08, 0x60, 0x00, 0x03, 0x00, 0x00]);
    feed(&mut f, 5, &[0x4C, 0x02, 0x09, 0x08]);
    let heard = feed(&mut f, 5, &[0x44, 0x02, 0x00, 0x09]);
    assert!((seconds(&heard[0]) - 3.0).abs() < 1e-6);
}

/// **A speed other than play speed is not play**: VARIABLE PLAY at 1x plays; at
/// 2x it is ignored and said so.
#[test]
fn only_play_speed_is_play() {
    let mut f = MachineControlFollower::new(OURS);
    assert_eq!(
        feed(&mut f, 5, &[0x45, 0x03, 0x01, 0x00, 0x00]),
        [MachineControlEvent::Play]
    );
    assert!(matches!(
        feed(&mut f, 5, &[0x45, 0x03, 0x02, 0x00, 0x00])[..],
        [MachineControlEvent::Ignored(_)]
    ));
}

const RATE: SampleRate = SampleRate::SR_48K;

fn look(
    sender: &mut MachineControlSender,
    playing: bool,
    seconds: f64,
    frame: u64,
) -> Option<Vec<u8>> {
    sender.observe(playing, Seconds::new(seconds), Frame::new(frame), RATE)
}

/// **The sender's commands are the changes**: nothing at the first look; PLAY at
/// a start; nothing while rolling on at the pace of the frames; STOP at a stop.
#[test]
fn the_sender_sends_changes_and_nothing_else() {
    let mut s = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps30);
    assert_eq!(look(&mut s, false, 0.0, 0), None);
    assert_eq!(look(&mut s, false, 0.0, 480), None);
    assert_eq!(
        look(&mut s, true, 0.0, 960),
        Some(vec![0x7F, 0x7F, 0x06, 0x02])
    );
    // A second of frames later, a second further: rolling on.
    assert_eq!(look(&mut s, true, 1.0, 960 + 48_000), None);
    assert_eq!(
        look(&mut s, false, 1.5, 960 + 72_000),
        Some(vec![0x7F, 0x7F, 0x06, 0x01])
    );
}

/// **A position that is not where motion left it is a locate (RP-013 p.28)**:
/// stated as the time code of the position at the sender's rate with
/// subframes, with DEFERRED PLAY when rolling.
#[test]
fn a_jump_is_a_locate_with_the_positions_time_code() {
    let mut s = MachineControlSender::new(Recipient::Device(SysexDeviceId::of(2)), MtcRate::Fps25);
    look(&mut s, false, 0.0, 0);
    // 1.5 s is frame 37.5 at 25 frames: 00:00:01:12 and 50 hundredths; a stopped
    // transport is not deferred.
    assert_eq!(
        look(&mut s, false, 1.5, 480),
        Some(vec![
            0x7F, 0x02, 0x06, 0x44, 0x06, 0x01, 0x20, 0x00, 0x01, 0x0C, 0x32
        ])
    );
    look(&mut s, true, 1.5, 960);
    // Rolling, it jumps back.
    assert_eq!(
        look(&mut s, true, 0.0, 1440),
        Some(vec![
            0x7F, 0x02, 0x06, 0x44, 0x06, 0x01, 0x20, 0x00, 0x00, 0x00, 0x00, 0x03
        ])
    );
}

/// **Drop-frame labels (RP-013 p.10)**: 60.07 s at 30 drop is frame 1800 and a
/// third, `00:01:00:02`.
#[test]
fn a_drop_frame_locate_skips_the_dropped_labels() {
    let mut s = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps2997Df);
    look(&mut s, false, 0.0, 0);
    let sent = look(&mut s, false, 60.07, 480).expect("a locate");
    assert_eq!(&sent[3..9], [0x44, 0x06, 0x01, 0x40, 0x01, 0x00]);
    assert_eq!(sent[9], 0x02, "frame 2 of the minute");
}

/// **A stop that also moved sends STOP alone.**
#[test]
fn a_stop_that_moved_is_only_a_stop() {
    let mut s = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps30);
    look(&mut s, true, 5.0, 0);
    assert_eq!(
        look(&mut s, false, 0.0, 480),
        Some(vec![0x7F, 0x7F, 0x06, 0x01])
    );
}
