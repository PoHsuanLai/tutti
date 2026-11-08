//! The unit's five outputs: the dry pair, the chorus send pair and the reverb
//! send. A send is a level, so every figure here is a product the definition
//! gives: the voice's send (SF2 2.04 section 8.1.2, generators 15 and 16, in
//! 0.1% units, summed with the channel's CC 93 and CC 91 over 127 and clamped
//! to 0..1) times the dry gains it follows. Nothing is read from another
//! render.

mod support;

use support::font::{Inst, Preset, font};
use support::rig::Rig;
use tutti_graph::{Instrument, Node, SilenceMask, Status};
use tutti_soundfont::SoundFontUnit;

/// Generators 15 and 16 (section 8.1.2), 17 and the instrument zone's other
/// figures are the specification's numbers.
const CHORUS_SEND: u16 = 15;
const REVERB_SEND: u16 = 16;
const PAN: u16 = 17;

/// One preset over one instrument zone with `generators`, played at the
/// default channel (CC 91 and CC 93 are set by the test).
fn rig(generators: Vec<(u16, i16)>) -> Rig {
    let font = font(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![generators],
        }],
    );
    Rig::dry(&font)
}

/// Sounds a note, sets the sends, lets the attack settle, and returns 512 steady
/// frames of the five outputs.
fn steady(rig: &mut Rig, cc91: u8, cc93: u8) -> [Vec<f32>; 5] {
    rig.cc(0, 91, cc91);
    rig.cc(0, 93, cc93);
    rig.note_on(0, 69, 100);
    rig.render_all(8192);
    rig.render_all(512)
}

/// `a` and `b` equal to within what a float product of two gains carries.
fn close(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len());
    let peak = a.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.01, "{what}: the plane sounds ({peak})");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert!((x - y).abs() < 1e-5, "{what}[{i}]: {x} against {y}");
    }
}

fn scaled(plane: &[f32], by: f32) -> Vec<f32> {
    plane.iter().map(|s| s * by).collect()
}

