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
//! cargo nextest run -p tutti-vst3-host --features conformance --test conformance
//! ```
//!
//! `VST3_SDK_DIR` is optional. The SDK is an in-repo submodule and `build.rs`
//! compiles the `audio-probe` reference plugin from in-repo sources, so a
//! recursive checkout has everything this suite needs; the variable only
//! substitutes an external SDK.
//!
//! Nothing here skips: a missing artifact panics with its build command
//! (`require_probe`, `require_twin`, `require_sample`).
//!
//! The `harness_ready()` gate asserts (its two requirements are met by any
//! recursive checkout, so absence has one cause and one fix).
//!
//! The plugins the suites drive are all built from this repository: `audio-probe`
//! and the SDK samples by `build.rs`, and the C++ twin of the shared test plugin
//! by `tutti-shared-plugin-vst3-cpp` (through `tutti_fixtures::vst3_cpp_module`).
//! `build.rs` also compiles HostChecker's six validation `.cpp`s directly (they
//! need only the header-only `pluginterfaces`), and every check-driven test
//! runs against `audio-probe`.

// Each conformance test binary includes this module and uses a subset of it.
#![allow(dead_code)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tutti_fixtures::main_thread;
use tutti_graph::UmpAt;

use tutti_plugin_types::ParamId;
use tutti_vst3_host::{
    AudioBuffer, InputEvents, LoadOptions, ParameterChanges, ProcessMode, TransportInfo,
    Vst3Loaded, Vst3Sample, host::conformance,
};
#[path = "active.rs"]
pub mod active;
#[allow(unused_imports)]
pub use active::{Activate, Active, ActiveExt, LoadError, ParamExt, StateExt};

// ── HostCheck C ABI (tests/support/hostcheck_shim.cpp) ───────────────────────

