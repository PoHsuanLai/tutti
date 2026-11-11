//! The behaviour of the unit's controllers and notes over the reference
//! SoundFont, driven through MIDI 1.0 messages as a keyboard sends them.
//!

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use support::rig::{RATE, Rig, timgm6mb};
use tutti_ump::{Bank, CCNumber, U7, Voice2};

use tutti_sf2::SoundFont;
use tutti_soundfont::{Error, Settings, SoundFontUnit};
use tutti_types::SampleRate;

/// A unit at the default settings, whose dry pair is true silence once a voice
/// stops.
fn dry() -> Rig {
    Rig::dry(&timgm6mb())
}

fn refused(settings: Settings) -> Error {
    SoundFontUnit::new(timgm6mb(), &settings).unwrap_err()
}

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
#[test]
fn a_pool_of_one_voice_gives_way_to_the_next_note() {
    let font = support::font::one_preset(vec![vec![]]);
    let level = |voices: usize| {
        let settings = Settings::new(RATE).with_polyphony(NonZeroUsize::new(voices).unwrap());
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
        release.cc(ch, CCNumber::ALL_NOTES_OFF, 0);
    }
    assert!(energy(&mut release, 512) > 0.0, "the release tail sounds");
}

#[test]
fn a_channel_cut_leaves_the_other_channels_sounding() {
    let mut synthesizer = dry();
    synthesizer.note_on(0, 60, 100);
    synthesizer.note_on(1, 64, 100);
    energy(&mut synthesizer, 512);
    synthesizer.cc(0, CCNumber::ALL_SOUND_OFF, 0);
    assert!(
        energy(&mut synthesizer, 512) > 1.0,
        "channel 1 still sounds"
    );
    synthesizer.cc(1, CCNumber::ALL_SOUND_OFF, 0);
    assert_eq!(energy(&mut synthesizer, 512), 0.0);
}

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
// through the unit's message face. The sends are `sends.rs`'s.

