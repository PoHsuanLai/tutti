//! What the SoundFont 2.04 specification requires of the player, as tests. The
//! section numbers are the spec's (`sfspec24`); the wording is ours. Where the
//! spec is silent the behaviour is a tutti rule, named in `player.rs` and
//! tested in `tutti-voicing` (a key struck again, who gives way, the pedals'
//! numbers).
//!
//! The pitch is read off the sine of the built fonts (441 Hz at key 69, one
//! voice a zone); the level off the summed magnitude of a render. Mutations
//! are written beside each test, as run.

mod support;

use support::font::{Inst, Preset, font, generator, one_preset, range};
use support::rig::{Rig, timgm6mb};
use tutti_graph::Node;
use tutti_types::Tail;

/// A violin: a held key sustains, a released one rings out in about a second.
fn violin() -> Rig {
    let mut rig = Rig::dry(&timgm6mb());
    rig.program(0, 40);
    rig
}

/// Long enough for any release of the violin to have ended.
const RING_OUT: usize = 44_100 * 20;

// ---- 9.2 pedals and panics ----------------------------------------------------

/// 9.2, CC 64: active from 64 up. A key-off while it is active is stored, not
/// run, and runs when it goes inactive. (CC 64 at 63 is inactive.)
///
/// Mutation (run): `Controller::pedal` (`tutti-voicing`) comparing `> 64` fails
/// the first line of the table; `Holds::key_up` ignoring the pedal fails the
/// held row; `Holds::sustain_up` not releasing fails the last assertion.
#[test]
fn sustain_stores_key_offs_until_it_goes_inactive() {
    for (pedal, held) in [(63, false), (64, true)] {
        let mut rig = violin();
        rig.cc(0, 64, pedal);
        rig.note_on(0, 69, 100);
        rig.energy(4096);
        rig.note_off(0, 69);
        rig.energy(RING_OUT);
        let after = rig.energy(512);
        assert_eq!(after > 1.0, held, "pedal {pedal}: {after}");
        if held {
            rig.cc(0, 64, 0);
            rig.energy(RING_OUT);
            assert_eq!(rig.energy(512), 0.0, "and the stored key-off runs at 0");
        }
    }
}

/// 9.2, CC 64: the pedal holds notes in the key-on state. A note already
/// released when the pedal goes down is not in it.
///
/// Mutation: none can be planted in the core (a released note is outside the
/// set a pedal acts on). What it pins is the removal of the old per-block poll
/// of the pedal, which held a released voice while the pedal was down; putting
/// the poll back in `Voice::release_if_necessary` fails it.
#[test]
fn sustain_down_after_the_key_off_holds_nothing() {
    let mut rig = violin();
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    rig.note_off(0, 69);
    rig.energy(64);
    rig.cc(0, 64, 127);
    rig.energy(RING_OUT);
    assert_eq!(rig.energy(512), 0.0);
}

/// 9.2 lists CC 66 as "soft" and CC 67 as "sostenuto"; MIDI 1.0 numbers them
/// the other way (66 sostenuto, 67 soft), and a keyboard sends MIDI's. The
/// listing is an erratum in the spec's text, which this player does not copy (the
/// owner decided so on 2026-11-08):
/// CC 66 is the sostenuto here, and CC 67 holds nothing. The behaviour is the
/// spec's: sostenuto holds the notes in key-on when it goes active; a note
/// struck after it behaves normally; and what it holds stays held through the
/// sustain pedal going on and off.
///
/// Mutation (run): `CCNumber::SOSTENUTO` 66 -> 67 in `Controller::effect` fails
/// the first assertion; `Holds::sostenuto_down` not marking fails the second;
/// `Holds::sustain_up` releasing a sostenuto-held note fails the third (the
/// sustain pedal is down when the key goes up, so it holds the note too).
#[test]
fn sostenuto_holds_what_sounded_when_it_went_active() {
    // A note struck after the pedal went down behaves normally.
    let mut after = violin();
    after.cc(0, 66, 127);
    after.note_on(0, 69, 100);
    after.energy(4096);
    after.note_off(0, 69);
    after.energy(RING_OUT);
    assert_eq!(after.energy(512), 0.0, "a later note is released normally");

    // A note sounding when it went down is held, through the sustain pedal.
    let mut held = violin();
    held.note_on(0, 69, 100);
    held.energy(4096);
    held.cc(0, 66, 127);
    held.cc(0, 64, 127);
    held.note_off(0, 69);
    held.energy(RING_OUT);
    assert!(held.energy(512) > 1.0, "the sostenuto holds the note");
    held.cc(0, 64, 0);
    held.energy(RING_OUT);
    assert!(
        held.energy(512) > 1.0,
        "and the sustain pedal going on and off does not let it go"
    );
    held.cc(0, 66, 0);
    held.energy(RING_OUT);
    assert_eq!(held.energy(512), 0.0, "until the sostenuto goes inactive");

    // CC 67 is MIDI's soft pedal: it holds nothing.
    let mut soft = violin();
    soft.note_on(0, 69, 100);
    soft.energy(4096);
    soft.cc(0, 67, 127);
    soft.note_off(0, 69);
    soft.energy(RING_OUT);
    assert_eq!(soft.energy(512), 0.0, "CC 67 is not the sostenuto");
}

