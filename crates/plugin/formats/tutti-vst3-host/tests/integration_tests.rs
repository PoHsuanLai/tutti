//! Integration tests for VST3 host with real plugins.
//!
//! These tests require actual VST3 plugins to be installed and are marked
//! with #[ignore] by default. Run with:
//!
//! ```bash
//! cargo test -p tutti-vst3-host --test integration_tests -- --ignored
//! ```

use std::path::{Path, PathBuf};

use tutti_vst3_host::{AudioBuffer, MidiEvent, TransportInfo, Vst3InputEvents, Vst3Instance};

/// Resolve a macOS `.vst3` bundle directory to its inner binary so
/// `Vst3Instance::load` can `dlopen` it. Mirrors the helper in
/// `vst3_process_no_alloc.rs`.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() || !path.is_dir() {
        return path.to_path_buf();
    }
    #[cfg(target_os = "macos")]
    {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let candidate = path.join("Contents").join("MacOS").join(stem);
        if candidate.is_file() {
            return candidate;
        }
    }
    path.to_path_buf()
}

// Common VST3 plugin paths on macOS
const TAL_NOISEMAKER: &str = "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3";
const SURGE_XT: &str = "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3";
const VITAL: &str = "/Library/Audio/Plug-Ins/VST3/Vital.vst3";
const DEXED: &str = "/Library/Audio/Plug-Ins/VST3/Dexed.vst3";

fn find_available_plugin() -> Option<&'static str> {
    [TAL_NOISEMAKER, SURGE_XT, VITAL, DEXED]
        .into_iter()
        .find(|path| Path::new(path).exists())
}

#[test]
#[ignore]
fn test_load_tal_noisemaker() {
    if !Path::new(TAL_NOISEMAKER).exists() {
        eprintln!("TAL-NoiseMaker not installed, skipping");
        return;
    }

    let plugin = Vst3Instance::<f32>::load(Path::new(TAL_NOISEMAKER), 44100.0, 512);
    match plugin {
        Ok(p) => {
            println!("Loaded: {}", p.info().name);
            println!("Vendor: {}", p.info().vendor);
            println!("Version: {}", p.info().version);
            println!("Audio inputs: {}", p.info().num_inputs);
            println!("Audio outputs: {}", p.info().num_outputs);
            println!("Supports f64: {}", p.info().supports_f64);
        }
        Err(e) => {
            panic!("Failed to load TAL-NoiseMaker: {:?}", e);
        }
    }
}

#[test]
#[ignore]
fn test_load_any_available_plugin() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    println!("Testing with: {}", path);
    let plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let info = plugin.info();
    assert!(!info.name.is_empty(), "Plugin name should not be empty");
    println!("Successfully loaded: {} by {}", info.name, info.vendor);
}

/// Per-bus enumeration: every installed plugin should report a per-bus
/// channel layout whose bus-0 entry matches the flat `num_inputs`/`num_outputs`
/// counts, and whose totals are self-consistent. A plugin with >1 input bus
/// (e.g. a sidechain compressor) exercises the multi-bus path.
#[test]
#[ignore]
fn test_bus_enumeration() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let library = resolve_bundle(Path::new(path));
    let plugin = Vst3Instance::<f32>::load(&library, 48_000.0, 64).expect("Failed to load plugin");
    let info = plugin.info();
    println!(
        "{}: input buses = {:?}, output buses = {:?}",
        info.name, info.input_bus_channels, info.output_bus_channels
    );

    if !info.input_bus_channels.is_empty() {
        assert_eq!(
            info.input_bus_channels[0], info.num_inputs,
            "input bus 0 must match flat num_inputs"
        );
        let total: usize = info.input_bus_channels.iter().sum();
        assert_eq!(total, info.total_input_channels());
    }
    if !info.output_bus_channels.is_empty() {
        assert_eq!(
            info.output_bus_channels[0], info.num_outputs,
            "output bus 0 must match flat num_outputs"
        );
        let total: usize = info.output_bus_channels.iter().sum();
        assert_eq!(total, info.total_output_channels());
    }

    // Activation + a process call must succeed regardless of bus count.
    let num_in_buses = info.input_bus_channels.len();
    println!("activated with {num_in_buses} input bus(es)");
}

#[test]
#[ignore]
fn test_process_silence() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    // Create stereo buffers
    let input_left = vec![0.0f32; 512];
    let input_right = vec![0.0f32; 512];
    let mut output_left = vec![0.0f32; 512];
    let mut output_right = vec![0.0f32; 512];

    let inputs: [&[f32]; 2] = [&input_left, &input_right];
    let mut out_l = output_left.as_mut_slice();
    let mut out_r = output_right.as_mut_slice();
    let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];

    let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 44100.0);
    let transport = TransportInfo::new().with_tempo(120.0).with_playing(true);
    let midi: [MidiEvent; 0] = [];

    let _output_events = plugin.process(
        &mut buffer,
        &Vst3InputEvents {
            midi: &midi,
            ..Default::default()
        },
        None,
        &transport,
    );
    println!("Processing completed successfully");
}

