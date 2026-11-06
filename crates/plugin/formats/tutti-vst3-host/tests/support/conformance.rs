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
//! Neither env var is required. The SDK is an in-repo submodule and `build.rs`
//! compiles the `audio-probe` reference plugin from in-repo sources, so a
//! recursive checkout has everything this suite needs; `VST3_SDK_DIR` and
//! `VST3_SAMPLE_PLUGIN_DIR` only substitute an external SDK or an external
//! plugin tree.
//!
//! **Nothing here skips silently.** Every test either asserts or carries an
//! explicit `#[ignore = "..."]`; there is no `eprintln!("skipping"); return;`
//! in the file. That shape reports `ok` while executing nothing, which is worse
//! than having no suite, because it claims coverage of the `ProcessData` this
//! host builds.
//!
//! The `harness_ready()` gate asserts (its two requirements are met by any
//! recursive checkout, so absence has one cause and one fix). There are no
//! per-plugin skip gates either: `sample`/`sample_plugins` search the in-repo
//! bundle directory as well as the external one, and the lookups that must
//! succeed go through `require_sample` / `require_host_checker`, which panic
//! by name.
//!
//! # What is `#[ignore]`d, and why
//!
//! 18 tests need `host-checker` or `note-expression-synth`. Both SDK samples
//! ship a controller that **inherits from** `VSTGUI::VST3EditorDelegate`, and
//! each sample's `factory.cpp` registers that controller — so the UI
//! translation unit sits on the only path to `GetPluginFactory`. VSTGUI is a
//! separate Steinberg repository and is not among this repo's three pinned
//! submodules, so `build.rs` has nothing to compile it against. Run them with
//! `--ignored` against a `VST3_SAMPLE_PLUGIN_DIR` that holds an external SDK
//! build.
//!
//! This does **not** weaken the checks themselves: `build.rs` compiles
//! HostChecker's six validation `.cpp`s directly (they need only the
//! header-only `pluginterfaces`, never the controller), and every check-driven
//! test runs against the in-repo `audio-probe`.

// Each conformance test binary includes this module and uses a subset of it.
#![allow(dead_code)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_plugin_types::ParamId;
use tutti_vst3_host::{
    AudioBuffer, InputEvents, LoadOptions, MidiEvent, ParameterChanges, ProcessMode, TransportInfo,
    Vst3Loaded, Vst3Sample, host::conformance,
};
#[path = "active.rs"]
pub mod active;
#[allow(unused_imports)]
pub use active::{Activate, Active, LoadError, ParamExt, StateExt};

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

/// Serializes *all* plugin work in this binary.
///
/// Two separate reasons, both real:
/// - `HostCheck` is a process-global singleton, so a configure→drive→read
///   sequence must not interleave with another test's.
/// - VST3 module lifecycle is not thread-safe here: loading and unloading the
///   same DSO concurrently races module init/exit and segfaults. Every test
///   that constructs a `Active`/`Vst3Loaded` must hold this, not just the
///   ones that touch `HostCheck`.
///
/// Held for the whole of each test rather than per-call, since instances are
/// dropped at end of scope and the unload is half the race.
pub static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning (a failed test elsewhere must
/// not cascade into spurious failures here).
pub fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub const AVAILABLE: &str = env!("VST3_HOSTCHECK_AVAILABLE");
pub const SAMPLE_PLUGIN_DIR: &str = env!("VST3_SAMPLE_PLUGIN_DIR");

/// The bundles `build.rs` builds from the vendored SDK — `audio-probe` and
/// `multiple-program-changes`. Present on any recursive checkout, because they
/// are compiled by this very `cargo test` invocation.
///
/// [`sample_plugins`] searches this as well as `VST3_SAMPLE_PLUGIN_DIR`, which
/// names a hand-built external SDK tree and is unset on essentially every
/// machine. The sibling suites (`vst3_audio_correctness.rs`,
/// `support/gui_lifecycle.rs`) fall back here for the same reason.
pub const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// `ProcessModes_::kOffline`. Lets tests assert the plugin observed the mode we
/// asked for, independent of the enum's Rust-side representation.
pub const K_OFFLINE: c_int = 2;

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

/// Every sample plugin we can find, as (name, resolved binary path).
///
/// Searches the external `VST3_SAMPLE_PLUGIN_DIR` first, then the in-repo
/// [`PROBE_DIR_BUILT`], so an externally built SDK tree still wins when one is
/// configured and the in-repo bundles are used otherwise. Both are read, not
/// just the first non-empty one: a machine with an external tree should get its
/// plugins *and* the probe, since several checks below want more than one
/// plugin to be meaningful.
pub fn sample_plugins() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in [SAMPLE_PLUGIN_DIR, PROBE_DIR_BUILT] {
        if dir.is_empty() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(Path::new(dir)) else {
            continue;
        };
        out.extend(
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "vst3"))
                .filter_map(|p| {
                    let name = p.file_stem()?.to_str()?.to_string();
                    let bin = resolve_bundle(&p);
                    bin.is_file().then_some((name, bin))
                }),
        );
    }
    out.sort();
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// Whether the plugin declares at least one event input bus, i.e. whether it
/// is legal for a host to send it MIDI at all.
pub fn accepts_midi(path: &Path) -> bool {
    Active::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.plugin_info().has_midi_input)
        .unwrap_or(false)
}

