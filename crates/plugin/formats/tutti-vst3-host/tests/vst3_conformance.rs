//! VST3 host-conformance harness — validates the `ProcessData` **this host
//! builds** against Steinberg's own spec checks.
//!
//! Steinberg ships a VST3 plugin, HostChecker, whose job is the inverse of a
//! plugin validator: it inspects what a host hands it and reports spec
//! violations. Its six check modules depend only on the header-only
//! `pluginterfaces`, so this test compiles them directly (see `build.rs`) and
//! calls `HostCheck::validate` on the *live* `ProcessData` our `process` path
//! assembled — captured through the `conformance` observer seam, never
//! reconstructed. A reconstruction would test the reconstruction.
//!
//! ~185 checks come along for free, covering block size, sample-size and
//! process-mode agreement with `ProcessSetup`, null channel/bus pointers, bus
//! counts vs `IComponent`, event ordering and validity, and parameter-queue
//! ordering, duplication, and range.
//!
//! ## Running
//!
//! ```bash
//! VST3_SDK_DIR=/path/to/vst3sdk \
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! cargo test -p tutti-vst3-host --features conformance --test vst3_conformance
//! ```
//!
//! Both env vars are baked in at build time. Unset either and every test
//! skips with a printed message rather than failing, so a default `cargo test`
//! stays green without the SDK.

#![cfg(feature = "conformance")]

use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_types::meter::{BarNumber, TimeSignature};
use tutti_vst3_host::{
    host::conformance, AudioBuffer, MidiEvent, ParameterChanges, ProcessMode, TransportInfo,
    Vst3InputEvents, Vst3Instance, Vst3Library, Vst3Loaded, Vst3Sample,
};

// ── HostCheck C ABI (tests/support/hostcheck_shim.cpp) ───────────────────────

extern "C" {
    fn hc_num_log_events() -> c_int;
    fn hc_log_description(id: c_int) -> *const c_char;
    fn hc_log_severity(id: c_int) -> *const c_char;
    fn hc_configure(
        sample_rate: c_double,
        max_block: c_int,
        sample_size: c_int,
        process_mode: c_int,
        in_channels: *const c_int,
        n_in: c_int,
        out_channels: *const c_int,
        n_out: c_int,
        event_in: c_int,
        event_out: c_int,
    );
    fn hc_add_parameter(param_id: c_uint);
    fn hc_validate(
        data: *const c_void,
        min_in: c_int,
        min_out: c_int,
        counts: *mut c_longlong,
    ) -> c_int;
}

/// Serializes *all* plugin work in this binary.
///
/// Two separate reasons, both real:
/// - `HostCheck` is a process-global singleton, so a configure→drive→read
///   sequence must not interleave with another test's.
/// - VST3 module lifecycle is not thread-safe here: loading and unloading the
///   same DSO concurrently races module init/exit and segfaults. Every test
///   that constructs a `Vst3Instance`/`Vst3Loaded` must hold this, not just the
///   ones that touch `HostCheck`.
///
/// Held for the whole of each test rather than per-call, since instances are
/// dropped at end of scope and the unload is half the race.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning (a failed test elsewhere must
/// not cascade into spurious failures here).
fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

const AVAILABLE: &str = env!("VST3_HOSTCHECK_AVAILABLE");
const SAMPLE_PLUGIN_DIR: &str = env!("VST3_SAMPLE_PLUGIN_DIR");

/// `ProcessModes_::kOffline`. Used to assert the plugin observed the mode we
/// asked for, independent of the enum's Rust-side representation.
const K_OFFLINE: c_int = 2;

// ── Findings ─────────────────────────────────────────────────────────────────

/// One spec violation: which check fired, how often, and its severity.
#[derive(Debug, Clone)]
struct Finding {
    severity: String,
    description: String,
    count: i64,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} (x{})",
            self.severity, self.description, self.count
        )
    }
}

fn findings_from(counts: &[i64]) -> Vec<Finding> {
    counts
        .iter()
        .enumerate()
        .filter(|(_, &c)| c > 0)
        .map(|(id, &count)| unsafe {
            let d = CStr::from_ptr(hc_log_description(id as c_int));
            let s = CStr::from_ptr(hc_log_severity(id as c_int));
            Finding {
                severity: s.to_string_lossy().into_owned(),
                description: d.to_string_lossy().into_owned(),
                count,
            }
        })
        .collect()
}

/// Checks suppressed because HostChecker itself reports them incorrectly.
///
/// Matched on description text rather than log-id index so an SDK update that
/// reorders `LOG_EVENT_LIST` can't silently redirect the suppression onto an
/// unrelated check — it would stop matching and the finding would resurface.
const KNOWN_FALSE_POSITIVES: &[(&str, &str)] = &[(
    "A parameter ID is more than 1 time in the IParameterChanges list.",
    // `ParameterChangesCheck::updateParameterIDs` does
    // `mTempUsedId.resize(mParameterIds->size())`, which value-initializes the
    // seen-list with *zeros*. `checkAllChanges` then scans those zeros as
    // already-seen ids, so any queue for the perfectly legal ParamID 0 is
    // reported as a duplicate. Verified empirically: of 18 sample plugins,
    // exactly the 9 whose first ParamID is 0 trip this, and every plugin with
    // a nonzero first ParamID is clean.
    "HostChecker pre-seeds its seen-id list with zeros; ParamID 0 is legal",
)];

/// Only `Error`-severity findings fail a test. `Warn`/`Info` are reported —
/// several are advisory (e.g. a null `inputParameterChanges` pointer, which is
/// legal when there is no automation) or informational feature probes.
/// Known-bad checks (see [`KNOWN_FALSE_POSITIVES`]) are dropped too.
fn errors(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.severity == "Error")
        .filter(|f| {
            !KNOWN_FALSE_POSITIVES
                .iter()
                .any(|(desc, _)| f.description.contains(desc))
        })
        .collect()
}

// ── Plugin discovery ─────────────────────────────────────────────────────────

/// Resolve a `.vst3` bundle to the loadable binary inside it.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    for sub in [
        "Contents/x86_64-linux",
        "Contents/MacOS",
        "Contents/x86_64-win",
    ] {
        let dir = path.join(sub);
        for ext in ["so", "", "vst3", "dylib"] {
            let cand = if ext.is_empty() {
                dir.join(stem)
            } else {
                dir.join(format!("{stem}.{ext}"))
            };
            if cand.is_file() {
                return cand;
            }
        }
    }
    path.to_path_buf()
}

/// Every sample plugin we can find, as (name, resolved binary path).
fn sample_plugins() -> Vec<(String, PathBuf)> {
    if SAMPLE_PLUGIN_DIR.is_empty() {
        return Vec::new();
    }
    let dir = Path::new(SAMPLE_PLUGIN_DIR);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "vst3"))
        .filter_map(|p| {
            let name = p.file_stem()?.to_str()?.to_string();
            let bin = resolve_bundle(&p);
            bin.is_file().then_some((name, bin))
        })
        .collect();
    out.sort();
    out
}

/// Whether the plugin declares at least one event input bus, i.e. whether it
/// is legal for a host to send it MIDI at all.
fn accepts_midi(path: &Path) -> bool {
    Vst3Instance::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.info().has_midi_input)
        .unwrap_or(false)
}

/// The plugin's first declared ParamID, if it has any parameters.
fn first_parameter_id(path: &Path) -> Option<u32> {
    let inst = Vst3Instance::<f32>::load(path, 48_000.0, 512).ok()?;
    (inst.parameter_count() > 0).then(|| inst.parameter_id_at(0))?
}

/// Skip guard: prints why and returns false when the harness isn't available.
fn harness_ready() -> bool {
    if AVAILABLE != "1" {
        eprintln!("VST3_SDK_DIR unset or hostchecker sources missing; skipping");
        return false;
    }
    if sample_plugins().is_empty() {
        eprintln!(
            "no sample plugins under VST3_SAMPLE_PLUGIN_DIR ({SAMPLE_PLUGIN_DIR:?}); skipping"
        );
        return false;
    }
    true
}

// ── Driving one block through the real host path ─────────────────────────────

