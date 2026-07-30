//! Recording the master output: the capture path that had no expression.
//!
//! `AudioTap` is the engine's lock-free copy of what reaches the speakers, and
//! `AudioPump` is the ECS-owned pump that drives a source into a sink. Both
//! shipped, and "record what I am hearing" still did not compile: `open()` hands
//! back a bare `HeapCons<(f32, f32)>`, and `AudioPump::start` wants an
//! `AudioIn`. The bound simply did not hold.
//!
//! `TapIn` is the join. These pin that it holds, and that audio survives the
//! trip — a type that satisfied the bound but dropped every frame would compile
//! just as well.

#![cfg(feature = "audio-io")]

use std::path::PathBuf;

use bevy_app::prelude::*;
use bevy_tutti::graph::{AudioPump, AudioPumpAppExt, AudioTapRes};
use bevy_tutti::io::{BitDepth, ChannelLayout, TapIn, WavOut};

const SAMPLE_RATE: f64 = 48_000.0;

fn sink(path: &PathBuf) -> WavOut {
    WavOut::create(path, SAMPLE_RATE, ChannelLayout::Stereo, BitDepth::Float32)
        .expect("sink should open")
}

/// An app with the stereo-`f32` pump drain registered — no engine, no device.
fn app() -> App {
    let mut app = App::new();
    app.add_audio_pump::<f32>();
    app
}

/// Run frames until every pump has drained, or give up.
///
/// The pump is on a real thread, so the drain needs wall clock to advance.
fn run_until_drained(app: &mut App, max_frames: usize) -> bool {
    for _ in 0..max_frames {
        app.update();
        if app
            .world_mut()
            .query::<&AudioPump>()
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

/// The headline: a tap feeds a pump, and what the audio callback pushed lands
/// in the file.
///
/// The push here stands in for `meter_output`, which is what calls
/// `AudioTap::push` on the real RT path — interleaved stereo, straight from the
/// output buffer.
#[test]
fn what_the_callback_pushed_reaches_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("master.wav");

    let tap = AudioTapRes::default();
    let src = TapIn::new(tap.open().expect("a fresh tap opens"));

    let mut app = app();
    app.world_mut()
        .spawn(AudioPump::start(src, sink(&path), 512));

    // Play the audio callback: interleaved stereo, the shape `push` takes.
    let block: Vec<f32> = (0..256).flat_map(|i| [i as f32 / 256.0, -1.0]).collect();
    tap.push(&block, 256);

    // Give the pump a pass at the ring before stopping it.
    for _ in 0..4 {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for pump in app.world_mut().query::<&AudioPump>().iter(app.world()) {
        pump.stop();
    }
    assert!(run_until_drained(&mut app, 200), "pump should finish");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.spec().channels, 2);

    let samples: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.expect("sample"))
        .collect();
    assert!(
        !samples.is_empty(),
        "the tap fed the pump but nothing was written"
    );

    // Right channel is a constant -1.0, so a channel swap or an off-by-one in
    // the (f32, f32) -> [f32; 2] conversion shows up here rather than passing.
    for (i, pair) in samples.chunks_exact(2).enumerate().take(64) {
        assert!(
            (pair[0] - i as f32 / 256.0).abs() < 1e-6,
            "frame {i} left: expected ramp, got {}",
            pair[0]
        );
        assert!(
            (pair[1] + 1.0).abs() < 1e-6,
            "frame {i} right: expected -1.0, got {}",
            pair[1]
        );
    }
}

/// A closed tap starves the pump rather than ending it.
///
/// `TapIn::ON_EMPTY` is `Starved`, so a pump on a silent graph keeps waiting.
/// Were it `EndOfStream`, a recording armed a moment before playback started
/// would finalize an empty file and look like a bug in the sink.
#[test]
fn a_silent_graph_does_not_end_the_recording() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("silent.wav");

    let tap = AudioTapRes::default();
    let src = TapIn::new(tap.open().expect("a fresh tap opens"));

    let mut app = app();
    app.world_mut()
        .spawn(AudioPump::start(src, sink(&path), 512));

    // Nothing pushed: the graph is running but silent.
    for _ in 0..6 {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let still_running = app
        .world_mut()
        .query::<&AudioPump>()
        .iter(app.world())
        .next()
        .is_some();
    assert!(
        still_running,
        "an empty tap means 'nothing yet', not 'finished' — the pump must \
         still be waiting"
    );

    for pump in app.world_mut().query::<&AudioPump>().iter(app.world()) {
        pump.stop();
    }
    assert!(run_until_drained(&mut app, 200), "pump should finish");
}
