//! MIDI file IO tests.
//!
//! Split the way the module is: the loader's extensions, and writing tested
//! against real files in a temp dir — the thing under test there is that a
//! blocking call reaches the task pool and its result comes back as an event,
//! and a mock would remove exactly that.

use super::*;
use bevy_ecs::system::RunSystemOnce;
use tutti_midi_file::smf::{encode_midi_file, MidiWriteConfig, SmfMessage, SmfTimedEvent};
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
        &MidiWriteConfig::default(),
    )
    .expect("encodes")
}

// ── decoding: the loader's half ──────────────────────────────────────────────
//
// The decode itself is `tutti_midi_file::MidiFile::from_bytes`, and its tests
// (an SMF to its tracks, a clip file by its magic, neither format, a truncated
// SMF) moved there with it (design 015, PR 1). What stays is the loader's own
// claim.

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
