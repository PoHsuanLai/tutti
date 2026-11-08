//! The behaviour of the unit's controllers and notes over the reference
//! SoundFont, driven through MIDI 1.0 messages as a keyboard sends them.
//!
//! Ported from the tests of `tutti-rustysynth`'s `Synthesizer`, which this
//! unit replaced: the same properties, now through the unit's message face.
//! (Two properties went with the type they tested: the block size is no longer
//! a setting, and the polyphony is 1 to 4096 where it was 8 to 256.)

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use support::rig::{RATE, Rig, timgm6mb};

use tutti_sf2::SoundFont;
use tutti_soundfont::{Error, Settings, SoundFontUnit};
use tutti_types::SampleRate;

/// A unit with no effect lines, so a stopped voice leaves true silence.
fn dry() -> Rig {
    Rig::dry(&timgm6mb())
}

fn refused(settings: Settings) -> Error {
    SoundFontUnit::new(timgm6mb(), &settings).unwrap_err()
}

// Mutation (run): widening the lower bound of the rate range to 15_000 lets
// 15_999 Hz through and fails the first assertion.
#[test]
fn a_rate_outside_16_to_192_khz_is_refused_with_the_rate() {
    for hz in [15_999_u32, 192_001] {
        let rate = SampleRate::from(hz);
        let error = refused(Settings::new(rate));
        assert!(matches!(error, Error::SampleRateOutOfRange(r) if r == rate));
    }
    for hz in [16_000_u32, 192_000] {
        let settings = Settings::new(SampleRate::from(hz));
        assert!(SoundFontUnit::new(timgm6mb(), &settings).is_ok());
    }
}

// The polyphony is a count of voices that cannot be zero (`NonZeroUsize`), and
// one voice is enough: a second note takes the first's place, so two notes
// sound at the power of one, where a pool of two sounds both (two sines an
// octave apart add in power, twice the one).
//
// Mutation (run): `Player::new` reading the polyphony as 64 whatever the
// settings say lets both sound at once and fails the last assertion.
#[test]
fn a_pool_of_one_voice_gives_way_to_the_next_note() {
    let font = support::font::one_preset(vec![vec![]]);
    let level = |voices: usize| {
        let settings = Settings::new(RATE)
            .with_reverb_and_chorus(false)
            .with_polyphony(NonZeroUsize::new(voices).unwrap());
        let mut rig = Rig::with(&font, &settings);
        rig.note_on(0, 60, 100);
        rig.energy(4096);
        rig.note_on(0, 72, 100);
        rig.energy(8192);
        let (left, _) = rig.render(4096);
        left.iter().map(|s| s * s).sum::<f32>()
    };
    let one = level(1);
    let two = level(2);
    assert!(one > 1.0);
    assert!(
        two > one * 1.8,
        "two voices sound both: {two} against {one}"
    );
}

// A count of voices past `Settings::MAX_POLYPHONY` is refused with its value
// instead of panicking in the pool's allocation (`usize::MAX` overflows it);
// the maximum itself and one voice build.
//
// Mutation (run): removing the upper bound in `Player::new` panics with a
// capacity overflow on `usize::MAX` and fails the test.
#[test]
fn a_polyphony_past_the_maximum_is_refused_and_the_maximum_builds() {
    let at = |voices: usize| Settings::new(RATE).with_polyphony(NonZeroUsize::new(voices).unwrap());
    assert!(SoundFontUnit::new(timgm6mb(), &at(1)).is_ok());
    assert!(SoundFontUnit::new(timgm6mb(), &at(Settings::MAX_POLYPHONY)).is_ok());
    for too_many in [Settings::MAX_POLYPHONY + 1, usize::MAX] {
        assert!(
            matches!(refused(at(too_many)), Error::PolyphonyOutOfRange(n) if n == too_many),
            "{too_many}"
        );
    }
}

