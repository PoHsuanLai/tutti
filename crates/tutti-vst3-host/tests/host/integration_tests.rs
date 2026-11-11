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
use tutti_fixtures::main_thread;
use tutti_graph::UmpAt;

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_plugin_types::{Control, Normalized, ParamId};
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_vst3_host::{AudioBuffer, InputEvents, TransportInfo};

use crate::active;
#[allow(unused_imports)]
use active::{Activate, Active, ActiveExt, LoadError, ParamExt, StateExt};

/// Serialises every test that constructs an `Active`, which load real
/// third-party plugins (macOS only; they skip elsewhere). `Module::open` and
/// a module's teardown serialise themselves, and a plugin's own instance
/// creation is not known to need this. Third-party code has process globals
/// we cannot see, so the guard stays until measured against those plugins.
///
/// Per-file: each test binary is its own process.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning so one failing test does not
/// cascade into spurious failures in every test after it.
fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Resolve a macOS `.vst3` bundle directory to its inner binary so
/// `Vst3Active::load` can `dlopen` it. Mirrors the helper in
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

/// Every VST3 bundle available here — the in-repo probe first, then whatever
/// this machine happens to have installed.
///
/// The absolute paths below are macOS-only and name third-party installs, so on
/// any other machine they contribute nothing. The in-repo `audio-probe` is
/// what makes the corpus non-empty everywhere; the rest only widen coverage:
///
/// - `built_binaries()` — `audio-probe`, compiled by this crate's build script
///   from the SDK submodules. Always present.
/// - `TUTTI_TEST_VST3_PLUGIN` — an explicit path to any VST3 binary, for
///   running these against a real third-party plugin.
fn corpus() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("TUTTI_TEST_VST3_PLUGIN") {
        out.push(PathBuf::from(p));
    }
    out.extend(built_binaries());
    if let Some(user) = dirs_home().map(|h| h.join("Library/Audio/Plug-Ins/VST3")) {
        out.extend(SDK_SAMPLES.iter().map(|n| user.join(n)));
    }
    out.extend([TAL_NOISEMAKER, SURGE_XT, VITAL, DEXED].map(PathBuf::from));
    out.retain(|p| p.exists());
    out
}

/// The corpus, which is never empty.
///
/// `audio-probe` is built by this crate's own build script — `conformance` is
/// on for tests via a dev-dependency on itself — so there is always at least one
/// plugin to load. The SDK is a submodule, so an empty corpus means the build
/// is broken rather than the machine being bare — hence a failure, not a skip.
///
/// # Panics
///
/// If no plugin resolves — see above.
fn corpus_or_fail() -> Vec<PathBuf> {
    let c = corpus();
    assert!(
        !c.is_empty(),
        "the corpus is empty: `audio-probe` should have been built by this \
         crate's build script into VST3_PROBE_DIR. If the SDK submodules are \
         empty, run: git submodule update --init --recursive"
    );
    c
}

/// The `audio-probe` bundle `build.rs` builds into `VST3_PROBE_DIR`.
///
/// Built from the in-repo SDK submodules during this same `cargo test`, so on a
/// correctly cloned tree this is never empty.
fn built_binaries() -> Vec<PathBuf> {
    let Ok(dir) = std::env::var("VST3_PROBE_DIR") else {
        return Vec::new();
    };
    if dir.is_empty() {
        return Vec::new();
    }
    binary_in_bundle(&Path::new(&dir).join("audio-probe.vst3"))
        .into_iter()
        .collect()
}

