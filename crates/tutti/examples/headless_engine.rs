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
//! 3. A `Net`, with a `TransportClock` pushed into it so nodes can see the
//!    playhead.
//! 4. `net.backend()` — the audio thread's half. The control thread keeps
//!    `net` and `commit`s edits across.
//! 5. `Engine`, then `AudioCallbackState`, then `start`.
//! 6. `TuttiDriver::from_parts` to hold the pieces together.
//!
//! **The `net` must outlive the backend**, which borrows through it. Here it
//! lives in `main`; a real host stores it beside the driver.
//!
//! Deliberately not wrapped in a `TuttiEngine::builder()`. A builder here is
//! precisely the artifact `4b5bd2fd` deleted — see `src/lib.rs`.
//!
//! Run: `cargo run -p tutti --features device --example headless_engine`
//! (needs a real output device; it plays a 440 Hz tone for two seconds).

use std::sync::Arc;

use tutti::core::{AudioTap, Engine, MasterMeter, Transport, TransportClock};
use tutti::device::{AudioCallbackState, AudioEngine, TuttiDriver};
use tutti::dsp::{sine_hz, Net};
use tutti::prelude::*;

fn main() -> tutti::device::Result<()> {
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
    let mut net = Net::new(0, audio_engine.channels().count() as usize);
    net.push(Box::new(TransportClock::new(
        transport.clock_links(),
        sample_rate.get(),
    )));
    let tone = net.push(Box::new(sine_hz::<f32>(440.0) * 0.2));
    net.pipe_output(tone);

    // 4-5. The audio thread's half, and the state its callback reads.
    let engine = Engine::new(transport.motion.clone(), net.backend());
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

    // Dropping the driver stops the stream. `net` is still alive here, which
    // is what the backend required all along.
    drop(driver);
    drop(net);
    Ok(())
}
