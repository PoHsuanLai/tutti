//! The declared tail is a sound upper bound: a note let go rings out no longer
//! than the node says, whatever the preset. The instrument contract cannot
//! catch a tail that is too short, so this is the guard.
//!
//! "Rings out" is the last frame after the note-off whose sample is at or above
//! [`THRESHOLD`], 1e-4 of full scale (-80 dBFS), on either channel. The voices
//! themselves end at 1e-3 of their own envelope (-60 dB), so the figure is
//! measured well below where the tail is promised. The effects are on, the
//! unit's default, so the reverb's ring is in the figure.

mod support;

use support::font::{Inst, Preset, font};
use support::rig::{RATE, Rig, timgm6mb};
use tutti_fixtures::sf2::generator;
use tutti_graph::Node;
use tutti_soundfont::Settings;
use tutti_types::Tail;

/// The level below which the render counts as silent: -80 dBFS.
const THRESHOLD: f32 = 1e-4;

/// The most frames rendered after a note-off: well past the longest release a
/// font can ask for (8000 timecents, 101.6 s, section 8.1.3).
const CAP: usize = 44_100 * 130;

/// The release time of a pair of zones, from the specification alone: the two
/// generator 38 values added, replaced by the nearest value of the range
/// (8.1.3, 10.5), as seconds (9.3); never below the 10 ms floor the voice
/// gives a release.
fn release_seconds(
    preset_zone: &tutti_sf2::PresetRegion,
    zone: &tutti_sf2::InstrumentRegion,
) -> f64 {
    use tutti_sf2::GeneratorType::ReleaseVolumeEnvelope as Release;
    let sum = i32::from(preset_zone.generator(Release)) + i32::from(zone.generator(Release));
    2f64.powf(f64::from(sum.clamp(-12000, 8000)) / 1200.0)
        .max(0.01)
}

/// A preset's pair with the longest release, and a key and velocity that start
/// it.
struct Longest {
    preset: usize,
    release: f64,
    key: u8,
    velocity: u8,
}

fn longest_of(font: &tutti_sf2::SoundFont, preset: usize) -> Option<Longest> {
    let mut best: Option<Longest> = None;
    for pz in font.presets()[preset].regions() {
        for iz in font.instruments()[pz.instrument_id()].regions() {
            let keys = (*pz.key_range().start().max(iz.key_range().start()))
                ..=(*pz.key_range().end().min(iz.key_range().end()));
            let vels = (*pz.velocity_range().start().max(iz.velocity_range().start()))
                ..=(*pz.velocity_range().end().min(iz.velocity_range().end()));
            if keys.is_empty() || vels.is_empty() {
                continue;
            }
            let release = release_seconds(pz, iz);
            if best.as_ref().is_none_or(|b| release > b.release) {
                best = Some(Longest {
                    preset,
                    release,
                    key: *keys.start(),
                    velocity: (*vels.end()).min(100).max(*vels.start()),
                });
            }
        }
    }
    best
}

/// Plays the longest zone of `preset` for a second, lets it go, and returns the
/// frames it rang after the note-off and the declared tail in frames.
fn rang(font: &std::sync::Arc<tutti_sf2::SoundFont>, longest: &Longest) -> (usize, usize) {
    let mut rig = Rig::with(font, &Settings::new(RATE));
    let p = &font.presets()[longest.preset];
    let bank = p.bank().map_or(0, |b| b.get());
    let patch = p.patch().map_or(0, |p| p.get());
    let channel = if bank == 128 { 9 } else { 0 };
    if bank != 128 && bank != 0 {
        rig.cc(channel, 0, bank as u8);
    }
    rig.program(channel, patch);
    rig.note_on(channel, longest.key, longest.velocity);
    rig.energy(44_100);
    rig.note_off(channel, longest.key);
    let mut last = 0;
    let mut elapsed = 0;
    let mut quiet = 0;
    while elapsed < CAP && quiet < 3 {
        let (l, r) = rig.render(4096);
        let loud = |c: &[f32]| c.iter().rposition(|s| s.abs() >= THRESHOLD);
        match loud(&l).max(loud(&r)) {
            Some(i) => {
                last = elapsed + i + 1;
                quiet = 0;
            }
            None => quiet += 1,
        }
        elapsed += 4096;
    }
    let Tail::Finite(tail) = rig.hand.shape().tail else {
        panic!("a finite tail")
    };
    (last, tail.get())
}

