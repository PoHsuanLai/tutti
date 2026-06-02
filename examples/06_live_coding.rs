//! # 06 - Live Coding
//!
//! Dynamically update the audio graph while playing.
//!
//! **Concepts:** Real-time graph updates, hot-swapping DSP
//!
//! ```bash
//! cargo run --example 06_live_coding
//! ```

use std::io::{self, Write};
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    engine.graph.master(sine_hz::<f64>(440.0) * 0.5);
    engine.graph.commit();

    engine.transport.play();
    println!("1=sine 2=saw 3=square 4=noise 5=chord q=quit");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim() {
            "1" => {
                engine.graph.master(sine_hz::<f64>(440.0) * 0.5);
                engine.graph.commit();
            }
            "2" => {
                engine.graph.master(saw_hz(220.0) * 0.3);
                engine.graph.commit();
            }
            "3" => {
                engine.graph.master(square_hz(330.0) * 0.3);
                engine.graph.commit();
            }
            "4" => {
                engine.graph.master(pink::<f64>() * 0.2);
                engine.graph.commit();
            }
            "5" => {
                let c = sine_hz::<f64>(261.63) * 0.2;
                let e = sine_hz::<f64>(329.63) * 0.2;
                let g = sine_hz::<f64>(392.00) * 0.2;
                engine.graph.master(c + e + g);
                engine.graph.commit();
            }
            "q" => break,
            _ => println!("?"),
        }
    }

    Ok(())
}