fn sum(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

/// Generators 15 and 16 are the voice's send: 500 is half of full level to the
/// chorus (per channel, as the dry pair is), 250 a quarter to the reverb (mono,
/// the two channels' gains together). With the channel's CCs at 0 nothing else
/// adds. The pan generator (-250: three quarters left) makes left and right
/// differ, so a send that mixed the channels up fails.
///
/// Mutations (run): the chorus pair reading the right gain for both channels
/// fails the left; the reverb send taking only the left gain fails the sum;
/// generators 15 and 16 swapped fails both.
#[test]
fn the_generators_set_each_voices_sends() {
    let mut rig = rig(vec![(CHORUS_SEND, 500), (REVERB_SEND, 250), (PAN, -250)]);
    let [dry_l, dry_r, chorus_l, chorus_r, reverb] = steady(&mut rig, 0, 0);
    let (l, r) = (
        dry_l.iter().fold(0.0f32, |m, s| m.max(s.abs())),
        dry_r.iter().fold(0.0f32, |m, s| m.max(s.abs())),
    );
    assert!(l > r * 1.5, "panned left: {l} against {r}");
    close(&chorus_l, &scaled(&dry_l, 0.5), "chorus left");
    close(&chorus_r, &scaled(&dry_r, 0.5), "chorus right");
    close(&reverb, &scaled(&sum(&dry_l, &dry_r), 0.25), "reverb");
}

/// The channel's CC 93 and CC 91 are the send over 127, added to the generator.
/// With no generator, CC 91 at 127 sends everything to the reverb and CC 93 at
/// 64 sends 64/127 to the chorus.
///
/// Mutation (run): dividing by 128 instead of 127 in `Channel::reverb_send`
/// fails the reverb.
#[test]
fn the_controllers_set_the_channels_sends() {
    let mut rig = rig(vec![]);
    let [dry_l, dry_r, chorus_l, chorus_r, reverb] = steady(&mut rig, 127, 64);
    close(&reverb, &sum(&dry_l, &dry_r), "reverb at CC 91 = 127");
    let share = 64.0 / 127.0;
    close(
        &chorus_l,
        &scaled(&dry_l, share),
        "chorus left at CC 93 = 64",
    );
    close(
        &chorus_r,
        &scaled(&dry_r, share),
        "chorus right at CC 93 = 64",
    );
}

/// The controller adds to the generator, and the total stops at full level:
/// 800 tenths of a percent and CC 91 at 127 is 1.8 clamped to 1.
///
/// Mutation (run): removing the clamp in `Voice::process` gives 1.8.
#[test]
fn a_send_sums_the_generator_and_the_controller_and_stops_at_full_level() {
    let mut rig = rig(vec![(REVERB_SEND, 800)]);
    let [dry_l, dry_r, _, _, reverb] = steady(&mut rig, 127, 0);
    close(
        &reverb,
        &sum(&dry_l, &dry_r),
        "reverb clamped to full level",
    );
}

/// Both generators below zero send nothing: "0% or less" (section 8.1.2), the
/// sum is clamped at 0.
///
/// Mutation: none observable. Removing the lower clamp in `Voice::process`
/// (run: -10 for 0) leaves this passing, because a negative gain is under the
/// audible floor and `write_block` writes nothing for it; the clamp keeps the
/// figure a send is documented to be, not what is heard.
#[test]
fn a_negative_send_sends_nothing() {
    let mut rig = rig(vec![(CHORUS_SEND, -400), (REVERB_SEND, -400)]);
    let planes = steady(&mut rig, 0, 0);
    for (c, plane) in planes.iter().enumerate().skip(2) {
        assert!(plane.iter().all(|&s| s == 0.0), "plane {c}");
    }
    let dry = planes[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(dry > 0.01, "the dry pair sounds ({dry})");
}

/// A send nobody sends to is reported silent, and is exact zeros: with the
/// controllers at 0 and no generator, only the dry pair is heard, and the
/// status names the other three. With the reverb on only, the chorus pair is
/// the silent one. With everything sending, nothing is claimed.
///
/// Mutations (run): returning `Status::Modified` always fails it; naming the
/// planes that were heard rather than those that were not fails it.
#[test]
fn a_send_with_nothing_to_send_is_reported_silent() {
    let masked = |silent: &[usize]| {
        silent
            .iter()
            .fold(SilenceMask::NONE, |mask, &c| mask.with(c))
    };
    let status_of = |generators, cc91, cc93| {
        let mut rig = rig(generators);
        let planes = steady(&mut rig, cc91, cc93);
        (rig.hand.status(), planes)
    };

    let (status, planes) = status_of(vec![], 0, 0);
    let Status::Masked { silent, .. } = status else {
        panic!("masked, not {status:?}")
    };
    assert_eq!(silent, masked(&[2, 3, 4]));
    for (c, plane) in planes.iter().enumerate().skip(2) {
        assert!(plane.iter().all(|&s| s == 0.0), "plane {c} is zeros");
    }

    let (status, _) = status_of(vec![], 64, 0);
    let Status::Masked { silent, .. } = status else {
        panic!("masked, not {status:?}")
    };
    assert_eq!(silent, masked(&[2, 3]));

    let (status, _) = status_of(vec![], 64, 64);
    assert_eq!(status, Status::Modified);
}

/// A unit that has not played, or whose voices have all ended and whose last
/// chunk is read out, is idle; one with a note held or ringing is not.
///
/// Mutation (run): `Player::is_idle` always false fails the first and last
/// lines. (Always true is caught by the next test, not this one: a sounding
/// note has something written, which keeps the status from `Idle`.)
#[test]
fn the_unit_is_idle_once_its_voices_end() {
    let mut rig = rig(vec![]);
    rig.render_all(64);
    assert_eq!(rig.hand.status(), Status::Idle, "before any note");
    rig.note_on(0, 69, 100);
    rig.render_all(4096);
    assert!(
        !matches!(rig.hand.status(), Status::Idle | Status::Silent),
        "held: {:?}",
        rig.hand.status()
    );
    rig.note_off(0, 69);
    rig.render_all(44_100 * 20);
    assert_eq!(rig.hand.status(), Status::Idle, "after the release");
}

/// A voice in its delay stage (generator 33, `delayVolEnv`, section 8.1.2) is
/// silent and not finished: the unit says `Masked`, never `Idle`, which is the
/// status that lets the executor stop calling a node. After the delay the note
/// sounds.
///
/// Mutation (run): `Player::is_idle` always true parks the unit in the delay
/// and fails the first assertion.
#[test]
fn a_voice_in_its_delay_is_silent_but_the_unit_is_not_idle() {
    // 1200 timecents is 2 s.
    let mut rig = rig(vec![(33, 1200)]);
    rig.note_on(0, 69, 100);
    let planes = rig.render_all(4096);
    assert!(
        matches!(rig.hand.status(), Status::Masked { .. }),
        "{:?}",
        rig.hand.status()
    );
    assert!(planes.iter().flatten().all(|&s| s == 0.0), "it is silent");
    let planes = rig.render_all(2 * 44_100);
    assert!(
        planes[0].iter().any(|&s| s.abs() > 0.01),
        "and then it sounds"
    );
}

/// The unit has five outputs, and the main pair the host plays is the dry one.
///
/// Mutation (run): `main_outputs` returning `[2, 3]` fails the second line;
/// four outputs fails the first.
#[test]
fn the_dry_pair_is_the_main_pair_of_five_outputs() {
    let rig = rig(vec![]);
    assert_eq!(rig.hand.shape().audio_out.count(), 5);
    let unit: &SoundFontUnit = &rig.hand;
    assert_eq!(unit.main_outputs(), [0, 1]);
}