#[test]
fn the_error_names_the_value_it_refused() {
    let message = refused(Settings::new(SampleRate::from(8_000))).to_string();
    assert!(message.contains("8000"), "{message}");
}

#[test]
fn the_rate_is_whole_hertz() {
    let unit = SoundFontUnit::new(timgm6mb(), &Settings::new(SampleRate(44_100.4)));
    assert_eq!(unit.unwrap().sample_rate(), RATE);
}

// Mutation (run): swapping the `Cut` and `Release` effects of CC 120 and CC 123
// in `Controller::effect` (`tutti-voicing`) leaves the cut note ringing and
// fails the first assertion.
#[test]
fn cut_silences_at_once_and_release_rings_out() {
    let mut cut = dry();
    cut.note_on(0, 60, 100);
    assert!(energy(&mut cut, 512) > 1.0, "the note sounds");
    cut.cut_all();
    assert_eq!(energy(&mut cut, 512), 0.0);

    let mut release = dry();
    release.note_on(0, 60, 100);
    energy(&mut release, 512);
    for ch in 0..16 {
        release.cc(ch, 123, 0);
    }
    assert!(energy(&mut release, 512) > 0.0, "the release tail sounds");
}

// Mutation (run): matching the note's channel against its key in
// `Voicing::effect` cuts the wrong voices and fails one of the two assertions.
#[test]
fn a_channel_cut_leaves_the_other_channels_sounding() {
    let mut synthesizer = dry();
    synthesizer.note_on(0, 60, 100);
    synthesizer.note_on(1, 64, 100);
    energy(&mut synthesizer, 512);
    synthesizer.cc(0, 120, 0);
    assert!(
        energy(&mut synthesizer, 512) > 1.0,
        "channel 1 still sounds"
    );
    synthesizer.cc(1, 120, 0);
    assert_eq!(energy(&mut synthesizer, 512), 0.0);
}

// Mutation (run): `widen` (`tutti-midi-protocol`) keeping a velocity-0 Note On a
// Note On starts a second voice instead of releasing the first (the retrigger
// rule releases the earlier strike but the new one then sustains), and fails
// the last assertion.
#[test]
fn a_note_on_at_velocity_zero_is_a_note_off() {
    let mut synthesizer = dry();
    // The violin loops its sustain: only a release ends it.
    synthesizer.program(0, 40);
    synthesizer.note_on(0, 69, 100);
    energy(&mut synthesizer, 44_100 * 3);
    assert!(energy(&mut synthesizer, 512) > 1.0, "the note sustains");

    synthesizer.note_on(0, 69, 0);
    // Released, not cut: the tail is still there in the next block.
    assert!(energy(&mut synthesizer, 64) > 0.0);
    energy(&mut synthesizer, 44_100 * 20);
    assert_eq!(
        energy(&mut synthesizer, 512),
        0.0,
        "the released voice has ended"
    );
}

// Control Change dispatch, one test per arm of the unit's controller match,
// through the unit's message face. The unit is dry (no effect lines) unless a
// test is about the sends.

fn cc(rig: &mut Rig, controller: u8, value: u8) {
    rig.cc(0, controller, value);
}

fn energy(rig: &mut Rig, frames: usize) -> f32 {
    rig.energy(frames)
}

fn energy_lr(rig: &mut Rig, frames: usize) -> (f32, f32) {
    rig.energy_lr(frames)
}

/// A synthesizer playing a sustained violin note once `setup` has run.
fn violin(setup: impl FnOnce(&mut Rig)) -> Rig {
    let mut synthesizer = dry();
    synthesizer.program(0, 40);
    setup(&mut synthesizer);
    synthesizer.note_on(0, 69, 100);
    synthesizer
}