/// 9.2, CC 120: every note in key-on skips its release and is shut off,
/// whatever the sustain or sostenuto positions: silent in the next block, with
/// a key still down, a pedal down, or a sostenuto holding.
///
/// Mutation (run): `Effect::Stop(Stop::Cut)` turned into `Stop::Release` in
/// `Controller::effect` leaves the pedal-held note ringing and fails it.
#[test]
fn all_sound_off_shuts_off_even_pedal_held_notes() {
    let mut rig = violin();
    rig.cc(0, 64, 127);
    rig.note_on(0, 69, 100);
    rig.note_on(0, 72, 100);
    rig.energy(4096);
    rig.note_off(0, 72);
    rig.cc(0, 66, 127);
    rig.energy(4096);
    assert!(rig.energy(512) > 1.0, "both held notes sound");
    rig.cc(0, 120, 0);
    assert_eq!(rig.energy(512), 0.0, "shut off at once, not released");
}

/// 9.2, CC 123: every note in key-on enters its release at once (a tail, not a
/// cut), pending its status under the sustain or sostenuto pedal.
///
/// Mutation (run): `Effect::Stop(Stop::Release)` made `Stop::Cut` fails the
/// tail assertion; `Holds::key_up` ignoring the pedal in the `Stop::Release`
/// arm of `Voicing::effect` fails the pedal one.
#[test]
fn all_notes_off_releases_unless_a_pedal_holds() {
    let mut rig = violin();
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    rig.cc(0, 123, 0);
    assert!(rig.energy(64) > 0.0, "it enters its release, it is not cut");
    rig.energy(RING_OUT);
    assert_eq!(rig.energy(512), 0.0, "and the release ends");

    let mut pedal = violin();
    pedal.cc(0, 64, 127);
    pedal.note_on(0, 69, 100);
    pedal.energy(4096);
    pedal.cc(0, 123, 0);
    pedal.energy(RING_OUT);
    assert!(pedal.energy(512) > 1.0, "the sustain pedal holds it");
    pedal.cc(0, 64, 0);
    pedal.energy(RING_OUT);
    assert_eq!(pedal.energy(512), 0.0, "until it lifts");

    let mut sostenuto = violin();
    sostenuto.note_on(0, 69, 100);
    sostenuto.energy(4096);
    sostenuto.cc(0, 66, 127);
    sostenuto.cc(0, 123, 0);
    sostenuto.energy(RING_OUT);
    assert!(sostenuto.energy(512) > 1.0, "the sostenuto holds it");
}

/// 9.2, CC 121: the continuous controllers go to their defaults, and the notes
/// sounding are not stopped by it. (Which defaults is MIDI's, RP-015.)
///
/// Mutation (run): the `0x79` arm of `Player::control` removed leaves the
/// expression at 0 and the note silent.
#[test]
fn reset_all_controllers_restores_the_defaults_and_stops_nothing() {
    let mut rig = violin();
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    let level = rig.energy(2048);
    rig.cc(0, 11, 0);
    rig.energy(64);
    assert_eq!(rig.energy(2048), 0.0, "expression 0 silences it");
    rig.cc(0, 121, 0);
    let back = rig.energy(2048);
    assert!(
        (back / level - 1.0).abs() < 0.2,
        "the expression is back: {back} against {level}"
    );
}

// ---- 8.1.2 generators 43 and 44, 9.4 ranges ---------------------------------

/// A level to compare against: the sine of one zone at full velocity.
fn level(font: &std::sync::Arc<tutti_sf2::SoundFont>, key: u8, velocity: u8) -> f32 {
    let mut rig = Rig::dry(font);
    rig.note_on(0, key, velocity);
    rig.energy(4096)
}

