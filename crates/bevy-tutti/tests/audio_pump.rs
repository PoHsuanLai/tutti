//! The pump's lifetime, which is the only thing this layer owns.
//!
//! `pump` itself is the engine's and is tested there. What is asserted here is
//! that the sink gets finalized **exactly once, on every path out** — because
//! `AudioOut::finalize` consumes `self`, can fail, and for a WAV decides whether
//! the file opens at all. A dropped finalize is not a degraded recording, it is
//! an unreadable one.
//!
//! Every test writes a real `WavOut` into a `tempfile` and reads it back with
//! `hound`, so "was it finalized" is answered by the file rather than by a flag
//! this crate set. No audio device is involved.

#![cfg(feature = "sampler")]

use std::path::PathBuf;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioPump, AudioPumpAppExt, PumpFinished};
use tutti_core::io::{AudioIn, OnEmpty};
use tutti_sampler::capture::CaptureFormat;
use tutti_sampler::WavOut;

const SAMPLE_RATE: f64 = 48_000.0;

/// A finite source: hands out its frames in bounded chunks, then `0` forever.
/// The shape a decoded file has.
struct SliceSource {
    frames: Vec<[f32; 2]>,
    pos: usize,
}

impl AudioIn for SliceSource {
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        let n = (self.frames.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

/// A live source: yields a little, then reports empty *without being
/// exhausted*, the way a mic ring does between callback pushes.
///
/// This is the fixture that makes `OnEmpty` observable — a pump that read its
/// `0` as end-of-stream would stop here with frames still to come.
struct LiveSource {
    polls: usize,
}

impl AudioIn for LiveSource {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        self.polls += 1;
        // Every other poll is empty; the rest yield one frame.
        if self.polls % 2 == 1 {
            return 0;
        }
        if out.is_empty() {
            return 0;
        }
        out[0] = [0.5, -0.5];
        1
    }
}

fn frames(n: usize) -> Vec<[f32; 2]> {
    (0..n)
        .map(|i| [i as f32 / n as f32, -(i as f32) / n as f32])
        .collect()
}

fn sink(path: &PathBuf) -> WavOut {
    WavOut::create(path, SAMPLE_RATE, 2, CaptureFormat::F32).expect("sink should open")
}

/// An app with the stereo-`f32` pump drain registered. No engine, no device —
/// the pump is deliberately not gated on `engine_ready`.
fn app() -> App {
    let mut app = App::new();
    app.add_audio_pump::<f32, 2>();
    app
}

/// Run frames until every pump has been drained, or give up.
///
/// The pump is on a real thread, so the drain needs the wall clock to advance;
/// this is the join point, not a poll-until-true hack.
fn run_until_drained(app: &mut App, max_frames: usize) -> bool {
    for _ in 0..max_frames {
        app.update();
        if app
            .world_mut()
            .query::<&AudioPump<f32, 2>>()
            .iter(app.world())
            .next()
            .is_none()
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

fn read_back(path: &PathBuf) -> hound::WavReader<std::io::BufReader<std::fs::File>> {
    hound::WavReader::open(path).expect("a finalized WAV must be readable")
}

/// A finite source drains itself: the pump exits on end-of-stream, finalizes,
/// and every frame reaches the file.
///
/// No `stop()` call anywhere — the source's own `ON_EMPTY` ends it.
#[test]
fn a_finite_source_finalizes_without_being_told_to_stop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("finite.wav");

    let mut app = app();
    let data = frames(3000);
    let src = SliceSource {
        frames: data.clone(),
        pos: 0,
    };
    app.world_mut()
        .spawn(AudioPump::start(src, sink(&path), 256));

    assert!(
        run_until_drained(&mut app, 200),
        "a finite source must end on its own"
    );

    let reader = read_back(&path);
    assert_eq!(reader.spec().channels, 2);
    // Two samples (L, R) per stereo frame.
    assert_eq!(
        reader.len() as usize,
        data.len() * 2,
        "every frame the source held must reach the file"
    );
}

/// A live source runs until stopped, then finalizes.
///
/// Its empty polls must not be mistaken for the end — the fixture is never
/// exhausted, so a pump that stopped at the first `0` would write far less than
/// it had time to.
#[test]
fn a_live_source_runs_until_stopped_then_finalizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wav");

    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(AudioPump::start(LiveSource { polls: 0 }, sink(&path), 64))
        .id();

    // Let it move some frames across several park cycles.
    for _ in 0..8 {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    app.world()
        .entity(entity)
        .get::<AudioPump<f32, 2>>()
        .expect("a live pump must still be running — it is never exhausted")
        .stop();

    assert!(
        run_until_drained(&mut app, 200),
        "a stopped pump must be joined and drained"
    );

    let reader = read_back(&path);
    assert!(
        reader.len() > 0,
        "a live source polled across many parks must have written something; \
         zero means its empty polls were read as end-of-stream"
    );
}

/// Despawning mid-pump still finalizes the sink.
///
/// This is the observer's entire reason to exist: the entity leaves the drain's
/// query, so nothing else can ever join that thread, and the sink it owns would
/// never be closed — leaving a WAV whose header was never patched.
#[test]
fn despawning_mid_pump_still_produces_a_readable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("despawned.wav");

    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(AudioPump::start(LiveSource { polls: 0 }, sink(&path), 64))
        .id();

    for _ in 0..4 {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // No `stop()` — the despawn is the only signal.
    app.world_mut().entity_mut(entity).despawn();
    app.update();

    let reader = read_back(&path);
    assert!(
        reader.len() > 0,
        "the despawn must finalize the sink, not detach the thread"
    );
}

/// The finalize result reaches the world as a message, so a host can react to a
/// sink that failed to close.
#[test]
fn a_finished_pump_reports_through_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reported.wav");

    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(AudioPump::start(
            SliceSource {
                frames: frames(500),
                pos: 0,
            },
            sink(&path),
            256,
        ))
        .id();

    assert!(run_until_drained(&mut app, 200), "the pump should finish");

    let messages = app.world().resource::<Messages<PumpFinished>>();
    let mut cursor = messages.get_cursor();
    let reported: Vec<&PumpFinished> = cursor.read(messages).collect();

    assert_eq!(reported.len(), 1, "exactly one report per pump");
    assert_eq!(
        reported[0].entity, entity,
        "naming the entity that carried it"
    );
    assert!(
        reported[0].result.is_ok(),
        "a WAV sink over a temp file should finalize cleanly: {:?}",
        reported[0].result
    );
}

/// Registering the same frame type twice drains once.
///
/// `add_systems` does not deduplicate, so a host and a library plugin both
/// asking for `<f32, 2>` would otherwise schedule two drains — and two
/// `PumpFinished` reports for one pump.
#[test]
fn registering_a_frame_type_twice_still_drains_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup.wav");