/// Zero crossings in one second of the violin note after a 1 s lead-in, with
/// the pitch bent fully up by `bend`. Sample playback scales the crossing
/// count with the playback rate, so the count is proportional to the pitch.
fn crossings(setup: impl FnOnce(&mut Rig), bend: u16) -> usize {
    let mut synthesizer = dry();
    synthesizer.program(0, 40);
    setup(&mut synthesizer);
    synthesizer.bend(0, bend);
    synthesizer.note_on(0, 69, 100);
    energy(&mut synthesizer, 44_100);
    let (left, _) = synthesizer.render(44_100);
    left.windows(2).filter(|w| w[0] * w[1] < 0.0).count()
}

const BEND_MAX: u16 = 16_383;

// Mutations (all run, each fails the test named for the arm): the arm's call
// replaced by `()` in `Player::control` for 0x00, 0x01, 0x21, 0x06, 0x26, 0x07,
// 0x27, 0x0A, 0x2A, 0x0B, 0x2B, 0x5B, 0x5D, 0x63, 0x62, 0x65, 0x64 and 0x79;
// CC 64 and CC 120 swapped with CC 123 in `Controller::effect`; the `_` arm
// changed to cut the channel. (These ran on the old `Synthesizer`'s match; on
// this one, the mutations that port are the arms of `Player::control` and of
// `Controller::effect`, rerun below in the PR.)

// Arm 0x07 (volume coarse) and 0x27 (fine).
#[test]
fn cc_7_and_39_set_the_channel_volume() {
    let mut loud = violin(|_| {});
    assert!(
        energy(&mut loud, 2048) > 1.0,
        "control: default volume sounds"
    );

    let mut silent = violin(|s| cc(s, 7, 0));
    assert_eq!(energy(&mut silent, 2048), 0.0, "coarse 0 is volume 0");

    // Half volume, then the fine byte adds 127/16383 of full scale on top.
    let level = |fine: u8| {
        let mut synthesizer = dry();
        cc(&mut synthesizer, 7, 64);
        cc(&mut synthesizer, 39, fine);
        synthesizer.note_on(0, 69, 100);
        energy(&mut synthesizer, 2048)
    };
    assert!(level(127) > level(0), "the fine byte raises the volume");
}

// Arm 0x0B (expression coarse) and 0x2B (fine).
#[test]
fn cc_11_and_43_set_the_expression() {
    let mut silent = violin(|s| cc(s, 11, 0));
    assert_eq!(energy(&mut silent, 2048), 0.0);

    let level = |fine: u8| {
        let mut synthesizer = dry();
        cc(&mut synthesizer, 11, 64);
        cc(&mut synthesizer, 43, fine);
        synthesizer.note_on(0, 69, 100);
        energy(&mut synthesizer, 2048)
    };
    assert!(level(127) > level(0), "the fine byte raises the expression");
}

// Arm 0x0A (pan coarse) and 0x2A (fine).
#[test]
fn cc_10_and_42_pan_the_channel() {
    let mut left = violin(|s| cc(s, 10, 0));
    let (l, r) = energy_lr(&mut left, 2048);
    assert!(
        l > 1.0 && r == 0.0,
        "hard left leaves the right silent: {l} {r}"
    );

    let mut right = violin(|s| cc(s, 10, 127));
    let (l, r) = energy_lr(&mut right, 2048);
    assert!(r > 1.0 && l < r * 0.05, "hard right: {l} {r}");

    let mut fine = violin(|s| {
        cc(s, 10, 0);
        cc(s, 42, 127);
    });
    let (_, r) = energy_lr(&mut fine, 2048);
    assert!(r > 0.0, "the fine byte moves the pan off the wall");
}

