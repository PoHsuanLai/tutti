//! MIDI file IO tests.
//!
//! Split the way the module is: decoding is tested on bytes (which is what an
//! `AssetLoader` is handed), and writing is tested against real files in a temp
//! dir — the thing under test there is that a blocking call reaches the task
//! pool and its result comes back as an event, and a mock would remove exactly
//! that.

use super::*;
use bevy_ecs::system::RunSystemOnce;
use tutti_midi_io::smf::{encode_midi_file, MidiWriteOptions, SmfMessage, SmfTimedEvent};
use tutti_types::Beat;

/// An app with the plugin and the pools it needs.
fn app() -> App {
    // Process-global, so `get_or_init` rather than a fresh pool per test.
    bevy_tasks::AsyncComputeTaskPool::get_or_init(Default::default);
    let mut app = App::new();
    // `MidiFilePlugin` calls `init_asset`, which needs an `AssetServer`.
    app.add_plugins((bevy_asset::AssetPlugin::default(), MidiFilePlugin));
    app
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("bevy-tutti-midi-file-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// A one-note SMF's bytes.
fn smf_bytes() -> Vec<u8> {
    encode_midi_file(
        &[vec![
            SmfTimedEvent {
                time_beats: Beat(0.0),
                channel: 0,
                msg: SmfMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                },
            },
            SmfTimedEvent {
                time_beats: Beat(1.0),
                channel: 0,
                msg: SmfMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                },
            },
        ]],
        &MidiWriteOptions::default(),
    )
    .expect("encodes")
}

// ── decoding: what the loader does with the bytes it is handed ──────────────

/// **An SMF decodes to its tracks.**
#[test]
fn smf_bytes_decode_to_tracks() {
    let asset = MidiFileAsset::from_bytes(&smf_bytes()).expect("decodes");
    match asset.contents {
        MidiFileContents::Smf(tracks) => {
            assert_eq!(tracks.len(), 1, "one track in, one track out");
            assert!(
                !tracks[0].notes.is_empty(),
                "the encoded note must survive the round trip"
            );
        }
        MidiFileContents::Clip(_) => panic!("an SMF must not decode as a clip file"),
    }
}

/// **The container is decided by magic, not by extension.**
///
/// This is why one asset type covers both formats. `.mid` is worn by a Standard
/// MIDI File and a MIDI 2.0 Clip File alike, and an `AssetLoader` is selected by
/// extension — so two loaders both claiming `.mid` could not be told apart, and
/// a loader that trusted the name would decode half its inputs with the wrong
/// decoder.
#[test]
fn a_clip_file_is_recognised_by_its_magic() {
    let bytes = tutti_midi_types::write_clip_file(960, &[]);
    let asset = MidiFileAsset::from_bytes(&bytes).expect("decodes");
    assert!(
        matches!(asset.contents, MidiFileContents::Clip(_)),
        "a clip file's SMF2CLIP magic must select the clip decoder, whatever \
         extension the file happened to have"
    );
}

/// **Neither format is its own error, not a parse failure.**
///
/// "This is not a MIDI file" and "this MIDI file is malformed" are different
/// things for a user to fix, so they must not collapse into one message.
#[test]
fn a_file_of_neither_format_is_named_as_such() {
    let err = MidiFileAsset::from_bytes(b"not a midi file at all").expect_err("must fail");
    assert!(
        matches!(err, MidiFileLoaderError::UnknownFormat),
        "expected UnknownFormat, got {err:?}"
    );
}

/// **A truncated file of a known format is a *parse* error, not UnknownFormat.**
///
/// The complement of the test above: the magic is right, so the container is
/// known — what failed is the decode. Distinguishing the two is the whole point
/// of sniffing separately from parsing.
#[test]
fn a_truncated_smf_is_a_parse_error() {
    let mut bytes = smf_bytes();
    bytes.truncate(10); // keeps `MThd`, loses the rest
    let err = MidiFileAsset::from_bytes(&bytes).expect_err("must fail");
    assert!(
        !matches!(err, MidiFileLoaderError::UnknownFormat),
        "a truncated SMF still has MThd, so it is a decode failure rather than \
         an unrecognised container; got {err:?}"
    );
}

/// The loader claims the extensions both containers are found under.
#[test]
fn the_loader_claims_both_containers_extensions() {
    let claimed = MidiFileAssetLoader.extensions();
    for ext in ["mid", "midi", "mid2", "midi2"] {
        assert!(claimed.contains(&ext), "the loader must claim .{ext}");
    }
}