    let mut app = App::new();
    app.add_audio_pump::<f32, 2>();
    app.add_audio_pump::<f32, 2>(); // the duplicate a second plugin would add

    app.world_mut().spawn(AudioPump::start(
        SliceSource {
            frames: frames(500),
            pos: 0,
        },
        sink(&path),
        256,
    ));

    assert!(run_until_drained(&mut app, 200), "the pump should finish");

    let messages = app.world().resource::<Messages<PumpFinished>>();
    let mut cursor = messages.get_cursor();
    assert_eq!(
        cursor.read(messages).count(),
        1,
        "one pump must report once however many times its frame type was registered"
    );
}

/// The README's registration + spawn shape, compiled.
///
/// A README example that does not build is the defect this whole commit is
/// about — the file documented a recording API that never existed. Pinning the
/// shape here means the next rename breaks a test rather than the docs.
#[test]
fn the_documented_shape_compiles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("documented.wav");

    let mut app = App::new();
    app.add_audio_pump::<f32, 2>();

    let src = SliceSource {
        frames: frames(128),
        pos: 0,
    };
    let wav =
        WavOut::create(&path, SAMPLE_RATE, 2, CaptureFormat::F32).expect("could not create WAV");
    let pump = app.world_mut().spawn(AudioPump::start(src, wav, 1024)).id();

    // The stop path a host writes, through the component.
    if let Some(p) = app.world().entity(pump).get::<AudioPump<f32, 2>>() {
        p.stop();
    }

    assert!(
        run_until_drained(&mut app, 200),
        "the documented pump drains"
    );
}