/// One `process` call's worth of host input.
#[derive(Default, Clone, Copy)]
struct Block<'a> {
    frames: usize,
    midi: &'a [MidiEvent],
    params: Option<&'a ParameterChanges>,
    transport: Option<&'a TransportInfo>,
}

impl<'a> Block<'a> {
    fn of(frames: usize) -> Self {
        Self {
            frames,
            ..Default::default()
        }
    }
    fn midi(mut self, midi: &'a [MidiEvent]) -> Self {
        self.midi = midi;
        self
    }
    fn transport(mut self, t: &'a TransportInfo) -> Self {
        self.transport = Some(t);
        self
    }
}

/// Load `path`, activate it as `T` (f32 or f64), and run one `process` block,
/// validating the `ProcessData` our host built.
fn drive_block<T: Vst3Sample + Default + Copy>(
    path: &Path,
    frames: usize,
    sample_rate: f64,
    block_size: usize,
    midi: &[MidiEvent],
    params: Option<&ParameterChanges>,
) -> Result<Vec<Finding>, String> {
    drive_blocks_in_mode::<T>(
        path,
        sample_rate,
        block_size,
        ProcessMode::Realtime,
        &[Block::of(frames).midi(midi).maybe_params(params)],
    )
    .map(|mut v| v.pop().unwrap_or_default())
}

impl<'a> Block<'a> {
    fn maybe_params(mut self, params: Option<&'a ParameterChanges>) -> Self {
        self.params = params;
        self
    }
}

/// Drive a sequence of blocks through one activated instance, validating each.
///
/// A sequence (rather than a fresh instance per block) is what makes the
/// *stateful* checks reachable: note-on/note-off pairing across blocks, and
/// `ProcessContext::systemTime` monotonicity, which by definition needs more
/// than one sample.
fn drive_blocks<T: Vst3Sample + Default + Copy>(
    path: &Path,
    sample_rate: f64,
    block_size: usize,
    blocks: &[Block<'_>],
) -> Result<Vec<Vec<Finding>>, String> {
    drive_blocks_in_mode::<T>(path, sample_rate, block_size, ProcessMode::Realtime, blocks)
}

/// As [`drive_blocks`], but activating the instance in `mode`.
///
/// The checker is configured from the `ProcessSetup` the observer reports —
/// the one the host actually negotiated — rather than from `mode` directly.
/// Feeding it `mode` would make `ProcessSetupCheck`'s setup-vs-data comparison
/// tautological on the very field under test: it would compare the test's
/// intent against the host's `ProcessData` and pass even if the host had
/// silently never delivered that mode to `setupProcessing`.
fn drive_blocks_in_mode<T: Vst3Sample + Default + Copy>(
    path: &Path,
    sample_rate: f64,
    block_size: usize,
    mode: ProcessMode,
    blocks: &[Block<'_>],
) -> Result<Vec<Vec<Finding>>, String> {
    // NOTE: the caller must already hold `PLUGIN_LOCK` — see its docs. Taking
    // it here instead would deadlock against the helpers (`accepts_midi`,
    // `first_parameter_id`, ...) that load plugins around this call.
    let mut inst = Vst3Instance::<T>::load_with_mode(path, sample_rate, block_size, mode)
        .map_err(|e| format!("load failed: {e:?}"))?;

    let info = inst.info().clone();
    let n = unsafe { hc_num_log_events() } as usize;

    // The per-bus channel lists come straight from the plugin: collapsing them
    // to one count would invent mismatches on any multi-bus plugin.
    let in_ch: Vec<c_int> = info
        .input_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let out_ch: Vec<c_int> = info
        .output_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let event_in = info.has_midi_input as c_int;
    let event_out = info.has_midi_output as c_int;

    // Register the plugin's parameter ids so unknown-id and out-of-range
    // param queues are reported. `hc_configure` (below, per block) resets only
    // the event log, not the parameter table, so registering once is enough.
    for i in 0..inst.parameter_count() {
        if let Some(id) = inst.parameter_id_at(i) {
            unsafe { hc_add_parameter(id as c_uint) };
        }
    }

    // Capture the counts the observer produces for this block.
    let counts_cell: &'static Mutex<Vec<i64>> = Box::leak(Box::new(Mutex::new(vec![0i64; n])));
    let seen: &'static Mutex<bool> = Box::leak(Box::new(Mutex::new(false)));

    conformance::set_observer(Box::new(move |data, setup| {
        // Configure the checker from the `ProcessSetup` the host actually
        // negotiated, captured through the same seam as the `ProcessData`
        // beside it. Deriving `processMode`/`symbolicSampleSize`/block size
        // from the test's *intent* instead would make `ProcessSetupCheck`'s
        // setup-vs-data comparison compare the test against itself — it would
        // pass even if the host had never delivered the requested mode to
        // `setupProcessing`, which is precisely the bug being guarded.
        unsafe {
            hc_configure(
                setup.sampleRate,
                setup.maxSamplesPerBlock,
                setup.symbolicSampleSize,
                setup.processMode,
                in_ch.as_ptr(),
                in_ch.len() as c_int,
                out_ch.as_ptr(),
                out_ch.len() as c_int,
                event_in,
                event_out,
            );
        }

        let mut counts = vec![0i64; n];
        let ptr = std::ptr::from_ref(data).cast::<c_void>();
        // min in/out buffer counts: the host must supply at least the bus
        // counts the component declares.
        unsafe { hc_validate(ptr, data.numInputs, data.numOutputs, counts.as_mut_ptr()) };
        *counts_cell.lock().unwrap() = counts;
        *seen.lock().unwrap() = true;
    }));

    let mut per_block = Vec::with_capacity(blocks.len());
    let default_transport = TransportInfo::default();

    for block in blocks {
        *seen.lock().unwrap() = false;

        let ins: Vec<Vec<T>> = (0..info.num_inputs.max(1))
            .map(|_| vec![T::default(); block.frames])
            .collect();
        let mut outs: Vec<Vec<T>> = (0..info.num_outputs.max(1))
            .map(|_| vec![T::default(); block.frames])
            .collect();
        let in_refs: Vec<&[T]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [T]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();

        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: block.frames,
            sample_rate,
        };
        let events = Vst3InputEvents {
            midi: block.midi,
            ..Default::default()
        };

        inst.process(
            &mut buffer,
            &events,
            block.params,
            block.transport.unwrap_or(&default_transport),
        );

        if !*seen.lock().unwrap() {
            return Err(
                "observer never fired — process() bailed before building ProcessData".into(),
            );
        }
        per_block.push(findings_from(&counts_cell.lock().unwrap()));
    }

    conformance::clear_observer();
    Ok(per_block)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The harness itself works: checks are linked and the table is populated.
#[test]
fn hostcheck_is_linked() {
    if AVAILABLE != "1" {
        eprintln!("VST3_SDK_DIR unset; skipping");
        return;
    }
    let n = unsafe { hc_num_log_events() };
    assert!(
        n > 100,
        "expected the full HostChecker table, got {n} checks"
    );
    let d = unsafe { CStr::from_ptr(hc_log_description(0)) };
    assert!(!d.to_string_lossy().is_empty());
}

/// Report how much of the corpus the per-bundle sweeps actually reach.
///
/// Every other test here loads a bundle by path, and `Vst3Loaded::load` calls
/// `find_audio_class`, which takes the **first** class whose category contains
/// `"Audio"` (`loaded.rs:1617-1630`). One bundle can export many: `mda-vst3`
/// packs the whole mda suite into a single binary, which is the normal shape
/// for commercial VST3 — plugin *suites* ship as one bundle, not one per
/// effect.
///
/// So "19 plugins swept" overstates the coverage: it is 19 *bundles*, and every
/// class after the first in each is never instantiated. This test measures the
/// gap rather than asserting a threshold, because the number is a property of
/// which plugins happen to be built here.
///
/// It also pins the real limitation behind that: there is no public API to
/// instantiate a *chosen* class. A DAW needs one — a user picking "mda Delay"
/// from a bundle that also holds "mda Bandisto" cannot be served by
/// first-audio-class-wins. Issue #54 item 7 is about real-plugin coverage; this
/// is the concrete, in-repo half of it.
#[test]
fn report_multi_class_bundle_coverage() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();

    let mut bundles = 0usize;
    let mut audio_classes = 0usize;
    let mut multi = Vec::new();

    for (name, path) in sample_plugins() {
        let Ok(library) = Vst3Library::load(&path) else {
            continue;
        };
        let names: Vec<String> = (0..library.count_classes())
            .filter_map(|i| library.get_class_info(i).ok())
            .filter(|c| c.category.contains("Audio"))
            .map(|c| c.name.clone())
            .collect();
        if names.is_empty() {
            continue;
        }
        bundles += 1;
        audio_classes += names.len();
        if names.len() > 1 {
            multi.push(format!("  {name}: {} classes {names:?}", names.len()));
        }
    }

    eprintln!(
        "corpus: {bundles} bundles, {audio_classes} audio classes; the sweeps \
         instantiate {bundles} (first audio class per bundle), leaving {} \
         untested",
        audio_classes - bundles
    );
    if !multi.is_empty() {
        eprintln!("multi-class bundles:\n{}", multi.join("\n"));
    }

    // The premise of every other sweep: each bundle must yield at least one
    // audio class, or those tests are silently measuring nothing.
    assert!(
        bundles > 0,
        "no bundle in the corpus exposed an audio class — the sweeps that \
         load by path are all vacuous"
    );
    assert!(
        audio_classes >= bundles,
        "counted {audio_classes} audio classes across {bundles} bundles, which \
         is arithmetically impossible — the class enumeration is wrong"
    );
}

/// Drive **every** audio class in the corpus, not just the first per bundle.
///
/// This is the coverage [`report_multi_class_bundle_coverage`] measures: 55
/// classes behind 19 bundles, of which the path-loading sweeps reach 19. The 36
/// others include 33 of `mda-vst3`'s — the closest thing in this corpus to real
/// shipped plugins rather than teaching examples, which is what issue #54 item
/// 7 is about.
///
/// Each is loaded, activated, and driven for a block. Failures are collected
/// rather than panicking on the first, because one broken class should not hide
/// the state of the other 54.
#[test]
fn every_audio_class_survives_a_block() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();

    let mut driven = 0usize;
    let mut failures = Vec::new();

    for (bundle, path) in sample_plugins() {
        let Ok(library) = Vst3Library::load(&path) else {
            continue;
        };
        let names: Vec<String> = (0..library.count_classes())
            .filter_map(|i| library.get_class_info(i).ok())
            .filter(|c| c.category.contains("Audio"))
            .map(|c| c.name.clone())
            .collect();
        // Drop before instantiating: `Vst3Loaded` opens its own handle, and
        // holding two to one DSO across module init/exit is the load/unload
        // race `PLUGIN_LOCK` exists to avoid.
        drop(library);

        for name in names {
            match Vst3Instance::<f32>::load_class(&path, &name, 48_000.0, 512) {
                Ok(mut inst) => {
                    let info = inst.info().clone();
                    // The class actually instantiated must be the one asked
                    // for. Without this the sweep counts *attempts*: a
                    // `load_class` that ignored its argument would load the
                    // first class 55 times and still report "drove 55".
                    // Measured — that mutation passed until this assert existed.
                    if info.name != name {
                        failures.push(format!(
                            "{bundle}: asked for {name:?}, got {:?} — load_class \
                             is not selecting by name",
                            info.name
                        ));
                        continue;
                    }
                    let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
                        .map(|_| vec![0.0f32; 512])
                        .collect();
                    let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
                        .map(|_| vec![0.0f32; 512])
                        .collect();
                    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
                    let mut out_refs: Vec<&mut [f32]> =
                        outs.iter_mut().map(|v| v.as_mut_slice()).collect();
                    let mut buffer = AudioBuffer {
                        inputs: &in_refs,
                        outputs: &mut out_refs,
                        num_samples: 512,
                        sample_rate: 48_000.0,
                    };
                    inst.process(
                        &mut buffer,
                        &Vst3InputEvents::default(),
                        None,
                        &TransportInfo::default(),
                    );

                    // Silence in must not become non-finite out: a NaN here
                    // propagates through every downstream node in the graph.
                    for (ch, out) in outs.iter().enumerate() {
                        if let Some(pos) = out.iter().position(|s| !s.is_finite()) {
                            failures.push(format!(
                                "{bundle}/{name}: non-finite sample at ch {ch} \
                                 index {pos} ({}) from silent input",
                                out[pos]
                            ));
                            break;
                        }
                    }
                    driven += 1;
                }
                Err(e) => failures.push(format!("{bundle}/{name}: load failed: {e:?}")),
            }
        }
    }

    eprintln!("drove {driven} audio classes");
    assert!(
        failures.is_empty(),
        "{} of {} audio classes failed:\n{}",
        failures.len(),
        driven + failures.len(),
        failures.join("\n")
    );
    // Guard the premise: if class enumeration regressed to one-per-bundle this
    // would still pass while covering a third of what it claims.
    assert!(
        driven > 40,
        "expected the corpus to yield ~55 audio classes, drove only {driven} — \
         class enumeration or the corpus regressed"
    );
}

