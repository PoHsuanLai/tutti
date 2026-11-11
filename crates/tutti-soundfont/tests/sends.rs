//! The unit's five outputs: the dry pair, the chorus send pair and the reverb
//! send. A send is a level, so every figure here is a product the definition
//! gives: the voice's send (SF2 2.04 section 8.1.2, generators 15 and 16, in
//! 0.1% units, summed with the default modulators of CC 93 and CC 91, sections
//! 8.4.9 and 8.4.8, and limited to 0..1) times the dry gains it follows. Nothing is read from another
//! render.

mod support;

use support::font::{Inst, Preset, font};
use support::rig::Rig;
use tutti_graph::{Instrument, Node, SilenceMask, Status};
use tutti_soundfont::SoundFontUnit;
use tutti_ump::CCNumber;

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
    rig.cc(0, CCNumber::REVERB_SEND, cc91);
    rig.cc(0, CCNumber::CHORUS_SEND, cc93);
    rig.note_on(0, 69, 100);
    rig.render_all(8192);
    rig.render_all(512)
}

/// `a` and `b` equal to within what a float product of two gains carries.
fn close(a: &[f32], b: &[f32], what: &str) {
    assert_eq!(a.len(), b.len());
    let peak = a.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.005, "{what}: the plane sounds ({peak})");
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

/// CC 91 and CC 93 are the default modulators of SF2 2.04 sections 8.4.8 and
/// 8.4.9: a linear positive unipolar source (the controller over 128, so 127
/// reads 127/128 and 64 reads one half) with an amount of 200 tenths of a
/// percent, added to the generator sum. With no generator, CC 91 at 127 sends
/// 0.2 * 127/128 of the voice's level to the reverb, and CC 93 at 64 sends 0.2 *
/// 64/128 = 0.1 to the chorus.
///
/// Mutations (run): the divisor 128 -> 127 in `Channel::reverb_send` fails the
/// reverb; the amount 0.2 -> 1.0 (the whole controller, as before this lane)
/// fails both; swapping the two controllers fails both.
#[test]
fn the_controllers_are_the_spec_default_modulators() {
    let mut rig = rig(vec![]);
    let [dry_l, dry_r, chorus_l, chorus_r, reverb] = steady(&mut rig, 127, 64);
    close(
        &reverb,
        &scaled(&sum(&dry_l, &dry_r), 0.2 * 127.0 / 128.0),
        "reverb at CC 91 = 127",
    );
    close(&chorus_l, &scaled(&dry_l, 0.1), "chorus left at CC 93 = 64");
    close(
        &chorus_r,
        &scaled(&dry_r, 0.1),
        "chorus right at CC 93 = 64",
    );
}

/// The modulator's output adds to the generator sum, and the total is limited
/// to the generator's range, 0 to 1000 tenths (section 8.1.3): 250 and CC 91 at
/// 127 is 0.25 + 0.2 * 127/128; 900 and CC 91 at 127 is 1.098, limited to 1.
///
/// Mutation (run): removing the upper limit in `Voice::process` gives 1.098.
#[test]
fn a_send_sums_the_generator_and_the_modulator_and_stops_at_full_level() {
    let mut rig = rig(vec![(REVERB_SEND, 250)]);
    let [dry_l, dry_r, _, _, reverb] = steady(&mut rig, 127, 0);
    close(
        &reverb,
        &scaled(&sum(&dry_l, &dry_r), 0.25 + 0.2 * 127.0 / 128.0),
        "reverb: generator plus modulator",
    );
    let mut rig = self::rig(vec![(REVERB_SEND, 900)]);
    let [dry_l, dry_r, _, _, reverb] = steady(&mut rig, 127, 0);
    close(
        &reverb,
        &sum(&dry_l, &dry_r),
        "reverb limited to full level",
    );
}

/// A send is never below zero: "0% or less" sends nothing (section 8.1.2),
/// and the limit applies to the sum with the modulator (section 8.1.3). With
/// the generators at -100 (-10%) and CC 91 and 93 dropping from 127 to 0
/// mid-note, the sum goes from +0.098 to -0.1; the send ramps down and ends at
/// zero, never past it, so no send sample has the opposite sign to the dry
/// sample it follows. (At -400 the sum stays negative either side, and the
/// audible floor hides it.) Steady with the controllers at 0, the sends are
/// exact zeros.
///
/// Mutation (run): the lower limit in `Voice::process` replaced by -10 inverts
/// the end of the ramp and fails the sign check.
#[test]
fn a_negative_send_sends_nothing() {
    let mut steady_rig = rig(vec![(CHORUS_SEND, -400), (REVERB_SEND, -400)]);
    let planes = steady(&mut steady_rig, 0, 0);
    for (c, plane) in planes.iter().enumerate().skip(2) {
        assert!(plane.iter().all(|&s| s == 0.0), "plane {c}");
    }
    let dry = planes[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(dry > 0.01, "the dry pair sounds ({dry})");

    // Controllers at full level lift the sum to 0.6; dropping them mid-note
    // takes it to -0.4.
    let mut live = rig(vec![(CHORUS_SEND, -100), (REVERB_SEND, -100)]);
    steady(&mut live, 127, 127);
    live.cc(0, CCNumber::REVERB_SEND, 0);
    live.cc(0, CCNumber::CHORUS_SEND, 0);
    let [dry_l, dry_r, chorus_l, chorus_r, reverb] = live.render_all(512);
    let inverted = |send: &[f32], dry: &[f32]| send.iter().zip(dry).any(|(s, d)| s * d < -1e-9);
    assert!(!inverted(&chorus_l, &dry_l), "chorus left");
    assert!(!inverted(&chorus_r, &dry_r), "chorus right");
    assert!(!inverted(&reverb, &sum(&dry_l, &dry_r)), "reverb");
    assert!(
        reverb.iter().any(|&s| s != 0.0),
        "the ramp down was heard, so the check bites"
    );
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
    assert_eq!(rig.hand.shape().audio_out().count(), 5);
    let unit: &SoundFontUnit = &rig.hand;
    assert_eq!(unit.main_outputs(), [0, 1]);
}