/// 8.1.2, generator 43 (keyRange): an instrument zone answers its keys, ends
/// included, and no others.
///
/// Mutation (run): `InstrumentRegion::contains` testing `<` for the upper end
/// fails key 72; ignoring the key range fails 59.
#[test]
fn a_zone_answers_the_keys_of_its_range() {
    let font = one_preset(vec![vec![(generator::KEY_RANGE, range(60, 72))]]);
    for (key, sounds) in [(59, false), (60, true), (72, true), (73, false)] {
        assert_eq!(level(&font, key, 100) > 1.0, sounds, "key {key}");
    }
}

/// 8.1.2, generator 44 (velRange): an instrument zone answers its velocities,
/// ends included, and no others.
///
/// Mutation (run): ignoring the velocity range fails velocity 29.
#[test]
fn a_zone_answers_the_velocities_of_its_range() {
    let font = one_preset(vec![vec![(generator::VEL_RANGE, range(30, 90))]]);
    for (velocity, sounds) in [(29, false), (30, true), (90, true), (91, false)] {
        assert_eq!(
            level(&font, 60, velocity) > 0.1,
            sounds,
            "velocity {velocity}"
        );
    }
}

/// 9.4: a range generator at the preset level is an intersection filter on the
/// instrument's range, not an addition to it. The instrument zone answers
/// 60 to 72, the preset zone 66 to 80: the note is heard from 66 to 72.
///
/// Mutation (run): `PresetRegion::contains` ignoring the preset's key range
/// fails key 62; testing only the preset's range fails key 75.
#[test]
fn a_preset_range_intersects_the_instruments() {
    let font = font(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![(generator::KEY_RANGE, range(66, 80))], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![vec![(generator::KEY_RANGE, range(60, 72))]],
        }],
    );
    for (key, sounds) in [
        (62, false),
        (65, false),
        (66, true),
        (72, true),
        (75, false),
    ] {
        assert_eq!(level(&font, key, 100) > 1.0, sounds, "key {key}");
    }
}

/// 8.1.2 and 9.4: every pair of a preset zone and an instrument zone whose
/// ranges hold the note plays (this is tutti's reading of "voice"). Two zones
/// on one key sound together, at twice the level of one; the executor plays
/// the zone each pair names: a loud zone and a silent one on different keys
/// are told apart.
///
/// Mutation (run): the patch stopping at its first match fails the first
/// assertion; `Voices::execute` taking the zone from the tag's high half for
/// both fails the second.
#[test]
fn each_matching_pair_is_a_voice_and_plays_its_own_zone() {
    let one = one_preset(vec![vec![]]);
    let two = one_preset(vec![vec![], vec![]]);
    let ratio = level(&two, 60, 100) / level(&one, 60, 100);
    assert!(
        (ratio - 2.0).abs() < 0.05,
        "two zones, twice the level: {ratio}"
    );

    let split = one_preset(vec![
        vec![
            (generator::KEY_RANGE, range(0, 59)),
            (generator::INITIAL_ATTENUATION, 1440),
        ],
        vec![(generator::KEY_RANGE, range(60, 127))],
    ]);
    assert!(level(&split, 60, 100) > 1.0, "key 60 plays the loud zone");
    assert!(level(&split, 59, 100) < 0.01, "key 59 plays the silent one");
}

// ---- 8.1.2 generator 57: the exclusive class --------------------------------

/// A kit: key 60 a loud sine of class 1 whose own release is 5 s, key 62 a
/// silent one of class 1, key 64 a silent one of class 0, key 65 a loud sine
/// of class 2, and keys 66 and 67 a loud sine of class 0. Presets 0 and 1 (bank 0) both play it.
fn kit() -> std::sync::Arc<tutti_sf2::SoundFont> {
    // Absolute timecents of 5 s: 1200 * log2(5).
    const FIVE_SECONDS: i16 = 2786;
    let silent = |key: u8, class: i16| {
        vec![
            (generator::KEY_RANGE, range(key, key)),
            (generator::INITIAL_ATTENUATION, 1440),
            (generator::EXCLUSIVE_CLASS, class),
        ]
    };
    font(
        &[
            Preset {
                name: "kit0",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            },
            Preset {
                name: "kit1",
                bank: 0,
                program: 1,
                zones: vec![(vec![], 0)],
            },
        ],
        &[Inst {
            name: "kit",
            zones: vec![
                vec![
                    (generator::KEY_RANGE, range(60, 60)),
                    (generator::RELEASE_VOL_ENV, FIVE_SECONDS),
                    (generator::EXCLUSIVE_CLASS, 1),
                ],
                silent(62, 1),
                silent(64, 0),
                vec![
                    (generator::KEY_RANGE, range(65, 65)),
                    (generator::EXCLUSIVE_CLASS, 2),
                ],
                vec![(generator::KEY_RANGE, range(66, 67))],
            ],
        }],
    )
}

