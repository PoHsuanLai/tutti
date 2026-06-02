//! # 22 - Plugin Loading
//!
//! Load and use VST3/CLAP plugins in the audio graph.
//!
//! **Concepts:** `tutti::plugin::vst3()`, out-of-process plugin hosting
//!
//! ```bash
//! cargo run --example 22_plugin_loading --features plugin
//! ```
//!
//! ## Setup
//!
//! Install a free VST3 plugin:
//! - [Dragonfly Reverb](https://github.com/michaelwillis/dragonfly-reverb/releases)
//! - [Surge XT](https://github.com/surge-synthesizer/releases-xt/releases)

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let engine = TuttiEngine::builder().build()?;

    let plugin_paths = [
        "/Library/Audio/Plug-Ins/VST3/DragonflyRoomReverb.vst3",
        "/usr/lib/vst3/DragonflyRoomReverb.vst3",
        "assets/plugins/DragonflyRoomReverb.vst3",
    ];

    let mut reverb_unit = None;
    #[cfg(feature = "vst3")]
    for path in &plugin_paths {
        if std::path::Path::new(path).exists() {
            if let Ok((unit, _handle)) = tutti::plugin::vst3(engine.sample_rate, path).build() {
                println!("Loaded: {}", path);
                reverb_unit = Some(unit);
                break;
            }
        }
    }
    let _ = plugin_paths; // suppress unused warning when vst3 feature is off

    let reverb_unit = match reverb_unit {
        Some(unit) => unit,
        None => {
            println!("No plugin found. Install DragonflyRoomReverb or adjust plugin_paths.");
            return Ok(());
        }
    };

    let mut engine = engine;
    let sine_id = engine.graph.add(sine_hz::<f32>(440.0) * 0.3f32);
    let reverb = engine.graph.add_boxed(reverb_unit);
    engine.graph.pipe_all(sine_id, reverb);
    engine.graph.pipe_output(reverb);
    engine.graph.commit();

    println!("Playing sine -> reverb...");
    std::thread::sleep(Duration::from_secs(5));

    Ok(())
}