// Arm 0x40 (hold pedal): 64 and above holds, below does not.
#[test]
fn cc_64_holds_released_notes_until_the_pedal_lifts() {
    let ring = |pedal: u8| {
        let mut synthesizer = violin(|s| cc(s, 64, pedal));
        energy(&mut synthesizer, 44_100);
        synthesizer.note_off(0, 69);
        energy(&mut synthesizer, 44_100 * 20);
        (energy(&mut synthesizer, 512), synthesizer)
    };
    let (held, mut synthesizer) = ring(64);
    assert!(held > 1.0, "a pedal at 64 keeps the released note sounding");
    cc(&mut synthesizer, 64, 63);
    energy(&mut synthesizer, 44_100 * 20);
    assert_eq!(
        energy(&mut synthesizer, 512),
        0.0,
        "lifting the pedal ends it"
    );
    assert_eq!(ring(63).0, 0.0, "63 is not down");
}

// Arms 0x5B (reverb send) and 0x5D (chorus send): after every voice is cut,
// the effect line's tail is all that sounds.
#[test]
fn cc_91_and_93_send_to_the_reverb_and_the_chorus() {
    let tail = |controller: u8, value: u8| {
        let mut synthesizer = Rig::with(&timgm6mb(), &Settings::new(RATE));
        synthesizer.program(0, 40);
        cc(&mut synthesizer, 91, 0);
        cc(&mut synthesizer, 93, 0);
        cc(&mut synthesizer, controller, value);
        synthesizer.note_on(0, 69, 100);
        energy(&mut synthesizer, 8192);
        synthesizer.cut_all();
        energy(&mut synthesizer, 64);
        energy(&mut synthesizer, 4096)
    };
    for controller in [91, 93] {
        let none = tail(controller, 0);
        let full = tail(controller, 127);
        assert!(
            full > none * 2.0 + 1.0,
            "CC {controller}: {none} against {full}"
        );
    }
}

// Arms 0x65 and 0x64 (RPN select), 0x06 and 0x26 (data entry): RPN 0 is the
// pitch-bend range.
#[test]
fn cc_101_100_6_and_38_set_the_pitch_bend_range() {
    let select = |s: &mut Rig| {
        cc(s, 101, 0);
        cc(s, 100, 0);
    };
    let default = crossings(|_| {}, BEND_MAX) as f64;
    let wide = crossings(
        |s| {
            select(s);
            cc(s, 6, 12);
        },
        BEND_MAX,
    ) as f64;
    // Bent fully up: 12 semitones against the default 2.
    let ratio = wide / default;
    assert!((1.6..1.9).contains(&ratio), "2^(10/12) = 1.78, got {ratio}");

    let none = crossings(
        |s| {
            select(s);
            cc(s, 6, 0);
        },
        BEND_MAX,
    ) as f64;
    let cents = crossings(
        |s| {
            select(s);
            cc(s, 6, 0);
            cc(s, 38, 127);
        },
        BEND_MAX,
    ) as f64;
    // 0 semitones, then 1.27 from the fine byte: 2^(1.27/12) = 1.076.
    let ratio = cents / none;
    assert!((1.05..1.1).contains(&ratio), "got {ratio}");
}

// Arms 0x63 and 0x62 (NRPN select): data entry after an NRPN select is not an
// RPN, so it must not change the range.
#[test]
fn cc_99_and_98_make_data_entry_ignore_the_rpn() {
    let default = crossings(|_| {}, BEND_MAX) as f64;
    for nrpn in [99, 98] {
        let ignored = crossings(
            |s| {
                cc(s, 101, 0);
                cc(s, 100, 0);
                cc(s, nrpn, 0);
                cc(s, 6, 12);
            },
            BEND_MAX,
        ) as f64;
        assert!(
            (ignored / default - 1.0).abs() < 0.01,
            "NRPN {nrpn}: {ignored} against {default}"
        );
    }
}

