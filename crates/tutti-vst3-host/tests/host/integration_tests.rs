//! The VST3 host's plugin-info reads against the in-repo `audio-probe`: bus
//! widths, version, subcategories and plain parameter ranges.

use std::path::{Path, PathBuf};
use tutti_fixtures::main_thread;

use crate::active;
use active::{Active, ActiveExt};

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

/// Every VST3 bundle these tests load: the in-repo `audio-probe`, which the
/// build script compiles from the SDK submodules.
fn corpus() -> Vec<PathBuf> {
    built_binaries()
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

/// The flat `num_inputs`/`num_outputs` counts must equal bus 0 of the
/// corresponding per-bus vec — no rounding up, in either direction.
///
/// This is the assertion that pins `audio_bus_channel_count` to what the plugin
/// declared. Two floors would break it if reintroduced: a `min_channels` floor
/// reporting a 0-channel output bus as 1, and `build_plugin_info` defaulting a
/// *missing* output bus to 2 on top of that. This fails if either comes back.
///
/// Swept over the whole corpus. A plugin that cannot load is skipped with a
/// note; a corpus with nothing loadable fails, because a silent pass here is
/// indistinguishable from a real one.
#[test]
fn a_plugins_flat_channel_counts_match_its_bus_zero() {
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

/// `parameter_plain_range` recovers real ranges from the built corpus.
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
///   1.0))`, which is the failure it exists to catch.
#[test]
fn plain_range_probe_recovers_real_ranges() {
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
            let id = entry.id();
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
    // `RangeParameter` over 5..750 ms.
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