/// A plugin's sidechain inputs must be staged, not dropped.
///
/// `host-checker` declares a stereo main input plus several `kAux` inputs
/// (`hostcheckerprocessor.cpp:76-88`). `Vst3Instance::activate_buses` loops
/// over `getBusCount` and activates every one, which is correct VST3: the
/// plugin's own `activateBus` scores `index > 0` as the informational feature
/// "IComponent::activateBus for SideChain supported!" rather than an error
/// (`hostcheckerprocessor.cpp:817-823`). Only an index *outside* the bus list
/// is a violation.
///
/// The observable consequence — and what this asserts — is that the host's own
/// per-bus staging covers every declared input bus. A host that stopped at bus
/// 0 would leave a compressor's sidechain reading silence, with no error raised
/// anywhere: `HostCheck::validate` reports process-time findings only, and the
/// plugin's `activateBus` log flushes from `setActive`, so neither channel
/// catches it. That is why this checks geometry rather than findings.
#[test]
fn sidechain_input_buses_are_staged() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    let buses = &inst.info().input_bus_channels;
    eprintln!("host-checker input buses: {buses:?}");

    // The premise: this plugin must actually declare a sidechain, or the test
    // proves nothing. Assert it rather than silently passing on a one-bus build.
    assert!(
        buses.len() > 1,
        "host-checker is expected to declare aux inputs beside its main bus, \
         but the host resolved {} input bus(es) — either the plugin was built \
         without them or the host is collapsing the bus list",
        buses.len()
    );

    // Every declared bus must carry channels. A staged-but-empty aux bus is the
    // shape a dropped sidechain takes: present in the count, silent in use.
    for (index, &channels) in buses.iter().enumerate() {
        assert!(
            channels > 0,
            "input bus {index} of {buses:?} was staged with 0 channels — a \
             plugin reading its sidechain here would get nothing"
        );
    }
}

/// A plain steady-state block against every sample plugin must produce no
/// Error-severity findings. This is the broad sweep: it exercises bus
/// staging, channel pointers, block size, and setup agreement for each
/// plugin's own bus layout.
#[test]
fn steady_state_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                } else {
                    eprintln!("  {name}: clean ({} advisory)", findings.len());
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in the ProcessData this host built:\n{}",
        failures.join("\n")
    );
}

/// A partial block (fewer frames than the negotiated max) must stay clean —
/// the host must not report a stale `numSamples` or mis-size its bus buffers.
#[test]
fn partial_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        for frames in [1usize, 17, 64, 511] {
            match drive_block::<f32>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
                Err(e) => eprintln!("  {name} @ {frames}: skipped ({e})"),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations on partial blocks:\n{}",
        failures.join("\n")
    );
}

// ── HostChecker's own parameter protocol ─────────────────────────────────────
//
// The reference plugin reports its verdict *through the audio path*: every
// block, `HostCheckerProcessor::process` bit-packs the log ids it accumulated
// into `outputParameterChanges` as 8 parameters of 24 bits each
// (`kProcessWarnTag + i`). Reading them back exercises our output-parameter
// handling and yields the plugin's findings at once — a second, independent
// opinion alongside the linked-in `HostCheck` (which sees the same struct but
// from our side of the FFI).