/// The single binary inside a `.vst3` bundle, whichever arch dir holds it.
fn binary_in_bundle(bundle: &Path) -> Option<PathBuf> {
    tutti_plugin_types::bundle::any_module_in_bundle(
        bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
}

/// The user's home directory, or `None` where the platform does not say.
///
/// Was `env::var("HOME").expect("HOME is set")`, which panics on Windows —
/// `HOME` is a Unix convention and Windows sets `USERPROFILE` instead. Every
/// test that builds a corpus goes through here, so one missing variable took
/// out the whole file the first time this ran on Windows.
///
/// `Option` rather than a fallback guess: the only thing this is used for is
/// the *macOS* user-domain plugin directory, so on a platform without a home
/// there is nothing to look for and skipping is the honest answer.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
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

    let plugin = Active::<f32>::load(Path::new(TAL_NOISEMAKER), 44100.0, 512);
    match plugin {
        Ok(p) => {
            println!("Loaded: {}", p.loaded().plugin_info().name);
            println!("Vendor: {}", p.loaded().plugin_info().vendor);
            println!("Version: {}", p.loaded().plugin_info().version);
            println!("Audio inputs: {}", p.loaded().plugin_info().num_inputs);
            println!("Audio outputs: {}", p.loaded().plugin_info().num_outputs);
            println!("Supports f64: {}", p.loaded().plugin_info().supports_f64);
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
    let plugin = Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let info = plugin.loaded().plugin_info();
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
    for path in corpus_or_fail() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Active::<f32>::load(&library, 48_000.0, 64) else {
            eprintln!("skipping {} (failed to load)", path.display());
            continue;
        };
        let info = plugin.loaded().plugin_info();
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
/// The real string is on `PClassInfoW::version` (e.g. `"1.0.0.512"`,
/// Major.Minor.Subversion.Build). `vendor` sits beside it, and the header calls
/// that field an *overwrite* of the factory's (`ipluginbase.h:357`) — reading
/// only the factory's would credit a distributor rather than the maker.
///
/// Asserted as "at least one plugin disagrees with a `"1.0.0"` placeholder"
/// rather than per-plugin: a plugin that genuinely is version 1.0.0, or that
/// declares none and falls back, is not a bug. What would be a bug is *every*
/// plugin agreeing with the placeholder, which a hardcoded version guarantees.
#[test]
fn a_plugins_version_is_read_from_it_rather_than_assumed() {
    let _plugins = plugin_guard();
    let mut loaded = 0;
    let mut non_placeholder = 0;

    for path in corpus_or_fail() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Active::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.loaded().plugin_info();
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
/// taxonomy — `"Fx|Reverb"`, `"Instrument|Synth"`. Without it a browser's
/// `is_instrument` test — `category.contains("Instrument")` — could never
/// return true for a VST3 plugin.
///
/// Note this is a different field from `ClassInfo::category`, which names the
/// COM class kind (`"Audio Module Class"`) and is the same for every audio
/// plugin. Reading that one instead looks plausible and answers nothing.
///
/// Asserted across the corpus rather than per-plugin: a plugin declaring no
/// subcategory is legal. What would be a bug is every plugin reporting nothing,
/// which is what never reading the field guarantees.
#[test]
fn a_plugins_subcategories_are_read_from_it_rather_than_left_blank() {
    let _plugins = plugin_guard();
    let mut loaded = 0;
    let mut declared = 0;

    for path in corpus_or_fail() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Active::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.loaded().plugin_info();
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

    for path in corpus_or_fail() {
        let library = resolve_bundle(&path);
        let Ok(plugin) = Active::<f32>::load(&library, 48_000.0, 64) else {
            continue;
        };
        let info = plugin.loaded().plugin_info();
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

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
    let midi: [UmpAt; 0] = [];

    let _output_events = plugin.process(
        &mut buffer,
        &InputEvents {
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

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

    let midi = [(
        Offset::ZERO,
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x9999),
    )];

    let _output_events = plugin.process(
        &mut buffer,
        &InputEvents {
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

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

        let midi: Vec<UmpAt> = if i == 0 {
            vec![(
                Offset::ZERO,
                midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x9999),
            )]
        } else if i == 9 {
            vec![(
                Offset::ZERO,
                midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
            )]
        } else {
            vec![]
        };

        plugin.process(
            &mut buffer,
            &InputEvents {
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let state = plugin.get_state();
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

    let plugin = Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let param_count = plugin.loaded().parameter_count(&main_thread());
    println!("Plugin has {} parameters", param_count);

    // The ids are the plugin's own, not list positions: read each through the
    // id it reported.
    let ids = plugin.loaded().parameter_ids(&main_thread());
    assert_eq!(
        ids.len(),
        param_count as usize,
        "every index below parameter_count must yield a ParameterInfo"
    );
    for id in ids.into_iter().take(10) {
        let value = Control::parameter(&*plugin, &main_thread(), id)
            .expect("a listed parameter has a value")
            .get();
        assert_eq!(
            value,
            plugin.get_parameter(id.get()),
            "typed and raw reads must agree for id {id:?}"
        );
        println!("  id {id:?} value: {value}");
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    if plugin.loaded().parameter_count(&main_thread()) > 0 {
        // The first listed parameter, by the id it reported: ParamID 0 need
        // not be the first parameter (or exist at all).
        let id = *plugin
            .loaded()
            .parameter_ids(&main_thread())
            .first()
            .expect("a nonzero count lists a parameter");
        println!("Setting ParamID {id:?} to 0.5");
        Control::set_parameter(&mut *plugin, &main_thread(), id, Normalized::of(0.5));
        let value = Control::parameter(&*plugin, &main_thread(), id)
            .expect("a listed parameter reads back")
            .get();
        assert_eq!(value, plugin.get_parameter(id.get()));
        println!("Read back value: {value}");

        // An id the plugin never listed has no descriptor.
        let listed = plugin.loaded().parameter_ids(&main_thread());
        let unlisted = (0..=u32::MAX)
            .map(ParamId::new)
            .find(|c| !listed.contains(c))
            .expect("some id is unlisted");
        assert!(
            plugin
                .loaded()
                .parameter_info(&main_thread(), unlisted)
                .is_none()
        );
        // Not silently written: it reads as `None` (`getParamNormalized`
        // answers 0 for an unknown id), and a write to it must leave every
        // listed parameter and that read unchanged.
        assert_eq!(Control::parameter(&*plugin, &main_thread(), unlisted), None);
        let before: Vec<_> = listed
            .iter()
            .map(|i| plugin.get_parameter(i.get()))
            .collect();
        Control::set_parameter(&mut *plugin, &main_thread(), unlisted, Normalized::of(0.25));
        assert_eq!(Control::parameter(&*plugin, &main_thread(), unlisted), None);
        let after: Vec<_> = listed
            .iter()
            .map(|i| plugin.get_parameter(i.get()))
            .collect();
        assert_eq!(before, after);
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
        Active::<f32>::load(Path::new(path), 44100.0, 512).expect("Failed to load plugin");

    let transport = TransportInfo::new().with_tempo(120.0).with_playing(true);
    let midi: [UmpAt; 0] = [];

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
            &InputEvents {
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
///   1.0))`, which is the failure it exists to catch. TAL-NoiseMaker alone
///   returns identity for all 64 of its parameters, so a single plugin cannot
///   tell the two apart.
///
/// Not `#[ignore]`d, unlike its neighbours: it skips only when the corpus is
/// empty. An ignored test exercising new FFI is indistinguishable from no test.
#[test]
fn plain_range_probe_recovers_real_ranges() {
    let _plugins = plugin_guard();
    let corpus = corpus_or_fail();

    let mut total_probed = 0usize;
    let mut total_non_unit = 0usize;

    for path in &corpus {
        let resolved = resolve_bundle(path);
        let Ok(loaded) = tutti_vst3_host::Vst3Loaded::load(
            &main_thread(),
            &resolved,
            tutti_vst3_host::LoadOptions::new(),
        ) else {
            eprintln!("  {}: failed to load, skipping", path.display());
            continue;
        };

        let mut probed = 0usize;
        let mut non_unit = 0usize;
        for entry in loaded.parameter_list(&main_thread()).into_iter().take(64) {
            let id = entry.id;
            let Some(info) = loaded.parameter_info(&main_thread(), id) else {
                continue;
            };
            // `None` is legitimate: no controller, or an incoherent map. That
            // is the case the caller turns into `ParamRange::Normalized`.
            let Some(tutti_vst3_host::PlainRange { min, max }) =
                loaded.parameter_plain_range(&main_thread(), id)
            else {
                continue;
            };
            probed += 1;

            assert!(
                min.is_finite() && max.is_finite(),
                "{}: param {} ('{}') probed a non-finite range [{min}, {max}]",
                path.display(),
                info.id(),
                info.title_string()
            );
            assert!(
                min <= max,
                "{}: param {} ('{}') probed an inverted range [{min}, {max}]",
                path.display(),
                info.id(),
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
    // The load-bearing half. Without a single non-unit range, a working probe
    // and the hardcoded `0..1` it replaced produce identical output and this
    // test proves nothing. `audio-probe` guarantees one: `kParamDelayMs` is a
    // `RangeParameter` over 5..750 ms, added for exactly this reason, so the
    // assertion holds on a bare checkout rather than depending on which
    // third-party plugins a machine happens to have.
    assert!(
        total_non_unit > 0,
        "all {total_probed} probed ranges across {} plugin(s) were exactly \
         0..1, so a working probe is indistinguishable from the hardcoded \
         range it replaced. audio-probe's `DelayMs` should have reported \
         5..750 — either the probe is stale or normalizedParamToPlain is not \
         being consulted.",
        corpus.len()
    );
}