/// A note of the longest-release zone of the whole font, and of programs across
/// the set (piano, guitar, violin, strings, trumpet, a pad, the standard drum
/// kit), rings out within the declared tail.
///
/// Figures are printed with `--no-capture`: on TimGM6mb the notes ring 0.8 to
/// 2.6 s against a declared 103.1 s (the font's longest zone, a non-looped
/// sample, is silent long before its release). The tail is a loose bound here;
/// the next test is the one that pins it near the longest release. Mutation
/// (run): the tail left at 0 (no `with_tail`) fails every row.
#[test]
fn a_note_rings_out_within_the_declared_tail() {
    let font = timgm6mb();
    let overall = (0..font.presets().len())
        .filter_map(|p| longest_of(&font, p))
        .max_by(|a, b| a.release.total_cmp(&b.release))
        .expect("a preset starts a note");
    let mut cases = vec![overall];
    for (bank, patch) in [
        (0, 0),
        (0, 24),
        (0, 40),
        (0, 48),
        (0, 56),
        (0, 89),
        (128, 0),
    ] {
        let preset = font
            .presets()
            .iter()
            .position(|p| {
                p.bank().map(|b| b.get()) == Some(bank) && p.patch().map(|p| p.get()) == Some(patch)
            })
            .expect("the program is in the font");
        cases.push(longest_of(&font, preset).expect("it starts a note"));
    }
    for longest in &cases {
        let (frames, tail) = rang(&font, longest);
        println!(
            "RINGOUT preset {} ({}): longest release {:.2} s, rang {:.2} s, declared tail {:.2} s",
            longest.preset,
            font.presets()[longest.preset].name(),
            longest.release,
            frames as f64 / 44_100.0,
            tail as f64 / 44_100.0,
        );
        assert!(
            frames <= tail,
            "preset {}: rang {frames} frames after the note-off, declared {tail}",
            longest.preset
        );
    }
}

/// A looped zone with the longest release a font can ask for: 6000 timecents at
/// the preset and the instrument, which sum to 12000 and are replaced by the
/// range's 8000 (101.6 s; sections 8.1.3 and 10.5). The note rings for a good
/// part of it (the voice is dropped when its envelope is 60 dB down, at 0.6 of
/// the release time), and never past the declared tail. Without the clamp the
/// sum would be 1024 s and the note would outlast the render's cap.
///
/// Mutation (run): the clamp removed from `RegionPair::gs` leaves the note
/// ringing past the cap and fails the upper assertion; the tail at a tenth of
/// the longest release fails the tail one.
#[test]
fn the_longest_release_a_font_can_have_rings_out_within_the_tail() {
    let font = font(
        &[Preset {
            name: "long",
            bank: 0,
            program: 0,
            zones: vec![(vec![(generator::RELEASE_VOL_ENV, 6000)], 0)],
        }],
        &[Inst {
            name: "long",
            zones: vec![vec![(generator::RELEASE_VOL_ENV, 6000)]],
        }],
    );
    let longest = Longest {
        preset: 0,
        release: 2f64.powf(8000.0 / 1200.0),
        key: 69,
        velocity: 100,
    };
    let (frames, tail) = rang(&font, &longest);
    let seconds = frames as f64 / 44_100.0;
    println!(
        "RINGOUT the clamped zone: rang {seconds:.2} s, declared tail {:.2} s",
        tail as f64 / 44_100.0
    );
    assert!(seconds > 0.25 * longest.release, "it rang {seconds} s");
    assert!(frames <= tail, "rang {frames} frames, declared {tail}");
    assert!(seconds < longest.release, "it rang {seconds} s");
}