/// First tag of the 8-parameter warning block.
///
/// From the anonymous enum in `hostcheckercontroller.h`, which starts at
/// `kProcessingLoadTag = 1000` and runs unbroken to `kProcessWarnTag`. Counted
/// from the header rather than guessed: an earlier off-by-five here decoded the
/// neighbouring `kProcessContext*` readouts as warning bitfields and invented
/// four "spec errors" that did not exist. [`warn_tag_block_is_isolated`] pins
/// the value against that class of mistake.
const K_PROCESS_WARN_TAG: u32 = 1027;
/// `kParamProcessModeTag`, immediately below the warning block — used to check
/// the block's lower bound.
const K_PARAM_PROCESS_MODE_TAG: u32 = 1026;
/// Parameters in the warning block.
const K_PARAM_WARN_COUNT: u32 = 8;
/// Bits packed into each warning parameter.
const K_PARAM_WARN_BIT_COUNT: u32 = 24;

/// Decode the log ids HostChecker packed into one `outputParameterChanges`
/// set. Returns the ids, which index the same table as [`hc_log_description`].
fn decode_hostchecker_warnings(out: &ParameterChanges) -> Vec<i32> {
    let mut ids = Vec::new();
    for i in 0..K_PARAM_WARN_COUNT {
        let Some(queue) = out.get_queue(K_PROCESS_WARN_TAG + i) else {
            continue;
        };
        let Some(point) = queue.points.last() else {
            continue;
        };
        // The plugin sends `bits / 2^24`; undo that to recover the bitfield.
        let bits = (point.value * f64::from(1u32 << K_PARAM_WARN_BIT_COUNT)).round() as u32;
        for bit in 0..K_PARAM_WARN_BIT_COUNT {
            if bits & (1 << bit) != 0 {
                ids.push((i * K_PARAM_WARN_BIT_COUNT + bit) as i32);
            }
        }
    }
    ids
}

/// Resolve a log id to its severity + description via the linked table.
fn describe_log_id(id: i32) -> Option<(String, String)> {
    if id < 0 || id >= unsafe { hc_num_log_events() } {
        return None;
    }
    unsafe {
        let d = CStr::from_ptr(hc_log_description(id));
        let s = CStr::from_ptr(hc_log_severity(id));
        Some((
            s.to_string_lossy().into_owned(),
            d.to_string_lossy().into_owned(),
        ))
    }
}

/// Path to the `host-checker` reference plugin, if it was built.
fn host_checker_path() -> Option<PathBuf> {
    sample_plugins()
        .into_iter()
        .find(|(name, _)| name == "host-checker")
        .map(|(_, path)| path)
}

/// Whether the plugin advertises `kSample64` processing.
fn supports_f64(path: &Path) -> bool {
    Vst3Instance::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.info().supports_f64)
        .unwrap_or(false)
}

// ── Controller-side surfaces ─────────────────────────────────────────────────
//
// The tests below drive `IEditController` rather than the audio path. That is
// where HostChecker's ~35 thread-affinity checks live, and where its
// `IComponentHandler2`/`3`, `IProgress` and `restartComponent` probes fire —
// none of which the process-path tests can reach.
//
// The thread checker is not a GUI dependency, which is what made this look
// hard: `ThreadChecker::create()` records the calling thread and `test()`
// compares against it. The plugin creates it during `initialize`, so "the main
// thread" is simply whichever thread loaded the plugin. Doing load and all
// controller calls on one thread — as a #[test] naturally does — satisfies it,
// and calling from another thread is what a host must not do.

/// `kScoreTag` — the plugin's weighted host-capability score, 0..1 over 75
/// features (`hostcheckercontroller.h`).
const K_SCORE_TAG: u32 = 1005;
/// Triggers `restartComponent(kParamValuesChanged)` when set > 0.
const K_RESTART_PARAM_VALUES_TAG: u32 = 1013;
/// Triggers `restartComponent(kParamTitlesChanged)` when set > 0.
const K_RESTART_PARAM_TITLES_TAG: u32 = 1014;
/// Starts/stops an `IProgress` operation.
const K_TRIGGER_PROGRESS_TAG: u32 = 1008;

/// The host-capability score HostChecker computes for us.
///
/// A measured coverage figure rather than a pass/fail: each of 75 features
/// carries an importance weight (2.0 for `IComponentHandler2`, tempo, and
/// `IPlugFrame::resizeView`; 1.0 for niche ones) and the score is the weighted
/// fraction observed *so far in this session*. It therefore only counts what
/// the test actually exercised — it is a gap map, not a verdict, and a low
/// number means "not yet driven" as much as "not implemented".
///
/// ## Why the number does not move much
///
/// Issue #54 item 10 assumed the figure was low mainly because this test drove
/// so little, and that feeding it everything the suite covers elsewhere would
/// raise it. Measured, that is not what happens: populating every transport
/// field (14 of the 75 are per-`ProcessContext`-flag), processing more blocks,
/// polling notifications repeatedly, and deactivating to flush the processor's
/// log all leave it at the same **28.9%**.
///
/// The reason is structural. `updateScoring` runs on the *controller*, from
/// `addFeatureLog`; the processor's observations only reach it as `"LogEvent"`
/// messages over `IConnectionPoint`. Most of what this test drives happens on
/// the processor side, so it does not credit the score however hard it is
/// driven. Raising the number means exercising controller-side surfaces —
/// editor, units, `IComponentHandler2`/`3`, keyswitches — not more audio.
///
/// So the figure is a **floor on controller-side coverage**, which is a
/// narrower claim than item 10 implied. Treat a change in it as signal; treat
/// its absolute value as close to meaningless.
#[test]
fn report_host_capability_score() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    // Exercise the surfaces we do support, so the score reflects them.
    //
    // A fully-populated transport, not `default()`. HostChecker scores each
    // `ProcessContext` field separately (`ProcessContextTempoSupported`,
    // `…TimeSigSupported`, `…BarPositionSupported`, `…CycleSupported`, and so
    // on — 14 of the 75), and a default transport leaves every one of those
    // flags clear. The host fills them from what it is handed, so driving it
    // with a bare default measured the fixture rather than the host.
    //
    // `kSystemTimeValid` / `kClockValid` / `kSmpteValid` / `kChordValid` stay
    // unreachable on purpose — see `types/transport.rs:134-204`: no upstream
    // producer exists, and advertising a field the host cannot fill would be
    // worse than saying nothing. They are a deliberate ceiling on this score.
    let info = inst.info().clone();
    let transport = TransportInfo::default()
        .with_tempo(128.0)
        .with_playing(true)
        .with_recording(true)
        .with_time_signature(TimeSignature::from_parts(7, 8))
        .with_sample_rate(48_000.0)
        .with_position_beats(8.0, 3.75)
        .with_position_samples(180_000)
        .with_continuous_samples(180_000)
        .with_bar(8.0, BarNumber::new(3))
        .with_loop(true, 4.0, 12.0);
    for _ in 0..4 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        inst.process(&mut buffer, &Vst3InputEvents::default(), None, &transport);
    }
    // Deactivate before reading: the processor half accumulates findings in a
    // local log and ships them to the controller as `"LogEvent"` messages
    // **only from `setActive`** (`hostcheckerprocessor.cpp:742`, the single call
    // site of `sendNowAllLogEvents`), and `updateScoring` runs controller-side
    // as those arrive. Reading while still active cannot see anything `process`
    // observed.
    let mut inst = inst.deactivate();
    let _ = inst.poll_plugin_notifications();

    let score = inst.parameter(K_SCORE_TAG);
    eprintln!(
        "host capability score: {:.1}% of HostChecker's 75 weighted features",
        score * 100.0
    );

    // Deliberately not a threshold assertion: the score measures coverage of
    // features this test drove, so pinning it would encode today's harness
    // rather than the host's behaviour. Assert only that it is well-formed —
    // a NaN or out-of-range value would mean we read the wrong parameter.
    assert!(
        (0.0..=1.0).contains(&score),
        "score {score} outside 0..=1 — likely reading the wrong parameter tag"
    );
}