fn kit_rig() -> Rig {
    let mut rig = Rig::dry(&kit());
    rig.note_on(0, 60, 100);
    rig.energy(4096);
    rig
}

/// 8.1.2, generator 57: a note with an exclusive class ends any other sounding
/// note of that class. The key is still down, and the zone's own release is 5
/// s; the note is gone within a tenth of a second. A note of class 0 ("no
/// exclusive class; no special action") and one of another class leave it be.
///
/// Mutation (run): `Rules::exclusive_groups` false in `player::rules` fails the
/// first assertion; the `group == 0` guard in `Voicing::damp_exclusive` removed
/// makes class 0 end its own and fails the last.
#[test]
fn a_note_of_an_exclusive_class_ends_the_others_of_it() {
    let mut same = kit_rig();
    same.note_on(0, 62, 100);
    same.energy(4410);
    assert!(
        same.energy(512) < 0.01,
        "gone within 100 ms, with its key down"
    );

    for other in [64, 65] {
        let mut rig = kit_rig();
        rig.note_on(0, other, 100);
        rig.energy(4410);
        assert!(
            rig.energy(512) > 1.0,
            "key {other} is not of its class: it sounds on"
        );
    }

    // Class 0 is no class: two notes of it do not end each other.
    let mut none = Rig::dry(&kit());
    none.note_on(0, 66, 100);
    none.energy(4410);
    let alone = none.energy(512);
    none.note_on(0, 67, 100);
    none.energy(4410);
    assert!(none.energy(512) > alone * 1.5, "both sound");
}

/// 8.1.2, generator 57: the other notes are terminated "rapidly", not shut off:
/// the note still sounds in the first millisecond (a cut is silent in the same
/// block) and has fallen by a tenth of a second. The release is quick whatever
/// the zone's own is (5 s here).
///
/// Mutation (run): `Voices::execute` turning `Damp` into `Cut` fails the first
/// assertion; into `Release` (the zone's 5 s) fails the second.
#[test]
fn the_class_is_ended_quickly_not_cut_and_not_released_slowly() {
    let mut rig = kit_rig();
    let sounding = {
        let mut still = kit_rig();
        still.energy(64)
    };
    rig.note_on(0, 62, 100);
    let first = rig.energy(64);
    assert!(
        first > sounding * 0.1,
        "still sounding in the first block: {first} against {sounding}"
    );
    assert!(
        first < sounding,
        "and already falling: {first} against {sounding}"
    );
    rig.energy(4410);
    assert!(rig.energy(512) < 0.01, "a tenth of a second on, it is gone");
}

/// 8.1.2, generator 57: "the scope of the exclusive class is the entire
/// preset". The same class in another preset is another class; another channel
/// playing the same preset shares it.
///
/// Mutation (run): scoping by channel (the scope set to the channel) fails the
/// second assertion; no scope (0) fails the first.
#[test]
fn the_scope_of_an_exclusive_class_is_the_preset() {
    // Channel 1 plays preset 1: its class 1 is not preset 0's.
    let mut other_preset = kit_rig();
    other_preset.program(1, 1);
    other_preset.note_on(1, 62, 100);
    other_preset.energy(4410);
    assert!(other_preset.energy(512) > 1.0, "another preset leaves it");

    // Channel 2 plays preset 0 too: the same preset, so the same class.
    let mut same_preset = kit_rig();
    same_preset.note_on(2, 62, 100);
    same_preset.energy(4410);
    assert!(
        same_preset.energy(512) < 0.01,
        "the preset is shared across channels"
    );
}

/// 8.1.2, generator 57 with 9.4: the voices of one note do not end each other.
/// Two zones of class 1 on one key both sound.
///
/// Mutation (run): dropping the strike test in `Voicing::damp_exclusive` ends
/// the first zone when the second starts.
#[test]
fn the_zones_one_note_starts_do_not_end_each_other() {
    let class = |class: i16| {
        one_preset(vec![
            vec![(generator::EXCLUSIVE_CLASS, class)],
            vec![(generator::EXCLUSIVE_CLASS, class)],
        ])
    };
    let classed = level(&class(1), 60, 100);
    let free = level(&class(0), 60, 100);
    assert!(
        classed > 1.0 && (classed / free - 1.0).abs() < 0.05,
        "{classed} {free}"
    );
}