/// A SoundFont built here, because the reference one has no bank a melodic
/// channel can reach but 0: preset 0 of bank 0 plays a looped sine, and preset
/// 0 of bank 1 plays the same sample 144 dB down. The loud one answers
/// velocities 0 to 63 only.
fn two_bank_soundfont() -> Arc<SoundFont> {
    use support::font::{Inst, Preset, font};
    use tutti_fixtures::sf2::{generator, range};
    font(
        &[
            Preset {
                name: "bank0",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            },
            Preset {
                name: "bank1",
                bank: 1,
                program: 0,
                zones: vec![(vec![], 1)],
            },
        ],
        &[
            Inst {
                name: "loud",
                zones: vec![vec![(generator::VEL_RANGE, range(0, 63))]],
            },
            Inst {
                name: "quiet",
                zones: vec![vec![(generator::INITIAL_ATTENUATION, 1440)]],
            },
        ],
    )
}

// Arm 0x00 (bank select). Mutation (run): the arm's `set_bank` -> `()` leaves
// bank 0 selected and fails the second assertion.
#[test]
fn cc_0_selects_the_bank() {
    let font = two_bank_soundfont();
    let level = |bank: Option<u8>| {
        let mut synthesizer = Rig::dry(&font);
        if let Some(bank) = bank {
            cc(&mut synthesizer, 0, bank);
        }
        synthesizer.note_on(0, 69, 40);
        energy(&mut synthesizer, 4096)
    };
    let loud = level(None);
    assert!(loud > 20.0, "bank 0 plays the sine: {loud}");
    assert!(level(Some(1)) < loud * 0.01, "bank 1 is 144 dB down");
}

// The region's velocity range, through the public synthesizer: the loud zone
// answers 0 to 63. Mutation (run): `InstrumentRegion::contains` ignoring the
// velocity (the fixture has no velocity ranges, so only this catches it) makes
// velocity 100 sound and fails the assertion.
#[test]
fn a_region_answers_only_the_velocities_of_its_range() {
    let font = two_bank_soundfont();
    let level = |velocity_value: u8| {
        let mut synthesizer = Rig::dry(&font);
        synthesizer.note_on(0, 69, velocity_value);
        energy(&mut synthesizer, 4096)
    };
    assert!(level(63) > 20.0, "the top of the range sounds");
    assert_eq!(level(64), 0.0, "one past it finds no region");
}

// Arms 0x01 (modulation coarse) and 0x21 (fine): vibrato depth, at most 50
// cents, which no zero-crossing count resolves. The strongest claim here is
// that the controller changes the render and a different value changes it
// differently.
#[test]
fn cc_1_and_33_change_the_vibrato() {
    let render = |setup: fn(&mut Rig)| {
        let mut synthesizer = violin(setup);
        synthesizer.render(8192).0
    };
    let none = render(|_| {});
    let coarse = render(|s| cc(s, 1, 127));
    let half = render(|s| cc(s, 1, 64));
    let fine = render(|s| {
        cc(s, 1, 64);
        cc(s, 33, 127);
    });
    assert_ne!(coarse, none, "coarse");
    assert_ne!(half, coarse, "the depth follows the value");
    assert_ne!(fine, half, "fine");
}

// Arm 0x78 (all sound off) against 0x7B (all notes off): the first cuts, the
// second releases into the envelope.
#[test]
fn cc_120_cuts_and_cc_123_releases() {
    let mut cut = violin(|_| {});
    energy(&mut cut, 4096);
    cc(&mut cut, 120, 0);
    assert_eq!(
        energy(&mut cut, 512),
        0.0,
        "all sound off is silent at once"
    );

    let mut released = violin(|_| {});
    energy(&mut released, 4096);
    cc(&mut released, 123, 0);
    assert!(energy(&mut released, 64) > 0.0, "all notes off rings on");
    energy(&mut released, 44_100 * 20);
    assert_eq!(energy(&mut released, 512), 0.0, "and then ends");
}

// Arm 0x79 (reset all controllers): the expression comes back to 127, and the
// pitch bend to none.
#[test]
fn cc_121_resets_the_expression() {
    let mut synthesizer = dry();
    cc(&mut synthesizer, 11, 0);
    cc(&mut synthesizer, 121, 0);
    synthesizer.note_on(0, 69, 100);
    assert!(energy(&mut synthesizer, 2048) > 1.0);
}