/// `restartComponent` requests must reach the host and surface as notifications.
///
/// The plugin fires `restartComponent(kParamValuesChanged / kParamTitlesChanged)`
/// when these trigger parameters are set, and logs whether the host returned
/// `kResultTrue`. A host that ignores restarts leaves stale parameter values or
/// titles in its UI after a preset change — and the plugin can detect that we
/// handled it, which is what the `kLogIdRestart*Supported` probes record.
#[test]
fn restart_component_requests_reach_the_host() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    // Drain anything latched during load so the assertion is about our trigger.
    let _ = inst.poll_plugin_notifications();

    inst.set_parameter(K_RESTART_PARAM_VALUES_TAG, 1.0);
    inst.set_parameter(K_RESTART_PARAM_TITLES_TAG, 1.0);
    let notifications = inst.poll_plugin_notifications();

    assert!(
        notifications.restart.param_values_changed || notifications.restart.param_titles_changed,
        "plugin requested restartComponent(kParamValuesChanged / kParamTitlesChanged) \
         but neither reached the host through poll_plugin_notifications()"
    );
}

/// `IProgress` reports must reach the host.
///
/// The plugin runs a timed progress operation when `kTriggerProgressTag` is
/// set, calling `IProgress::start`/`update`/`finish` on our handler. A host
/// that doesn't implement `IProgress` leaves the user with no feedback during
/// long plugin operations (sample loading, offline render).
///
/// The plugin drives progress from a **VSTGUI timer**, which only runs when its
/// editor is open — so with no window we may legitimately observe nothing. The
/// test therefore asserts the call is *accepted* and reports whether events
/// arrived, rather than requiring them.
#[test]
fn progress_reports_are_accepted() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };
    let _ = inst.poll_plugin_notifications();

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 1.0);
    let notifications = inst.poll_plugin_notifications();
    eprintln!(
        "IProgress: {} event(s) observed (0 is expected headless — the plugin \
         drives progress from an editor timer)",
        notifications.progress.len()
    );

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 0.0);
    let _ = inst.poll_plugin_notifications();
}

/// `IPrefetchableSupport` must be queryable without upsetting the plugin.
///
/// `host-checker` implements the interface; the value it reports is its own
/// business, but a host that queries it must do so on the main thread and
/// must tolerate any of the three prefetchable states.
#[test]
fn prefetchable_support_is_queryable() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    match inst.prefetchable_support() {
        Some(v) => {
            eprintln!("IPrefetchableSupport reports {v}");
            assert!(v <= 2, "unexpected prefetchable state {v} (expected 0..=2)");
        }
        None => eprintln!("plugin does not implement IPrefetchableSupport"),
    }
}

/// MIDI-learn arming must forward captured CCs to the plugin on the main thread.
///
/// `IMidiLearn::onLiveMIDIControllerInput` is main-thread-only, but the CCs it
/// receives are captured on the audio thread — so the host has to buffer and
/// forward. HostChecker asserts the thread context of the call, meaning a host
/// that forwards straight from `process` is caught here.
#[test]
fn midi_learn_forwards_from_the_main_thread() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    inst.arm_midi_learn(true);
    assert!(inst.is_midi_learn_armed());

    // Feed CCs through the *audio* path; the host must capture them there and
    // forward on the next main-thread poll, not call the plugin inline.
    let info = inst.info().clone();
    // MIDI 2.0 CC values are 32-bit; 0x8000_0000 is mid-scale.
    let cc = [
        MidiEvent::cc(0, 0, 7, 0x8000_0000).with_frame_offset(0),
        MidiEvent::cc(0, 0, 10, 0x4000_0000).with_frame_offset(64),
    ];
    let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
        .map(|_| vec![0.0; 512])
        .collect();
    let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
        .map(|_| vec![0.0; 512])
        .collect();
    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: 512,
        sample_rate: 48_000.0,
    };
    inst.process(
        &mut buffer,
        &Vst3InputEvents {
            midi: &cc,
            ..Default::default()
        },
        None,
        &TransportInfo::default(),
    );

    // The forward happens here, on this (main) thread.
    let _ = inst.poll_plugin_notifications();
    inst.arm_midi_learn(false);
    assert!(!inst.is_midi_learn_armed());
}

/// Pin [`K_PROCESS_WARN_TAG`] against the plugin's actual parameter layout.
///
/// The warning block is decoded as a bitfield; the parameters immediately
/// below it (`kParamProcessModeTag` and the `kProcessContext*` readouts) carry
/// ordinary normalized values. Decoding one as the other yields plausible
/// nonsense — it manufactured four phantom "spec errors" before this was
/// caught. Assert the block's identity structurally rather than trusting a
/// hand-counted enum offset.
#[test]
fn warn_tag_block_is_isolated() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    let declared: Vec<u32> = (0..inst.parameter_count())
        .filter_map(|i| inst.parameter_id_at(i))
        .collect();

    // Every id in the warning block must be a real parameter...
    for i in 0..K_PARAM_WARN_COUNT {
        let tag = K_PROCESS_WARN_TAG + i;
        assert!(
            declared.contains(&tag),
            "kProcessWarnTag + {i} = {tag} is not a parameter this plugin declares; \
             the enum offset in this test is stale"
        );
    }
    // ...and the tag just below it must be the process-mode readout, which
    // fixes the block's lower bound. If the enum shifted, one of these fails.
    assert!(
        declared.contains(&K_PARAM_PROCESS_MODE_TAG),
        "expected kParamProcessModeTag at {K_PARAM_PROCESS_MODE_TAG}"
    );
    assert!(
        !declared.contains(&(K_PROCESS_WARN_TAG + K_PARAM_WARN_COUNT)),
        "found a parameter just past the warning block — the block is larger \
         than kParamWarnCount, so the decode would drop findings"
    );
}

/// The reference plugin's own verdict, delivered through `outputParameterChanges`.
///
/// This is HostChecker judging us from *inside* the plugin, independent of the
/// `HostCheck` instance we link in — and getting it requires our output-param
/// path to work, so a silent failure to read `outputParameterChanges` shows up
/// as "no findings" rather than a false pass. The test therefore asserts we
/// received *something* before asserting the findings are clean.
#[test]
fn host_checker_reports_no_errors_through_output_params() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };

    let mut inst = match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("host-checker load failed ({e:?}); skipping");
            return;
        }
    };
    let info = inst.info().clone();

    // Drive several blocks: the plugin latches state-machine observations
    // across calls, and only reports once it has something to say.
    let mut all_ids = Vec::new();
    let transport = TransportInfo::default();
    for _ in 0..8 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        let out = inst.process(&mut buffer, &Vst3InputEvents::default(), None, &transport);
        all_ids.extend(decode_hostchecker_warnings(out.parameter_changes));
    }

    all_ids.sort_unstable();
    all_ids.dedup();

    let mut errors_found = Vec::new();
    for id in &all_ids {
        if let Some((severity, desc)) = describe_log_id(*id) {
            if KNOWN_FALSE_POSITIVES.iter().any(|(d, _)| desc.contains(d)) {
                continue;
            }
            if severity == "Error" {
                errors_found.push(format!("    [{severity}] {desc}"));
            } else {
                eprintln!("  advisory: [{severity}] {desc}");
            }
        }
    }

    assert!(
        errors_found.is_empty(),
        "host-checker reported VST3 spec errors against this host:\n{}",
        errors_found.join("\n")
    );
}