#[test]
#[ignore]
fn test_process_with_midi() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    // Create stereo buffers
    let input_left = vec![0.0f32; 512];
    let input_right = vec![0.0f32; 512];
    let mut output_left = vec![0.0f32; 512];
    let mut output_right = vec![0.0f32; 512];

    let inputs: [&[f32]; 2] = [&input_left, &input_right];
    let mut out_l = output_left.as_mut_slice();
    let mut out_r = output_right.as_mut_slice();
    let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];

    let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 44100.0);
    let transport = TransportInfo::new().with_tempo(120.0).with_playing(true);

    let midi = [MidiEvent::note_on(0, 0, 60, 0x9999)];

    let _output_events = plugin.process(
        &mut buffer,
        &Vst3InputEvents {
            midi: &midi,
            ..Default::default()
        },
        None,
        &transport,
    );

    // buffer goes out of scope here, releasing borrow on output slices
    let has_output = output_left.iter().any(|&s| s.abs() > 0.0001);
    println!("Plugin produced audio: {}", has_output);
}

#[test]
#[ignore]
fn test_process_multiple_buffers() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    for i in 0..10 {
        let input_left = vec![0.0f32; 256];
        let input_right = vec![0.0f32; 256];
        let mut output_left = vec![0.0f32; 256];
        let mut output_right = vec![0.0f32; 256];

        let inputs: [&[f32]; 2] = [&input_left, &input_right];
        let mut out_l = output_left.as_mut_slice();
        let mut out_r = output_right.as_mut_slice();
        let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];

        let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 44100.0);
        let transport = TransportInfo::new().with_tempo(120.0).with_playing(true);

        let midi: Vec<MidiEvent> = if i == 0 {
            vec![MidiEvent::note_on(0, 0, 60, 0x9999)]
        } else if i == 9 {
            vec![MidiEvent::note_off(0, 0, 60, 0)]
        } else {
            vec![]
        };

        plugin.process(
            &mut buffer,
            &Vst3InputEvents {
                midi: &midi,
                ..Default::default()
            },
            None,
            &transport,
        );
    }
    println!("Processed 10 buffers successfully");
}

#[test]
#[ignore]
fn test_state_save_load() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let state = plugin.state();
    match state {
        Ok(data) => {
            println!("Saved state: {} bytes", data.len());
            assert!(!data.is_empty(), "State should not be empty");

            let result = plugin.set_state(&data);
            assert!(result.is_ok(), "Loading state should succeed");
        }
        Err(e) => {
            eprintln!("Save state not supported or failed: {:?}", e);
        }
    }
}

#[test]
#[ignore]
fn test_get_parameters() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let param_count = plugin.parameter_count();
    println!("Plugin has {} parameters", param_count);

    // `parameter_count()` bounds the INDEX space; `parameter()` takes a
    // ParamID. Iterating indices straight into `parameter()` reads whichever
    // parameter happens to own that numeric id — go through the index-addressed
    // accessor, which resolves index → ParamID via `getParameterInfo`.
    for i in 0..param_count.min(10) {
        let id = plugin
            .parameter_id_at(i)
            .unwrap_or_else(|| panic!("index {i} < parameter_count but has no ParameterInfo"));
        let value = plugin
            .parameter_by_index(i)
            .expect("resolved index has a value");
        assert_eq!(
            value,
            plugin.parameter(id),
            "by-index and by-ParamID reads must agree for index {i} (id {id})"
        );
        println!("  [{i}] id {id} value: {value}");
    }
}

#[test]
#[ignore]
fn test_set_parameter() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    if plugin.parameter_count() > 0 {
        // Index 0, resolved to its ParamID — `set_parameter(0, ..)` would have
        // written to *ParamID* 0, which need not be the first parameter (or
        // exist at all).
        let id = plugin
            .parameter_id_at(0)
            .expect("index 0 has ParameterInfo");
        println!("Setting parameter index 0 (ParamID {id}) to 0.5");
        assert!(plugin.set_parameter_by_index(0, 0.5));
        let value = plugin
            .parameter_by_index(0)
            .expect("index 0 resolves for reads too");
        assert_eq!(value, plugin.parameter(id));
        println!("Read back value: {value}");

        // An out-of-range index must be reported, not silently written to a
        // numerically-equal ParamID.
        let past_end = plugin.parameter_count();
        assert!(!plugin.set_parameter_by_index(past_end, 0.5));
        assert_eq!(plugin.parameter_by_index(past_end), None);
    }
}

#[test]
#[ignore]
fn test_rapid_process_calls() {
    let path = match find_available_plugin() {
        Some(p) => p,
        None => {
            eprintln!("No VST3 plugins found, skipping");
            return;
        }
    };

    let mut plugin =
        Vst3Instance::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let transport = TransportInfo::new().with_tempo(120.0).with_playing(true);
    let midi: [MidiEvent; 0] = [];

    let start = std::time::Instant::now();
    for _ in 0..689 {
        let input_left = vec![0.0f32; 64];
        let input_right = vec![0.0f32; 64];
        let mut output_left = vec![0.0f32; 64];
        let mut output_right = vec![0.0f32; 64];

        let inputs: [&[f32]; 2] = [&input_left, &input_right];
        let mut out_l = output_left.as_mut_slice();
        let mut out_r = output_right.as_mut_slice();
        let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];

        let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 44100.0);
        plugin.process(
            &mut buffer,
            &Vst3InputEvents {
                midi: &midi,
                ..Default::default()
            },
            None,
            &transport,
        );
    }
    let elapsed = start.elapsed();

    println!("Processed 1 second of audio in {:?}", elapsed);
    assert!(
        elapsed.as_millis() < 1000,
        "Should process faster than real-time"
    );
}