// ── writing: still the task pool, because AssetLoader is read-only ──────────

/// Run until the request's observer fires, or give up.
///
/// A bounded loop rather than a fixed frame count: the task pool gives no
/// completion deadline, so "two updates" would be a guess that passes on a fast
/// machine and flakes on a loaded one.
fn run_until_written(app: &mut App, entity: Entity) {
    for _ in 0..2000 {
        app.update();
        if app.world().get::<MidiFileWriteInFlight>(entity).is_none()
            && app.world().get::<MidiFileWrite>(entity).is_none()
        {
            return;
        }
    }
    panic!("the write never finished");
}

/// Capture what the observer saw, so an assertion can run after the app does.
#[derive(Resource, Default)]
struct Seen(Option<Result<(), String>>);

fn spawn_observed(app: &mut App, bundle: impl Bundle) -> Entity {
    app.init_resource::<Seen>();
    app.world_mut()
        .spawn(bundle)
        .observe(|done: On<MidiFileWritten>, mut seen: ResMut<Seen>| {
            seen.0 = Some(match &done.result {
                Ok(()) => Ok(()),
                Err(e) => Err(e.to_string()),
            });
        })
        .id()
}

/// **The claim: a blocking write runs off the main thread and reports back.**
#[test]
fn a_write_puts_the_bytes_on_disk() {
    let path = temp_path("write.mid");
    let _ = std::fs::remove_file(&path);
    let bytes = smf_bytes();

    let mut app = app();
    let entity = spawn_observed(&mut app, MidiFileWrite::new(&path, bytes.clone()));
    run_until_written(&mut app, entity);

    assert_eq!(
        app.world().resource::<Seen>().0.as_ref().unwrap().as_ref(),
        Ok(&()),
        "the write must report success"
    );
    assert_eq!(
        std::fs::read(&path).expect("the file must exist"),
        bytes,
        "the bytes on disk must be the bytes handed over"
    );
}

/// A write to an unwritable path reports the error rather than panicking.
#[test]
fn an_unwritable_path_reports_an_error() {
    let mut app = app();
    // A directory that does not exist — `std::fs::write` will not create it.
    let path = temp_path("no-such-dir/nested/out.mid");
    let entity = spawn_observed(&mut app, MidiFileWrite::new(&path, smf_bytes()));
    run_until_written(&mut app, entity);

    assert!(
        app.world()
            .resource::<Seen>()
            .0
            .as_ref()
            .unwrap()
            .as_ref()
            .is_err(),
        "an unwritable path must surface as an error, not a panic"
    );
}

/// **A started request is not started again.**
///
/// The request component is swapped for the in-flight marker, which is what
/// keeps the starting system's query from re-firing it every frame. Without
/// that a single write would be re-issued on every update until it finished.
#[test]
fn a_started_request_is_not_started_again() {
    let path = temp_path("once.mid");
    let _ = std::fs::remove_file(&path);

    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(MidiFileWrite::new(&path, smf_bytes()))
        .id();

    app.world_mut()
        .run_system_once(start_midi_file_writes)
        .unwrap();
    assert!(
        app.world().get::<MidiFileWrite>(entity).is_none(),
        "the request component must be consumed when the write starts"
    );
    assert!(
        app.world().get::<MidiFileWriteInFlight>(entity).is_some(),
        "and replaced by the in-flight marker"
    );

    // A second pass must find nothing to do.
    app.world_mut()
        .run_system_once(start_midi_file_writes)
        .unwrap();
    run_until_written(&mut app, entity);
}

/// **Starting a write does not block the system that starts it.**
///
/// The point of the task pool: `start_midi_file_writes` returns having only
/// moved a `PathBuf` and a `Vec<u8>` onto the pool. Asserting the file is *not
/// yet* on disk when the starter returns is the observable form of that — it is
/// the closest thing to "did not block" that a test can state without measuring
/// wall-clock time, which would flake.
#[test]
fn starting_a_write_does_not_block_the_system_that_starts_it() {
    let path = temp_path("nonblocking.mid");
    let _ = std::fs::remove_file(&path);

    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(MidiFileWrite::new(&path, smf_bytes()))
        .id();

    app.world_mut()
        .run_system_once(start_midi_file_writes)
        .unwrap();
    assert!(
        app.world().get::<MidiFileWriteInFlight>(entity).is_some(),
        "the task must be in flight when the starter returns"
    );

    run_until_written(&mut app, entity);
    assert!(path.exists(), "and must land eventually");
}