/// Plugin state must survive a `getState` → `setState` → `getState` round trip.
///
/// Untested until now, and it is the persistence path: a project save/reload
/// runs exactly this. `host-checker` implements state on both the processor and
/// the controller (`HostCheckerProcessor::setState` /
/// `HostCheckerController::setState`), each with a thread-affinity assertion,
/// so a host that touches state off the main thread is caught too.
#[test]
fn plugin_state_round_trips() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        let mut inst = match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("  {name}: skipped (load failed {e:?})");
                continue;
            }
        };

        let Ok(first) = inst.state() else {
            // A plugin with no state at all is legal; nothing to round-trip.
            continue;
        };
        if first.is_empty() {
            continue;
        }
        exercised += 1;

        if let Err(e) = inst.set_state(&first) {
            failures.push(format!("{name}: set_state rejected its own state: {e:?}"));
            continue;
        }
        match inst.state() {
            Ok(second) => {
                if first != second {
                    failures.push(format!(
                        "{name}: state changed across round trip ({} bytes -> {} bytes)",
                        first.len(),
                        second.len()
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: second getState failed: {e:?}")),
        }
    }

    if exercised == 0 {
        eprintln!("no sample plugin exposes state; round-trip not exercised");
    } else {
        eprintln!("state round-trip: {exercised} plugins exercised");
    }
    assert!(
        failures.is_empty(),
        "plugin state did not round-trip:\n{}",
        failures.join("\n")
    );
}

/// f64 processing must build the same spec-clean `ProcessData` as f32.
///
/// This is a separate code path, not a parameter: `Vst3Sample` selects
/// `symbolicSampleSize` *and* which arm of the `channelBuffers32`/`64` union
/// the host writes. A host that sets one without the other hands the plugin a
/// pointer table it will read at the wrong width — silent garbage, since the
/// two union members overlay the same bytes. `ProcessSetupCheck` catches the
/// mismatch by comparing against the negotiated `ProcessSetup`.
#[test]
fn f64_processing_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Activating f64 on a plugin that only does f32 is the host's error,
        // not something to assert on.
        if !supports_f64(&path) {
            continue;
        }
        for frames in [512usize, 1, 64] {
            exercised += 1;
            match drive_block::<f64>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames (f64):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
                Err(e) => eprintln!("  {name} (f64): skipped ({e})"),
            }
        }
    }

    if exercised == 0 {
        eprintln!("no f64-capable sample plugin available; f64 path not exercised");
    } else {
        eprintln!("f64: {exercised} blocks exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations on the f64 path:\n{}",
        failures.join("\n")
    );
}

/// A playing transport across consecutive blocks must stay spec-clean.
///
/// Reaches `ProcessContextCheck`, which the other tests leave almost untouched:
/// it verifies the context sample rate matches `ProcessSetup` and — the part
/// that needs a *sequence* — that `systemTime` increases monotonically. A host
/// that recomputes system time per block from a wall clock, or forwards a
/// stale value, trips the latter.
#[test]
fn transport_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        let transport = TransportInfo::default();
        let blocks: Vec<Block<'_>> = (0..8)
            .map(|_| Block::of(512).transport(&transport))
            .collect();

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations with a transport across blocks:\n{}",
        failures.join("\n")
    );
}

/// Note-on / note-off pairs spanning blocks must arrive correctly paired.
///
/// `EventListCheck` tracks live notes across `process` calls by both pitch and
/// note id, so this reaches four checks nothing else does: note-off with no
/// matching note-on (by id, and by pitch), and note-on for a pitch/id already
/// sounding. A host that drops, duplicates, or reorders note events across the
/// block boundary — or mangles the note id — shows up here as a stuck or
/// orphaned note, which is exactly how it would be heard.
#[test]
fn note_lifecycle_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;

        // Block 0 starts three notes; block 1 ends them. Nothing is left
        // sounding, and no note-off lacks its note-on.
        let on = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_on(0, 0, 64, 0x6000).with_frame_offset(64),
            MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(128),
        ];
        let off = [
            MidiEvent::note_off(0, 0, 60, 0x4000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 64, 0x4000).with_frame_offset(64),
            MidiEvent::note_off(0, 0, 67, 0x4000).with_frame_offset(128),
        ];
        let blocks = [
            Block::of(512).midi(&on),
            Block::of(512).midi(&off),
            Block::of(512), // silence: nothing should still be sounding
        ];

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; note lifecycle not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in note lifecycle across blocks:\n{}",
        failures.join("\n")
    );
}

/// Parameter automation staged into `inputParameterChanges` must reach the
/// plugin spec-legally.
///
/// This unlocks a whole check category the other tests never reach: with a
/// null `inputParameterChanges` pointer, HostCheck's `ParameterChangesCheck`
/// exits immediately. Populated, it verifies value range (0..=1), that every
/// queue's ParamID is one `IEditController` actually declared, that no ParamID
/// appears twice in one list, that points within a queue are sorted by sample
/// offset, and that no queue pointer is null at a valid index.
///
/// Points are fed deliberately out of order — the host must sort them.
#[test]
fn parameter_changes_are_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Automate the plugin's own first parameter — an invented ParamID
        // would (correctly) be reported as unknown, testing the fixture
        // rather than the host.
        let Some(param_id) = first_parameter_id(&path) else {
            continue;
        };
        let mut params = ParameterChanges::new();
        params.add_change(param_id, 384, 0.75);
        params.add_change(param_id, 0, 0.25);
        params.add_change(param_id, 128, 0.5);

        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], Some(&params)) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no sample plugin exposes a parameter; automation path not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged parameter changes:\n{}",
        failures.join("\n")
    );
}

/// MIDI staged into the input event list must reach the plugin spec-legally:
/// sorted by sample offset, in-range pitch/velocity, valid bus and channel.
/// Feed events deliberately out of order — the host is required to sort.
#[test]
fn midi_event_list_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let midi = [
        MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(200),
        MidiEvent::note_on(0, 0, 64, 0x5000).with_frame_offset(50),
        MidiEvent::note_off(0, 0, 60, 0x4000).with_frame_offset(400),
        MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(100),
    ];
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Only plugins that declare an event input bus can legally receive
        // MIDI; feeding an audio-only effect a note-on is an invalid call by
        // the *host*, and HostCheck rightly flags the bus index.
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &midi, None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; event-list path not exercised");
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged event list:\n{}",
        failures.join("\n")
    );
}

// ── Process modes ────────────────────────────────────────────────────────────
//
// `ProcessSetup::processMode` and `ProcessData::processMode` must agree;
// `ProcessSetupCheck::check` reports `kLogIdInvalidProcessMode` (severity
// Error) when they do not, permitting exactly one divergence — a per-block
// toggle between `kRealtime` and `kPrefetch`. Because the checker is configured
// from the observed `ProcessSetup` (see `drive_blocks_in_mode`), every one of
// these tests validates the host's *real* setup against the host's real block,
// so a mode that never reached `setupProcessing` fails rather than passes.

/// An offline bounce must be spec-clean for every sample plugin.
///
/// The regression this pins: `processMode` was hardcoded to `kRealtime` at
/// every site, so asking for an offline render silently produced the realtime
/// result — no error, and nothing downstream could tell. Plugins with lookahead
/// limiters, high-quality resamplers, or longer offline FFT windows are
/// entitled to behave differently here, and could not.
#[test]
fn offline_mode_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Partial blocks alongside the full one: offline renders end on a
        // short block, which is where a stale `numSamples` would show up.
        let blocks = [Block::of(512), Block::of(64), Block::of(1)];
        exercised += 1;
        match drive_blocks_in_mode::<f32>(&path, 48_000.0, 512, ProcessMode::Offline, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i} (offline):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(exercised > 0, "no sample plugins to drive offline");
    assert!(
        failures.is_empty(),
        "VST3 spec violations in offline mode:\n{}",
        failures.join("\n")
    );
}

/// Prefetch mode, selected at activation, must be spec-clean.
///
/// `prefetchable.vst3` is the corpus plugin that implements
/// `IPrefetchableSupport`, but the mode is the *host's* to request and every
/// plugin must tolerate it, so this sweeps them all.
#[test]
fn prefetch_mode_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        let blocks = [Block::of(512), Block::of(64)];
        match drive_blocks_in_mode::<f32>(&path, 48_000.0, 512, ProcessMode::Prefetch, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i} (prefetch):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in prefetch mode:\n{}",
        failures.join("\n")
    );
}