/// The plugin's first declared ParamID, if it has any parameters.
pub fn first_parameter_id(path: &Path) -> Option<u32> {
    let inst = Active::<f32>::load(path, 48_000.0, 512).ok()?;
    inst.parameter_ids().first().map(|id| id.get())
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
         produce it — not that this machine lacks plugins. \
         (VST3_SAMPLE_PLUGIN_DIR={SAMPLE_PLUGIN_DIR:?} is the optional external \
         override and may legitimately be unset.)"
    );
    true
}

/// The reason `host-checker` and `note-expression-synth` are unavailable, and
/// the text every `#[ignore]` on a test needing one repeats.
///
/// Both samples ship a controller that **inherits from**
/// `VSTGUI::VST3EditorDelegate` (`hostcheckercontroller.h:110`,
/// `note_expression_synth_ui.h:36`), and each sample's `factory.cpp` registers
/// that controller — so the UI translation unit is not optional, it sits on the
/// only path to `GetPluginFactory`. VSTGUI is a separate Steinberg repository
/// and is **not among this repo's three pinned submodules** (`base`,
/// `pluginterfaces`, `public.sdk`), so `build.rs` has nothing to compile it
/// against.
///
/// This is the one genuinely environmental gap in this file, and it is stated as
/// `#[ignore]` rather than a printed skip so it is visible in the run summary
/// instead of hiding inside a passing test. Set `VST3_SAMPLE_PLUGIN_DIR` to an
/// externally built SDK tree and run with `--ignored` to execute them.
///
/// Note this does **not** affect the HostChecker *validation modules*:
/// `build.rs` compiles those six `.cpp`s directly and they depend only on the
/// header-only `pluginterfaces`, never on the controller. Every check-driven
/// test in this file runs against `audio-probe`.
pub const NEEDS_VSTGUI: &str = "needs a VST3 SDK sample whose controller inherits \
     from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned \
     submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run \
     with --ignored.";

/// Resolve a sample plugin that **must** be present, panicking by name if not.
///
/// Used instead of `let Some(x) = sample(..) else { eprintln!("skipping");
/// return; }`, a shape that makes a test report `ok` while executing nothing.
///
/// A test whose plugin genuinely cannot be built carries `#[ignore]` instead —
/// see [`NEEDS_VSTGUI`]. So reaching this panic means a plugin `build.rs` *does*
/// produce went missing, which is a build failure and not an environment.
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
    Vst3Loaded::load(&path, LoadOptions::new())
        .unwrap_or_else(|e| panic!("{bundle} is present at {path:?} but failed to load: {e:?}"))
}

// ── Driving one block through the real host path ─────────────────────────────

