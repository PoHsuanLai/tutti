//! The typed `Synthesizer` surface over the reference SoundFont.

use std::fs::File;
use std::sync::Arc;

use tutti_fixtures::asset;
use tutti_rustysynth::{Stop, Synthesizer, SynthesizerError, SynthesizerSettings};
use tutti_sf2::SoundFont;
use tutti_types::{SampleRate, Samples};
use tutti_ump::{MidiChannel, NoteNumber, U7};

const RATE: SampleRate = SampleRate::SR_44K1;

fn timgm6mb() -> Arc<SoundFont> {
    let mut file = File::open(asset("soundfonts/TimGM6mb.sf2")).unwrap();
    Arc::new(SoundFont::new(&mut file).unwrap())
}

/// A synthesizer with no effect lines, so a stopped voice leaves true silence.
fn dry() -> Synthesizer {
    let mut settings = SynthesizerSettings::new(RATE);
    settings.enable_reverb_and_chorus = false;
    Synthesizer::new(&timgm6mb(), &settings).unwrap()
}

fn key(n: u8) -> NoteNumber {
    NoteNumber::new(n).unwrap()
}

fn channel(n: u8) -> MidiChannel {
    MidiChannel::new(n).unwrap()
}

fn velocity(n: u8) -> U7 {
    U7::new(n).unwrap()
}

/// The summed magnitude of the next `frames` frames.
fn energy(synthesizer: &mut Synthesizer, frames: usize) -> f32 {
    let mut left = vec![0.0; frames];
    let mut right = vec![0.0; frames];
    synthesizer.render(&mut left, &mut right);
    left.iter().chain(&right).map(|s| s.abs()).sum()
}

fn refused(settings: SynthesizerSettings) -> SynthesizerError {
    Synthesizer::new(&timgm6mb(), &settings).unwrap_err()
}

// Mutation (run): widening the lower bound in `validate` to 15_000 lets
// 15_999 Hz through and fails the first assertion.
#[test]
fn a_rate_outside_16_to_192_khz_is_refused_with_the_rate() {
    for hz in [15_999_u32, 192_001] {
        let rate = SampleRate::from(hz);
        let error = refused(SynthesizerSettings::new(rate));
        assert!(matches!(error, SynthesizerError::SampleRateOutOfRange(r) if r == rate));
    }
    for hz in [16_000_u32, 192_000] {
        let settings = SynthesizerSettings::new(SampleRate::from(hz));
        assert!(Synthesizer::new(&timgm6mb(), &settings).is_ok());
    }
}

#[test]
fn a_block_or_polyphony_outside_its_range_is_refused_with_the_value() {
    for frames in [7, 1025] {
        let mut settings = SynthesizerSettings::new(RATE);
        settings.block_size = Samples(frames);
        assert!(
            matches!(refused(settings), SynthesizerError::BlockSizeOutOfRange(b) if b == Samples(frames))
        );
    }
    for voices in [7, 257] {
        let mut settings = SynthesizerSettings::new(RATE);
        settings.maximum_polyphony = voices;
        assert!(
            matches!(refused(settings), SynthesizerError::MaximumPolyphonyOutOfRange(v) if v == voices)
        );
    }
}

#[test]
fn the_error_names_the_value_it_refused() {
    let message = refused(SynthesizerSettings::new(SampleRate::from(8_000))).to_string();
    assert!(message.contains("8000"), "{message}");
}

#[test]
fn the_rate_is_whole_hertz() {
    let synthesizer =
        Synthesizer::new(&timgm6mb(), &SynthesizerSettings::new(SampleRate(44_100.4)));
    assert_eq!(synthesizer.unwrap().sample_rate(), RATE);
}

// Mutation (run): swapping the `Cut` and `Release` arms of `note_off_all`
// leaves the cut note ringing and fails the first assertion.
#[test]
fn cut_silences_at_once_and_release_rings_out() {
    let mut cut = dry();
    cut.note_on(channel(0), key(60), velocity(100));
    assert!(energy(&mut cut, 512) > 1.0, "the note sounds");
    cut.note_off_all(Stop::Cut);
    assert_eq!(energy(&mut cut, 512), 0.0);

    let mut release = dry();
    release.note_on(channel(0), key(60), velocity(100));
    energy(&mut release, 512);
    release.note_off_all(Stop::Release);
    assert!(energy(&mut release, 512) > 0.0, "the release tail sounds");
}