/// Toggling realtime↔prefetch on a *live* instance must stay spec-clean.
///
/// This is the one setup/data divergence VST3 allows without re-running
/// `setupProcessing` (`ProcessSetupCheck::check`'s explicit exception), so the
/// blocks here deliberately run with `ProcessData::processMode` differing from
/// the negotiated `ProcessSetup::processMode` — and `kLogIdInvalidProcessMode`
/// must still not fire. A host that re-ran setup on the toggle, or one that
/// refused the toggle outright, would both fail to reach this state.
#[test]
fn live_realtime_prefetch_toggle_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };

    let n = unsafe { hc_num_log_events() } as usize;
    let mut inst = match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("host-checker load failed ({e:?}); skipping");
            return;
        }
    };
    let info = inst.info().clone();

    assert_eq!(
        inst.process_mode(),
        ProcessMode::Realtime,
        "plain activation must stay realtime"
    );

    let in_ch: Vec<c_int> = info
        .input_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let out_ch: Vec<c_int> = info
        .output_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let event_in = info.has_midi_input as c_int;
    let event_out = info.has_midi_output as c_int;
    for i in 0..inst.parameter_count() {
        if let Some(id) = inst.parameter_id_at(i) {
            unsafe { hc_add_parameter(id as c_uint) };
        }
    }

    // Record the mode pair the host presented each block alongside the
    // findings, so the assertions below can tell "clean because the toggle
    // worked" from "clean because nothing ever changed".
    let observed: &'static Mutex<Vec<(c_int, c_int)>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let counts_cell: &'static Mutex<Vec<i64>> = Box::leak(Box::new(Mutex::new(vec![0i64; n])));

    conformance::set_observer(Box::new(move |data, setup| {
        unsafe {
            hc_configure(
                setup.sampleRate,
                setup.maxSamplesPerBlock,
                setup.symbolicSampleSize,
                setup.processMode,
                in_ch.as_ptr(),
                in_ch.len() as c_int,
                out_ch.as_ptr(),
                out_ch.len() as c_int,
                event_in,
                event_out,
            );
        }
        let mut counts = vec![0i64; n];
        let ptr = std::ptr::from_ref(data).cast::<c_void>();
        unsafe { hc_validate(ptr, data.numInputs, data.numOutputs, counts.as_mut_ptr()) };
        observed
            .lock()
            .unwrap()
            .push((setup.processMode, data.processMode));
        *counts_cell.lock().unwrap() = counts;
    }));

    let transport = TransportInfo::default();
    let mut failures = Vec::new();

    for (i, prefetch) in [false, true, true, false, true].into_iter().enumerate() {
        assert!(
            inst.set_prefetch(prefetch),
            "block {i}: realtime<->prefetch toggle refused on a realtime-activated instance"
        );

        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        inst.process(&mut buffer, &Vst3InputEvents::default(), None, &transport);

        let findings = findings_from(&counts_cell.lock().unwrap());
        let errs = errors(&findings);
        if !errs.is_empty() {
            failures.push(format!(
                "block {i} (prefetch={prefetch}):\n{}",
                errs.iter()
                    .map(|e| format!("    {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    conformance::clear_observer();

    let seen = observed.lock().unwrap().clone();
    assert_eq!(seen.len(), 5, "observer did not fire once per block");
    // The setup half must never move: the whole point of the exception is that
    // a toggle does *not* re-run `setupProcessing`.
    assert!(
        seen.iter().all(|&(setup_mode, _)| setup_mode == seen[0].0),
        "ProcessSetup::processMode changed across a live toggle — the host re-ran \
         setupProcessing instead of using the spec's realtime<->prefetch exception: {seen:?}"
    );
    // ...and the data half must actually have moved, or this test proves
    // nothing beyond "a constant equals itself".
    assert!(
        seen.iter()
            .any(|&(setup_mode, data_mode)| setup_mode != data_mode),
        "ProcessData::processMode never diverged from the setup — the toggle was a no-op, \
         so the spec exception under test was never exercised: {seen:?}"
    );
    assert!(
        failures.is_empty(),
        "VST3 spec violations across a live realtime<->prefetch toggle:\n{}",
        failures.join("\n")
    );
}

/// An offline-activated instance must refuse the live realtime/prefetch toggle.
///
/// Reaching realtime or prefetch from `kOffline` needs a fresh
/// `setupProcessing`; doing it per-block would put `ProcessData` out of
/// agreement with `ProcessSetup` in a way the spec's exception does *not*
/// cover, and `kLogIdInvalidProcessMode` would fire. The host therefore
/// declines rather than producing an invalid block — and must keep rendering
/// offline, which is what a bounce asked for.
#[test]
fn offline_instance_refuses_the_live_toggle() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) =
        Vst3Instance::<f32>::load_with_mode(&path, 48_000.0, 512, ProcessMode::Offline)
    else {
        eprintln!("host-checker offline load failed; skipping");
        return;
    };

    assert_eq!(inst.process_mode(), ProcessMode::Offline);
    assert!(
        !inst.set_prefetch(true),
        "offline instance accepted a prefetch toggle; that would put ProcessData out of \
         agreement with the negotiated ProcessSetup"
    );
    assert!(
        !inst.set_prefetch(false),
        "offline instance accepted a realtime toggle"
    );
    assert_eq!(
        inst.process_mode(),
        ProcessMode::Offline,
        "a refused toggle must leave the mode untouched"
    );
}

/// The plugin's own view: `host-checker` must report back that it saw offline.
///
/// An independent check on the linked-in `HostCheck`. `HostCheckerProcessor`
/// publishes the mode it observed through `outputParameterChanges` as
/// `kParamProcessModeTag` carrying `processMode * 0.5`, so decoding it proves
/// `kOffline` crossed the FFI and arrived inside the plugin — not merely that
/// our own struct held the right integer. If the host regressed to a hardcoded
/// `kRealtime`, this reads 0.0 instead of 1.0 and fails.
#[test]
fn plugin_observes_the_offline_mode_we_requested() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) =
        Vst3Instance::<f32>::load_with_mode(&path, 48_000.0, 512, ProcessMode::Offline)
    else {
        eprintln!("host-checker offline load failed; skipping");
        return;
    };
    let info = inst.info().clone();
    let transport = TransportInfo::default();

    // The plugin emits the tag only when the mode *changed* since the last
    // block (`mLastProcessMode`), and it initialises that field to -1 — so the
    // first block reports, and later ones stay silent. Scan every block rather
    // than only the last.
    let mut reported: Option<f64> = None;
    for _ in 0..4 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        let out = inst.process(&mut buffer, &Vst3InputEvents::default(), None, &transport);
        if let Some(queue) = out.parameter_changes.get_queue(K_PARAM_PROCESS_MODE_TAG) {
            if let Some(point) = queue.points.last() {
                reported = Some(point.value);
            }
        }
    }

    let Some(value) = reported else {
        panic!(
            "host-checker never published kParamProcessModeTag; either the host emitted no \
             ProcessData or output parameter changes are not being read back"
        );
    };
    // The plugin sends `processMode * 0.5` (hostcheckerprocessor.cpp), so
    // kOffline == 2 arrives as 1.0.
    let observed = (value * 2.0).round() as c_int;
    assert_eq!(
        observed, K_OFFLINE,
        "plugin observed process mode {observed}, but the host was asked for kOffline \
         ({K_OFFLINE}) — the requested mode did not reach ProcessData"
    );
}

/// The host **lends** its `IHostApplication` to the plugin: `IPluginBase::
/// initialize` borrows the context rather than consuming a reference, so a
/// load must not strand one.
///
/// This is the load-path counterpart to the accessor-level test in
/// `com::host_application` — that one pins the `to_com_ptr`/`as_com_ref`
/// primitives, this one pins the call site that has to pick the borrowing form.
///
/// Each `Vst3Loaded` builds its own `HostApplication`, so the count is read
/// within one instance: after `initialize()` it is the host's own reference
/// plus however many the plugin chose to retain. A same-object controller
/// shares the component's retain and a separate one takes its own, so the legal
/// ceiling is host + component + controller. Measured against the SDK sample
/// plugins: 3 with the borrowing hand-off, 4 once `to_com_ptr().into_raw()`
/// strands its extra reference — so this bound discriminates, it is not slack.
///
/// Needs only a sample plugin, not the HostChecker sources.
#[test]
fn host_context_is_borrowed_not_consumed() {
    if sample_plugins().is_empty() {
        eprintln!(
            "no sample plugins under VST3_SAMPLE_PLUGIN_DIR ({SAMPLE_PLUGIN_DIR:?}); skipping"
        );
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        let Ok(loaded) = Vst3Loaded::load(&path) else {
            eprintln!("  {name}: skipped (load failed)");
            continue;
        };
        exercised += 1;

        let after_init = loaded.host_context_refcount();
        if after_init > 3 {
            failures.push(format!(
                "{name}: host-context refcount {after_init} after initialize() exceeds \
                 host(1) + component(1) + controller(1); the hand-off leaked a reference"
            ));
        }
        drop(loaded);
    }

    if exercised == 0 {
        eprintln!("no sample plugin loaded; host-context hand-off not exercised");
    }

    assert!(
        failures.is_empty(),
        "host context is being leaked at initialize():\n{}",
        failures.join("\n")
    );
}

