//! **Open a device and start the audio thread, with no ECS.**
//!
//! This bootstrap exists nowhere else in the repository. The only other one
//! is `bevy_tutti::engine::build`, which is 388 lines of Bevy systems
//! inserting resources into an `App`. Every *part* below is public and
//! Bevy-free already; nothing assembled them, so a headless host had to read
//! those 388 lines and subtract the ECS to learn the order.
//!
//! That order is the whole content of this file, and it is not arbitrary:
//!
//! 1. **Open the device first.** It reports the rate, and the graph must be
//!    built at that rate — not the other way round. Building a 48 kHz graph
//!    and then opening a 44.1 kHz device is how a project ends up playing
//!    slightly sharp.
//! 2. `Transport`, at that rate.
//! 3. A graph (`GraphBuilder`), prepared at that rate: its `Editor` stays on
//!    the control thread and `commit`s edits across; its `Executor` is the
//!    audio thread's half. The engine drives the transport's clock itself,
//!    so nodes see the playhead in each block's `Env`.
//! 4. `Engine` over the transport, the editor and the executor.
//! 5. `AudioCallbackState`, then `start`.
//! 6. `TuttiDriver::from_parts` to hold the pieces together.
//!
//! **Keep the editor.** It is how the host edits the graph from then on;
//! here it lives in `main`, and a real host stores it beside the driver.
//!
//! Deliberately not wrapped in a `TuttiEngine::builder()`. A builder here is
//! precisely the artifact `4b5bd2fd` deleted — see `src/lib.rs`.
//!
//! Run: `cargo run -p tutti --features device --example headless_engine`
//! (needs a real output device; it plays a 440 Hz tone for two seconds).

use std::sync::Arc;

use tutti::core::{AudioTap, Engine, MasterMeter, Transport};
use tutti::device::{AudioCallbackState, AudioEngine, TuttiDriver};
use tutti::graph::{GraphBuilder, Prepare};
use tutti::prelude::*;
use tutti_core::Hz;
use tutti_nodes::testing::Osc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. The device first: it reports the rate the graph must be built at.
    let mut audio_engine = AudioEngine::new(None)?;
    let sample_rate = audio_engine.sample_rate();
    println!(
        "device: {} at {} Hz, {} channels",
        audio_engine.device_name().unwrap_or_else(|_| "?".into()),
        sample_rate.get(),
        audio_engine.channels().count()
    );

    // Take the fault sink BEFORE anything can go wrong. CPAL's error callback
    // returns nothing, so this handle is the only way a mid-session
    // disconnect reaches you.
    let faults = audio_engine.faults();

    // 2-3. Transport and graph, at the device's rate.
    let transport = Transport::new(sample_rate.get());
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, audio_engine.channels());
    let tone = g.add_unit(Box::new(
        Osc::sine(Hz(440.0)).with_amplitude(Amplitude(0.2)),
    ));
    g.pipe_output(tone);
    let (mut editor, executor) = g.build(Prepare::new(sample_rate, Samples(512)))?;

    // 4-5. The engine (the audio thread's half), and the state its callback
    // reads.
    let engine = Engine::new(&transport, &mut editor, executor)?;
    let state = Arc::new(AudioCallbackState::new(
        engine,
        MasterMeter::new(),
        AudioTap::new(),
    ));
    audio_engine.start(Arc::clone(&state))?;

    // 6. One handle a host stores.
    let driver = TuttiDriver::from_parts(audio_engine, state);

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);

    println!("playing for two seconds…");
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        // What a host's status line would poll. `is_running` goes false on a
        // disconnect, which is the half that used to be silently true.
        if !driver.is_running() {
            if let Some(fault) = faults.last() {
                eprintln!("audio stopped: {} ({:?})", fault.message, fault.kind);
            }
            break;
        }
    }

    // Dropping the driver stops the stream. The editor outlives it: it is
    // the host's handle on the graph for the whole session.
    drop(driver);
    drop(editor);
    Ok(())
}
