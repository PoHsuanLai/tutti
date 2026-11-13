//! File loads are jobs (doc 017 L1f-b): `Midi::load_file` and
//! `Midi::load_soundfont` return a `MidiLoad` at once and the result lands as
//! `MidiEvent::FileLoaded` / `SoundFontLoaded` carrying that id, in `update()`.

use std::path::Path;
use std::time::{Duration, Instant};

use tutti_midi_file::{MidiFile, SmfFormat, Tick, Track};
use tutti_session::{JobFailed, MidiError, MidiEvent, MidiLoad, Session, SessionEvent};
use tutti_types::SampleRate;
use tutti_ump::{MidiChannel, NoteNumber, U7};

fn session() -> Session {
    Session::builder()
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// A one-note SMF written to `dir`; returns the file as built.
fn write_file(dir: &Path, name: &str, key: u8) -> (std::path::PathBuf, MidiFile) {
    let track = Track::new().note(
        Tick(0),
        480,
        MidiChannel::FIRST,
        NoteNumber::of(key),
        U7::of(100),
    );
    let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track]).expect("builds");
    let path = dir.join(name);
    file.write(&path).expect("writes");
    (path, file)
}

/// Updates until `want` events of `pick` have landed, or panics after 10 s.
fn land<T>(s: &mut Session, want: usize, mut pick: impl FnMut(MidiEvent) -> Option<T>) -> Vec<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < want {
        assert!(Instant::now() < deadline, "the load never landed");
        for event in s.update() {
            if let SessionEvent::Midi(event) = event
                && let Some(t) = pick(event)
            {
                got.push(t);
            }
        }
        std::thread::yield_now();
    }
    got
}

fn files(event: MidiEvent) -> Option<(MidiLoad, Result<MidiFile, MidiError>)> {
    match event {
        MidiEvent::FileLoaded { load, file } => Some((load, file)),
        _ => None,
    }
}

/// A file lands with the id `load_file` returned, holding what was written,
/// and not before `update()`.
#[test]
fn a_file_lands_in_update_with_its_id_and_its_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let (path, built) = write_file(dir.path(), "a.mid", 60);
    let mut s = session();
    let id = s.midi().load_file(path);
    let mut landed = land(&mut s, 1, files);
    let (load, file) = landed.pop().unwrap();
    assert_eq!(load, id);
    assert_eq!(file.expect("reads").tracks(), built.tracks());
}

#[test]
fn a_missing_file_lands_as_a_file_error_with_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session();
    let id = s.midi().load_file(dir.path().join("none.mid"));
    let (load, file) = land(&mut s, 1, files).pop().unwrap();
    assert_eq!(load, id);
    assert!(matches!(file, Err(MidiError::File(_))), "{file:?}");
}

#[test]
fn two_loads_land_each_with_its_own_id_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, fa) = write_file(dir.path(), "a.mid", 60);
    let (pb, fb) = write_file(dir.path(), "b.mid", 72);
    assert_ne!(fa.tracks(), fb.tracks());
    let mut s = session();
    let a = s.midi().load_file(pa);
    let b = s.midi().load_file(pb);
    assert_ne!(a, b);
    for (load, file) in land(&mut s, 2, files) {
        let want = if load == a { &fa } else { &fb };
        assert_eq!(file.unwrap().tracks(), want.tracks());
    }
}

/// A spawner that drops the job lands the load as `Job(Dropped)`.
#[test]
fn a_dropped_job_lands_as_a_job_error() {
    let mut s = Session::builder()
        .spawn_with(drop)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let id = s.midi().load_file("never-read.mid");
    let (load, file) = land(&mut s, 1, files).pop().unwrap();
    assert_eq!(load, id);
    assert!(
        matches!(file, Err(MidiError::Job(JobFailed::Dropped))),
        "{file:?}"
    );
}

#[cfg(feature = "soundfont")]
mod soundfont {
    use super::*;
    use tutti_soundfont::{Settings, SoundFontUnit};

    fn fonts(event: MidiEvent) -> Option<(MidiLoad, Result<Box<SoundFontUnit>, MidiError>)> {
        match event {
            MidiEvent::SoundFontLoaded { load, unit } => Some((load, unit)),
            _ => None,
        }
    }

    #[test]
    fn a_soundfont_lands_as_a_unit_with_its_id() {
        let mut s = session();
        let id = s.midi().load_soundfont(
            tutti_fixtures::asset("soundfonts/TimGM6mb.sf2"),
            Settings::new(),
        );
        let (load, unit) = land(&mut s, 1, fonts).pop().unwrap();
        assert_eq!(load, id);
        assert!(unit.is_ok(), "{:?}", unit.err());
    }

    #[test]
    fn a_file_that_is_no_soundfont_lands_as_a_soundfont_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.sf2");
        std::fs::write(&path, b"this is not a RIFF file at all").unwrap();
        let mut s = session();
        let id = s.midi().load_soundfont(path, Settings::new());
        let (load, unit) = land(&mut s, 1, fonts).pop().unwrap();
        assert_eq!(load, id);
        assert!(matches!(unit, Err(MidiError::SoundFont(_))));
    }
}