/// One `process` call's worth of host input.
#[derive(Default, Clone, Copy)]
pub struct Block<'a> {
    pub frames: usize,
    pub midi: &'a [MidiEvent],
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
    pub fn midi(mut self, midi: &'a [MidiEvent]) -> Self {
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
    // NOTE: the caller must already hold `PLUGIN_LOCK` — see its docs. Taking
    // it here instead would deadlock against the helpers (`accepts_midi`,
    // `first_parameter_id`, ...) that load plugins around this call.
    let mut inst = Active::<T>::load_with_mode(path, sample_rate, block_size, mode)
        .map_err(|e| format!("load failed: {e:?}"))?;

    let info = inst.plugin_info().clone();
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
    for id in inst.parameter_ids() {
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
/// from the header rather than guessed: an off-by-five here decodes the
/// neighbouring `kProcessContext*` readouts as warning bitfields and invents
/// "spec errors" that do not exist. [`warn_tag_block_is_isolated`] pins the
/// value against that class of mistake.
pub const K_PROCESS_WARN_TAG: u32 = 1027;
/// `kParamProcessModeTag`, immediately below the warning block; it checks
/// the block's lower bound.
pub const K_PARAM_PROCESS_MODE_TAG: u32 = 1026;
/// Parameters in the warning block.
pub const K_PARAM_WARN_COUNT: u32 = 8;
/// Bits packed into each warning parameter.
pub const K_PARAM_WARN_BIT_COUNT: u32 = 24;

/// A VST3 `ParamID` as the automation vocabulary's address.
///
/// The tags above stay bare `u32` because they are arithmetic (`TAG + i`) and
/// bounds checks; only the queue lookups need the address form. VST3 ids are
/// `Opaque` — the format hands out numbers whose meaning only the plugin knows.
pub fn param_address(tag: u32) -> ParamId {
    ParamId::new(tag)
}

/// Decode the log ids HostChecker packed into one `outputParameterChanges`
/// set. Returns the ids, which index the same table as [`hc_log_description`].
pub fn decode_hostchecker_warnings(out: &ParameterChanges) -> Vec<i32> {
    let mut ids = Vec::new();
    for i in 0..K_PARAM_WARN_COUNT {
        let Some(queue) = out.queue(param_address(K_PROCESS_WARN_TAG + i)) else {
            continue;
        };
        let Some(point) = queue.points.last() else {
            continue;
        };
        // The plugin sends `bits / 2^24`; undo that to recover the bitfield.
        let bits = (point.value.get() * f64::from(1u32 << K_PARAM_WARN_BIT_COUNT)).round() as u32;
        for bit in 0..K_PARAM_WARN_BIT_COUNT {
            if bits & (1 << bit) != 0 {
                ids.push((i * K_PARAM_WARN_BIT_COUNT + bit) as i32);
            }
        }
    }
    ids
}

/// Resolve a log id to its severity + description via the linked table.
pub fn describe_log_id(id: i32) -> Option<(String, String)> {
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
pub fn host_checker_path() -> Option<PathBuf> {
    sample_plugins()
        .into_iter()
        .find(|(name, _)| name == "host-checker")
        .map(|(_, path)| path)
}

/// [`host_checker_path`], panicking with the reason when it is absent.
///
/// Every caller is `#[ignore]`d with [`NEEDS_VSTGUI`], so on a default run this
/// is never reached. It exists for the `--ignored` run against an external SDK
/// tree: there, the plugin is expected to be present, and its absence should
/// name itself rather than reappear as a silent skip.
#[track_caller]
pub fn require_host_checker() -> PathBuf {
    host_checker_path().unwrap_or_else(|| {
        panic!(
            "host-checker is not available. {NEEDS_VSTGUI} Plugins found: {:?}",
            sample_plugins()
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        )
    })
}

/// Whether the plugin advertises `kSample64` processing.
pub fn supports_f64(path: &Path) -> bool {
    Active::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.plugin_info().supports_f64)
        .unwrap_or(false)
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
///
/// **The caller must already hold [`plugin_guard`].** This constructs a
/// `Vst3Loaded`, so it is bound by the rule on [`PLUGIN_LOCK`]: concurrent
/// load/unload of one DSO races the module entry/exit counter. It cannot take
/// the lock itself — several tests below load two bundles, and a per-call guard
/// would deadlock on the second.
pub fn sample(name: &str) -> Option<Vst3Loaded> {
    Vst3Loaded::load(&sample_path(name)?, LoadOptions::new()).ok()
}

/// Locate a sample bundle by file name, **searching both plugin directories**.
///
/// Searching `SAMPLE_PLUGIN_DIR` alone (the external, almost-always-unset tree)
/// would make every lookup miss. These tests name a specific bundle rather than
/// sweeping the corpus, so they need the in-repo directory too.
pub fn sample_path(name: &str) -> Option<PathBuf> {
    [SAMPLE_PLUGIN_DIR, PROBE_DIR_BUILT]
        .into_iter()
        .filter(|d| !d.is_empty())
        .map(|d| resolve_bundle(&Path::new(d).join(name)))
        .find(|p| p.is_file())
}