fn cc(rig: &mut Rig, controller: CCNumber, value: u8) {
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

// Arm 0x07 (volume coarse) and 0x27 (fine).
#[test]
fn cc_7_and_39_set_the_channel_volume() {
    let mut loud = violin(|_| {});
    assert!(
        energy(&mut loud, 2048) > 1.0,
        "control: default volume sounds"
    );

    let mut silent = violin(|s| cc(s, CCNumber::VOLUME, 0));
    assert_eq!(energy(&mut silent, 2048), 0.0, "coarse 0 is volume 0");

    // Half volume, then the fine byte adds 127/16383 of full scale on top.
    let level = |fine: u8| {
        let mut synthesizer = dry();
        cc(&mut synthesizer, CCNumber::VOLUME, 64);
        cc(&mut synthesizer, CCNumber::of(39), fine);
        synthesizer.note_on(0, 69, 100);
        energy(&mut synthesizer, 2048)
    };
    assert!(level(127) > level(0), "the fine byte raises the volume");
}

// Arm 0x0B (expression coarse) and 0x2B (fine).
#[test]
fn cc_11_and_43_set_the_expression() {
    let mut silent = violin(|s| cc(s, CCNumber::EXPRESSION, 0));
    assert_eq!(energy(&mut silent, 2048), 0.0);

    let level = |fine: u8| {
        let mut synthesizer = dry();
        cc(&mut synthesizer, CCNumber::EXPRESSION, 64);
        cc(&mut synthesizer, CCNumber::of(43), fine);
        synthesizer.note_on(0, 69, 100);
        energy(&mut synthesizer, 2048)
    };
    assert!(level(127) > level(0), "the fine byte raises the expression");
}

// Arm 0x0A (pan coarse) and 0x2A (fine), by RP-036: "values 0 and 1 both pan
// hard left", 64 is "true center".
#[test]
fn cc_10_and_42_pan_the_channel() {
    let mut left = violin(|s| cc(s, CCNumber::PAN, 0));
    let (l, r) = energy_lr(&mut left, 2048);
    assert!(
        l > 1.0 && r == 0.0,
        "hard left leaves the right silent: {l} {r}"
    );

    let mut one = violin(|s| cc(s, CCNumber::PAN, 1));
    let (l, r) = energy_lr(&mut one, 2048);
    assert!(l > 1.0 && r == 0.0, "1 is hard left too: {l} {r}");

    let mut right = violin(|s| cc(s, CCNumber::PAN, 127));
    let (l, r) = energy_lr(&mut right, 2048);
    assert!(r > 1.0 && l < r * 0.05, "hard right: {l} {r}");

    // The fine byte moves the pan past the wall: CC 10 = 1 is hard left, and
    // the next 127 steps of the 14-bit value are the first of the pan (RP-036
    // subtracts 1, a step of 128, before the formula). Before this lane the test
    // used CC 10 = 0 and a fine byte of 127, which the RP puts under that 1:
    // still hard left.
    let mut fine = violin(|s| {
        cc(s, CCNumber::PAN, 1);
        cc(s, CCNumber::of(42), 127);
    });
    let (_, r) = energy_lr(&mut fine, 2048);
    assert!(r > 0.0, "the fine byte moves the pan off the wall");
}

/// RP-036: "64 (40H) is Center", "an equal power distribution", "true center":
/// the same level in both channels, to the 14-bit step. On a synthetic font
/// whose one zone has no pan of its own (the reference font's presets are
/// panned zones, which the SF2 pan generator adds to this).
#[test]
fn cc_10_at_64_is_true_centre() {
    let mut centre = Rig::dry(&two_bank_soundfont());
    cc(&mut centre, CCNumber::PAN, 64);
    centre.note_on(0, 69, 30);
    let (l, r) = energy_lr(&mut centre, 2048);
    assert!(l > 0.0 && (l - r).abs() <= 1e-4 * l, "{l} {r}");
}

/// RP-036: "if a currently sounding note is panned, the panning shall be done
/// without audible artifacts, clicks or zipper noise". A pan change from hard
/// left to hard right on a sounding note is not a step: the left channel, which
/// the new pan silences, still sounds in the first chunk after the change and
/// is silent by the end of the next one.
#[test]
fn a_pan_change_on_a_sounding_note_ramps() {
    let mut synthesizer = violin(|s| cc(s, CCNumber::PAN, 0));
    synthesizer.render(4096);
    cc(&mut synthesizer, CCNumber::PAN, 127);
    let (left, _) = synthesizer.render(256);
    let first: f32 = left[..8].iter().map(|x| x * x).sum();
    assert!(first > 0.0, "a step would have silenced the left at once");
    let last: f32 = left[248..].iter().map(|x| x * x).sum();
    assert_eq!(last, 0.0, "and the ramp ends silent");
}

// Arm 0x40 (hold pedal): 64 and above holds, below does not.
#[test]
fn cc_64_holds_released_notes_until_the_pedal_lifts() {
    let ring = |pedal: u8| {
        let mut synthesizer = violin(|s| cc(s, CCNumber::SUSTAIN, pedal));
        energy(&mut synthesizer, 44_100);
        synthesizer.note_off(0, 69);
        energy(&mut synthesizer, 44_100 * 20);
        (energy(&mut synthesizer, 512), synthesizer)
    };
    let (held, mut synthesizer) = ring(64);
    assert!(held > 1.0, "a pedal at 64 keeps the released note sounding");
    cc(&mut synthesizer, CCNumber::SUSTAIN, 63);
    energy(&mut synthesizer, 44_100 * 20);
    assert_eq!(
        energy(&mut synthesizer, 512),
        0.0,
        "lifting the pedal ends it"
    );
    assert_eq!(ring(63).0, 0.0, "63 is not down");
}

/// A Registered Controller of bank 0 as the translator sends one: the MIDI
/// 1.0 pair zero-extended (M2-115 section 4.1).
fn registered(index: u8, msb: u8, lsb: u8) -> Voice2 {
    Voice2::Registered {
        bank: U7::of(0),
        index: U7::of(index),
        value: ((u32::from(msb) << 7) | u32::from(lsb)) << 18,
    }
}

// The arm for Registered Controller 0 (RPN 0, pitch bend sensitivity:
// semitones, then cents).
#[test]
fn registered_controller_0_sets_the_pitch_bend_range() {
    let default = crossings(|_| {}, BEND_MAX) as f64;
    let wide = crossings(|s| s.voice2(0, registered(0, 12, 0)), BEND_MAX) as f64;
    // Bent fully up: 12 semitones against the default 2.
    let ratio = wide / default;
    assert!((1.6..1.9).contains(&ratio), "2^(10/12) = 1.78, got {ratio}");

    let none = crossings(|s| s.voice2(0, registered(0, 0, 0)), BEND_MAX) as f64;
    let cents = crossings(|s| s.voice2(0, registered(0, 0, 127)), BEND_MAX) as f64;
    // 0 semitones, then 1.27 from the fine byte: 2^(1.27/12) = 1.076.
    let ratio = cents / none;
    assert!((1.05..1.1).contains(&ratio), "got {ratio}");
}

// Registered Controller 2 (coarse tuning, 64 is none): 76 is +12 semitones,
// so the note sounds an octave up.
#[test]
fn registered_controller_2_sets_the_coarse_tuning() {
    let base = crossings(|_| {}, 8192) as f64;
    let octave = crossings(|s| s.voice2(0, registered(2, 76, 0)), 8192) as f64;
    let ratio = octave / base;
    assert!((1.9..2.1).contains(&ratio), "an octave up, got {ratio}");
}

// RPN 1, Channel Fine Tuning (RP-022), 14 bits centred at 2000H over 100 cents:
// 0x4000 is a semitone up.
#[test]
fn registered_controller_1_sets_the_fine_tuning() {
    let base = crossings(|_| {}, 8192) as f64;
    let up = crossings(|s| s.voice2(0, registered(1, 127, 127)), 8192) as f64;
    let ratio = up / base;
    // 2^(1/12) = 1.059, less one step of the 14 bits.
    assert!((1.05..1.07).contains(&ratio), "a semitone up, got {ratio}");
}

// CA-026: RPN 5 scales the Modulation Wheel. A range of 0 leaves the note as
// it is with the wheel at 0 (the depth is the position times the range), and
// the default (GM2 section 3.4.4: +/-50 cents) does not.
#[test]
fn registered_controller_5_scales_the_modulation_wheel() {
    let render = |setup: fn(&mut Rig)| {
        let mut synthesizer = violin(setup);
        synthesizer.render(8192).0
    };
    let none = render(|_| {});
    let zero_range = render(|s| {
        s.voice2(0, registered(5, 0, 0));
        cc(s, CCNumber::MOD_WHEEL, 127);
    });
    let default_range = render(|s| cc(s, CCNumber::MOD_WHEEL, 127));
    assert_eq!(zero_range, none, "a range of 0 is no vibrato");
    assert_ne!(default_range, none);
}

// RP-015: Reset All Controllers does "NOT reset Volume (#7)" nor Pan (#10), and
// sets Expression to 127 and Modulation to 0.
#[test]
fn cc_121_keeps_the_volume_and_the_pan() {
    let mut silent = dry();
    cc(&mut silent, CCNumber::VOLUME, 0);
    cc(&mut silent, CCNumber::RESET_ALL, 0);
    silent.note_on(0, 69, 100);
    assert_eq!(energy(&mut silent, 2048), 0.0, "the volume stays 0");

    let mut left = dry();
    cc(&mut left, CCNumber::PAN, 0);
    cc(&mut left, CCNumber::RESET_ALL, 0);
    left.note_on(0, 69, 100);
    let (_, r) = energy_lr(&mut left, 2048);
    assert_eq!(r, 0.0, "the pan stays hard left");
}

// M1 p.14, Legato Footswitch: the note struck while another sounds moves the
// sounding voice "without re-attacking"; M1 p.16, Portamento Control: "If there
// is a currently sounding voice whose note number is coincident with the source
// note number, the voice's pitch will glide to the new Note-On's pitch [...]
// Then, no new voice should be assigned."
#[test]
fn a_portamento_control_retunes_the_sounding_voice() {
    let cents = |portamento_time: u8| {
        let mut s = dry();
        s.program(0, 40);
        cc(&mut s, CCNumber::PORTAMENTO_TIME, portamento_time);
        s.note_on(0, 60, 100);
        energy(&mut s, 4096);
        cc(&mut s, CCNumber::PORTAMENTO_CONTROL, 60);
        s.note_on(0, 72, 100);
        energy(&mut s, 4096);
        let (left, _) = s.render(44_100);
        left.windows(2).filter(|w| w[0] * w[1] < 0.0).count() as f64
    };
    let plain = |key: u8| {
        let mut s = dry();
        s.program(0, 40);
        s.note_on(0, key, 100);
        energy(&mut s, 8192);
        let (left, _) = s.render(44_100);
        left.windows(2).filter(|w| w[0] * w[1] < 0.0).count() as f64
    };
    // At Portamento Time 0 (1,000 cent/msec) the glide is over in a
    // millisecond: the voice sounds the new key, alone.
    let fast = cents(0) / plain(72);
    assert!((0.97..1.03).contains(&fast), "{fast}");
    // At 127 (0.01 cent/msec) it has moved 150 cents in the 15 s of this test.
    // The voice is still near its source key.
    let slow = cents(127) / plain(60);
    assert!((0.98..1.2).contains(&slow), "{slow}");
}

// M1 p.14: with the Legato Footswitch down a second note does not start a
// voice: "pitch is changed without re-attacking". One voice sounds the new key
// (its zero crossings are the new key's, which a lower voice left sounding would
// not leave) and no second adds to its energy.
#[test]
fn the_legato_footswitch_moves_the_voice() {
    let measure = |footswitch: bool, first: Option<u8>| {
        let mut s = dry();
        s.program(0, 40);
        if footswitch {
            cc(&mut s, CCNumber::LEGATO, 127);
        }
        if let Some(first) = first {
            s.note_on(0, first, 100);
            energy(&mut s, 4096);
        }
        s.note_on(0, 72, 100);
        energy(&mut s, 8192);
        let (left, _) = s.render(44_100);
        let crossings = left.windows(2).filter(|w| w[0] * w[1] < 0.0).count() as f64;
        let energy: f32 = left.iter().map(|x| x * x).sum();
        (crossings, energy)
    };
    let (moved, moved_energy) = measure(true, Some(60));
    let (plain, plain_energy) = measure(false, None);
    let (layered, layered_energy) = measure(false, Some(60));
    assert!(
        (moved / plain - 1.0).abs() < 0.03,
        "{moved} against {plain}"
    );
    assert!(
        (moved_energy / plain_energy - 1.0).abs() < 0.3,
        "one voice: {moved_energy} against {plain_energy}"
    );
    // Control: without the footswitch the first note keeps sounding under the
    // second, and the energy shows it.
    assert!(layered_energy > 1.5 * plain_energy, "{layered_energy}");
    assert!(layered > 0.0);
}

// An Assignable Controller (NRPN), or a Registered Controller outside bank 0,
// names nothing the channel follows: the range stays.
#[test]
fn other_controllers_leave_the_pitch_bend_range() {
    let default = crossings(|_| {}, BEND_MAX) as f64;
    for voice in [
        Voice2::Assignable {
            bank: U7::of(0),
            index: U7::of(0),
            // M2-115 §3: 12 as a 7-bit value, left-justified (shift 25).
            value: 12 << 25,
        },
        Voice2::Registered {
            bank: U7::of(1),
            index: U7::of(0),
            value: 12 << 25,
        },
    ] {
        let ignored = crossings(|s| s.voice2(0, voice), BEND_MAX) as f64;
        assert!(
            (ignored / default - 1.0).abs() < 0.01,
            "{voice:?}: {ignored} against {default}"
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

// Bank Select is held until the next Program Change (M1 p.13: "the receiving
// device must remember that bank number in readiness for the following Program
// Change").
#[test]
fn bank_select_is_held_until_the_program_change() {
    let font = two_bank_soundfont();
    let level = |select: bool, program: bool| {
        let mut synthesizer = Rig::dry(&font);
        if select {
            cc(&mut synthesizer, CCNumber::BANK_SELECT, 1);
        }
        if program {
            synthesizer.voice2(
                0,
                Voice2::ProgramChange {
                    program: U7::of(0),
                    bank: None,
                },
            );
        }
        synthesizer.note_on(0, 69, 40);
        energy(&mut synthesizer, 4096)
    };
    let loud = level(false, false);
    assert!(loud > 20.0, "bank 0 plays the sine: {loud}");
    assert!(level(true, false) > 20.0, "Bank Select alone keeps bank 0");
    assert!(
        level(true, true) < loud * 0.01,
        "the Program Change applies bank 1, 144 dB down"
    );
}

// A Program Change's bank (the translator puts a MIDI 1.0 Bank Select there)
// selects the bank, by its MSB.
#[test]
fn a_program_changes_bank_selects_the_bank() {
    let font = two_bank_soundfont();
    let level = |bank: Option<u8>| {
        let mut synthesizer = Rig::dry(&font);
        synthesizer.voice2(
            0,
            Voice2::ProgramChange {
                program: U7::of(0),
                bank: bank.map(|msb| Bank {
                    msb: U7::of(msb),
                    lsb: U7::of(0),
                }),
            },
        );
        synthesizer.note_on(0, 69, 40);
        energy(&mut synthesizer, 4096)
    };
    let loud = level(Some(0));
    assert!(loud > 20.0, "bank 0 plays the sine: {loud}");
    assert!(level(Some(1)) < loud * 0.01, "bank 1 is 144 dB down");
}

// The region's velocity range, through the public synthesizer: the loud zone
// answers 0 to 63.
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
    let coarse = render(|s| cc(s, CCNumber::MOD_WHEEL, 127));
    let half = render(|s| cc(s, CCNumber::MOD_WHEEL, 64));
    let fine = render(|s| {
        cc(s, CCNumber::MOD_WHEEL, 64);
        cc(s, CCNumber::of(33), 127);
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
    cc(&mut cut, CCNumber::ALL_SOUND_OFF, 0);
    assert_eq!(
        energy(&mut cut, 512),
        0.0,
        "all sound off is silent at once"
    );

    let mut released = violin(|_| {});
    energy(&mut released, 4096);
    cc(&mut released, CCNumber::ALL_NOTES_OFF, 0);
    assert!(energy(&mut released, 64) > 0.0, "all notes off rings on");
    energy(&mut released, 44_100 * 20);
    assert_eq!(energy(&mut released, 512), 0.0, "and then ends");
}

// Arm 0x79 (reset all controllers): the expression comes back to 127, and the
// pitch bend to none.
#[test]
fn cc_121_resets_the_expression() {
    let mut synthesizer = dry();
    cc(&mut synthesizer, CCNumber::EXPRESSION, 0);
    cc(&mut synthesizer, CCNumber::RESET_ALL, 0);
    synthesizer.note_on(0, 69, 100);
    assert!(energy(&mut synthesizer, 2048) > 1.0);
}

// The `_` arm: a controller the synthesizer does not follow changes nothing.
#[test]
fn an_unfollowed_controller_leaves_the_note_sounding() {
    let mut synthesizer = violin(|_| {});
    energy(&mut synthesizer, 4096);
    cc(&mut synthesizer, CCNumber::of(20), 127);
    cc(&mut synthesizer, CCNumber::of(119), 127);
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

fn per_note_pitch(note: u8, hcu: u32) -> Voice2 {
    Voice2::RegisteredPerNote {
        note: tutti_ump::NoteNumber::of(note),
        controller: tutti_ump::RegisteredPerNoteController::Pitch,
        value: hcu << 25,
    }
}

fn management(note: u8, detach: bool, reset: bool) -> Voice2 {
    Voice2::PerNoteManagement {
        note: tutti_ump::NoteNumber::of(note),
        detach,
        reset,
    }
}

/// Zero crossings in one second of the violin note struck on Note Number 69
/// after `after` has run on the sounding note.
fn crossings_after(after: impl FnOnce(&mut Rig)) -> f64 {
    let mut synthesizer = dry();
    synthesizer.program(0, 40);
    synthesizer.note_on(0, 69, 100);
    energy(&mut synthesizer, 4096);
    after(&mut synthesizer);
    energy(&mut synthesizer, 44_100);
    let (left, _) = synthesizer.render(44_100);
    left.windows(2).filter(|w| w[0] * w[1] < 0.0).count() as f64
}

/// M2-104 Appendix C.3 and section 7.4.15.2: a Registered Per-Note Controller
/// #3 (Pitch 7.25) sent before the Note On is the pitch the note sounds at,
/// the Note Number being an index. Note Number 69 told 81.0 sounds an octave
/// up: twice the zero crossings of the same note untouched.
#[test]
fn a_per_note_pitch_sent_before_the_note_on_is_the_pitch_it_sounds_at() {
    let plain = crossings(|_| {}, 8192) as f64;
    let told = crossings(|s| s.voice2(0, per_note_pitch(69, 81)), 8192) as f64;
    let ratio = told / plain;
    assert!((1.9..2.1).contains(&ratio), "an octave up, got {ratio}");
}

/// M2-104 section 7.4.5, Detach: a note that was detached "shall no longer
/// respond to any Per-Note Controllers". The control half shows the Pitch
/// reaches a sounding note (an octave up); with Detach before it, the note
/// stays where it was.
#[test]
fn a_detached_note_no_longer_follows_its_per_note_pitch() {
    let plain = crossings_after(|_| {});
    let followed = crossings_after(|s| s.voice2(0, per_note_pitch(69, 81)));
    assert!(
        (1.9..2.1).contains(&(followed / plain)),
        "{followed}/{plain}"
    );
    let detached = crossings_after(|s| {
        s.voice2(0, management(69, true, false));
        s.voice2(0, per_note_pitch(69, 81));
    });
    assert!(
        (0.95..1.05).contains(&(detached / plain)),
        "{detached}/{plain}"
    );
}

/// M2-104 section 7.4.13: a Per-Note Pitch Bend is a position over the
/// channel's sensitivity. Sensitivity 12 and the wheel fully up, sent before
/// the Note On, is an octave.
#[test]
fn a_per_note_bend_spans_the_per_note_sensitivity() {
    let plain = crossings(|_| {}, 8192) as f64;
    let bent = crossings(
        |s| {
            s.voice2(
                0,
                Voice2::Registered {
                    bank: U7::of(0),
                    index: U7::of(7),
                    value: 12 << 25,
                },
            );
            s.voice2(
                0,
                Voice2::PerNotePitchBend {
                    note: tutti_ump::NoteNumber::of(69),
                    value: u32::MAX,
                },
            );
        },
        8192,
    ) as f64;
    let ratio = bent / plain;
    assert!((1.9..2.1).contains(&ratio), "an octave up, got {ratio}");
}

/// M2-104 section 7.4.5, Reset: the Note Number's per-note controllers return
/// to their defaults, so a Pitch sent and then reset is not what the next
/// note, or the one sounding, sounds at.
#[test]
fn a_reset_per_note_pitch_is_forgotten_by_the_next_note() {
    let plain = crossings(|_| {}, 8192) as f64;
    let reset = crossings(
        |s| {
            s.voice2(0, per_note_pitch(69, 81));
            s.voice2(0, management(69, false, true));
        },
        8192,
    ) as f64;
    assert!((0.95..1.05).contains(&(reset / plain)), "{reset}/{plain}");

    // The note already sounding is reset too (no Detach): back to its key.
    let plain = crossings_after(|_| {});
    let playing = crossings_after(|s| {
        s.voice2(0, per_note_pitch(69, 81));
        s.voice2(0, management(69, false, true));
    });
    assert!(
        (0.95..1.05).contains(&(playing / plain)),
        "{playing}/{plain}"
    );
}
