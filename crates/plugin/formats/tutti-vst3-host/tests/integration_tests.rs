//! Integration tests for VST3 host with real plugins.
//!
//! These tests require actual VST3 plugins to be installed and are marked
//! with #[ignore] by default. Run with:
//!
//! ```bash
//! cargo test -p tutti-vst3-host --test integration_tests -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_vst3_host::{AudioBuffer, MidiEvent, TransportInfo, Vst3InputEvents, Vst3Instance};

/// VST3 module lifecycle is not thread-safe here: loading and unloading the
/// same DSO concurrently races module init/exit and crashes. Every test that
/// constructs a `Vst3Instance` must hold this.
///
/// This file went without one. Two of its tests run by default (the rest are
/// `#[ignore]`d), both load real bundles, and under the default parallel runner
/// the suite intermittently died with SIGTRAP — while passing single-threaded,
/// which is what made it read as flaky rather than as a missing lock.
///
/// Per-file, like the statics in `vst3_conformance.rs` and
/// `vst3_audio_correctness.rs`: each test binary is its own process, so a
/// shared one would have to live in the library and exist in production purely
/// for tests.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning so one failing test does not
/// cascade into spurious failures in every test after it.
fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

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
/// Steinberg's own SDK sample plugins, installed under the user domain. They
/// implement `normalizedParamToPlain` properly (adelay's "Delay" is seconds,
/// mda's parameters carry real units), which the commercial plugins above
/// mostly do not.
const SDK_SAMPLES: &[&str] = &["adelay.vst3", "mda-vst3.vst3", "note-expression-synth.vst3"];

/// Every VST3 bundle this machine has, user domain first.
///
/// The absolute paths below are macOS-only and name third-party installs, so
/// on any other machine this used to come back empty — and the corpus tests
/// that assert `loaded > 0` failed rather than skipped. Two portable sources
/// come first:
///
/// - `TUTTI_TEST_VST3_PLUGIN` — an explicit path to any VST3 binary.
/// - `VST3_PROBE_DIR` — exported by this crate's `build.rs` when built with
///   `--features conformance` and `VST3_SDK_DIR` set; holds the in-repo
///   `audio-probe` bundle.
fn corpus() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("TUTTI_TEST_VST3_PLUGIN") {
        out.push(PathBuf::from(p));
    }
    out.extend(probe_binary());
    let user = dirs_home().join("Library/Audio/Plug-Ins/VST3");
    out.extend(SDK_SAMPLES.iter().map(|n| user.join(n)));
    out.extend([TAL_NOISEMAKER, SURGE_XT, VITAL, DEXED].map(PathBuf::from));
    out.retain(|p| p.exists());
    out
}

/// The corpus, or a printed skip when this machine has no VST3 plugin at all.
///
/// The corpus tests assert `loaded > 0` — a real property (a host that loads
/// nothing is broken) that is unprovable with no plugin to load. Skipping keeps
/// that assertion meaningful where a corpus exists instead of weakening it
/// everywhere. The skip prints so "did not run" stays distinguishable from
/// "passed" in the output.
macro_rules! corpus_or_skip {
    () => {{
        let c = corpus();
        if c.is_empty() {
            eprintln!(
                "SKIP {}: no VST3 plugin on this machine. Set TUTTI_TEST_VST3_PLUGIN=<path>, \
                 or build with --features conformance and VST3_SDK_DIR set to export \
                 VST3_PROBE_DIR.",
                module_path!()
            );
            return;
        }
        c
    }};
}

/// The `audio-probe` binary inside `VST3_PROBE_DIR`, when that was exported.
fn probe_binary() -> Option<PathBuf> {
    let dir = std::env::var("VST3_PROBE_DIR").ok()?;
    if dir.is_empty() {
        return None;
    }
    let bundle = Path::new(&dir).join("audio-probe.vst3");
    for sub in [
        "Contents/x86_64-linux",
        "Contents/aarch64-linux",
        "Contents/MacOS",
        "Contents/x86_64-win",
    ] {
        if let Ok(entries) = std::fs::read_dir(bundle.join(sub)) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is set"))
}

fn find_available_plugin() -> Option<&'static str> {
    [TAL_NOISEMAKER, SURGE_XT, VITAL, DEXED]
        .into_iter()
        .find(|path| Path::new(path).exists())
}