// Mutation (run): comparing the voice's channel against the key number in
// `note_off_all_channel` cuts the wrong voices and fails one of the two
// assertions.
#[test]
fn a_channel_cut_leaves_the_other_channels_sounding() {
    let mut synthesizer = dry();
    synthesizer.note_on(channel(0), key(60), velocity(100));
    synthesizer.note_on(channel(1), key(64), velocity(100));
    energy(&mut synthesizer, 512);
    synthesizer.note_off_all_channel(channel(0), Stop::Cut);
    assert!(
        energy(&mut synthesizer, 512) > 1.0,
        "channel 1 still sounds"
    );
    synthesizer.note_off_all_channel(channel(1), Stop::Cut);
    assert_eq!(energy(&mut synthesizer, 512), 0.0);
}

// Mutation (run): deleting the velocity-0 early return in `note_on` starts a
// second voice instead of releasing the first, which sustains for the whole
// render and fails the last assertion.
#[test]
fn a_note_on_at_velocity_zero_is_a_note_off() {
    let mut synthesizer = dry();
    // The violin loops its sustain: only a release ends it.
    synthesizer.program_change(channel(0), velocity(40));
    synthesizer.note_on(channel(0), key(69), velocity(100));
    energy(&mut synthesizer, 44_100 * 3);
    assert!(energy(&mut synthesizer, 512) > 1.0, "the note sustains");

    synthesizer.note_on(channel(0), key(69), velocity(0));
    // Released, not cut: the tail is still there in the next block.
    assert!(energy(&mut synthesizer, 64) > 0.0);
    energy(&mut synthesizer, 44_100 * 20);
    assert_eq!(
        energy(&mut synthesizer, 512),
        0.0,
        "the released voice has ended"
    );
}

// Control Change dispatch, one test per arm of `Synthesizer::control_change`,
// through the public method. The synthesizer is dry (no effect lines) unless a
// test is about the sends.

use tutti_ump::CCNumber;

fn cc(synthesizer: &mut Synthesizer, controller: u8, value: u8) {
    synthesizer.control_change(
        channel(0),
        CCNumber::new(controller).unwrap(),
        velocity(value),
    );
}

/// The summed magnitude of the next `frames` frames, per channel.
fn energy_lr(synthesizer: &mut Synthesizer, frames: usize) -> (f32, f32) {
    let mut left = vec![0.0; frames];
    let mut right = vec![0.0; frames];
    synthesizer.render(&mut left, &mut right);
    let sum = |b: &[f32]| b.iter().map(|s| s.abs()).sum();
    (sum(&left), sum(&right))
}

/// A synthesizer playing a sustained violin note once `setup` has run.
fn violin(setup: impl FnOnce(&mut Synthesizer)) -> Synthesizer {
    let mut synthesizer = dry();
    synthesizer.program_change(channel(0), velocity(40));
    setup(&mut synthesizer);
    synthesizer.note_on(channel(0), key(69), velocity(100));
    synthesizer
}

/// Zero crossings in one second of the violin note after a 1 s lead-in, with
/// the pitch bent fully up by `bend`. Sample playback scales the crossing
/// count with the playback rate, so the count is proportional to the pitch.
fn crossings(setup: impl FnOnce(&mut Synthesizer), bend: u16) -> usize {
    let mut synthesizer = dry();
    synthesizer.program_change(channel(0), velocity(40));
    setup(&mut synthesizer);
    synthesizer.pitch_bend(channel(0), tutti_ump::U14::new(bend).unwrap());
    synthesizer.note_on(channel(0), key(69), velocity(100));
    energy(&mut synthesizer, 44_100);
    let mut left = vec![0.0; 44_100];
    let mut right = vec![0.0; 44_100];
    synthesizer.render(&mut left, &mut right);
    left.windows(2).filter(|w| w[0] * w[1] < 0.0).count()
}

const BEND_MAX: u16 = 16_383;