// The `_` arm: a controller the synthesizer does not follow changes nothing.
#[test]
fn an_unfollowed_controller_leaves_the_note_sounding() {
    let mut synthesizer = violin(|_| {});
    energy(&mut synthesizer, 4096);
    cc(&mut synthesizer, 20, 127);
    cc(&mut synthesizer, 119, 127);
    assert!(energy(&mut synthesizer, 2048) > 1.0);
}

// SF2 2.04 section 7.2: a preset whose bank or program is not a MIDI value
// "cannot be played but should be maintained". It is never the one a channel
// plays, not even as the default a channel falls back on when its own bank and
// program name nothing: the first *playable* preset is.
//
// The font: preset 0 has bank 300 (unplayable) and a loud zone; preset 1 is
// bank 0, program 3 and silent (144 dB down). A channel on bank 0, program 0
// names nothing, so it falls back, and must land on the silent preset 1.
//
// Mutation (run): the default left at preset 0 in `Player::new` plays the loud
// unplayable one and fails the first assertion.
#[test]
fn an_unplayable_preset_is_never_chosen() {
    use support::font::{Inst, Preset, font};
    use tutti_fixtures::sf2::generator;
    let preset = |name, bank, program, instrument| Preset {
        name,
        bank,
        program,
        zones: vec![(vec![], instrument)],
    };
    let font = font(
        &[preset("unplayable", 300, 0, 0), preset("playable", 0, 3, 1)],
        &[
            Inst {
                name: "loud",
                zones: vec![vec![]],
            },
            Inst {
                name: "silent",
                zones: vec![vec![(generator::INITIAL_ATTENUATION, 1440)]],
            },
        ],
    );
    let mut rig = Rig::dry(&font);
    rig.note_on(0, 69, 100);
    assert!(
        rig.energy(4096) < 0.01,
        "the unplayable preset must not sound"
    );
}

// A font none of whose presets is playable is refused, not built into a unit
// that is silent or plays what the file says cannot be played.
//
// Mutation (run): the `NoPlayablePreset` return removed builds the unit and
// fails the assertion.
#[test]
fn a_font_with_nothing_playable_is_refused() {
    use support::font::{Inst, Preset, font};
    let font = font(
        &[
            Preset {
                name: "bank",
                bank: 300,
                program: 0,
                zones: vec![(vec![], 0)],
            },
            Preset {
                name: "program",
                bank: 0,
                program: 200,
                zones: vec![(vec![], 0)],
            },
        ],
        &[Inst {
            name: "i",
            zones: vec![vec![]],
        }],
    );
    let error = SoundFontUnit::new(font, &Settings::new(RATE)).unwrap_err();
    assert!(matches!(error, Error::NoPlayablePreset), "{error:?}");
}

// SF2 2.04 section 7.2: of two presets with one bank and program, "the first
// occurring preset in the PHDR chunk is the active preset".
//
// Mutation (run): `entry().or_insert` back to `insert` in `Player::new` lets
// the last win, the loud one, and fails the assertion.
#[test]
fn the_first_of_two_presets_with_one_bank_and_program_is_active() {
    use support::font::{Inst, Preset, font};
    use tutti_fixtures::sf2::generator;
    let preset = |name, instrument| Preset {
        name,
        bank: 0,
        program: 0,
        zones: vec![(vec![], instrument)],
    };
    let font = font(
        &[preset("first, silent", 0), preset("second, loud", 1)],
        &[
            Inst {
                name: "silent",
                zones: vec![vec![(generator::INITIAL_ATTENUATION, 1440)]],
            },
            Inst {
                name: "loud",
                zones: vec![vec![]],
            },
        ],
    );
    let mut rig = Rig::dry(&font);
    rig.note_on(0, 69, 100);
    assert!(
        rig.energy(4096) < 0.01,
        "the first preset is the active one"
    );
}