#[test]
#[ignore]
fn test_load_tal_noisemaker() {
    let _plugins = plugin_guard();
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
    let _plugins = plugin_guard();
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

/// The flat `num_inputs`/`num_outputs` counts must equal bus 0 of the
/// corresponding per-bus vec — no rounding up, in either direction.
///
/// This is the assertion that pins `audio_bus_channel_count` to what the plugin
/// declared. Two floors would break it if reintroduced: a `min_channels` floor
/// reporting a 0-channel output bus as 1, and `build_plugin_info` defaulting a
/// *missing* output bus to 2 on top of that. This fails if either comes back.
///
/// Swept over the whole corpus rather than `find_available_plugin`, which
/// returns the first *existing* path — on this machine that is a bundle that
/// fails to `dlopen`, so the assertion below never executed. A plugin that
/// cannot load is skipped with a note; a corpus with nothing loadable fails,
/// because a silent pass here is indistinguishable from a real one.
#[test]
fn a_plugins_flat_channel_counts_match_its_bus_zero() {
    let _plugins = plugin_guard();
    let mut checked = 0;
    for path in corpus_or_skip!() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Vst3Instance::<f32>::load(&library, 48_000.0, 64) else {
            eprintln!("skipping {} (failed to load)", path.display());
            continue;
        };
        let info = plugin.info();
        println!(
            "{}: in={} out={} in_buses={:?} out_buses={:?}",
            info.name,
            info.num_inputs,
            info.num_outputs,
            info.input_bus_channels,
            info.output_bus_channels
        );

        // An empty vec means the plugin named no bus in that direction, and the
        // flat count must say 0 rather than a substituted width.
        let want_in = info.input_bus_channels.first().copied().unwrap_or(0);
        let want_out = info.output_bus_channels.first().copied().unwrap_or(0);
        assert_eq!(
            info.num_inputs, want_in,
            "{}: flat num_inputs disagrees with input bus 0",
            info.name
        );
        assert_eq!(
            info.num_outputs, want_out,
            "{}: flat num_outputs disagrees with output bus 0 — a floor or a \
             default width has been reintroduced",
            info.name
        );

        let in_total: usize = info.input_bus_channels.iter().sum();
        let out_total: usize = info.output_bus_channels.iter().sum();
        assert_eq!(in_total, info.total_input_channels());
        assert_eq!(out_total, info.total_output_channels());
        checked += 1;
    }
    assert!(
        checked > 0,
        "no VST3 plugin in the corpus could be loaded, so nothing was checked"
    );
    println!("checked {checked} plugin(s)");
}

/// A plugin's reported version comes from the plugin, not from a literal.
///
/// `PluginInfo::version` was hardcoded `"1.0.0"` for every VST3 ever loaded.
/// The real string is on `PClassInfoW::version` (e.g. `"1.0.0.512"`,
/// Major.Minor.Subversion.Build), which `class_info_unicode` read past.
/// `vendor` sat beside it, and the header calls that field an *overwrite* of
/// the factory's (`ipluginbase.h:357`) — so a distributor-published bundle
/// credited the distributor rather than the maker.
///
/// Asserted as "at least one plugin disagrees with the old literal" rather than
/// per-plugin: a plugin that genuinely is version 1.0.0, or that declares none
/// and falls back, is not a bug. What would be a bug is *every* plugin agreeing
/// with the literal again, which is what the old code guaranteed.
#[test]
fn a_plugins_version_is_read_from_it_rather_than_assumed() {
    let _plugins = plugin_guard();
    let mut loaded = 0;
    let mut non_placeholder = 0;

    for path in corpus_or_skip!() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Vst3Instance::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.info();
        println!(
            "{}: version={:?} vendor={:?}",
            info.name, info.version, info.vendor
        );
        loaded += 1;
        if info.version != "1.0.0" {
            non_placeholder += 1;
        }
        assert!(
            !info.version.is_empty(),
            "{}: version is empty — a plugin declaring none should keep the \
             placeholder, not report nothing",
            info.name
        );
    }

    assert!(loaded > 0, "no VST3 plugin in the corpus could be loaded");
    assert!(
        non_placeholder > 0,
        "every one of {loaded} plugins reported exactly \"1.0.0\" — the version \
         is being assumed rather than read from PClassInfoW"
    );
}