unsafe extern "C" {
    pub fn hc_num_log_events() -> c_int;
    pub fn hc_log_description(id: c_int) -> *const c_char;
    pub fn hc_log_severity(id: c_int) -> *const c_char;
    pub fn hc_configure(
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
    pub fn hc_add_parameter(param_id: c_uint);
    pub fn hc_validate(
        data: *const c_void,
        min_in: c_int,
        min_out: c_int,
        counts: *mut c_longlong,
    ) -> c_int;
}

/// Serialises the `hc_*` calls that change or read `HostCheck`.
///
/// The shim (`hostcheck_shim.cpp`) keeps process-global state with no lock of
/// its own: `HostCheck::Instance()`, the stub `gComponent` whose bus-count
/// vectors `hc_configure` reassigns, the event log that `hc_configure` and
/// `hc_validate` reset and fill, and the parameter-id table `hc_add_parameter`
/// grows. Two tests calling in at once see each other's setup (a spurious
/// "sample size" or "process mode" mismatch) and corrupt the heap.
///
/// Hold it across `hc_configure` and `hc_validate` together, and around each
/// `hc_add_parameter`; it covers no plugin work.
static HOSTCHECK: Mutex<()> = Mutex::new(());

/// Acquire the `HostCheck` lock, ignoring poisoning.
pub fn hostcheck_lock() -> std::sync::MutexGuard<'static, ()> {
    HOSTCHECK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Serialises every test of `vst3_conformance_modes`, the one conformance
/// binary that still holds it: with the `HostCheck` lock alone it failed once
/// in 5,400 runs, and the cause is not found. Not a module-lifecycle guard;
/// `Module::open` and teardown serialise themselves.
pub static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning (a failed test elsewhere must
/// not cascade into spurious failures here).
pub fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub const AVAILABLE: &str = env!("VST3_HOSTCHECK_AVAILABLE");

/// The bundles `build.rs` builds from the vendored SDK — `audio-probe` and
/// `multiple-program-changes`. Present on any recursive checkout, because they
/// are compiled by this very `cargo test` invocation.
pub const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// `ProcessModes_::kOffline`. Lets tests assert the plugin observed the mode we
/// asked for, independent of the enum's Rust-side representation.
pub const K_OFFLINE: c_int = 2;

/// `kParamObservedProcessMode` in `audio-probe/source/probeids.h`: the process
/// mode the plugin saw, as `processMode * 0.5`.
pub const K_PARAM_OBSERVED_PROCESS_MODE: u32 = 106;

// ── Findings ─────────────────────────────────────────────────────────────────

/// One spec violation: which check fired, how often, and its severity.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: String,
    pub description: String,
    pub count: i64,
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

pub fn findings_from(counts: &[i64]) -> Vec<Finding> {
    counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
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
pub const KNOWN_FALSE_POSITIVES: &[(&str, &str)] = &[(
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
pub fn errors(findings: &[Finding]) -> Vec<&Finding> {
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
pub fn resolve_bundle(path: &Path) -> PathBuf {
    tutti_plugin_types::bundle::any_module_in_bundle(
        path,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or_else(|| path.to_path_buf())
}

/// Every sample plugin in [`PROBE_DIR_BUILT`], as (name, resolved binary path).
pub fn sample_plugins() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(Path::new(PROBE_DIR_BUILT)) else {
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
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// Whether the plugin declares at least one event input bus, i.e. whether it
/// is legal for a host to send it MIDI at all.
pub fn accepts_midi(path: &Path) -> bool {
    Active::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.loaded().plugin_info().has_midi_input)
        .unwrap_or(false)
}

/// The plugin's first declared ParamID, if it has any parameters.
pub fn first_parameter_id(path: &Path) -> Option<u32> {
    let inst = Active::<f32>::load(path, 48_000.0, 512).ok()?;
    inst.loaded()
        .parameter_ids(&main_thread())
        .first()
        .map(|id| id.get())
}

/// Assert the harness is usable, **panicking with the reason when it is not**.
///
/// # Why this is not a skip
///
/// A gate that returns `false` and lets its call sites `return` early turns
/// every test into a pass that executed nothing. That claims coverage of the
/// `ProcessData` this host builds while running zero assertions, and a
/// regression in it would ship silently.
///
/// Both requirements are satisfied by a recursive checkout and nothing else:
/// the hostchecker sources live in the `public.sdk` submodule, and `build.rs`
/// compiles `audio-probe` into [`PROBE_DIR_BUILT`] from in-repo sources. Neither
/// absence is environmental, so neither should be tolerated — an unusable
/// harness has exactly one cause and one fix, and the panic names both.
///
/// The one genuinely conditional case is the `conformance` feature itself,
/// which the `#![cfg(feature = "conformance")]` at the top of this file already
/// handles: without it, the file does not compile in and nothing claims to
/// have run.
#[track_caller]
pub fn harness_ready() -> bool {
    assert_eq!(
        AVAILABLE, "1",
        "the HostChecker sources are missing, so this suite cannot validate \
         anything. They ship inside the `public.sdk` VST3 submodule, so this \
         means the submodules are not checked out.\n\
         Run:  git submodule update --init --recursive\n\
         (or set VST3_SDK_DIR to an external SDK checkout.)"
    );
    assert!(
        !sample_plugins().is_empty(),
        "no VST3 plugin to drive. `build.rs` builds `audio-probe` from \
         tests/support/audio-probe into {PROBE_DIR_BUILT:?} whenever the \
         `conformance` feature is on, so an empty list means the build did not \
         produce it — not that this machine lacks plugins."
    );
    true
}

/// Resolve a sample plugin that **must** be present, panicking by name if not.
///
/// Reaching this panic means a plugin `build.rs` produces went missing, which is
/// a build failure and not an environment.
#[track_caller]
pub fn require_sample(bundle: &str) -> Vst3Loaded {
    let path = sample_path(bundle).unwrap_or_else(|| {
        panic!(
            "{bundle} is not present. `build.rs` builds audio-probe, \
             multiple-program-changes and remap-paramid from the vendored SDK \
             into {PROBE_DIR_BUILT:?} on every conformance build, so this means \
             the build did not produce it. Plugins found: {:?}",
            sample_plugins()
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        )
    });
    Vst3Loaded::load(&main_thread(), &path, LoadOptions::new())
        .unwrap_or_else(|e| panic!("{bundle} is present at {path:?} but failed to load: {e:?}"))
}

// ── Driving one block through the real host path ─────────────────────────────

/// One `process` call's worth of host input.
#[derive(Default, Clone, Copy)]
pub struct Block<'a> {
    pub frames: usize,
    pub midi: &'a [UmpAt],
    pub params: Option<&'a ParameterChanges>,
    pub transport: Option<&'a TransportInfo>,
}

impl<'a> Block<'a> {
    pub fn of(frames: usize) -> Self {
        Self {
            frames,
            ..Default::default()
        }
    }
    pub fn midi(mut self, midi: &'a [UmpAt]) -> Self {
        self.midi = midi;
        self
    }
    pub fn transport(mut self, t: &'a TransportInfo) -> Self {
        self.transport = Some(t);
        self
    }
}

/// Load `path`, activate it as `T` (f32 or f64), and run one `process` block,
/// validating the `ProcessData` our host built.
pub fn drive_block<T: Vst3Sample + Default + Copy>(
    path: &Path,
    frames: usize,
    sample_rate: f64,
    block_size: usize,
    midi: &[UmpAt],
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
    pub fn maybe_params(mut self, params: Option<&'a ParameterChanges>) -> Self {
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
pub fn drive_blocks<T: Vst3Sample + Default + Copy>(
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
pub fn drive_blocks_in_mode<T: Vst3Sample + Default + Copy>(
    path: &Path,
    sample_rate: f64,
    block_size: usize,
    mode: ProcessMode,
    blocks: &[Block<'_>],
) -> Result<Vec<Vec<Finding>>, String> {
    let mut inst = Active::<T>::load_with_mode(path, sample_rate, block_size, mode)
        .map_err(|e| format!("load failed: {e:?}"))?;

    let info = inst.loaded().plugin_info().clone();
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
    for id in inst.loaded().parameter_ids(&main_thread()) {
        let _hc = hostcheck_lock();
        unsafe { hc_add_parameter(id.get() as c_uint) };
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
        let _hc = hostcheck_lock();
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
        let events = InputEvents {
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

/// A VST3 `ParamID` as the automation vocabulary's address.
///
/// VST3 ids are opaque: the format hands out numbers whose meaning only the
/// plugin knows.
pub fn param_address(tag: u32) -> ParamId {
    ParamId::new(tag)
}

/// Loads `audio-probe`, the reference plugin `build.rs` builds from
/// `tests/support/audio-probe` on every `conformance` build, panicking with the
/// build command when it is absent.
#[track_caller]
pub fn require_probe() -> Vst3Loaded {
    require_sample("audio-probe.vst3")
}

/// The path of `audio-probe`'s module, panicking with the build command when it
/// is absent.
#[track_caller]
pub fn probe_path() -> PathBuf {
    sample_path("audio-probe.vst3").unwrap_or_else(|| {
        panic!(
            "audio-probe.vst3 is not present in {PROBE_DIR_BUILT:?}. `build.rs` builds it \
             when the `conformance` feature is on:\n\n  \
             cargo build -p tutti-vst3-host --features conformance --tests\n"
        )
    })
}

/// Loads the C++ twin of the shared test plugin (`testing/tutti-shared-plugin-vst3-cpp`).
/// Cargo builds it with the tests, through `tutti-fixtures`' `vst3-cpp` feature,
/// so an absent module is a failed build, and the panic names the command.
#[track_caller]
pub fn require_twin() -> Vst3Loaded {
    let module = tutti_fixtures::vst3_cpp_module();
    Vst3Loaded::load(&main_thread(), &module, LoadOptions::new()).unwrap_or_else(|e| {
        panic!(
            "the C++ twin at {} failed to load: {e:?}; rebuild it with \
             `cargo build -p tutti-shared-plugin-vst3-cpp`",
            module.display()
        )
    })
}

/// Whether the plugin advertises `kSample64` processing.
pub fn supports_f64(path: &Path) -> bool {
    Active::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.loaded().plugin_info().supports_f64)
        .unwrap_or(false)
}

// ── Optional controller interfaces ───────────────────────────────────────────
//
// Each assertion is anchored to a figure the C++ twin publishes (`require_twin`)
// and to a plugin without the interface (`require_probe`), so a host that
// returned a constant, or ignored its arguments, cannot satisfy both.

/// Locate a sample bundle by file name in [`PROBE_DIR_BUILT`].
pub fn sample_path(name: &str) -> Option<PathBuf> {
    (!PROBE_DIR_BUILT.is_empty())
        .then(|| resolve_bundle(&Path::new(PROBE_DIR_BUILT).join(name)))
        .filter(|p| p.is_file())
}
