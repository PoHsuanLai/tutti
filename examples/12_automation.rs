//! # 12 - Automation
//!
//! Automate a synth pad's volume over time using an automation envelope.
//! The automation lane reads the transport beat position and outputs a control
//! signal that modulates the pad amplitude.
//!
//! **Concepts:** AutomationEnvelope, AutomationPoint, CurveType, AutomationLane, graph routing
//!
//! ```bash
//! cargo run --example 12_automation --features automation
//! ```

use std::time::Duration;
use tutti::automation::{AutomationEnvelope, AutomationLane, AutomationPoint, CurveType};
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // Automation envelope: fade in -> hold -> fade out
    //
    //   1.0 |        ___________
    //       |      /             \
    //   0.5 |    /                 \
    //       |  /                     \
    //   0.0 |/________________________\___
    //       0    4    8         16   20  24
    //                  beats
    let mut envelope = AutomationEnvelope::new("volume");
    envelope.add_point(AutomationPoint::new(0.0, 0.0));
    envelope.add_point(AutomationPoint::with_curve(4.0, 1.0, CurveType::SCurve));
    envelope.add_point(AutomationPoint::new(8.0, 1.0));
    envelope.add_point(AutomationPoint::new(16.0, 1.0));
    envelope.add_point(AutomationPoint::with_curve(20.0, 0.0, CurveType::SCurve));

    println!("Envelope shape:");
    for beat in [0.0, 2.0, 4.0, 8.0, 12.0, 16.0, 18.0, 20.0] {
        let lane_preview = AutomationLane::new(envelope.clone(), engine.transport.clone());
        println!(
            "  Beat {:5.1}: {:.2}",
            beat,
            lane_preview.get_value_at(beat)
        );
    }

    let lane = AutomationLane::new(envelope, engine.transport.clone());

    // Build the graph:
    //   pad (saw chord) -- -- --+
    //                           +-- multiply --> master output
    //   automation lane --------+
    //
    // The multiplier (pass() * pass()) takes 2 inputs and outputs their product.
    let pad = (saw_hz(220.0) + saw_hz(330.0) + saw_hz(440.0)) * 0.15;
    let mult = pass() * pass();

    let pad_id = engine.graph.add(pad);
    let lane_id = engine.graph.add(lane);
    let mult_id = engine.graph.add(mult);

    // pad -> mult input 0 (audio signal)
    engine.graph.connect(pad_id, 0, mult_id, 0);
    // automation -> mult input 1 (volume envelope)
    engine.graph.connect(lane_id, 0, mult_id, 1);
    // mult -> master output
    engine.graph.pipe_output(mult_id);

    engine.graph.commit();

    engine.transport.tempo(120.0).play();
    println!("\nPlaying: saw chord with automated volume (120 BPM, 20 beats = 10s)");
    println!("  0-4 beats:   fade in (S-curve)");
    println!("  4-16 beats:  full volume");
    println!("  16-20 beats: fade out (S-curve)");

    std::thread::sleep(Duration::from_secs(10));
    println!("Done.");

    Ok(())
}