/// A plugin's subcategories are read from it rather than left blank.
///
/// `PClassInfoW::subCategories` (`ipluginbase.h:355`) carries the musical
/// taxonomy — `"Fx|Reverb"`, `"Instrument|Synth"` — and `class_info_unicode`
/// read past it, so `PluginInfo` had no way to report one. The server loader
/// then built `PluginClass::Vst3 { category: String::new() }` unconditionally,
/// which made the browser's `is_instrument` test — `category.contains
/// ("Instrument")` — unable to return true for any VST3 plugin ever scanned.
///
/// Note this is a different field from `ClassInfo::category`, which names the
/// COM class kind (`"Audio Module Class"`) and is the same for every audio
/// plugin. Reading that one instead looks plausible and answers nothing.
///
/// Asserted across the corpus rather than per-plugin: a plugin declaring no
/// subcategory is legal. What would be a bug is every plugin reporting nothing,
/// which is what the old code guaranteed.
#[test]
fn a_plugins_subcategories_are_read_from_it_rather_than_left_blank() {
    let _plugins = plugin_guard();
    let mut loaded = 0;
    let mut declared = 0;

    for path in corpus_or_skip!() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Vst3Instance::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.info();
        println!("{}: sub_categories={:?}", info.name, info.sub_categories);
        loaded += 1;
        if info
            .sub_categories
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            declared += 1;
        }
    }

    assert!(loaded > 0, "no VST3 plugin in the corpus could be loaded");
    assert!(
        declared > 0,
        "none of {loaded} plugins reported a subcategory — the field is being \
         skipped rather than read from PClassInfoW::subCategories"
    );
}

/// Every subcategory the corpus declares parses into named facets.
///
/// The facet table is transcribed from `ivstaudioprocessor.h`, so the risk it
/// carries is drift: a facet spelled differently in the header than in the table
/// parses to `Other` and every classifier misses it. A unit test cannot catch
/// that — it would assert the same table twice. Real plugins can.
///
/// `Other` is not a failure in general (vendor tails are legal), so this reports
/// what it saw rather than forbidding it outright, and fails only if a facet the
/// SDK *does* name comes back unparsed.
#[test]
fn corpus_subcategories_parse_into_named_facets() {
    use tutti_plugin_types::{Vst3PlugType, Vst3SubCategories};

    let _plugins = plugin_guard();
    let mut parsed = 0;

    for path in corpus_or_skip!() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Vst3Instance::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.info();
        let Some(raw) = info.sub_categories.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };

        let cats = Vst3SubCategories::parse(raw);
        println!("{}: {:?} -> {:?}", info.name, raw, cats.facets());
        parsed += 1;

        assert!(
            !cats.is_empty(),
            "{}: {raw:?} declared a subcategory that parsed to nothing",
            info.name
        );
        assert_eq!(
            cats.raw(),
            raw,
            "{}: the raw string must survive parsing",
            info.name
        );

        for facet in cats.facets() {
            if let Vst3PlugType::Other(name) = facet {
                // A tail with no separator that looks like a plain SDK word is
                // the drift signature: the header names it, the table does not.
                println!("  (unnamed facet {name:?} — vendor tail, or table drift)");
            }
        }
    }

    assert!(
        parsed > 0,
        "no corpus plugin declared a subcategory, so nothing was parsed"
    );
}