// ---- 8.4.1 and 9.1.7: what a note does ---------------------------------------

/// 8.4.1: a harder key is louder. (The scale is the player's.)
///
/// Mutation (run): ignoring the velocity in `Voice::start` fails it.
#[test]
fn a_harder_key_is_louder() {
    let font = one_preset(vec![vec![]]);
    assert!(level(&font, 60, 120) > level(&font, 60, 60) * 1.5);
}

/// 9.1.7: a key-off makes the envelope fall to zero in its release phase: the
/// note ends, after a tail. (A pedal-less key-off.)
///
/// Mutation (run): `Voices::execute` ignoring `Release` fails it.
#[test]
fn a_key_off_ends_the_note_after_its_release() {
    let mut rig = violin();
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    rig.note_off(0, 69);
    assert!(rig.energy(64) > 0.0, "it rings on at first");
    rig.energy(RING_OUT);
    assert_eq!(rig.energy(512), 0.0);
}

// ---- the node: reset and tail --------------------------------------------------

/// The node's tail is the longest release any voice can have (the zone's 5 s
/// here, a generator-38 figure of 2786 timecents, 9.3: a time ratio 2^(t/1200)
/// from one second), and with the effects on, the effects ringing out too.
///
/// Mutation (run): the tail left at the default fails the first; leaving the
/// effects out of the second fails it.
#[test]
fn the_unit_declares_the_longest_release_as_its_tail() {
    let font = kit();
    let release = |rig: &Rig| match rig.hand.shape().tail {
        Tail::Finite(n) => n.get(),
        other => panic!("not a finite tail: {other:?}"),
    };
    let dry = release(&Rig::dry(&font));
    let seconds = dry as f64 / 44_100.0;
    assert!(
        (seconds - 2f64.powf(2786.0 / 1200.0)).abs() < 0.01,
        "{seconds} s"
    );
    let wet = release(&Rig::with(
        &font,
        &tutti_soundfont::Settings::new(support::rig::RATE),
    ));
    assert!(
        wet > dry + 44_100,
        "the effects ring on after the voices: {wet} vs {dry}"
    );
}

/// The longest release is over every zone the font can start, not the first
/// preset's: a quiet corner of the font with a long release sets the tail.
///
/// Mutation (run): taking only the first preset's regions fails it.
#[test]
fn the_tail_covers_every_preset() {
    let long = font(
        &[
            Preset {
                name: "short",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            },
            Preset {
                name: "long",
                bank: 0,
                program: 1,
                zones: vec![(vec![], 1)],
            },
        ],
        &[
            Inst {
                name: "short",
                zones: vec![vec![]],
            },
            Inst {
                name: "long",
                zones: vec![vec![(generator::RELEASE_VOL_ENV, 2786)]],
            },
        ],
    );
    let rig = Rig::dry(&long);
    let Tail::Finite(n) = rig.hand.shape().tail else {
        panic!("finite")
    };
    assert!(n.get() > 4 * 44_100);
}

/// `Node::reset` returns the node to the state of a freshly prepared one:
/// every voice is cut (silent in the next block), the pedals and held keys are
/// forgotten, and what the host selected stays (the program here).
///
/// Mutation (run): `reset` releasing instead of cutting fails the first
/// assertion; leaving the core's pedals fails the second; resetting the
/// program fails the third.
#[test]
fn reset_cuts_the_voices_forgets_the_pedals_and_keeps_the_program() {
    let mut rig = violin();
    rig.cc(0, 64, 127);
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    rig.note_off(0, 69);
    rig.energy(64);
    Node::reset(&mut *rig.hand);
    assert_eq!(rig.energy(512), 0.0, "cut, not released");

    // The pedal is up again: a note played and let go ends.
    rig.note_on(0, 69, 100);
    rig.energy(4096);
    rig.note_off(0, 69);
    rig.energy(RING_OUT);
    assert_eq!(rig.energy(512), 0.0, "the pedal was forgotten");

    // The program is still the violin: it sustains (a piano would not).
    rig.note_on(0, 69, 100);
    rig.energy(44_100 * 3);
    assert!(rig.energy(512) > 1.0, "still the violin");
}