// ── Optional controller interfaces ───────────────────────────────────────────
//
// Everything below drives a `Vst3Loaded` accessor that had **zero call sites**
// in either src or tests — code that had never once executed, so "untested" and
// "unknown to work at all" were the same statement.
//
// Each assertion is anchored to a *contrast* between two real plugins rather
// than to a single expected number: `host-checker` implements seven of these
// interfaces and `note-expression-synth` a different three, so a host that
// returned a constant (or ignored its arguments) cannot satisfy both.

/// Load a named sample bundle, or `None` when this machine lacks it.
fn sample(name: &str) -> Option<Vst3Loaded> {
    let p = resolve_bundle(&Path::new(SAMPLE_PLUGIN_DIR).join(name));
    if !p.is_file() {
        return None;
    }
    Vst3Loaded::load(&p).ok()
}

/// `IProcessContextRequirements` is read at load and drives whether the host
/// bothers filling a transport snapshot each block.
///
/// The contrast is the test: `host-checker` requests everything (`0x7ff`), and
/// `note-expression-synth` implements the interface but requests nothing (`0`).
/// A host that hardcoded either answer — or that failed to query the interface
/// and fell back to `u32::MAX` — fails on one of the two.
#[test]
fn context_requirements_are_read_from_the_plugin() {
    let Some(checker) = sample("host-checker.vst3") else {
        eprintln!("host-checker not built; skipping");
        return;
    };

    let req = checker.context_requirements();
    assert_ne!(
        req,
        u32::MAX,
        "host-checker implements IProcessContextRequirements, so the host must \
         have its real flags rather than the everything-fallback"
    );
    assert!(
        checker.wants_transport(),
        "host-checker requests tempo/playhead/bar fields ({req:#x}), so \
         wants_transport must be true"
    );
    assert!(
        checker.wants_sequencer_context(),
        "host-checker requests kNeedChord ({req:#x}), so \
         wants_sequencer_context must be true"
    );

    // The discriminating half: a plugin that implements the interface and asks
    // for nothing must come back false on both, or these accessors are
    // constants rather than reads.
    let Some(nes) = sample("note-expression-synth.vst3") else {
        eprintln!("note-expression-synth not built; skipping the contrast");
        return;
    };
    assert!(
        !nes.wants_transport() && !nes.wants_sequencer_context(),
        "note-expression-synth requests no context fields ({:#x}), but the host \
         reports transport={} sequencer={} — these accessors are not reading \
         the plugin's flags",
        nes.context_requirements(),
        nes.wants_transport(),
        nes.wants_sequencer_context()
    );
}

/// `INoteExpressionController` enumeration: count, then per-index descriptors.
///
/// `note-expression-synth` exposes 12 types and `host-checker` exposes 1, so a
/// hardcoded count satisfies neither. Reading every descriptor back and
/// requiring the ids to be distinct is what proves `index` is actually being
/// passed through rather than ignored.
#[test]
fn note_expression_types_enumerate_per_plugin() {
    let Some(nes) = sample("note-expression-synth.vst3") else {
        eprintln!("note-expression-synth not built; skipping");
        return;
    };

    let count = nes.note_expression_count(0, 0);
    assert!(
        count > 1,
        "note-expression-synth advertises several expression types, got {count}"
    );

    let mut ids = Vec::new();
    for i in 0..count {
        let info = nes.note_expression_info(0, 0, i).unwrap_or_else(|| {
            panic!(
                "type {i} of {count} has no descriptor — the count and the \
                    per-index lookup disagree"
            )
        });
        ids.push(info.type_id);
    }
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "every index returned the same descriptor(s) ({ids:?}) — the index \
         argument is being ignored"
    );

    // Out of range must be None, not a wrapped or clamped entry.
    assert!(
        nes.note_expression_info(0, 0, count + 1000).is_none(),
        "an out-of-range note-expression index returned a descriptor"
    );
}

/// `IKeyswitchController` — the articulation map a sample library advertises.
#[test]
fn keyswitches_enumerate_and_bound_check() {
    let Some(checker) = sample("host-checker.vst3") else {
        eprintln!("host-checker not built; skipping");
        return;
    };

    let count = checker.keyswitch_count(0, 0);
    assert!(
        count > 0,
        "host-checker implements IKeyswitchController but the host read 0 \
         entries"
    );
    for i in 0..count {
        assert!(
            checker.keyswitch_info(0, 0, i).is_some(),
            "keyswitch {i} of {count} has no descriptor"
        );
    }
    assert!(
        checker.keyswitch_info(0, 0, count + 1000).is_none(),
        "an out-of-range keyswitch index returned a descriptor"
    );

    // A plugin without the interface must answer 0 rather than guessing.
    if let Some(nes) = sample("note-expression-synth.vst3") {
        assert_eq!(
            nes.keyswitch_count(0, 0),
            0,
            "note-expression-synth does not implement IKeyswitchController, so \
             the count must be 0"
        );
    }
}

/// `IRemapParamID` — parameter migration when one plugin replaces another.
///
/// The strongest assertion available here: the `remap_paramid` sample maps
/// *AGain's* parameter 0 onto its own `kMyGainParamTag` (123) and refuses every
/// other UID and id (`remapparamidcontroller.cpp:76`). So the expected value is
/// fixed by the sample's source, not by observation, and a host that passed the
/// wrong UID through would get `None`.
#[test]
fn remap_param_id_migrates_a_known_parameter() {
    const AGAIN_PROCESSOR_UID: [u8; 16] = [
        0x84, 0xE8, 0xDE, 0x5F, 0x92, 0x55, 0x4F, 0x53, 0x96, 0xFA, 0xE4, 0x13, 0x3C, 0x93, 0x5A,
        0x18,
    ];
    /// `kMyGainParamTag` in `remapparamidcids.h`.
    const EXPECTED_NEW_ID: u32 = 123;

    let Some(remap) = sample("remap-paramid.vst3") else {
        eprintln!("remap-paramid not built; skipping");
        return;
    };
    let uid: [i8; 16] = AGAIN_PROCESSOR_UID.map(|b| b as i8);

    assert_eq!(
        remap.remap_param_id(&uid, 0),
        Some(EXPECTED_NEW_ID),
        "the sample maps AGain's param 0 onto {EXPECTED_NEW_ID}; a different \
         answer means the UID or the id is not reaching the plugin"
    );
    assert_eq!(
        remap.remap_param_id(&uid, 999),
        None,
        "param 999 has no mapping, so the host must report None rather than a \
         stale or defaulted id"
    );
    assert_eq!(
        remap.remap_param_id(&[0i8; 16], 0),
        None,
        "a zero UID is not AGain, so the plugin refuses — a Some here means the \
         host is not passing the UID through"
    );
}

/// `INoteExpressionPhysicalUIMapping` — how physical controllers (x/y/pressure)
/// map onto note-expression ids.
#[test]
fn physical_ui_mapping_is_read_per_plugin() {
    let Some(nes) = sample("note-expression-synth.vst3") else {
        eprintln!("note-expression-synth not built; skipping");
        return;
    };
    let mapping = nes.physical_ui_mapping(0, 0);
    assert!(
        !mapping.is_empty(),
        "note-expression-synth implements INoteExpressionPhysicalUIMapping but \
         the host read no entries"
    );

    // A plugin without the interface must yield nothing rather than a default
    // table — otherwise the host would invent controller routings.
    if let Some(pn) = sample("pitch-names.vst3") {
        assert!(
            pn.physical_ui_mapping(0, 0).is_empty(),
            "pitch-names does not implement the interface, so the mapping must \
             be empty rather than a synthesised default"
        );
    }
}
