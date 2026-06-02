//! # 01 - Hello Sine
//!
//! Generate a 440Hz sine wave and play it through the default audio device.
//!
//! **Concepts:** Engine setup, audio graph basics, transport
//!
//! ```bash
//! cargo run --example 01_hello_sine
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    engine.graph.master(sine_hz::<f64>(440.0) * 0.5);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing 440Hz sine...");
    std::thread::sleep(Duration::from_secs(3));

    Ok(())
}
