//! # 05 - Effect Chain
//!
//! Process audio through multiple effects: oscillator -> filter -> reverb.
//!
//! **Concepts:** Effect nodes, audio graph routing, signal flow
//!
//! ```bash
//! cargo run --example 05_effect_chain
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // Build the chain via fundsp operators, then hand the composed unit to
    // the graph. The inner `split::<U2>()` expands the mono signal to stereo
    // before the stereo reverb.
    let source = saw_hz(110.0) * 0.3;
    let filter = lowpole_hz::<f64>(800.0);
    let reverb = reverb_stereo(10.0, 2.0, 0.5);

    engine
        .graph
        .master(source >> filter >> split::<U2>() >> reverb);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing: saw -> lowpass -> reverb...");
    std::thread::sleep(Duration::from_secs(5));

    Ok(())
}