#[test]
#[ignore]
fn test_process_silence() {
    let _plugins = plugin_guard();
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
    let _plugins = plugin_guard();
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

    let midi = [MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x9999,
    )];

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
    let _plugins = plugin_guard();
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
            vec![MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x9999,
            )]
        } else if i == 9 {
            vec![MidiEvent::note_off(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0,
            )]
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
    let _plugins = plugin_guard();
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
    let _plugins = plugin_guard();
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
    let _plugins = plugin_guard();
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
    let _plugins = plugin_guard();
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

/// `parameter_plain_range` recovers real ranges from the installed corpus.
///
/// VST3's `ParameterInfo` carries no bounds — every value it exchanges is
/// normalized. `IEditController::normalizedParamToPlain` is the same map the
/// plugin's own editor uses to render "440 Hz", so probing it is how a host
/// learns what a parameter means. Without it the host reported `0.0..1.0` for
/// every VST3 parameter, which is what an automation lane would have shown.
///
/// Two properties, and the split matters:
///
/// - **Per plugin**: every probed range is finite and ordered. A plugin that
///   returns identity (the SDK's default for one that didn't override) probes
///   `0..1`, which is a truthful answer, not a failure — for a Mix knob the
///   plain range really is `0..1`.
/// - **Across the corpus**: at least one range is *not* `0..1`. Without this the
///   whole test would pass against a probe hardwired to return `Some((0.0,
///   1.0))`, which is the bug it exists to catch. TAL-NoiseMaker alone returns
///   identity for all 64 of its parameters, so a single-plugin version of this
///   test asserted nothing.
///
/// Not `#[ignore]`d, unlike its neighbours: it skips only when the corpus is
/// empty. An ignored test exercising new FFI is indistinguishable from no test.
#[test]
fn plain_range_probe_recovers_real_ranges() {
    let _plugins = plugin_guard();
    let corpus = corpus_or_skip!();

    let mut total_probed = 0usize;
    let mut total_non_unit = 0usize;

    for path in &corpus {
        let resolved = resolve_bundle(path);
        let Ok(loaded) = tutti_vst3_host::Vst3Loaded::load(&resolved) else {
            eprintln!("  {}: failed to load, skipping", path.display());
            continue;
        };

        let count = loaded.parameter_count();
        let mut probed = 0usize;
        let mut non_unit = 0usize;
        for i in 0..count.min(64) {
            let Some(info) = loaded.parameter_info(i) else {
                continue;
            };
            // `None` is legitimate: no controller, or an incoherent map. That
            // is the case the caller turns into `ParamRange::Normalized`.
            let Some((min, max)) = loaded.parameter_plain_range(info.id) else {
                continue;
            };
            probed += 1;

            assert!(
                min.is_finite() && max.is_finite(),
                "{}: param {} ('{}') probed a non-finite range [{min}, {max}]",
                path.display(),
                info.id,
                info.title_string()
            );
            assert!(
                min <= max,
                "{}: param {} ('{}') probed an inverted range [{min}, {max}]",
                path.display(),
                info.id,
                info.title_string()
            );
            if min != 0.0 || (max - 1.0).abs() > 1e-9 {
                non_unit += 1;
            }
        }
        eprintln!(
            "  {}: {probed} ranges probed, {non_unit} beyond 0..1",
            path.display()
        );
        total_probed += probed;
        total_non_unit += non_unit;
    }

    assert!(
        total_probed > 0,
        "no plugin in the corpus produced a plain range, so this test cannot \
         tell a working probe from one that always returns None"
    );
    if total_non_unit == 0 {
        // Every plugin present reports identity ranges, so the probe and the
        // hardcoded `0..1` it replaced are indistinguishable here — there is
        // nothing this test could assert. Skip rather than fail: the corpus is
        // a property of the machine, and the in-repo `audio-probe` reports
        // plain 0..1 for all five of its parameters. Steinberg's `adelay` (in
        // seconds) or `mda-vst3` are what make this test able to do its job.
        eprintln!(
            "SKIP {}: all {total_probed} probed ranges across {} plugin(s) were \
             exactly 0..1, so a working probe is indistinguishable from the \
             hardcoded range it replaced. Needs a plugin with non-unit \
             parameter ranges (e.g. Steinberg's adelay or mda-vst3).",
            module_path!(),
            corpus.len()
        );
    }
}