// Mutation (run, for each arm below): replacing the arm's call with `()` in
// `control_change` fails the named test.

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
        synthesizer.note_on(channel(0), key(69), velocity(100));
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
        synthesizer.note_on(channel(0), key(69), velocity(100));
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
        synthesizer.note_off(channel(0), key(69));
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
        let mut synthesizer =
            Synthesizer::new(&timgm6mb(), &SynthesizerSettings::new(RATE)).unwrap();
        synthesizer.program_change(channel(0), velocity(40));
        cc(&mut synthesizer, 91, 0);
        cc(&mut synthesizer, 93, 0);
        cc(&mut synthesizer, controller, value);
        synthesizer.note_on(channel(0), key(69), velocity(100));
        energy(&mut synthesizer, 8192);
        synthesizer.note_off_all(Stop::Cut);
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
    let select = |s: &mut Synthesizer| {
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
/// 0 of bank 1 plays the same sample 144 dB down.
fn two_bank_soundfont() -> Arc<SoundFont> {
    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(data);
        bytes
    }
    fn list(kind: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut data = kind.to_vec();
        data.extend(chunks.concat());
        chunk(b"LIST", &data)
    }
    fn name(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.resize(20, 0);
        bytes
    }
    fn words(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    let sine: Vec<u8> = (0..2100)
        .flat_map(|i| {
            let phase = i as f32 / 100.0 * std::f32::consts::TAU;
            ((phase.sin() * 20_000.0) as i16).to_le_bytes()
        })
        .collect();
    let preset = |label: &str, bank: u16, bag: u16| {
        [name(label), words(&[0, bank, bag]), vec![0; 12]].concat()
    };
    let phdr = [
        preset("bank0", 0, 0),
        preset("bank1", 1, 1),
        preset("EOP", 0, 2),
    ]
    .concat();
    let pbag = words(&[0, 0, 1, 0, 2, 0]);
    let pgen = words(&[41, 0, 41, 1, 0, 0]);
    let inst = |label: &str, bag: u16| [name(label), words(&[bag])].concat();
    let insts = [inst("loud", 0), inst("quiet", 1), inst("EOI", 2)].concat();
    let ibag = words(&[0, 0, 2, 0, 5, 0]);
    // sampleModes 1 (loop), sampleID 0; the quiet one adds initialAttenuation.
    let igen = words(&[54, 1, 53, 0, 48, 1440, 54, 1, 53, 0, 0, 0]);
    let sample = |label: &str, start: u32, end: u32, rate: u32, pitch: u8| {
        let mut bytes = name(label);
        for v in [start, end, 100, 1900, rate] {
            bytes.extend(v.to_le_bytes());
        }
        bytes.extend([pitch, 0, 0, 0, 0, 0]);
        bytes
    };
    let shdr = [
        sample("sine", 0, 2000, 44_100, 69),
        sample("EOS", 0, 0, 0, 0),
    ]
    .concat();
    let terminal_mod = vec![0; 10];
    let body = [
        list(b"INFO", &[chunk(b"ifil", &words(&[2, 1]))]),
        list(b"sdta", &[chunk(b"smpl", &sine)]),
        list(
            b"pdta",
            &[
                chunk(b"phdr", &phdr),
                chunk(b"pbag", &pbag),
                chunk(b"pmod", &terminal_mod),
                chunk(b"pgen", &pgen),
                chunk(b"inst", &insts),
                chunk(b"ibag", &ibag),
                chunk(b"imod", &terminal_mod),
                chunk(b"igen", &igen),
                chunk(b"shdr", &shdr),
            ],
        ),
    ];
    let mut file = b"RIFF".to_vec();
    let payload = [b"sfbk".to_vec(), body.concat()].concat();
    file.extend((payload.len() as u32).to_le_bytes());
    file.extend(payload);
    Arc::new(SoundFont::new(&mut std::io::Cursor::new(file)).expect("the built SoundFont parses"))
}

// Arm 0x00 (bank select). Mutation (run): the arm's `set_bank` -> `()` leaves
// bank 0 selected and fails the second assertion.
#[test]
fn cc_0_selects_the_bank() {
    let mut settings = SynthesizerSettings::new(RATE);
    settings.enable_reverb_and_chorus = false;
    let font = two_bank_soundfont();
    let level = |bank: Option<u8>| {
        let mut synthesizer = Synthesizer::new(&font, &settings).unwrap();
        if let Some(bank) = bank {
            cc(&mut synthesizer, 0, bank);
        }
        synthesizer.note_on(channel(0), key(69), velocity(127));
        energy(&mut synthesizer, 4096)
    };
    let loud = level(None);
    assert!(loud > 100.0, "bank 0 plays the sine: {loud}");
    assert!(level(Some(1)) < loud * 0.01, "bank 1 is 144 dB down");
}

// Arms 0x01 (modulation coarse) and 0x21 (fine): vibrato depth, at most 50
// cents, which no zero-crossing count resolves. The strongest claim here is
// that the controller changes the render and a different value changes it
// differently.
#[test]
fn cc_1_and_33_change_the_vibrato() {
    let render = |setup: fn(&mut Synthesizer)| {
        let mut synthesizer = violin(setup);
        let mut left = vec![0.0; 8192];
        let mut right = vec![0.0; 8192];
        synthesizer.render(&mut left, &mut right);
        left
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
    synthesizer.note_on(channel(0), key(69), velocity(100));
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
